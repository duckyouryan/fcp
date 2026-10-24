use fcp_client::{AutomationHandoff, ClientSet};
use serde_json::{json, Value};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

slint::include_modules!();

#[derive(Clone, Default)]
struct Snapshot {
    fans: Vec<Value>,
    sensors: Vec<Value>,
    rules: Vec<Value>,
    automation_enabled: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Explicitly initialize the selected Winit/software backend before creating any component.
    slint::BackendSelector::new().select()?;
    slint::set_xdg_app_id("org.fcp.control")?;
    let clients = ClientSet::from_env().map_err(std::io::Error::other)?;
    let window = MainWindow::new()?;
    let tray = FcpTray::new()?;
    window.window().on_close_requested(|| slint::CloseRequestResponse::HideWindow);
    let snapshot = Arc::new(Mutex::new(Snapshot::default()));
    let refresh_busy = Arc::new(AtomicBool::new(false));
    let pending_form_reload = Arc::new(AtomicBool::new(false));

    bind_window(&window, clients.clone(), snapshot.clone(), refresh_busy.clone(), pending_form_reload.clone());

    let weak_window = window.as_weak();
    tray.on_open_requested({
        let weak_window = weak_window.clone();
        move || {
            if let Some(window) = weak_window.upgrade() {
                let _ = window.show();
            }
        }
    });
    tray.on_refresh_requested({
        let weak_window = weak_window.clone();
        let clients = clients.clone();
        let snapshot = snapshot.clone();
        let refresh_busy = refresh_busy.clone();
        let pending_form_reload = pending_form_reload.clone();
        move || request_refresh(weak_window.clone(), clients.clone(), snapshot.clone(), refresh_busy.clone(), pending_form_reload.clone(), true)
    });
    tray.on_quit_requested(|| { let _ = slint::quit_event_loop(); });
    tray.show()?;
    window.show()?;

    // Refresh visible state periodically. The API calls run away from Slint's UI thread.
    {
        let weak_window = weak_window.clone();
        let clients = clients.clone();
        let snapshot = snapshot.clone();
        let refresh_busy = refresh_busy.clone();
        let pending_form_reload = pending_form_reload.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(2));
            request_refresh(weak_window.clone(), clients.clone(), snapshot.clone(), refresh_busy.clone(), pending_form_reload.clone(), false);
        });
    }

    request_refresh(weak_window, clients, snapshot, refresh_busy, pending_form_reload, true);
    slint::run_event_loop()?;
    Ok(())
}

