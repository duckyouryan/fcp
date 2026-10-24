use crate::app::{App, log_info, log_warn};
use crate::config::Rule;
use crate::sensors;
use fcp_client::{ensure_ok, request as socket_request};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

const MAX_REQUEST_BYTES: usize = 64 * 1024;

pub fn handle_request(request: Value, app: &Arc<App>) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let command = match request.get("command").and_then(Value::as_str) {
        Some(value) => value,
        None => return api_error(id, "invalid_request", "missing string field 'command'"),
    };

    match command {
        "status" => status(id, app),
        "sensors.list" => sensors_list(id, app),
        "rules.list" => rules_list(id, app),
        "rule.set" => set_rule(id, &request, app),
        "rule.delete" => delete_rule(id, &request, app),
        "control.set" => set_control(id, &request, app),
        _ => api_error(id, "unknown_command", format!("unknown command: {command}")),
    }
}

fn status(id: Value, app: &Arc<App>) -> Value {
    let state = app.state_snapshot();
    let runtime = app.runtime_snapshot();
    let fans: Vec<Value> = state.rules.values().map(|rule| {
        let current = runtime.get(&rule.fan).cloned().unwrap_or_default();
        json!({
            "fan": rule.fan,
            "enabled": rule.enabled,
            "sensor": rule.sensor,
            "status": if !state.automation_enabled { "automation_disabled" } else if !rule.enabled { "fan_disabled" } else { current.status.as_str() },
            "temperature_c": current.temperature_c,
            "calculated_throttle": current.calculated_throttle,
            "last_applied_throttle": current.last_applied_throttle,
            "error": current.error
        })
    }).collect();
    json!({
        "id": id,
        "ok": true,
        "data": {
            "automation_enabled": state.automation_enabled,
            "fcp_socket": app.config.fcp_socket,
            "poll_interval_ms": app.config.poll_interval_ms,
            "rule_count": state.rules.len(),
            "fans": fans
        }
    })
}

fn sensors_list(id: Value, app: &Arc<App>) -> Value {
    match sensors::discover(&app.config.hwmon_root) {
        Ok(found) => json!({
            "id": id,
            "ok": true,
            "data": {"sensors": found.into_iter().map(|sensor| sensor.view).collect::<Vec<_>>()}
        }),
        Err(message) => api_error(id, "sensor_scan_failed", message),
    }
}

fn rules_list(id: Value, app: &Arc<App>) -> Value {
    let state = app.state_snapshot();
    let runtime = app.runtime_snapshot();
    let rules: Vec<Value> = state.rules.values().map(|rule| {
        let current = runtime.get(&rule.fan).cloned().unwrap_or_default();
        json!({
            "rule": rule,
            "runtime": {
                "status": current.status,
                "temperature_c": current.temperature_c,
                "calculated_throttle": current.calculated_throttle,
                "last_applied_throttle": current.last_applied_throttle,
                "error": current.error
            }
        })
    }).collect();
    json!({
        "id": id,
        "ok": true,
        "data": {"automation_enabled": state.automation_enabled, "rules": rules}
    })
}

fn set_rule(id: Value, request: &Value, app: &Arc<App>) -> Value {
    let rule_value = match request.get("rule") {
        Some(value) => value.clone(),
        None => return api_error(id, "invalid_parameter", "missing object field 'rule'"),
    };
    let rule: Rule = match serde_json::from_value(rule_value) {
        Ok(value) => value,
        Err(error) => return api_error(id, "invalid_parameter", format!("invalid rule object: {error}")),
    };
    if let Err(error) = rule.validate() {
        return api_error(id, "invalid_rule", error);
    }

    let available_sensor = match sensors::discover(&app.config.hwmon_root) {
        Ok(sensors) => sensors.into_iter().find(|sensor| sensor.view.id == rule.sensor),
        Err(error) => return api_error(id, "sensor_scan_failed", error),
    };
    match available_sensor {
        Some(sensor) if sensor.view.available => {}
        Some(sensor) => return api_error(
            id,
            "sensor_unavailable",
            format!("sensor '{}' is unavailable: {}", rule.sensor, sensor.view.reason.unwrap_or_else(|| "unknown reason".to_owned())),
        ),
        None => return api_error(id, "unknown_sensor", format!("unknown sensor: {}", rule.sensor)),
    }

    if let Err(error) = ensure_fcp_fan_controlled(&app.config.fcp_socket, &rule.fan) {
        return api_error(id, "fan_unavailable", error);
    }

    let fan = rule.fan.clone();
    let _apply = app.lock_apply();
    if let Err(error) = app.install_rule(rule) {
        return api_error(id, "state_save_failed", error);
    }
    app.clear_send_cache(&fan);
    log_info(format!("saved temperature rule for {fan}"));
    json!({"id": id, "ok": true, "data": {"fan": fan, "saved": true}})
}

fn delete_rule(id: Value, request: &Value, app: &Arc<App>) -> Value {
    let fan = match request.get("fan").and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => value,
        _ => return api_error(id, "invalid_parameter", "missing non-empty string field 'fan'"),
    };
    let _apply = app.lock_apply();
    match app.remove_rule(fan) {
        Ok(true) => {
            log_info(format!("removed temperature rule for {fan}"));
            json!({"id": id, "ok": true, "data": {"fan": fan, "removed": true}})
        }
        Ok(false) => api_error(id, "unknown_rule", format!("no temperature rule for fan: {fan}")),
        Err(error) => api_error(id, "state_save_failed", error),
    }
}

