//! Shared request helpers for the fcp and fcptemp JSON Lines APIs.
//!
//! This crate intentionally has no client-side async runtime. GUI and CLI
//! frontends can call it from a worker thread and share the same protocol code.

use serde_json::{json, Value};
use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(3);
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Shared endpoint set for clients that talk to both long-running services.
/// A CLI and GUI can each own one instance and use the same transport/error logic.
#[derive(Debug, Clone)]
pub struct ClientSet {
    fcp_socket: std::path::PathBuf,
    fcptemp_socket: std::path::PathBuf,
}

/// Result of handing a fan from temperature automation to manual control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomationHandoff {
    /// The rule was enabled and `fcptemp` successfully disabled it.
    RuleDisabled,
    /// No enabled rule was configured for the fan.
    NoEnabledRule,
    /// `fcptemp` could not be reached; the manual operation was still attempted.
    ServiceUnavailable(String),
}

/// Outcome of a manual command, including whether automation was handed off first.
#[derive(Debug, Clone)]
pub struct ManualControlResult {
    pub data: Value,
    pub automation: AutomationHandoff,
}

impl ClientSet {
    pub fn new(fcp_socket: impl Into<std::path::PathBuf>, fcptemp_socket: impl Into<std::path::PathBuf>) -> Self {
        Self { fcp_socket: fcp_socket.into(), fcptemp_socket: fcptemp_socket.into() }
    }

    /// Build endpoints from environment variables and the services' default socket locations.
    /// Explicit CLI overrides should be applied with `with_fcp_socket` / `with_fcptemp_socket`.
    pub fn from_env() -> Result<Self, String> {
        let home = env::var_os("HOME").filter(|value| !value.is_empty()).map(std::path::PathBuf::from);
        let runtime = env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()).map(std::path::PathBuf::from);

        let fcp_socket = env_path(&["FCP_SOCKET_PATH"])
            .or_else(|| existing_path("/run/fcp/control.sock"))
            .or_else(|| runtime.as_ref().map(|path| path.join("fcp.sock")))
            .or_else(|| home.as_ref().map(|path| path.join(".local/state/fancontrol-custom-pwm/fcp.sock")))
            .ok_or_else(|| "cannot determine fcp socket path; set FCP_SOCKET_PATH".to_owned())?;

        let fcptemp_socket = env_path(&["FCPTEMP_SOCKET_PATH"])
            .or_else(|| existing_path("/run/fcptemp/control.sock"))
            .or_else(|| runtime.as_ref().map(|path| path.join("fcptemp.sock")))
            .or_else(|| home.as_ref().map(|path| path.join(".local/state/fcptemp/fcptemp.sock")))
            .ok_or_else(|| "cannot determine fcptemp socket path; set FCPTEMP_SOCKET_PATH".to_owned())?;