fn bind_window(
    window: &MainWindow,
    clients: ClientSet,
    snapshot: Arc<Mutex<Snapshot>>,
    refresh_busy: Arc<AtomicBool>,
    pending_form_reload: Arc<AtomicBool>,
) {
    let weak = window.as_weak();
    window.on_refresh_requested({
        let weak = weak.clone();
        let clients = clients.clone();
        let snapshot = snapshot.clone();
        let refresh_busy = refresh_busy.clone();
        let pending_form_reload = pending_form_reload.clone();
        move |reload_form| request_refresh(weak.clone(), clients.clone(), snapshot.clone(), refresh_busy.clone(), pending_form_reload.clone(), reload_form)
    });
    window.on_fan_selected({
        let weak = weak.clone();
        let snapshot = snapshot.clone();
        move |fan| {
            if let Some(window) = weak.upgrade() {
                let current = snapshot.lock().map(|s| s.clone()).unwrap_or_default();
                load_selected_fan(&window, &current, fan.as_str());
            }
        }
    });
    window.on_set_throttle({
        let weak = weak.clone();
        let clients = clients.clone();
        let snapshot = snapshot.clone();
        let refresh_busy = refresh_busy.clone();
        let pending_form_reload = pending_form_reload.clone();
        move |fan, throttle| {
            let fan = fan.to_string();
            if fan.is_empty() {
                set_status(&weak, "Select a fan first.");
                return;
            }
            if !(0..=100).contains(&throttle) {
                set_status(&weak, "Throttle must be from 0 to 100%.");
                return;
            }
            spawn_action(
                weak.clone(), clients.clone(), snapshot.clone(), refresh_busy.clone(), pending_form_reload.clone(), true,
                move |clients| {
                    let result = clients.set_manual_throttle(&fan, throttle as u8)?;
                    let handoff = handoff_message(&result.automation);
                    Ok(format!("Set {fan} to {throttle}%. {handoff}"))
                },
            );
        }
    });
    window.on_set_mode({
        let weak = weak.clone();
        let clients = clients.clone();
        let snapshot = snapshot.clone();
        let refresh_busy = refresh_busy.clone();
        let pending_form_reload = pending_form_reload.clone();
        move |fan, mode| {
            let fan = fan.to_string();
            let mode = mode.to_string();
            if fan.is_empty() {
                set_status(&weak, "Select a fan first.");
                return;
            }
            if !matches!(mode.as_str(), "low" | "high") {
                set_status(&weak, "Unsupported fan mode.");
                return;
            }
            spawn_action(
                weak.clone(), clients.clone(), snapshot.clone(), refresh_busy.clone(), pending_form_reload.clone(), true,
                move |clients| {
                    let result = clients.set_manual_mode(&fan, &mode, None)?;
                    Ok(format!("Set {fan} mode to {mode}. {}", handoff_message(&result.automation)))
                },
            );
        }
    });
    window.on_kickstart({
        let weak = weak.clone();
        let clients = clients.clone();
        let snapshot = snapshot.clone();
        let refresh_busy = refresh_busy.clone();
        let pending_form_reload = pending_form_reload.clone();
        move |fan| {
            let fan = fan.to_string();
            if fan.is_empty() {
                set_status(&weak, "Select a fan first.");
                return;
            }
            spawn_action(
                weak.clone(), clients.clone(), snapshot.clone(), refresh_busy.clone(), pending_form_reload.clone(), true,
                move |clients| {
                    clients.fcp_command("fan.kickstart", json!({"label": fan}))?;
                    Ok(format!("Kickstart requested for {fan}."))
                },
            );
        }
    });
    window.on_toggle_automation({
        let weak = weak.clone();
        let clients = clients.clone();
        let snapshot = snapshot.clone();
        let refresh_busy = refresh_busy.clone();
        let pending_form_reload = pending_form_reload.clone();
        move |enabled| {
            spawn_action(
                weak.clone(), clients.clone(), snapshot.clone(), refresh_busy.clone(), pending_form_reload.clone(), true,
                move |clients| {
                    clients.fcptemp_command("control.set", json!({"enabled": enabled}))?;
                    Ok(if enabled {
                        "Global temperature automation enabled.".to_owned()
                    } else {
                        "Global temperature automation disabled; fcp retains its current fan settings.".to_owned()
                    })
                },
            );
        }
    });
    window.on_save_rule({
        let weak = weak.clone();
        let clients = clients.clone();
        let snapshot = snapshot.clone();
        let refresh_busy = refresh_busy.clone();
        let pending_form_reload = pending_form_reload.clone();
        move |fan, sensor, curve_type, min_temp, max_temp, min_throttle, max_throttle, exponent, points| {
            let fan = fan.to_string();
            let sensor = sensor.to_string();
            let curve_type = curve_type.to_string();
            let min_temp = min_temp.to_string();
            let max_temp = max_temp.to_string();
            let exponent = exponent.to_string();
            let points = points.to_string();
            if fan.is_empty() {
                set_status(&weak, "Select a fan first.");
                return;
            }
            if sensor.is_empty() {
                set_status(&weak, "Select an available temperature sensor.");
                return;
            }
            let rule = match build_rule(
                &fan, &sensor, &curve_type, &min_temp, &max_temp,
                min_throttle, max_throttle, &exponent, &points,
            ) {
                Ok(rule) => rule,
                Err(error) => {
                    set_status(&weak, &error);
                    return;
                }
            };
            spawn_action(
                weak.clone(), clients.clone(), snapshot.clone(), refresh_busy.clone(), pending_form_reload.clone(), true,
                move |clients| {
                    clients.fcptemp_command("rule.set", json!({"rule": rule}))?;
                    Ok(format!("Saved the rule for {fan} as disabled. Enable it explicitly after reviewing it."))
                },
            );
        }
    });
    window.on_toggle_rule({
        let weak = weak.clone();
        let clients = clients.clone();
        let snapshot = snapshot.clone();
        let refresh_busy = refresh_busy.clone();
        let pending_form_reload = pending_form_reload.clone();
        move |fan, enabled| {
            let fan = fan.to_string();
            if fan.is_empty() {
                set_status(&weak, "Select a fan first.");
                return;
            }
            spawn_action(
                weak.clone(), clients.clone(), snapshot.clone(), refresh_busy.clone(), pending_form_reload.clone(), true,
                move |clients| {
                    clients.fcptemp_command("control.set", json!({"fan": fan, "enabled": enabled}))?;
                    Ok(format!("Temperature rule for {fan} {}.", if enabled { "enabled" } else { "disabled" }))
                },
            );
        }
    });
    window.on_delete_rule({
        let weak = weak.clone();
        let clients = clients.clone();
        let snapshot = snapshot.clone();
        let refresh_busy = refresh_busy.clone();
        let pending_form_reload = pending_form_reload.clone();
        move |fan| {
            let fan = fan.to_string();
            if fan.is_empty() {
                set_status(&weak, "Select a fan first.");
                return;
            }
            spawn_action(
                weak.clone(), clients.clone(), snapshot.clone(), refresh_busy.clone(), pending_form_reload.clone(), true,
                move |clients| {
                    clients.fcptemp_command("rule.delete", json!({"fan": fan}))?;
                    Ok(format!("Deleted temperature rule for {fan}."))
                },
            );
        }
    });
}

