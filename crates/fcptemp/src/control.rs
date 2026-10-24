use crate::app::{App, log_info, log_warn};
use crate::config::Rule;
use crate::sensors::{self, Sensor};
use fcp_client::{ensure_ok, request};
use serde_json::json;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub fn run_loop(app: Arc<App>, shutdown: Arc<std::sync::atomic::AtomicBool>) {
    log_info(format!("temperature control loop started; interval={} ms", app.config.poll_interval_ms));
    while !shutdown.load(std::sync::atomic::Ordering::SeqCst) {
        cycle(&app);
        let mut remaining_ms = app.config.poll_interval_ms;
        while remaining_ms > 0 {
            if shutdown.load(std::sync::atomic::Ordering::SeqCst) { break; }
            let step_ms = remaining_ms.min(100);
            thread::sleep(Duration::from_millis(step_ms));
            remaining_ms -= step_ms;
        }
    }
    log_info("temperature control loop stopped");
}

fn cycle(app: &Arc<App>) {
    let state = app.state_snapshot();
    if !state.automation_enabled {
        for fan in state.rules.keys() {
            transition_status(app, fan, "automation_disabled", None, None, None, false);
        }
        return;
    }

    let discovered = match sensors::discover(&app.config.hwmon_root) {
        Ok(sensors) => sensors,
        Err(error) => {
            for rule in state.rules.values().filter(|rule| rule.enabled) {
                transition_status(app, &rule.fan, "sensor_unavailable", None, None, Some(error.clone()), true);
            }
            return;
        }
    };

    for rule in state.rules.values() {
        if !rule.enabled {
            transition_status(app, &rule.fan, "fan_disabled", None, None, None, false);
            continue;
        }
        apply_rule(app, rule, &discovered);
    }
}

fn apply_rule(app: &Arc<App>, rule: &Rule, discovered: &[Sensor]) {
    let sensor = match discovered.iter().find(|sensor| sensor.view.id == rule.sensor) {
        Some(sensor) => sensor,
        None => {
            transition_status(
                app,
                &rule.fan,
                "sensor_unavailable",
                None,
                None,
                Some(format!("configured sensor '{}' was not found", rule.sensor)),
                true,
            );
            return;
        }
    };
    if !sensor.view.available {
        transition_status(
            app,
            &rule.fan,
            "sensor_unavailable",
            None,
            None,
            sensor.view.reason.clone().or_else(|| Some("sensor is not available".to_owned())),
            true,
        );
        return;
    }

    let temperature = match sensor.read_again() {
        Ok(value) => value,
        Err(error) => {
            transition_status(app, &rule.fan, "sensor_unavailable", None, None, Some(error), true);
            return;
        }
    };
    let throttle = rule.curve.calculate(temperature);
    let runtime = app.runtime_snapshot().get(&rule.fan).cloned().unwrap_or_default();
    if runtime.last_sent_throttle == Some(throttle) && runtime.status == "active" {
        app.update_runtime(&rule.fan, |current| {
            current.temperature_c = Some(temperature);
            current.calculated_throttle = Some(throttle);
            current.error = None;
        });
        return;
    }

    // Serialize the last state check and fcp write with control.set/rule.set.
    // Once a disable command returns, no stale automatic write can follow it.
    let _apply = app.lock_apply();
    let latest = app.state_snapshot();
    if !latest.automation_enabled || !latest.rules.get(&rule.fan).map(|current| current.enabled && current.sensor == rule.sensor && current.curve.calculate(temperature) == throttle).unwrap_or(false) {
        return;
    }

    let request_value = json!({
        "id": "fcptemp-auto",
        "command": "fan.set",
        "label": rule.fan,
        "mode": "percent",
        "throttle": throttle
    });
    let result = request(&app.config.fcp_socket, &request_value)
        .and_then(|response| ensure_ok(&response).map(|_| response));
    match result {
        Ok(_) => {
            app.update_runtime(&rule.fan, |current| {
                current.status = "active".to_owned();
                current.temperature_c = Some(temperature);
                current.calculated_throttle = Some(throttle);
                current.last_applied_throttle = Some(throttle);
                current.last_sent_throttle = Some(throttle);
                current.error = None;
            });
            log_info(format!("{}: {:.1} °C -> throttle {}%", rule.fan, temperature, throttle));
        }
        Err(error) => {
            transition_status(app, &rule.fan, "fcp_unavailable", Some(temperature), Some(throttle), Some(error), true);
        }
    }
}

fn transition_status(
    app: &Arc<App>,
    fan: &str,
    next_status: &str,
    temperature_c: Option<f64>,
    calculated_throttle: Option<u8>,
    error: Option<String>,
    clear_send_cache: bool,
) {
    let previous = app.runtime_snapshot().get(fan).cloned().unwrap_or_default();
    let changed = previous.status != next_status || previous.error != error;
    app.update_runtime(fan, |current| {
        current.status = next_status.to_owned();
        current.temperature_c = temperature_c;
        current.calculated_throttle = calculated_throttle;
        current.error = error.clone();
        if clear_send_cache {
            current.last_sent_throttle = None;
        }
    });
    if changed {
        match error {
            Some(message) => log_warn(format!("{fan}: {next_status}: {message}")),
            None => log_info(format!("{fan}: {next_status}")),
        }
    }
}

