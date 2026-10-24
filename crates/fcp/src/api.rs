use crate::config::{FanMode, StateFile};
use crate::fan::FanControl;
use crate::logging::{error, info, warn};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::MutexGuard;

const MAX_REQUEST_BYTES: usize = 64 * 1024;

pub struct App {
    pub fans: BTreeMap<String, Arc<FanControl>>,
    state_path: PathBuf,
    persist_lock: Mutex<()>,
}

impl App {
    pub fn new(fans: BTreeMap<String, Arc<FanControl>>, state_path: PathBuf) -> Self {
        Self { fans, state_path, persist_lock: Mutex::new(()) }
    }

    fn lock_persist(&self) -> MutexGuard<'_, ()> {
        match self.persist_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error("recovering poisoned state-persistence lock");
                poisoned.into_inner()
            }
        }
    }

    pub fn save_state(&self) -> Result<(), String> {
        let _serial = self.lock_persist();
        let fans = self.fans.iter()
            .map(|(label, fan)| (label.clone(), fan.save_settings()))
            .collect();
        let state = StateFile { version: 1, fans };
        let data = serde_json::to_vec_pretty(&state)
            .map_err(|error| format!("cannot serialize state: {error}"))?;

        let parent = match self.state_path.parent() {
            Some(path) if !path.as_os_str().is_empty() => path,
            _ => Path::new("."),
        };
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create state directory {}: {error}", parent.display()))?;

        let filename = self.state_path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "state.json".to_owned());
        let temporary_path = parent.join(format!(".{filename}.tmp-{}", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary_path)
            .map_err(|error| format!("cannot open temporary state {}: {error}", temporary_path.display()))?;
        file.write_all(&data)
            .map_err(|error| format!("cannot write temporary state {}: {error}", temporary_path.display()))?;
        file.write_all(b"\n")
            .map_err(|error| format!("cannot finish temporary state {}: {error}", temporary_path.display()))?;
        file.sync_all()
            .map_err(|error| format!("cannot sync temporary state {}: {error}", temporary_path.display()))?;
        drop(file);
        fs::rename(&temporary_path, &self.state_path)
            .map_err(|error| format!("cannot replace state {}: {error}", self.state_path.display()))?;
        Ok(())
    }
}

pub fn handle_request(request: Value, app: &Arc<App>) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let command = match request.get("command").and_then(Value::as_str) {
        Some(value) => value,
        None => return api_error(id, "invalid_request", "missing string field 'command'"),
    };

    match command {
        "status" => {
            let fans: Vec<Value> = app.fans.values().map(|fan| {
                let view = fan.view(false);
                json!({
                    "label": view.label,
                    "mode": view.mode.map(|mode| mode.as_str()),
                    "throttle": view.throttle
                })
            }).collect();
            json!({"id": id, "ok": true, "data": {"fans": fans}})
        }
        "fans.list" => {
            let filter = match request.get("label") {
                None => None,
                Some(Value::String(label)) => Some(label.as_str()),
                Some(_) => return api_error(id, "invalid_parameter", "'label' must be a string"),
            };
            if let Some(label) = filter {
                let fan = match app.fans.get(label) {
                    Some(value) => value,
                    None => return api_error(id, "unknown_fan", format!("unknown fan: {label}")),
                };
                let view = fan.view(true);
                json!({"id": id, "ok": true, "data": {"fans": [view_to_json(view)]}})
            } else {
                let fans: Vec<Value> = app.fans.values().map(|fan| view_to_json(fan.view(true))).collect();
                json!({"id": id, "ok": true, "data": {"fans": fans}})
            }
        }
        "fan.set" => set_fan(id, &request, app),
        "fan.kickstart" => kickstart_fan(id, &request, app),
        _ => api_error(id, "unknown_command", format!("unknown command: {command}")),
    }
}