fn request_refresh(
    weak: Weak<MainWindow>,
    clients: ClientSet,
    snapshot: Arc<Mutex<Snapshot>>,
    refresh_busy: Arc<AtomicBool>,
    pending_form_reload: Arc<AtomicBool>,
    reload_form: bool,
) {
    if refresh_busy.swap(true, Ordering::AcqRel) {
        if reload_form { pending_form_reload.store(true, Ordering::Release); }
        return;
    }
    let busy_for_ui = refresh_busy.clone();
    thread::spawn(move || {
        let result = fetch_snapshot(&clients);
        let reload_form = reload_form || pending_form_reload.swap(false, Ordering::AcqRel);
        let update = slint::invoke_from_event_loop(move || {
            busy_for_ui.store(false, Ordering::Release);
            let Some(window) = weak.upgrade() else { return; };
            match result {
                Ok((new_snapshot, message)) => {
                    if let Ok(mut shared) = snapshot.lock() {
                        *shared = new_snapshot.clone();
                    }
                    apply_snapshot(&window, &new_snapshot, &message, reload_form);
                }
                Err(_error) => {
                    window.set_connection_summary("fcp service unavailable".into());
                    window.set_fcptemp_connected(false);
                    window.set_automation_enabled(false);
                    window.set_automation_label("Automation unavailable".into());
                    window.set_status_message("Cannot reach fcp. Check systemctl status fcp.service.".into());
                },
            }
        });
        if update.is_err() {
            refresh_busy.store(false, Ordering::Release);
        }
    });
}

fn fetch_snapshot(clients: &ClientSet) -> Result<(Snapshot, String), String> {
    let fans_data = clients.fcp_command("fans.list", json!({}))?;
    let fans = value_array(&fans_data, "fans").map_or_else(Vec::new, |values| values.clone());
    let fan_count = fans.len();

    let mut snapshot = Snapshot { fans, ..Snapshot::default() };
    let connection_summary = match clients.fcptemp_command("status", json!({})) {
        Ok(status) => {
            snapshot.automation_enabled = status.get("automation_enabled").and_then(Value::as_bool).unwrap_or(false);
            match clients.fcptemp_command("sensors.list", json!({})) {
                Ok(data) => snapshot.sensors = value_array(&data, "sensors").map_or_else(Vec::new, |values| values.clone()),
                Err(_) => (),
            }
            match clients.fcptemp_command("rules.list", json!({})) {
                Ok(data) => snapshot.rules = value_array(&data, "rules").map_or_else(Vec::new, |values| values.clone()),
                Err(_) => (),
            }
            format!("FCP online ({fan_count} fans)\nfcptemp online — automation {}", if snapshot.automation_enabled { "on" } else { "off" })
        }
        Err(error) if error.contains("No such file or directory") => {
            format!("FCP online ({fan_count} fans)\nfcptemp service offline")
        }
        Err(_) => format!("FCP online ({fan_count} fans)\nfcptemp connection error"),
    };
    Ok((snapshot, connection_summary))
}

