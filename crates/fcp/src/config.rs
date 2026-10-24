use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

fn default_version() -> u32 {
    1
}

fn default_hwmon_root() -> PathBuf {
    PathBuf::from("/sys/class/hwmon")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    pub profile: String,
    #[serde(default)]
    pub hwmon_name: Option<String>,
    #[serde(default = "default_hwmon_root")]
    pub hwmon_root: PathBuf,
    pub frequency_hz: f64,
    pub fans: Vec<FanConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanConfig {
    pub label: String,
    #[serde(default)]
    pub pwm_path: Option<PathBuf>,
    #[serde(default)]
    pub rpm_path: Option<PathBuf>,
    pub low: i64,
    pub high: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FanMode {
    Low,
    High,
    Percent,
}

impl FanMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Percent => "percent",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "high" => Some(Self::High),
            "percent" => Some(Self::Percent),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanSettings {
    pub mode: FanMode,
    pub throttle: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFile {
    pub version: u32,
    pub fans: BTreeMap<String, FanSettings>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|error| format!("cannot read config {}: {error}", path.display()))?;
        let config: Self = serde_json::from_str(&content)
            .map_err(|error| format!("cannot parse config {}: {error}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!("unsupported config version: {}", self.version));
        }
        if self.profile.trim().is_empty() {
            return Err("config profile must not be empty".to_owned());
        }
        if !self.frequency_hz.is_finite() || self.frequency_hz < 0.1 || self.frequency_hz > 1000.0 {
            return Err("frequency_hz must be between 0.1 and 1000".to_owned());
        }
        if self.fans.is_empty() {
            return Err("config must define at least one fan".to_owned());
        }

        let mut labels = HashSet::new();
        for fan in &self.fans {
            if fan.label.trim().is_empty() {
                return Err("fan label must not be empty".to_owned());
            }
            if !labels.insert(fan.label.clone()) {
                return Err(format!("duplicate fan label: {}", fan.label));
            }
            if fan.low == fan.high {
                return Err(format!("fan {} has identical low and high values", fan.label));
            }
            for (kind, path) in [("pwm_path", &fan.pwm_path), ("rpm_path", &fan.rpm_path)] {
                if let Some(path) = path {
                    if path.as_os_str().is_empty() {
                        return Err(format!("fan {} has an empty {kind}", fan.label));
                    }
                    if !path.is_absolute() && self.hwmon_name.is_none() {
                        return Err(format!(
                            "fan {} uses relative {kind}, but hwmon_name is not configured",
                            fan.label
                        ));
                    }
                }
            }
        }
        if let Some(name) = &self.hwmon_name {
            if name.trim().is_empty() {
                return Err("hwmon_name must not be empty when specified".to_owned());
            }
        }
        Ok(())
    }
}

impl StateFile {
    pub fn load_or_default(path: &Path, labels: &[String]) -> Result<Self, String> {
        let content = match fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("cannot read state {}: {error}", path.display())),
        };
        if let Some(content) = content {
            let state: Self = serde_json::from_str(&content)
                .map_err(|error| format!("cannot parse state {}: {error}", path.display()))?;
            if state.version != 1 {
                return Err(format!("unsupported state version: {}", state.version));
            }
            let mut fans = BTreeMap::new();
            for label in labels {
                let settings = match state.fans.get(label) {
                    Some(settings) if settings.throttle <= 100 => settings.clone(),
                    Some(_) => return Err(format!("state throttle for {label} must be 0..=100")),
                    None => FanSettings { mode: FanMode::Low, throttle: 50 },
                };
                fans.insert(label.clone(), settings);
            }
            Ok(Self { version: 1, fans })
        } else {
            let fans = labels
                .iter()
                .map(|label| (label.clone(), FanSettings { mode: FanMode::Low, throttle: 50 }))
                .collect();
            Ok(Self { version: 1, fans })
        }
    }
}

pub fn find_hwmon_dir(root: &Path, name: &str) -> Result<PathBuf, String> {
    let entries = fs::read_dir(root)
        .map_err(|error| format!("cannot scan hwmon directory {}: {error}", root.display()))?;
    let mut matches = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(value) => value,
            Err(error) => {
                return Err(format!("cannot read entry in {}: {error}", root.display()));
            }
        };
        let name_path = entry.path().join("name");
        let contents = match fs::read_to_string(&name_path) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if contents.trim() == name {
            matches.push(entry.path());
        }
    }

    match matches.len() {
        0 => Err(format!("hwmon controller '{name}' not found under {}", root.display())),
        1 => match matches.into_iter().next() {
            Some(path) => Ok(path),
            None => Err("internal error while selecting hwmon directory".to_owned()),
        },
        count => Err(format!(
            "found {count} hwmon controllers named '{name}' under {}; configure unique paths",
            root.display()
        )),
    }
}