fn set_control(id: Value, request: &Value, app: &Arc<App>) -> Value {
    let enabled = match request.get("enabled").and_then(Value::as_bool) {
        Some(value) => value,
        None => return api_error(id, "invalid_parameter", "missing boolean field 'enabled'"),
    };
    let fan = match request.get("fan") {
        None => None,
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.as_str()),
        Some(_) => return api_error(id, "invalid_parameter", "'fan' must be a non-empty string when specified"),
    };

    let _apply = app.lock_apply();
    if let Some(fan) = fan {
        let previous_enabled = {
            let mut state = app.lock_state();
            match state.rules.get_mut(fan) {
                Some(rule) => {
                    let previous = rule.enabled;
                    rule.enabled = enabled;
                    Some(previous)
                }
                None => None,
            }
        };
        let Some(previous_enabled) = previous_enabled else {
            return api_error(id, "unknown_rule", format!("no temperature rule for fan: {fan}"));
        };
        if let Err(error) = app.save_state() {
            if let Some(rule) = app.lock_state().rules.get_mut(fan) {
                rule.enabled = previous_enabled;
            }
            return api_error(id, "state_save_failed", error);
        }
        app.clear_send_cache(fan);
        app.update_runtime(fan, |runtime| {
            runtime.status = (if enabled { "waiting" } else { "fan_disabled" }).to_owned();
            runtime.error = None;
        });
        log_info(format!("automatic temperature control for {fan}: {}", if enabled { "enabled" } else { "disabled" }));
        json!({"id": id, "ok": true, "data": {"fan": fan, "enabled": enabled}})
    } else {
        let previous_enabled = {
            let mut state = app.lock_state();
            let previous = state.automation_enabled;
            state.automation_enabled = enabled;
            previous
        };
        if let Err(error) = app.save_state() {
            app.lock_state().automation_enabled = previous_enabled;
            return api_error(id, "state_save_failed", error);
        }
        app.clear_all_send_caches();
        log_info(format!("global temperature automation: {}", if enabled { "enabled" } else { "disabled" }));
        json!({"id": id, "ok": true, "data": {"automation_enabled": enabled}})
    }
}

fn ensure_fcp_fan_controlled(socket: &std::path::Path, label: &str) -> Result<(), String> {
    let request = json!({"id": "fcptemp-check", "command": "fans.list", "label": label});
    let response = socket_request(socket, &request).map_err(|error| format!("cannot query fcp: {error}"))?;
    ensure_ok(&response).map_err(|error| format!("fcp rejected fan query: {error}"))?;
    let fan = response.pointer("/data/fans/0")
        .ok_or_else(|| format!("fcp returned no data for fan '{label}'"))?;
    if fan.get("mode").and_then(Value::as_str).is_none() {
        return Err(format!("fan '{label}' has no active PWM control in fcp"));
    }
    Ok(())
}

fn api_error(id: Value, code: &str, message: impl Into<String>) -> Value {
    json!({"id": id, "ok": false, "error": {"code": code, "message": message.into()}})
}

pub fn handle_client(mut stream: UnixStream, app: Arc<App>) {
    if let Err(error) = stream.set_read_timeout(Some(std::time::Duration::from_secs(30))) {
        log_warn(format!("cannot set client read timeout: {error}"));
    }
    if let Err(error) = stream.set_write_timeout(Some(std::time::Duration::from_secs(30))) {
        log_warn(format!("cannot set client write timeout: {error}"));
    }
    let reader_stream = match stream.try_clone() {
        Ok(value) => value,
        Err(error) => {
            log_warn(format!("cannot clone client socket: {error}"));
            return;
        }
    };
    let mut reader = BufReader::new(reader_stream);
    loop {
        let line = match read_limited_line(&mut reader) {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                let _ = write_response(&mut stream, &api_error(Value::Null, "request_too_large", "request exceeds 64 KiB"));
                return;
            }
            Err(error) => {
                log_warn(format!("cannot read client request: {error}"));
                return;
            }
        };
        if line.iter().all(|byte| byte.is_ascii_whitespace()) { continue; }
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(request) => handle_request(request, &app),
            Err(error) => api_error(Value::Null, "invalid_json", error.to_string()),
        };
        if write_response(&mut stream, &response).is_err() { return; }
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
        let newline = available.iter().position(|byte| *byte == b'\n');
        let length = newline.unwrap_or(available.len());
        if line.len().saturating_add(length) > MAX_REQUEST_BYTES {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "request too large"));
        }
        line.extend_from_slice(&available[..length]);
        reader.consume(if newline.is_some() { length + 1 } else { length });
        if newline.is_some() { return Ok(Some(line)); }
    }
}

fn write_response(stream: &mut UnixStream, response: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(response)
        .map_err(|error| format!("cannot serialize response: {error}"))?;
    bytes.push(b'\n');
    stream.write_all(&bytes).map_err(|error| format!("cannot write response: {error}"))
}
