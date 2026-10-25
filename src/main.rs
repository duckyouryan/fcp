mod config;
mod daemon;

use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::exit;
use std::time::Duration;

const USAGE: &str = "\
usage:
  fcp daemon              run the daemon (normally started by systemd)
  fcp fan list            show all fans
  fcp fan NAME PERCENT    manual: fixed speed, 0-100
  fcp fan NAME auto       follow the temperature curve from the config
  fcp fan NAME system     give the fan back to the system
  fcp config-path         print the config file location
";

fn path_from(var: &str, default: &str) -> PathBuf {
    env::var_os(var)
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(default), PathBuf::from)
}

pub fn config_path() -> PathBuf {
    path_from("FCP_CONFIG", "/etc/fcp.conf")
}
pub fn state_path() -> PathBuf {
    path_from("FCP_STATE", "/var/lib/fcp/state")
}
pub fn socket_path() -> PathBuf {
    path_from("FCP_SOCKET", "/run/fcp.sock")
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["daemon"] => daemon::run(),
        ["fan", "list"] => ask("list"),
        ["fan", name, value] => ask(&format!("set {name} {value}")),
        ["config-path"] => {
            println!("{}", config_path().display());
            Ok(())
        }
        [] | ["help" | "-h" | "--help"] => {
            print!("{USAGE}");
            Ok(())
        }
        _ => {
            eprint!("{USAGE}");
            exit(2)
        }
    };
    if let Err(e) = result {
        eprintln!("fcp: {e}");
        exit(1);
    }
}

/// Send one line to the daemon, print its reply.
fn ask(request: &str) -> Result<(), String> {
    let path = socket_path();
    let mut s = UnixStream::connect(&path).map_err(|e| {
        format!("cannot connect to {}: {e} (is `fcp daemon` running?)", path.display())
    })?;
    s.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    writeln!(s, "{request}").map_err(|e| e.to_string())?;
    let mut reply = String::new();
    s.read_to_string(&mut reply).map_err(|e| e.to_string())?;
    match reply.strip_prefix("error: ") {
        Some(msg) => Err(msg.trim_end().to_string()),
        None => {
            print!("{reply}");
            Ok(())
        }
    }
}
