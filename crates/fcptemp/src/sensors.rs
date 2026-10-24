use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct SensorView {
    pub id: String,
    pub controller: String,
    pub label: String,
    pub input: String,
    pub path: String,
    pub temperature_c: Option<f64>,
    pub available: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Sensor {
    pub view: SensorView,
    input_path: PathBuf,
    index: usize,
}

impl Sensor {
    pub fn read_again(&self) -> Result<f64, String> {
        let (_, available, reason) = read_sensor_state(&self.input_path, self.index);
        if !available {
            return Err(reason.unwrap_or_else(|| "sensor is not available".to_owned()));
        }
        read_sensor_temperature(&self.input_path)
    }
}

pub fn discover(root: &Path) -> Result<Vec<Sensor>, String> {
    let entries = fs::read_dir(root)
        .map_err(|error| format!("cannot scan hwmon directory {}: {error}", root.display()))?;
    let mut candidates: Vec<(PathBuf, String, String, usize, String)> = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read entry in {}: {error}", root.display()))?;
        let directory = entry.path();
        if !entry.file_name().to_string_lossy().starts_with("hwmon") || !directory.is_dir() {
            continue;
        }
        let controller = match fs::read_to_string(directory.join("name")) {
            Ok(value) => value.trim().to_owned(),
            Err(_) => continue,
        };
        if controller.is_empty() { continue; }

        let files = match fs::read_dir(&directory) {
            Ok(value) => value,
            Err(_) => continue,
        };
        for file in files.flatten() {
            let file_name = file.file_name().to_string_lossy().into_owned();
            let Some(index_text) = file_name.strip_prefix("temp").and_then(|v| v.strip_suffix("_input")) else {
                continue;
            };
            let Ok(index) = index_text.parse::<usize>() else { continue; };
            if index == 0 { continue; }
            let label_path = directory.join(format!("temp{index}_label"));
            let label = fs::read_to_string(label_path)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| format!("temp{index}"));
            candidates.push((directory.join(file_name), controller.clone(), label, index, directory.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()));
        }
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    let mut base_counts: BTreeMap<String, usize> = BTreeMap::new();
    for (_, controller, label, _, _) in &candidates {
        *base_counts.entry(format!("{}/{}", slug(controller), slug(label))).or_default() += 1;
    }

    let mut sensors = Vec::with_capacity(candidates.len());
    for (input_path, controller, label, index, hwmon_name) in candidates {
        let mut id = format!("{}/{}", slug(&controller), slug(&label));
        if base_counts.get(&id).copied().unwrap_or(0) > 1 {
            // Device numbers are stable in the common case. This suffix is only
            // necessary when multiple controllers expose the same named sensor.
            id.push('@');
            id.push_str(&format!("{hwmon_name}-temp{index}"));
        }
        let (temperature_c, available, reason) = read_sensor_state(&input_path, index);
        let input = format!("temp{index}_input");
        sensors.push(Sensor {
            view: SensorView {
                id,
                controller,
                label,
                input,
                path: input_path.to_string_lossy().into_owned(),
                temperature_c,
                available,
                reason,
            },
            input_path,
            index,
        });
    }
    sensors.sort_by(|a, b| a.view.id.cmp(&b.view.id));
    Ok(sensors)
}

fn read_sensor_state(input_path: &Path, index: usize) -> (Option<f64>, bool, Option<String>) {
    let type_path = input_path.with_file_name(format!("temp{index}_type"));
    if let Ok(contents) = fs::read_to_string(&type_path) {
        if contents.trim().parse::<i64>().ok() == Some(0) {
            return (None, false, Some("sensor disabled by driver (tempN_type=0)".to_owned()));
        }
    }
    match read_sensor_temperature(input_path) {
        Ok(value) => (Some(value), true, None),
        Err(error) => (None, false, Some(error)),
    }
}

fn read_sensor_temperature(input_path: &Path) -> Result<f64, String> {
    let raw = fs::read_to_string(input_path)
        .map_err(|error| format!("cannot read {}: {error}", input_path.display()))?;
    let milli_celsius = raw.trim().parse::<i64>()
        .map_err(|error| format!("invalid temperature value in {}: {error}", input_path.display()))?;
    let temperature = milli_celsius as f64 / 1000.0;
    if !(-55.0..=200.0).contains(&temperature) {
        return Err(format!("temperature reading {:.1} °C is outside plausible range", temperature));
    }
    Ok(temperature)
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut last_dash = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            output.push(character);
            last_dash = false;
        } else if !last_dash && !output.is_empty() {
            output.push('-');
            last_dash = true;
        }
    }
    while output.ends_with('-') { output.pop(); }
    if output.is_empty() { "unnamed".to_owned() } else { output }
}

#[cfg(test)]
mod tests {
    use super::slug;

    #[test]
    fn slug_is_lowercase_and_path_safe() {
        assert_eq!(slug("CPU Temperature (Tctl)"), "cpu-temperature-tctl");
    }
}
