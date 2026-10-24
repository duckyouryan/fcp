mod api;
mod app;
mod config;
mod control;
mod curve;
mod sensors;

use crate::app::{log_error, log_info, log_warn, App};
use crate::config::{Config, StateFile};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

fn main() {
    if let Err(message) = run() {
        log_error(message);
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    install_signal_handlers()?;
    let home = home_dir();
    let config_override = configured_path("FCPTEMP_CONFIG_PATH");
    let config_path = match config_override.clone() {
        Some(path) => path,
        None => home.as_ref()
            .map(|path| path.join(".config/fcptemp/config.json"))
            .ok_or_else(|| "HOME is not set; set FCPTEMP_CONFIG_PATH explicitly".to_owned())?,
    };
    let state_path = match configured_path("FCPTEMP_STATE_PATH") {
        Some(path) => path,
        None => home.as_ref()
            .map(|path| path.join(".local/state/fcptemp/state.json"))
            .ok_or_else(|| "HOME is not set; set FCPTEMP_STATE_PATH explicitly".to_owned())?,
    };
    let socket_path = match configured_path("FCPTEMP_SOCKET_PATH") {
        Some(path) => path,
        None => match env::var_os("XDG_RUNTIME_DIR") {
            Some(path) if !path.is_empty() => PathBuf::from(path).join("fcptemp.sock"),
            _ => home.as_ref()
                .map(|path| path.join(".local/state/fcptemp/fcptemp.sock"))
                .ok_or_else(|| "HOME and XDG_RUNTIME_DIR are not set; set FCPTEMP_SOCKET_PATH explicitly".to_owned())?,
        },
    };

    log_info(format!("starting fcptemp; config={}", config_path.display()));
    log_info(format!("state file: {}", state_path.display()));
    log_info(format!("control socket: {}", socket_path.display()));
    if config_override.is_none() {
        create_default_config_if_missing(&config_path)?;
    }
    let config = Config::load(&config_path)?;
    let state = StateFile::load_or_default(&state_path)?;
    let app = Arc::new(App::new(config, state, state_path));
    app.validate_state()?;
    if let Err(error) = app.save_state() {
        return Err(format!("cannot initialize state file: {error}"));
    }

    let listener = bind_socket(&socket_path)
        .map_err(|error| format!("cannot create control socket {}: {error}", socket_path.display()))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let control_app = Arc::clone(&app);
    let control_shutdown = Arc::clone(&shutdown);
    let control_thread = match thread::Builder::new()
        .name("fcptemp-control".to_owned())
        .spawn(move || control::run_loop(control_app, control_shutdown))
    {
        Ok(handle) => handle,
        Err(error) => {
            drop(listener);
            let _ = fs::remove_file(&socket_path);
            return Err(format!("cannot start temperature control thread: {error}"));
        }
    };

    log_info(format!("fcptemp ready; listening on {}", socket_path.display()));
    let serve_result = serve(&listener, Arc::clone(&app));

    log_info("shutdown requested; stopping temperature controller");
    shutdown.store(true, Ordering::SeqCst);
    if control_thread.join().is_err() {
        log_warn("temperature control thread panicked during shutdown");
    }
    drop(listener);
    match fs::remove_file(&socket_path) {
        Ok(()) => log_info("control socket removed"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => log_warn(format!("cannot remove socket {}: {error}", socket_path.display())),
    }
    log_info("fcptemp stopped; fcp fan settings were left unchanged");
    serve_result
}

fn create_default_config_if_missing(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot inspect config path {}: {error}", path.display())),
    }
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create config directory {}: {error}", parent.display()))?;
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(format!("cannot create default config {}: {error}", path.display())),
    };
    file.write_all(include_bytes!("../examples/fcptemp.json"))
        .map_err(|error| format!("cannot write default config {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync default config {}: {error}", path.display()))?;
    log_info(format!("created default config at {}", path.display()));
    Ok(())
}

fn install_signal_handlers() -> Result<(), String> {
    // SAFETY: the signal handler performs only an atomic store.
    unsafe {
        let handler = handle_signal as *const () as libc::sighandler_t;
        if libc::signal(libc::SIGINT, handler) == libc::SIG_ERR {
            return Err("cannot install SIGINT handler".to_owned());
        }
        if libc::signal(libc::SIGTERM, handler) == libc::SIG_ERR {
            return Err("cannot install SIGTERM handler".to_owned());
        }
    }
    Ok(())
}

fn home_dir() -> Option<PathBuf> {
    match env::var_os("HOME") {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => None,
    }
}

fn configured_path(variable: &str) -> Option<PathBuf> {
    match env::var_os(variable) {
        Some(value) if !value.is_empty() => Some(expand_home(PathBuf::from(value))),
        _ => None,
    }
}

fn expand_home(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        if let Some(home) = env::var_os("HOME") {
            let mut expanded = PathBuf::from(home);
            if text.len() > 2 { expanded.push(&text[2..]); }
            return expanded;
        }
    }
    path
}

fn bind_socket(path: &Path) -> io::Result<UnixListener> {
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => match UnixStream::connect(path) {
            Ok(_) => return Err(io::Error::new(io::ErrorKind::AddrInUse, "socket is already active")),
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused || error.kind() == io::ErrorKind::NotFound => {
                fs::remove_file(path)?;
                log_info(format!("removed stale socket {}", path.display()));
            }
            Err(error) => return Err(error),
        },
        Ok(_) => return Err(io::Error::new(io::ErrorKind::AlreadyExists, "socket path exists and is not a socket")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn serve(listener: &UnixListener, app: Arc<App>) -> Result<(), String> {
    while !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let app = Arc::clone(&app);
                if let Err(error) = thread::Builder::new().name("fcptemp-client".to_owned()).spawn(move || api::handle_client(stream, app)) {
                    log_warn(format!("cannot start API client thread: {error}"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("control socket accept failed: {error}")),
        }
    }
    Ok(())
}
