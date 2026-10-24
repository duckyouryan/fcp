mod api;
mod config;
mod fan;
mod logging;

use crate::api::{default_socket_path, handle_client, App};
use crate::config::{Config, FanSettings, StateFile};
use crate::fan::{spawn_worker, FanControl};
use crate::logging::{error, info, warn};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

fn main() {
    if let Err(message) = run() {
        error(message);
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    install_signal_handlers()?;

    let home = home_dir();
    let config_override = configured_path("CONFIG_PATH");
    let config_path = match config_override.clone() {
        Some(path) => path,
        None => home.as_ref()
            .map(|path| path.join(".local/share/fancontrol-custom-pwm/config.json"))
            .ok_or_else(|| "HOME is not set; set CONFIG_PATH explicitly".to_owned())?,
    };
    let state_path = match configured_path("STATE_PATH") {
        Some(path) => path,
        None => home.as_ref()
            .map(|path| path.join(".local/share/fancontrol-custom-pwm/state.json"))
            .ok_or_else(|| "HOME is not set; set STATE_PATH explicitly".to_owned())?,
    };
    let socket_path = match configured_path("SOCKET_PATH") {
        Some(path) => path,
        None => match env::var_os("XDG_RUNTIME_DIR") {
            Some(path) if !path.is_empty() => PathBuf::from(path).join("fcp.sock"),
            _ => home.as_ref().map(|path| default_socket_path(path))
                .ok_or_else(|| "HOME and XDG_RUNTIME_DIR are not set; set SOCKET_PATH explicitly".to_owned())?,
        },
    };

    info(format!("starting fcp; config={}", config_path.display()));
    info(format!("state file: {}", state_path.display()));
    info(format!("control socket: {}", socket_path.display()));

    if config_override.is_none() {
        create_default_config_if_missing(&config_path)?;
    }
    let config = Config::load(&config_path)?;
    info(format!("loaded profile '{}' at {} Hz", config.profile, config.frequency_hz));

    let labels: Vec<String> = config.fans.iter().map(|fan| fan.label.clone()).collect();
    let state = StateFile::load_or_default(&state_path, &labels)?;
    let mut fans = BTreeMap::new();
    for fan_config in config.fans.iter().cloned() {
        let settings = match state.fans.get(&fan_config.label) {
            Some(value) => value.clone(),
            None => FanSettings { mode: config::FanMode::Low, throttle: 50 },
        };
        let control = FanControl::new(
            fan_config.clone(),
            config.hwmon_name.clone(),
            config.hwmon_root.clone(),
            config.frequency_hz,
            settings,
        );
        info(format!(
            "configured fan {}: pwm_path={:?}, rpm_path={:?}",
            fan_config.label, fan_config.pwm_path, fan_config.rpm_path
        ));
        let _ = control.view(true);
        fans.insert(fan_config.label.clone(), control);
    }

    let app = Arc::new(App::new(fans, state_path));
    let listener = bind_socket(&socket_path).map_err(|error| {
        format!("cannot create control socket {}: {error}", socket_path.display())
    })?;
    if let Err(message) = app.save_state() {
        drop(listener);
        let _ = fs::remove_file(&socket_path);
        return Err(format!("cannot initialize state file: {message}"));
    }
    let mut worker_handles: Vec<JoinHandle<()>> = Vec::new();
    for fan in app.fans.values() {
        match spawn_worker(Arc::clone(fan)) {
            Ok(handle) => worker_handles.push(handle),
            Err(message) => {
                for item in app.fans.values() {
                    item.stop();
                }
                for handle in worker_handles {
                    if handle.join().is_err() {
                        error("fan worker panicked during startup cleanup");
                    }
                }
                drop(listener);
                let _ = fs::remove_file(&socket_path);
                return Err(message);
            }
        }
    }

    info(format!("fcp ready; listening on {}", socket_path.display()));
    let serve_result = serve(&listener, Arc::clone(&app));

    info("shutdown requested; stopping fan workers");
    for fan in app.fans.values() {
        fan.stop();
    }
    for handle in worker_handles {
        if handle.join().is_err() {
            error("fan worker panicked during shutdown");
        }
    }
    drop(listener);
    match fs::remove_file(&socket_path) {
        Ok(()) => info("control socket removed"),
        Err(error_value) if error_value.kind() == io::ErrorKind::NotFound => {}
        Err(error_value) => warn(format!("cannot remove socket {}: {error_value}", socket_path.display())),
    }
    info("fcp stopped");
    serve_result
}


fn create_default_config_if_missing(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(_) => return Ok(()),
        Err(error_value) if error_value.kind() == io::ErrorKind::NotFound => {}
        Err(error_value) => return Err(format!("cannot inspect config path {}: {error_value}", path.display())),
    }
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent)
        .map_err(|error_value| format!("cannot create config directory {}: {error_value}", parent.display()))?;
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error_value) if error_value.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error_value) => return Err(format!("cannot create default config {}: {error_value}", path.display())),
    };
    file.write_all(include_bytes!("../examples/thinkcentre-m79.json"))
        .map_err(|error_value| format!("cannot write default config {}: {error_value}", path.display()))?;
    file.sync_all()
        .map_err(|error_value| format!("cannot sync default config {}: {error_value}", path.display()))?;
    info(format!("created default ThinkCentre M79 config at {}", path.display()));
    Ok(())
}

fn install_signal_handlers() -> Result<(), String> {
    // SAFETY: the handler only performs an atomic store, which is safe in this signal context.
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
            if text.len() > 2 {
                expanded.push(&text[2..]);
            }
            return expanded;
        }
    }
    path
}

fn bind_socket(path: &Path) -> io::Result<UnixListener> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent)?;

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            match UnixStream::connect(path) {
                Ok(_) => return Err(io::Error::new(io::ErrorKind::AddrInUse, "socket is already active")),
                Err(error_value)
                    if error_value.kind() == io::ErrorKind::ConnectionRefused
                        || error_value.kind() == io::ErrorKind::NotFound =>
                {
                    fs::remove_file(path)?;
                    info(format!("removed stale socket {}", path.display()));
                }
                Err(error_value) => return Err(error_value),
            }
        }
        Ok(_) => return Err(io::Error::new(io::ErrorKind::AlreadyExists, "socket path exists and is not a socket")),
        Err(error_value) if error_value.kind() == io::ErrorKind::NotFound => {}
        Err(error_value) => return Err(error_value),
    }

    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn serve(listener: &UnixListener, app: Arc<App>) -> Result<(), String> {
    while !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _address)) => {
                let app = Arc::clone(&app);
                match thread::Builder::new().name("fcp-client".to_owned()).spawn(move || handle_client(stream, app)) {
                    Ok(_) => {}
                    Err(error_value) => error(format!("cannot start API client thread: {error_value}")),
                }
            }
            Err(error_value) if error_value.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error_value) if error_value.kind() == io::ErrorKind::Interrupted => {}
            Err(error_value) => {
                error(format!("control socket accept failed: {error_value}"));
                return Err(format!("control socket accept failed: {error_value}"));
            }
        }
    }
    Ok(())
}