fn set_fan(id: Value, request: &Value, app: &Arc<App>) -> Value {
    let label = match request.get("label").and_then(Value::as_str) {
        Some(value) => value,
        None => return api_error(id, "invalid_parameter", "missing string field 'label'"),
    };
    let fan = match app.fans.get(label) {
        Some(value) => value,
        None => return api_error(id, "unknown_fan", format!("unknown fan: {label}")),
    };

    let supplied_mode = match request.get("mode") {
        None => None,
        Some(Value::String(value)) => match FanMode::parse(value) {
            Some(mode) => Some(mode),
            None => return api_error(id, "invalid_parameter", "mode must be 'low', 'high', or 'percent'"),
        },
        Some(_) => return api_error(id, "invalid_parameter", "'mode' must be a string"),
    };

    let supplied_throttle = match request.get("throttle") {
        None => None,
        Some(value) => match value.as_u64() {
            Some(number) if number <= 100 => Some(number as u8),
            _ => return api_error(id, "invalid_parameter", "'throttle' must be an integer from 0 to 100"),
        },
    };

    if supplied_mode.is_none() && supplied_throttle.is_none() {
        return api_error(id, "invalid_parameter", "provide at least one of 'mode' or 'throttle'");
    }

    let (_settings, revision, was_active) = fan.update_settings(supplied_mode, supplied_throttle);
    if was_active && !fan.wait_for_revision(revision) {
        warn(format!("{}: updated setting was not confirmed by the hardware worker", label));
    }
    if let Err(error_message) = app.save_state() {
        error(format!("cannot save state after setting {}: {error_message}", label));
    }
    let view = fan.view(true);
    info(format!("API fan.set handled for {label}"));
    json!({"id": id, "ok": true, "data": view_to_json(view)})
}

fn kickstart_fan(id: Value, request: &Value, app: &Arc<App>) -> Value {
    let label = match request.get("label").and_then(Value::as_str) {
        Some(value) => value,
        None => return api_error(id, "invalid_parameter", "missing string field 'label'"),
    };
    let fan = match app.fans.get(label) {
        Some(value) => value,
        None => return api_error(id, "unknown_fan", format!("unknown fan: {label}")),
    };

    match fan.request_kickstart() {
        Ok((active, reason)) => {
            if active {
                info(format!("{label}: kickstart succeeded"));
            } else if let Some(reason) = reason {
                info(format!("{label}: kickstart did not restore control: {reason}"));
            }
            json!({"id": id, "ok": true, "data": view_to_json(fan.view(true))})
        }
        Err(reason) => api_error(id, "kickstart_failed", reason),
    }
}

fn view_to_json(view: crate::fan::FanView) -> Value {
    json!({
        "label": view.label,
        "mode": view.mode.map(|mode| mode.as_str()),
        "throttle": view.throttle,
        "rpm": view.rpm
    })
}

fn api_error(id: Value, code: &str, message: impl Into<String>) -> Value {
    json!({"id": id, "ok": false, "error": {"code": code, "message": message.into()}})
}

pub fn handle_client(mut stream: std::os::unix::net::UnixStream, app: Arc<App>) {
    use std::io::BufReader;

    if let Err(error_value) = stream.set_read_timeout(Some(std::time::Duration::from_secs(30))) {
        warn(format!("cannot set API socket read timeout: {error_value}"));
    }
    if let Err(error_value) = stream.set_write_timeout(Some(std::time::Duration::from_secs(30))) {
        warn(format!("cannot set API socket write timeout: {error_value}"));
    }

    let reader_stream = match stream.try_clone() {
        Ok(value) => value,
        Err(error_value) => {
            error(format!("cannot clone client socket: {error_value}"));
            return;
        }
    };
    let mut reader = BufReader::new(reader_stream);
    loop {
        let line = match read_limited_line(&mut reader) {
            Ok(Some(value)) => value,
            Ok(None) => return,
            Err(error_value) if error_value.kind() == std::io::ErrorKind::InvalidData => {
                let response = api_error(Value::Null, "request_too_large", "request exceeds 64 KiB");
                let _ = write_response(&mut stream, &response);
                return;
            }
            Err(error_value) => {
                error(format!("error reading client request: {error_value}"));
                return;
            }
        };
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(request) => handle_request(request, &app),
            Err(error_value) => api_error(Value::Null, "invalid_json", error_value.to_string()),
        };
        if write_response(&mut stream, &response).is_err() {
            return;
        }
    }
}

fn read_limited_line<R: std::io::BufRead>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::ErrorKind;

    let mut line = Vec::with_capacity(256);
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() { Ok(None) } else { Ok(Some(line)) };
        }
        let newline_at = available.iter().position(|byte| *byte == b'\n');
        let length = newline_at.unwrap_or(available.len());
        if line.len().saturating_add(length) > MAX_REQUEST_BYTES {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "request too large"));
        }
        line.extend_from_slice(&available[..length]);
        let consumed = if newline_at.is_some() { length + 1 } else { length };
        reader.consume(consumed);
        if newline_at.is_some() {
            return Ok(Some(line));
        }
    }
}

fn write_response(stream: &mut std::os::unix::net::UnixStream, response: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(response).map_err(|error| format!("cannot serialize API response: {error}"))?;
    bytes.push(b'\n');
    stream.write_all(&bytes).map_err(|error| format!("cannot write API response: {error}"))
}

pub fn default_socket_path(home: &Path) -> PathBuf {
    PathBuf::from(home).join(".local/state/fancontrol-custom-pwm/fcp.sock")
}