fn apply_snapshot(window: &MainWindow, snapshot: &Snapshot, message: &str, reload_form: bool) {
    let fan_labels: Vec<String> = snapshot.fans.iter()
        .filter_map(|fan| fan.get("label").and_then(Value::as_str).map(str::to_owned))
        .collect();

    let mut fan_modes = Vec::with_capacity(snapshot.fans.len());
    let mut fan_throttles = Vec::with_capacity(snapshot.fans.len());
    let mut fan_rpms = Vec::with_capacity(snapshot.fans.len());
    for fan in &snapshot.fans {
        let raw_mode = string_field(fan, "mode", "unavailable");
        let mode = match raw_mode.as_str() {
            "percent" => "Custom",
            "low" => "Low",
            "high" => "High",
            _ => "Unavailable",
        };
        let throttle = fan.get("throttle").and_then(Value::as_i64)
            .map(|value| format!("{value}%"))
            .unwrap_or_else(|| "—".to_owned());
        let rpm = fan.get("rpm").and_then(Value::as_i64)
            .map(|value| format!("{value} RPM"))
            .unwrap_or_else(|| "—".to_owned());
        fan_modes.push(mode.to_owned());
        fan_throttles.push(throttle);
        fan_rpms.push(rpm);
    }

    let sensor_ids: Vec<String> = snapshot.sensors.iter()
        .filter(|sensor| sensor.get("available").and_then(Value::as_bool) == Some(true))
        .filter_map(|sensor| sensor.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let mut sensor_row_ids = Vec::with_capacity(snapshot.sensors.len());
    let mut sensor_values = Vec::with_capacity(snapshot.sensors.len());
    let mut sensor_details = Vec::with_capacity(snapshot.sensors.len());
    let mut sensor_states = Vec::with_capacity(snapshot.sensors.len());
    for sensor in &snapshot.sensors {
        let id = string_field(sensor, "id", "unknown sensor");
        let available = sensor.get("available").and_then(Value::as_bool) == Some(true);
        let temperature = sensor.get("temperature_c").and_then(Value::as_f64);
        let healthy = available && temperature.is_some_and(f64::is_finite);
        let value = if healthy {
            format!("{:.1} °C", temperature.unwrap_or_default())
        } else {
            "—".to_owned()
        };
        let detail = if healthy {
            String::new()
        } else {
            string_field(sensor, "reason", "Temperature reading unavailable")
        };
        sensor_row_ids.push(id);
        sensor_values.push(value);
        sensor_details.push(detail);
        sensor_states.push(if healthy { "ok".to_owned() } else { "warning".to_owned() });
    }

    let mut rule_fans = Vec::with_capacity(snapshot.rules.len());
    let mut rule_details = Vec::with_capacity(snapshot.rules.len());
    let mut rule_states = Vec::with_capacity(snapshot.rules.len());
    for entry in &snapshot.rules {
        let rule = entry.get("rule").unwrap_or(&Value::Null);
        let runtime = entry.get("runtime").unwrap_or(&Value::Null);
        let fan = string_field(rule, "fan", "Unknown fan");
        let sensor = string_field(rule, "sensor", "Unknown sensor");
        let enabled = rule.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        let curve = rule.get("curve").unwrap_or(&Value::Null);
        let curve_type = string_field(curve, "type", "unknown");
        let curve_label = match curve_type.as_str() {
            "linear" => "Linear",
            "power" => "Power",
            "points" => "Points",
            _ => "Unknown curve",
        };
        let min_temp = number_field(curve, "min_temp_c", 30.0);
        let max_temp = number_field(curve, "max_temp_c", 70.0);
        let min_throttle = curve.get("min_throttle").and_then(Value::as_i64).unwrap_or(20);
        let max_throttle = curve.get("max_throttle").and_then(Value::as_i64).unwrap_or(100);
        let runtime_status = string_field(runtime, "status", "waiting");
        let has_error = runtime.get("error").and_then(Value::as_str).is_some()
            || runtime_status.eq_ignore_ascii_case("error");

        rule_fans.push(fan);
        rule_details.push(format!(
            "{sensor}\n{curve_label} curve, {min_temp}–{max_temp} °C, {min_throttle}–{max_throttle}% throttle\nRuntime: {runtime_status}"
        ));
        rule_states.push(if has_error { "Error".to_owned() } else if enabled { "Enabled".to_owned() } else { "Disabled".to_owned() });
    }

    window.set_fan_labels(model(fan_labels));
    window.set_fan_modes(model(fan_modes));
    window.set_fan_throttles(model(fan_throttles));
    window.set_fan_rpms(model(fan_rpms));
    window.set_sensor_ids(model(sensor_ids));
    window.set_sensor_row_ids(model(sensor_row_ids));
    window.set_sensor_values(model(sensor_values));
    window.set_sensor_details(model(sensor_details));
    window.set_sensor_states(model(sensor_states));
    window.set_rule_fans(model(rule_fans));
    window.set_rule_details(model(rule_details));
    window.set_rule_states(model(rule_states));
    let lower_message = message.to_lowercase();
    let fcptemp_connected = !lower_message.contains("fcptemp service offline")
        && !lower_message.contains("fcptemp connection error");
    window.set_fcptemp_connected(fcptemp_connected);
    window.set_automation_enabled(snapshot.automation_enabled);
    window.set_automation_label(if !fcptemp_connected {
        "Automation unavailable"
    } else if snapshot.automation_enabled {
        "Automation on"
    } else {
        "Automation off"
    }.into());
    window.set_connection_summary(message.into());
    let current_status = window.get_status_message().to_string();
    if lower_message.contains("fcptemp service offline") {
        window.set_status_message("fcptemp.service is not running. Install or start the service, then refresh.".into());
    } else if lower_message.contains("fcptemp connection error") {
        window.set_status_message("fcptemp is unreachable. Check systemctl status fcptemp.service.".into());
    } else if current_status.starts_with("Cannot reach fcp.")
        || current_status.starts_with("fcptemp.service is not running.")
        || current_status.starts_with("fcptemp is unreachable.")
    {
        window.set_status_message("Service connection restored.".into());
    }

    let previous_selected = window.get_selected_fan().to_string();
    let selection_exists = snapshot.fans.iter().any(|fan| fan.get("label").and_then(Value::as_str) == Some(previous_selected.as_str()));
    let selected = if selection_exists { previous_selected.clone() } else { fan_labels_first(snapshot).unwrap_or_default() };
    if !selected.is_empty() {
        window.set_selected_fan(selected.clone().into());
    }
    if reload_form || selected != previous_selected {
        load_selected_fan(window, snapshot, &selected);
    }
}

fn load_selected_fan(window: &MainWindow, snapshot: &Snapshot, fan_label: &str) {
    if let Some(fan) = snapshot.fans.iter().find(|fan| fan.get("label").and_then(Value::as_str) == Some(fan_label)) {
        if let Some(throttle) = fan.get("throttle").and_then(Value::as_i64) {
            window.set_manual_throttle(throttle.clamp(0, 100) as i32);
        }
    }
    let rule_entry = snapshot.rules.iter().find(|entry| {
        entry.pointer("/rule/fan").and_then(Value::as_str) == Some(fan_label)
    });
    let Some(entry) = rule_entry else {
        window.set_selected_rule_exists(false);
        window.set_selected_rule_enabled(false);
        window.set_selected_rule_summary("No saved rule for this fan. Saving will create it disabled.".into());
        window.set_selected_sensor(snapshot.sensors.iter()
            .find(|sensor| sensor.get("available").and_then(Value::as_bool) == Some(true))
            .and_then(|sensor| sensor.get("id")).and_then(Value::as_str).unwrap_or("").into());
        window.set_curve_type("linear".into());
        window.set_min_temp_text("30".into());
        window.set_max_temp_text("70".into());
        window.set_min_throttle(20);
        window.set_max_throttle(100);
        window.set_exponent_text("2".into());
        window.set_points_text("40:50".into());
        return;
    };
    let rule = entry.get("rule").unwrap_or(&Value::Null);
    let curve = rule.get("curve").unwrap_or(&Value::Null);
    let curve_type = string_field(curve, "type", "linear");
    let enabled = rule.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    window.set_selected_rule_exists(true);
    window.set_selected_rule_enabled(enabled);
    window.set_selected_rule_summary(format!(
        "Saved rule is {}; sensor {}; runtime {}.",
        if enabled { "enabled" } else { "disabled" },
        string_field(rule, "sensor", "unknown sensor"),
        string_field(entry.get("runtime").unwrap_or(&Value::Null), "status", "waiting")
    ).into());
    window.set_selected_sensor(string_field(rule, "sensor", "").into());
    window.set_curve_type(curve_type.into());
    window.set_min_temp_text(number_field(curve, "min_temp_c", 30.0).into());
    window.set_max_temp_text(number_field(curve, "max_temp_c", 70.0).into());
    window.set_min_throttle(curve.get("min_throttle").and_then(Value::as_i64).unwrap_or(20).clamp(0, 100) as i32);
    window.set_max_throttle(curve.get("max_throttle").and_then(Value::as_i64).unwrap_or(100).clamp(0, 100) as i32);
    window.set_exponent_text(number_field(curve, "exponent", 2.0).into());
    if let Some(points) = curve.get("points").and_then(Value::as_array) {
        let formatted = points.iter().filter_map(|point| {
            let temp = point.get("temp_c")?.as_f64()?;
            let throttle = point.get("throttle")?.as_i64()?;
            Some(format!("{}:{}", format_number(temp), throttle))
        }).collect::<Vec<_>>().join(", ");
        window.set_points_text(if formatted.is_empty() { "40:50" } else { &formatted }.into());
    } else {
        window.set_points_text("40:50".into());
    }
}

fn spawn_action<F>(
    weak: Weak<MainWindow>,
    clients: ClientSet,
    snapshot: Arc<Mutex<Snapshot>>,
    refresh_busy: Arc<AtomicBool>,
    pending_form_reload: Arc<AtomicBool>,
    reload_form: bool,
    action: F,
) where
    F: FnOnce(&ClientSet) -> Result<String, String> + Send + 'static,
{
    thread::spawn(move || {
        let message = match action(&clients) {
            Ok(message) => message,
            Err(error) => format!("Operation failed: {error}"),
        };
        let ui_weak = weak.clone();
        let update = slint::invoke_from_event_loop(move || {
            if let Some(window) = ui_weak.upgrade() {
                window.set_status_message(message.into());
            }
        });
        if update.is_err() {
            return;
        }
        request_refresh(weak, clients, snapshot, refresh_busy, pending_form_reload, reload_form);
    });
}

fn build_rule(
    fan: &str,
    sensor: &str,
    curve_type: &str,
    min_temp_text: &str,
    max_temp_text: &str,
    min_throttle: i32,
    max_throttle: i32,
    exponent_text: &str,
    points_text: &str,
) -> Result<Value, String> {
    let min_temp: f64 = min_temp_text.trim().parse().map_err(|_| "Minimum temperature must be numeric.".to_owned())?;
    let max_temp: f64 = max_temp_text.trim().parse().map_err(|_| "Maximum temperature must be numeric.".to_owned())?;
    if !min_temp.is_finite() || !max_temp.is_finite() || min_temp >= max_temp {
        return Err("Minimum temperature must be finite and lower than maximum temperature.".to_owned());
    }
    if !(0..=100).contains(&min_throttle) || !(0..=100).contains(&max_throttle) || min_throttle > max_throttle {
        return Err("Throttle bounds must satisfy 0 ≤ min ≤ max ≤ 100.".to_owned());
    }
    let mut curve = json!({
        "type": curve_type,
        "min_temp_c": min_temp,
        "max_temp_c": max_temp,
        "min_throttle": min_throttle,
        "max_throttle": max_throttle
    });
    match curve_type {
        "linear" => {}
        "power" => {
            let exponent: f64 = exponent_text.trim().parse().map_err(|_| "Power exponent must be numeric.".to_owned())?;
            if !exponent.is_finite() || !(0.25..=8.0).contains(&exponent) {
                return Err("Power exponent must be between 0.25 and 8.0.".to_owned());
            }
            curve["exponent"] = json!(exponent);
        }
        "points" => {
            let input = points_text.trim();
            let mut points = Vec::new();
            if !input.is_empty() {
                for (index, token) in input.split(',').enumerate() {
                    let Some((temp_text, throttle_text)) = token.trim().split_once(':') else {
                        return Err(format!("Point {} must use temperature:throttle format, e.g. 40:50.", index + 1));
                    };
                    let temp: f64 = temp_text.trim().parse()
                        .map_err(|_| format!("Point {} temperature must be numeric.", index + 1))?;
                    let throttle: i64 = throttle_text.trim().parse()
                        .map_err(|_| format!("Point {} throttle must be an integer.", index + 1))?;
                    points.push(json!({"temp_c": temp, "throttle": throttle}));
                }
            }
            if points.len() > 8 {
                return Err("A points curve supports at most 8 intermediate points.".to_owned());
            }
            let mut previous_temp = min_temp;
            let mut previous_throttle = min_throttle;
            for (index, point) in points.iter().enumerate() {
                let temp = point.get("temp_c").and_then(Value::as_f64)
                    .ok_or_else(|| format!("Point {} needs numeric temperature.", index + 1))?;
                let throttle = point.get("throttle").and_then(Value::as_i64)
                    .ok_or_else(|| format!("Point {} needs integer throttle.", index + 1))?;
                if !temp.is_finite() || temp <= previous_temp || temp >= max_temp {
                    return Err(format!("Point {} temperatures must be strictly increasing and inside the curve bounds.", index + 1));
                }
                if throttle < previous_throttle as i64 || throttle < 0 || throttle > max_throttle as i64 {
                    return Err(format!("Point {} throttle must not decrease or exceed max throttle.", index + 1));
                }
                previous_temp = temp;
                previous_throttle = throttle as i32;
            }
            curve["points"] = json!(points);
        }
        other => return Err(format!("Unknown curve type '{other}'.")),
    }
    Ok(json!({ "fan": fan, "sensor": sensor, "enabled": false, "curve": curve }))
}

fn handoff_message(handoff: &AutomationHandoff) -> String {
    match handoff {
        AutomationHandoff::RuleDisabled => "Enabled temperature rule was disabled first.".to_owned(),
        AutomationHandoff::NoEnabledRule => "No enabled temperature rule needed disabling.".to_owned(),
        AutomationHandoff::ServiceUnavailable(error) => format!("Warning: fcptemp was unavailable during handoff: {error}"),
    }
}

fn set_status(weak: &Weak<MainWindow>, message: &str) {
    let message = message.to_owned();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_status_message(message.into());
        }
    });
}

