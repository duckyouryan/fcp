use crate::config::{Config, Rule, StateFile};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

#[derive(Debug, Clone, Default)]
pub struct FanRuntime {
    pub status: String,
    pub temperature_c: Option<f64>,
    pub calculated_throttle: Option<u8>,
    pub last_applied_throttle: Option<u8>,
    pub last_sent_throttle: Option<u8>,
    pub error: Option<String>,
}

pub struct App {
    pub config: Config,
    state: Mutex<StateFile>,
    runtime: Mutex<BTreeMap<String, FanRuntime>>,
    state_path: PathBuf,
    persist_lock: Mutex<()>,
    apply_lock: Mutex<()>,
}

impl App {
    pub fn new(config: Config, state: StateFile, state_path: PathBuf) -> Self {
        let runtime = state.rules.keys().map(|fan| (fan.clone(), FanRuntime {
            status: "waiting".to_owned(),
            ..FanRuntime::default()
        })).collect();
        Self {
            config,
            state: Mutex::new(state),
            runtime: Mutex::new(runtime),
            state_path,
            persist_lock: Mutex::new(()),
            apply_lock: Mutex::new(()),
        }
    }

    pub fn lock_state(&self) -> MutexGuard<'_, StateFile> {
        self.state.lock().unwrap_or_else(|poisoned| {
            log_warn("recovering poisoned state lock");
            poisoned.into_inner()
        })
    }

    pub fn state_snapshot(&self) -> StateFile {
        self.lock_state().clone()
    }

    /// Serialize automatic writes to fcp with rule/enable changes. After a
    /// disable request returns, no stale control-loop write can follow it.
    pub fn lock_apply(&self) -> MutexGuard<'_, ()> {
        self.apply_lock.lock().unwrap_or_else(|poisoned| {
            log_warn("recovering poisoned apply lock");
            poisoned.into_inner()
        })
    }

    pub fn runtime_snapshot(&self) -> BTreeMap<String, FanRuntime> {
        self.runtime.lock().unwrap_or_else(|poisoned| {
            log_warn("recovering poisoned runtime-status lock");
            poisoned.into_inner()
        }).clone()
    }

    pub fn ensure_runtime_fan(&self, fan: &str) {
        let mut runtime = self.runtime.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        runtime.entry(fan.to_owned()).or_insert_with(|| FanRuntime {
            status: "waiting".to_owned(),
            ..FanRuntime::default()
        });
    }

    pub fn update_runtime(&self, fan: &str, update: impl FnOnce(&mut FanRuntime)) {
        let mut runtime = self.runtime.lock().unwrap_or_else(|poisoned| {
            log_warn("recovering poisoned runtime-status lock");
            poisoned.into_inner()
        });
        let status = runtime.entry(fan.to_owned()).or_default();
        update(status);
    }

    pub fn clear_send_cache(&self, fan: &str) {
        self.update_runtime(fan, |runtime| runtime.last_sent_throttle = None);
    }

    pub fn clear_all_send_caches(&self) {
        let mut runtime = self.runtime.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for item in runtime.values_mut() {
            item.last_sent_throttle = None;
        }
    }

    pub fn save_state(&self) -> Result<(), String> {
        let _serial = self.persist_lock.lock().unwrap_or_else(|poisoned| {
            log_warn("recovering poisoned state-persistence lock");
            poisoned.into_inner()
        });
        let snapshot = self.state_snapshot();
        let data = serde_json::to_vec_pretty(&snapshot)
            .map_err(|error| format!("cannot serialize state: {error}"))?;
        let parent = self.state_path.parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create state directory {}: {error}", parent.display()))?;
        let name = self.state_path.file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "state.json".to_owned());
        let temporary = parent.join(format!(".{name}.tmp-{}", std::process::id()));
        let mut file = OpenOptions::new().write(true).create(true).truncate(true).open(&temporary)
            .map_err(|error| format!("cannot open temporary state {}: {error}", temporary.display()))?;
        file.write_all(&data)
            .map_err(|error| format!("cannot write temporary state {}: {error}", temporary.display()))?;
        file.write_all(b"\n")
            .map_err(|error| format!("cannot finish temporary state {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("cannot sync temporary state {}: {error}", temporary.display()))?;
        drop(file);
        fs::rename(&temporary, &self.state_path)
            .map_err(|error| format!("cannot replace state {}: {error}", self.state_path.display()))?;
        Ok(())
    }

    pub fn validate_state(&self) -> Result<(), String> {
        let state = self.state_snapshot();
        for (key, rule) in &state.rules {
            if key != &rule.fan {
                return Err(format!("state rule key '{key}' does not match fan '{}'", rule.fan));
            }
            rule.validate()?;
        }
        Ok(())
    }

    pub fn install_rule(&self, rule: Rule) -> Result<(), String> {
        let fan = rule.fan.clone();
        let previous = {
            let mut state = self.lock_state();
            state.rules.insert(fan.clone(), rule)
        };
        if let Err(error) = self.save_state() {
            let mut state = self.lock_state();
            match previous {
                Some(old_rule) => { state.rules.insert(fan.clone(), old_rule); }
                None => { state.rules.remove(&fan); }
            }
            return Err(error);
        }
        self.ensure_runtime_fan(&fan);
        self.clear_send_cache(&fan);
        self.update_runtime(&fan, |runtime| {
            runtime.status = "waiting".to_owned();
            runtime.error = None;
        });
        Ok(())
    }

    pub fn remove_rule(&self, fan: &str) -> Result<bool, String> {
        let previous = self.lock_state().rules.remove(fan);
        let Some(previous) = previous else { return Ok(false); };
        if let Err(error) = self.save_state() {
            self.lock_state().rules.insert(fan.to_owned(), previous);
            return Err(error);
        }
        self.update_runtime(fan, |runtime| {
            runtime.status = "not_managed".to_owned();
            runtime.last_sent_throttle = None;
            runtime.error = None;
        });
        Ok(true)
    }
}

pub fn log_info(message: impl AsRef<str>) {
    println!("[fcptemp][INFO] {}", message.as_ref());
}
pub fn log_warn(message: impl AsRef<str>) {
    eprintln!("[fcptemp][WARN] {}", message.as_ref());
}
pub fn log_error(message: impl AsRef<str>) {
    eprintln!("[fcptemp][ERROR] {}", message.as_ref());
}