        Ok(Self::new(fcp_socket, fcptemp_socket))
    }

    pub fn with_fcp_socket(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.fcp_socket = expand_home(path.into());
        self
    }

    pub fn with_fcptemp_socket(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.fcptemp_socket = expand_home(path.into());
        self
    }

    pub fn fcp(&self, request_value: &Value) -> Result<Value, String> {
        request(&self.fcp_socket, request_value)
    }

    pub fn fcptemp(&self, request_value: &Value) -> Result<Value, String> {
        request(&self.fcptemp_socket, request_value)
    }

    /// Send a command to `fcp`, verify the standard API envelope and return its `data` value.
    pub fn fcp_command(&self, command: &str, parameters: Value) -> Result<Value, String> {
        self.call_command(&self.fcp_socket, command, parameters)
    }

    /// Send a command to `fcptemp`, verify the standard API envelope and return its `data` value.
    pub fn fcptemp_command(&self, command: &str, parameters: Value) -> Result<Value, String> {
        self.call_command(&self.fcptemp_socket, command, parameters)
    }

    /// Set the manual PWM throttle after disabling an enabled temperature rule for this fan.
    /// If `fcptemp` is unavailable, the call proceeds and reports that fact in the result.
    pub fn set_manual_throttle(&self, fan: &str, throttle: u8) -> Result<ManualControlResult, String> {
        if throttle > 100 {
            return Err("throttle must be an integer from 0 to 100".to_owned());
        }
        self.set_manual_settings(fan, json!({"label": fan, "throttle": throttle}))
    }

    /// Set the manual mode after disabling an enabled temperature rule for this fan.
    pub fn set_manual_mode(&self, fan: &str, mode: &str, throttle: Option<u8>) -> Result<ManualControlResult, String> {
        if !matches!(mode, "low" | "high" | "percent") {
            return Err("mode must be 'low', 'high', or 'percent'".to_owned());
        }
        if throttle.is_some_and(|value| value > 100) {
            return Err("throttle must be an integer from 0 to 100".to_owned());
        }
        let mut parameters = serde_json::Map::new();
        parameters.insert("label".to_owned(), json!(fan));
        parameters.insert("mode".to_owned(), json!(mode));
        if let Some(throttle) = throttle {
            parameters.insert("throttle".to_owned(), json!(throttle));
        }
        self.set_manual_settings(fan, Value::Object(parameters))
    }

    fn set_manual_settings(&self, fan: &str, parameters: Value) -> Result<ManualControlResult, String> {
        let automation = self.disable_rule_for_manual_control(fan)?;
        let data = self.fcp_command("fan.set", parameters)?;
        Ok(ManualControlResult { data, automation })
    }

    fn disable_rule_for_manual_control(&self, fan: &str) -> Result<AutomationHandoff, String> {
        let rules = match self.fcptemp_command("rules.list", json!({})) {
            Ok(data) => data,
            Err(error) => return Ok(AutomationHandoff::ServiceUnavailable(error)),
        };
        let rules = rules.get("rules").and_then(Value::as_array)
            .ok_or_else(|| "fcptemp returned an invalid rules.list response; manual control cancelled".to_owned())?;
        let matching = rules.iter().find(|entry| {
            entry.pointer("/rule/fan").and_then(Value::as_str) == Some(fan)
        });
        let Some(entry) = matching else { return Ok(AutomationHandoff::NoEnabledRule); };
        if entry.pointer("/rule/enabled").and_then(Value::as_bool) != Some(true) {
            return Ok(AutomationHandoff::NoEnabledRule);
        }
        self.fcptemp_command("control.set", json!({"fan": fan, "enabled": false}))
            .map(|_| AutomationHandoff::RuleDisabled)
            .map_err(|error| format!("cannot disable fcptemp rule for {fan}; manual change cancelled: {error}"))
    }

    fn call_command(&self, socket: &Path, command: &str, parameters: Value) -> Result<Value, String> {
        let mut object = match parameters {
            Value::Object(object) => object,
            _ => return Err("API command parameters must be a JSON object".to_owned()),
        };
        if object.contains_key("id") || object.contains_key("command") {
            return Err("API command parameters must not contain 'id' or 'command'".to_owned());
        }
        object.insert("id".to_owned(), json!(NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)));
        object.insert("command".to_owned(), json!(command));
        let response = request(socket, &Value::Object(object))?;
        ensure_ok(&response)?;
        response.get("data").cloned().ok_or_else(|| "API response is missing 'data'".to_owned())
    }

    pub fn fcp_socket(&self) -> &Path {
        &self.fcp_socket
    }

    pub fn fcptemp_socket(&self) -> &Path {
        &self.fcptemp_socket
    }
}

fn env_path(names: &[&str]) -> Option<std::path::PathBuf> {
    names.iter().find_map(|name| {
        env::var_os(name).filter(|value| !value.is_empty()).map(|value| expand_home(std::path::PathBuf::from(value)))
    })
}

