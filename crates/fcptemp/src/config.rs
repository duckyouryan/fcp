use crate::curve::Curve;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn default_version() -> u32 { 1 }
fn default_hwmon_root() -> PathBuf { PathBuf::from("/sys/class/hwmon") }
fn default_poll_interval_ms() -> u64 { 1000 }
fn default_rule_enabled() -> bool { true }

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    pub fcp_socket: PathBuf,
    #[serde(default = "default_hwmon_root")]
    pub hwmon_root: PathBuf,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub fan: String,
    pub sensor: String,
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,
    pub curve: Curve,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub automation_enabled: bool,
    #[serde(default)]
    pub rules: BTreeMap<String, Rule>,
}

impl Default for StateFile {
    fn default() -> Self {
        Self { version: 1, automation_enabled: false, rules: BTreeMap::new() }
    }
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
        if self.fcp_socket.as_os_str().is_empty() {
            return Err("fcp_socket must not be empty".to_owned());
        }
        if self.hwmon_root.as_os_str().is_empty() {
            return Err("hwmon_root must not be empty".to_owned());
        }
        if !(100..=60_000).contains(&self.poll_interval_ms) {
            return Err("poll_interval_ms must be between 100 and 60000".to_owned());
        }
        Ok(())
    }
}

impl Rule {
    pub fn validate(&self) -> Result<(), String> {
        if self.fan.trim().is_empty() {
            return Err("rule fan must not be empty".to_owned());
        }
        if self.sensor.trim().is_empty() {
            return Err("rule sensor must not be empty".to_owned());
        }
        self.curve.validate()
            .map_err(|error| format!("invalid curve for {}: {error}", self.fan))
    }
}

impl StateFile {
    pub fn load_or_default(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(content) => Self::parse(&content, path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("cannot read state {}: {error}", path.display())),
        }
    }

    fn parse(content: &str, path: &Path) -> Result<Self, String> {
        let state: Self = serde_json::from_str(content)
            .map_err(|error| format!("cannot parse state {}: {error}", path.display()))?;
        if state.version != 1 {
            return Err(format!("unsupported state version: {}", state.version));
        }
        for (fan, rule) in &state.rules {
            if fan != &rule.fan {
                return Err(format!("state rule key '{fan}' does not match rule fan '{}'", rule.fan));
            }
            rule.validate().map_err(|error| format!("invalid saved rule: {error}"))?;
        }
        Ok(state)
    }
}