fn model(values: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(values.into_iter().map(SharedString::from).collect::<Vec<_>>()))
}

fn value_array<'a>(object: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    object.get(key)?.as_array()
}

fn string_field(object: &Value, key: &str, fallback: &str) -> String {
    object.get(key).and_then(Value::as_str).unwrap_or(fallback).to_owned()
}

fn number_field(object: &Value, key: &str, fallback: f64) -> String {
    format_number(object.get(key).and_then(Value::as_f64).unwrap_or(fallback))
}

fn format_number(number: f64) -> String {
    if number.fract() == 0.0 { format!("{number:.0}") } else { format!("{number}") }
}

fn fan_labels_first(snapshot: &Snapshot) -> Option<String> {
    snapshot.fans.first().and_then(|fan| fan.get("label")).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_rule_is_created_disabled() {
        let rule = build_rule("CPU_FAN", "k10temp/temp1", "linear", "30", "70", 20, 100, "2", "[]")
            .expect("valid linear rule should parse");
        assert_eq!(rule["enabled"], false);
        assert_eq!(rule["curve"]["type"], "linear");
    }

    #[test]
    fn power_curve_validates_exponent() {
        assert!(build_rule("CPU_FAN", "k10temp/temp1", "power", "30", "70", 20, 100, "2", "[]").is_ok());
        assert!(build_rule("CPU_FAN", "k10temp/temp1", "power", "30", "70", 20, 100, "10", "[]").is_err());
    }

    #[test]
    fn points_curve_rejects_decreasing_throttle() {
        let points = "40:60, 50:55";
        assert!(build_rule("CPU_FAN", "k10temp/temp1", "points", "30", "70", 20, 100, "2", points).is_err());
    }

    #[test]
    fn temperature_bounds_must_increase() {
        assert!(build_rule("CPU_FAN", "k10temp/temp1", "linear", "70", "30", 20, 100, "2", "[]").is_err());
    }
}