fn existing_path(path: &str) -> Option<std::path::PathBuf> {
    let path = std::path::PathBuf::from(path);
    path.exists().then_some(path)
}

fn expand_home(path: std::path::PathBuf) -> std::path::PathBuf {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        if let Some(home) = env::var_os("HOME") {
            let mut expanded = std::path::PathBuf::from(home);
            if text.len() > 2 { expanded.push(&text[2..]); }
            return expanded;
        }
    }
    path
}

/// Send one JSON object and read one JSON response over a Unix stream socket.
///
/// The returned JSON value may represent an API-level failure (`ok: false`);
/// callers can use [`ensure_ok`] to convert that response into an error.
pub fn request(socket_path: impl AsRef<Path>, request: &Value) -> Result<Value, String> {
    let path = socket_path.as_ref();
    let mut stream = UnixStream::connect(path)
        .map_err(|error| format!("cannot connect to {}: {error}", path.display()))?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|error| format!("cannot set socket read timeout: {error}"))?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|error| format!("cannot set socket write timeout: {error}"))?;

    let mut payload = serde_json::to_vec(request)
        .map_err(|error| format!("cannot encode API request: {error}"))?;
    payload.push(b'\n');
    stream.write_all(&payload)
        .map_err(|error| format!("cannot write to {}: {error}", path.display()))?;
    stream.flush()
        .map_err(|error| format!("cannot flush request to {}: {error}", path.display()))?;

    let mut reader = BufReader::new(stream);
    let mut line = Vec::with_capacity(512);
    loop {
        let available = reader.fill_buf()
            .map_err(|error| format!("cannot read response from {}: {error}", path.display()))?;
        if available.is_empty() {
            if line.is_empty() {
                return Err(format!("{} closed without an API response", path.display()));
            }
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let length = newline.unwrap_or(available.len());
        if line.len().saturating_add(length) > MAX_RESPONSE_BYTES {
            return Err(format!("response from {} exceeds 64 KiB", path.display()));
        }
        line.extend_from_slice(&available[..length]);
        reader.consume(if newline.is_some() { length + 1 } else { length });
        if newline.is_some() {
            break;
        }
    }

    serde_json::from_slice(&line)
        .map_err(|error| format!("invalid JSON response from {}: {error}", path.display()))
}

/// Return the API error message when a response contains `ok: false`.
pub fn ensure_ok(response: &Value) -> Result<(), String> {
    match response.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        Some(false) => {
            let code = response.pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or("api_error");
            let message = response.pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("API request failed");
            Err(format!("{code}: {message}"))
        }
        None => Err("API response is missing boolean field 'ok'".to_owned()),
    }
}

/// Read a newline-delimited response from an already connected stream.
/// Useful to frontends that need to handle more than one request per connection.
pub fn read_json_line<R: BufRead>(reader: &mut R) -> Result<Value, String> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|error| format!("socket read failed: {error}"))?;
        if available.is_empty() {
            return if line.is_empty() {
                Err("socket closed before a response arrived".to_owned())
            } else {
                parse_response(&line)
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let length = newline.unwrap_or(available.len());
        if line.len().saturating_add(length) > MAX_RESPONSE_BYTES {
            return Err("response exceeds 64 KiB".to_owned());
        }
        line.extend_from_slice(&available[..length]);
        reader.consume(if newline.is_some() { length + 1 } else { length });
        if newline.is_some() {
            return parse_response(&line);
        }
    }
}

fn parse_response(bytes: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(bytes).map_err(|error| format!("invalid JSON response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ensure_ok_accepts_success() {
        assert!(ensure_ok(&json!({"ok": true})).is_ok());
    }

    #[test]
    fn ensure_ok_formats_api_error() {
        assert_eq!(
            ensure_ok(&json!({"ok": false, "error": {"code": "bad", "message": "nope"}})),
            Err("bad: nope".to_owned())
        );
    }
}
