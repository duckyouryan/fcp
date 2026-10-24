use crate::config::{find_hwmon_dir, FanConfig, FanMode, FanSettings};
use crate::logging::{error, info, warn};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const HEALTH_CHECK_INTERVAL: Duration = Duration::from_millis(750);
const KICKSTART_TIMEOUT: Duration = Duration::from_secs(3);

struct Runtime {
    mode: FanMode,
    throttle: u8,
    active: bool,
    revision: u64,
    applied_revision: u64,
    kickstart_seq: u64,
    completed_kickstart_seq: u64,
    stopping: bool,
    control_error: Option<String>,
    last_rpm_error: Option<String>,
}

#[derive(Clone)]
pub struct FanView {
    pub label: String,
    pub mode: Option<FanMode>,
    pub throttle: u8,
    pub rpm: Option<u64>,
}

pub struct FanControl {
    pub config: FanConfig,
    hwmon_name: Option<String>,
    hwmon_root: PathBuf,
    hwmon_dir_cache: Mutex<Option<PathBuf>>,
    frequency_hz: f64,
    runtime: Mutex<Runtime>,
    changed: Condvar,
}

impl FanControl {
    pub fn new(
        config: FanConfig,
        hwmon_name: Option<String>,
        hwmon_root: PathBuf,
        frequency_hz: f64,
        settings: FanSettings,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            hwmon_name,
            hwmon_root,
            hwmon_dir_cache: Mutex::new(None),
            frequency_hz,
            runtime: Mutex::new(Runtime {
                mode: settings.mode,
                throttle: settings.throttle,
                active: false,
                revision: 1,
                applied_revision: 0,
                kickstart_seq: 1,
                completed_kickstart_seq: 0,
                stopping: false,
                control_error: None,
                last_rpm_error: None,
            }),
            changed: Condvar::new(),
        })
    }

    fn lock_runtime(&self) -> MutexGuard<'_, Runtime> {
        match self.runtime.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error(format!("{}: recovering poisoned runtime lock", self.config.label));
                poisoned.into_inner()
            }
        }
    }

    fn lock_hwmon_cache(&self) -> MutexGuard<'_, Option<PathBuf>> {
        match self.hwmon_dir_cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error(format!("{}: recovering poisoned hwmon cache lock", self.config.label));
                poisoned.into_inner()
            }
        }
    }

    fn clear_hwmon_cache(&self) {
        *self.lock_hwmon_cache() = None;
    }

    fn resolve(&self, configured: Option<&Path>, kind: &str) -> Result<PathBuf, String> {
        let configured = match configured {
            Some(path) => path,
            None => return Err(format!("{kind} is not configured")),
        };
        if configured.is_absolute() {
            return Ok(configured.to_path_buf());
        }
        let name = match self.hwmon_name.as_deref() {
            Some(value) => value,
            None => return Err("relative sysfs path requires hwmon_name".to_owned()),
        };
        let mut cache = self.lock_hwmon_cache();
        if cache.is_none() {
            *cache = Some(find_hwmon_dir(&self.hwmon_root, name)?);
        }
        match cache.as_ref() {
            Some(directory) => Ok(directory.join(configured)),
            None => Err("could not cache hwmon directory".to_owned()),
        }
    }

    fn write_control_at(&self, path: &Path, value: i64) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|error| format!("cannot open control file {}: {error}", path.display()))?;
        file.write_all(format!("{value}\n").as_bytes())
            .map_err(|error| format!("cannot write control file {}: {error}", path.display()))
    }

    fn write_control(&self, value: i64) -> Result<PathBuf, String> {
        let configured = self.config.pwm_path.as_deref();
        let path = self.resolve(configured, "pwm_path")?;
        match self.write_control_at(&path, value) {
            Ok(()) => Ok(path),
            Err(first_error) if configured.map(|value| !value.is_absolute()).unwrap_or(false) => {
                self.clear_hwmon_cache();
                let retry_path = self.resolve(configured, "pwm_path")
                    .map_err(|error| format!("{first_error}; hwmon rediscovery failed: {error}"))?;
                self.write_control_at(&retry_path, value)
                    .map(|()| retry_path)
                    .map_err(|retry_error| format!("{first_error}; retry failed: {retry_error}"))
            }
            Err(error) => Err(error),
        }
    }

    fn validate_cached_hwmon(&self) {
        let has_relative_path = self.config.pwm_path.as_deref()
            .map(|path| !path.is_absolute())
            .unwrap_or(false)
            || self.config.rpm_path.as_deref()
                .map(|path| !path.is_absolute())
                .unwrap_or(false);
        let expected_name = match (has_relative_path, self.hwmon_name.as_deref()) {
            (true, Some(name)) => name,
            _ => return,
        };
        let cached = self.lock_hwmon_cache().clone();
        if let Some(directory) = cached {
            let cached_name = fs::read_to_string(directory.join("name"));
            let matches = match cached_name {
                Ok(value) => value.trim() == expected_name,
                Err(_) => false,
            };
            if !matches {
                self.clear_hwmon_cache();
            }
        }
    }

    fn check_control_file(&self) -> Result<PathBuf, String> {
        self.validate_cached_hwmon();
        let configured = self.config.pwm_path.as_deref();
        let path = self.resolve(configured, "pwm_path")?;
        match OpenOptions::new().write(true).open(&path) {
            Ok(_file) => Ok(path),
            Err(first_error) if configured.map(|value| !value.is_absolute()).unwrap_or(false) => {
                self.clear_hwmon_cache();
                let retry_path = self.resolve(configured, "pwm_path")
                    .map_err(|error| format!("control file {} is not writable: {first_error}; hwmon rediscovery failed: {error}", path.display()))?;
                OpenOptions::new().write(true).open(&retry_path)
                    .map(|_file| retry_path.clone())
                    .map_err(|retry_error| format!("control file {} is not writable: {first_error}; retry failed: {retry_error}", retry_path.display()))
            }
            Err(error) => Err(format!("control file {} is not writable: {error}", path.display())),
        }
    }

    pub fn settings(&self) -> FanSettings {
        let runtime = self.lock_runtime();
        FanSettings { mode: runtime.mode, throttle: runtime.throttle }
    }

    pub fn view(&self, include_rpm: bool) -> FanView {
        self.refresh_control_status();
        let (mode, throttle) = {
            let runtime = self.lock_runtime();
            (if runtime.active { Some(runtime.mode) } else { None }, runtime.throttle)
        };
        let rpm = if include_rpm { self.read_rpm() } else { None };
        FanView { label: self.config.label.clone(), mode, throttle, rpm }
    }

    pub fn update_settings(
        &self,
        supplied_mode: Option<FanMode>,
        supplied_throttle: Option<u8>,
    ) -> (FanSettings, u64, bool) {
        let (settings, old_mode, old_throttle, revision, was_active) = {
            let mut runtime = self.lock_runtime();
            let old_mode = runtime.mode;
            let old_throttle = runtime.throttle;
            let was_active = runtime.active;
            let throttle = supplied_throttle.unwrap_or(runtime.throttle);
            let mode = match supplied_mode {
                Some(mode) => mode,
                None => match supplied_throttle {
                    Some(0) => FanMode::Low,
                    Some(100) => FanMode::High,
                    Some(_) => FanMode::Percent,
                    None => runtime.mode,
                },
            };
            runtime.mode = mode;
            runtime.throttle = throttle;
            runtime.revision = runtime.revision.saturating_add(1);
            (
                FanSettings { mode, throttle },
                old_mode,
                old_throttle,
                runtime.revision,
                was_active,
            )
        };
        info(format!(
            "{}: settings changed mode={} throttle={} (was mode={} throttle={})",
            self.config.label,
            settings.mode.as_str(),
            settings.throttle,
            old_mode.as_str(),
            old_throttle
        ));
        self.changed.notify_all();
        (settings, revision, was_active)
    }

    pub fn wait_for_revision(&self, revision: u64) -> bool {
        let deadline = Instant::now() + KICKSTART_TIMEOUT;
        let mut runtime = self.lock_runtime();
        while runtime.applied_revision < revision && runtime.active && !runtime.stopping {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let remaining = deadline.saturating_duration_since(now);
            runtime = match self.changed.wait_timeout(runtime, remaining) {
                Ok((guard, _)) => guard,
                Err(poisoned) => {
                    error(format!("{}: recovering poisoned condition-variable lock", self.config.label));
                    poisoned.into_inner().0
                }
            };
        }
        runtime.active && runtime.applied_revision >= revision
    }

    pub fn request_kickstart(&self) -> Result<(bool, Option<String>), String> {
        let requested_seq;
        {
            let mut runtime = self.lock_runtime();
            if runtime.stopping {
                return Err("program is shutting down".to_owned());
            }
            runtime.active = false;
            runtime.kickstart_seq = runtime.kickstart_seq.saturating_add(1);
            requested_seq = runtime.kickstart_seq;
        }
        info(format!("{}: kickstart requested", self.config.label));
        self.changed.notify_all();

        let deadline = Instant::now() + KICKSTART_TIMEOUT;
        let mut runtime = self.lock_runtime();
        while runtime.completed_kickstart_seq < requested_seq && !runtime.stopping {
            let now = Instant::now();
            if now >= deadline {
                return Err(format!("kickstart timed out for {}", self.config.label));
            }
            let remaining = deadline.saturating_duration_since(now);
            runtime = match self.changed.wait_timeout(runtime, remaining) {
                Ok((guard, _)) => guard,
                Err(poisoned) => {
                    error(format!("{}: recovering poisoned condition-variable lock", self.config.label));
                    poisoned.into_inner().0
                }
            };
        }
        Ok((runtime.active, runtime.control_error.clone()))
    }

    pub fn stop(&self) {
        {
            let mut runtime = self.lock_runtime();
            runtime.stopping = true;
        }
        self.changed.notify_all();
    }

    pub fn save_settings(&self) -> FanSettings {
        self.settings()
    }

    fn record_control_result(
        &self,
        result: &Result<PathBuf, String>,
        completed_seq: Option<u64>,
        revision: u64,
        kickstart_seq: u64,
    ) -> bool {
        let mut runtime = self.lock_runtime();
        if runtime.kickstart_seq != kickstart_seq || runtime.stopping {
            self.changed.notify_all();
            return false;
        }
        runtime.applied_revision = runtime.applied_revision.max(revision);
        match result {
            Ok(_) => {
                let was_inactive = !runtime.active;
                runtime.active = true;
                runtime.control_error = None;
                if let Some(sequence) = completed_seq {
                    runtime.completed_kickstart_seq = runtime.completed_kickstart_seq.max(sequence);
                }
                if was_inactive {
                    info(format!("{}: control active", self.config.label));
                }
            }
            Err(reason) => {
                let should_log = runtime.active || runtime.control_error.as_deref() != Some(reason.as_str());
                runtime.active = false;
                runtime.control_error = Some(reason.clone());
                if let Some(sequence) = completed_seq {
                    runtime.completed_kickstart_seq = runtime.completed_kickstart_seq.max(sequence);
                }
                if should_log {
                    if reason == "pwm_path is not configured" {
                        warn(format!("{}: control inactive: {reason}", self.config.label));
                    } else {
                        error(format!("{}: control inactive: {reason}", self.config.label));
                    }
                }
            }
        }
        self.changed.notify_all();
        true
    }

    fn read_rpm(&self) -> Option<u64> {
        let configured_path = self.config.rpm_path.as_deref();
        if configured_path.is_none() {
            return None;
        }
        self.validate_cached_hwmon();
        let read_value = || -> Result<u64, String> {
            let path = self.resolve(configured_path, "rpm_path")?;
            let mut file = fs::File::open(&path)
                .map_err(|error| format!("cannot open RPM file {}: {error}", path.display()))?;
            let mut contents = String::new();
            file.read_to_string(&mut contents)
                .map_err(|error| format!("cannot read RPM file {}: {error}", path.display()))?;
            contents.trim().parse::<u64>().map_err(|error| {
                format!("invalid RPM value in {}: {error}", path.display())
            })
        };
        let result = match read_value() {
            Ok(value) => Ok(value),
            Err(first_error) if configured_path.map(|path| !path.is_absolute()).unwrap_or(false) => {
                self.clear_hwmon_cache();
                read_value().map_err(|retry_error| {
                    format!("{first_error}; hwmon rediscovery/retry failed: {retry_error}")
                })
            }
            Err(error) => Err(error),
        };

        match result {
            Ok(rpm) => {
                let recovered = {
                    let mut runtime = self.lock_runtime();
                    runtime.last_rpm_error.take().is_some()
                };
                if recovered {
                    info(format!("{}: RPM readings recovered", self.config.label));
                }
                Some(rpm)
            }
            Err(reason) => {
                let should_log = {
                    let mut runtime = self.lock_runtime();
                    if runtime.last_rpm_error.as_deref() == Some(reason.as_str()) {
                        false
                    } else {
                        runtime.last_rpm_error = Some(reason.clone());
                        true
                    }
                };
                if should_log {
                    warn(format!("{}: RPM unavailable: {reason}", self.config.label));
                }
                None
            }
        }
    }

    fn shutdown_to_low(&self) {
        match self.write_control(self.config.low) {
            Ok(path) => info(format!("{}: shutdown value {} written to {}", self.config.label, self.config.low, path.display())),
            Err(reason) => warn(format!("{}: could not restore low state on shutdown: {reason}", self.config.label)),
        }
    }
}

#[derive(Clone, Copy)]
struct Snapshot {
    mode: FanMode,
    throttle: u8,
    revision: u64,
    active: bool,
    kickstart_seq: u64,
    stopping: bool,
}

impl FanControl {
    fn refresh_control_status(&self) {
        let active = {
            let runtime = self.lock_runtime();
            runtime.active
        };
        if active {
            if let Err(reason) = self.check_control_file() {
                self.mark_inactive(reason);
            }
        }
    }

    fn mark_inactive(&self, reason: String) {
        let mut runtime = self.lock_runtime();
        let should_log = runtime.active || runtime.control_error.as_deref() != Some(reason.as_str());
        runtime.active = false;
        runtime.control_error = Some(reason.clone());
        if should_log {
            if reason == "pwm_path is not configured" {
                warn(format!("{}: control inactive: {reason}", self.config.label));
            } else {
                error(format!("{}: control inactive: {reason}", self.config.label));
            }
        }
        self.changed.notify_all();
    }
}

#[derive(Clone, Copy)]
enum Action {
    Attempt { sequence: u64 },
    Reconfigure { sequence: u64 },
    Toggle,
    HealthCheck,
    Stop,
}

fn output_for(mode: FanMode, throttle: u8, low: i64, high: i64) -> (i64, bool) {
    match mode {
        FanMode::Low => (low, false),
        FanMode::High => (high, true),
        FanMode::Percent if throttle == 0 => (low, false),
        FanMode::Percent if throttle == 100 => (high, true),
        FanMode::Percent => (high, true),
    }
}

fn phase_duration(frequency_hz: f64, throttle: u8, high_phase: bool) -> Duration {
    let period = 1.0 / frequency_hz;
    let fraction = if high_phase {
        f64::from(throttle) / 100.0
    } else {
        f64::from(100_u8.saturating_sub(throttle)) / 100.0
    };
    Duration::from_secs_f64((period * fraction).max(0.000_001))
}

pub fn spawn_worker(fan: Arc<FanControl>) -> Result<JoinHandle<()>, String> {
    let thread_name = format!("fcp-{}", fan.config.label.replace(' ', "_"));
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || worker_loop(fan))
        .map_err(|error| format!("cannot start fan worker: {error}"))
}

fn worker_loop(fan: Arc<FanControl>) {
    let mut observed_revision = 0_u64;
    let mut observed_kickstart = 0_u64;
    let mut phase_high = false;
    let mut next_transition: Option<Instant> = None;
    let mut next_health_check = Instant::now();

    loop {
        let snapshot;
        let action;
        {
            let runtime = fan.lock_runtime();
            snapshot = Snapshot {
                mode: runtime.mode,
                throttle: runtime.throttle,
                revision: runtime.revision,
                active: runtime.active,
                kickstart_seq: runtime.kickstart_seq,
                stopping: runtime.stopping,
            };

            if snapshot.stopping {
                action = Action::Stop;
            } else if !snapshot.active && snapshot.kickstart_seq > observed_kickstart {
                action = Action::Attempt { sequence: snapshot.kickstart_seq };
            } else if snapshot.active && snapshot.revision != observed_revision {
                action = Action::Reconfigure { sequence: snapshot.kickstart_seq };
            } else if snapshot.active
                && snapshot.mode == FanMode::Percent
                && snapshot.throttle > 0
                && snapshot.throttle < 100
                && next_transition.map(|time| Instant::now() >= time).unwrap_or(true)
            {
                action = Action::Toggle;
            } else if snapshot.active && Instant::now() >= next_health_check {
                action = Action::HealthCheck;
            } else {
                let wait_for = if !snapshot.active {
                    None
                } else if snapshot.mode == FanMode::Percent && snapshot.throttle > 0 && snapshot.throttle < 100 {
                    let now = Instant::now();
                    let transition_wait = match next_transition {
                        Some(when) => when.saturating_duration_since(now),
                        None => Duration::from_millis(1),
                    };
                    Some(transition_wait.min(next_health_check.saturating_duration_since(now)))
                } else {
                    Some(next_health_check.saturating_duration_since(Instant::now()))
                };
                match wait_for {
                    Some(duration) => {
                        let _guard = match fan.changed.wait_timeout(runtime, duration.max(Duration::from_millis(1))) {
                            Ok((guard, _)) => guard,
                            Err(poisoned) => {
                                error(format!("{}: recovering poisoned condition-variable lock", fan.config.label));
                                poisoned.into_inner().0
                            }
                        };
                    }
                    None => {
                        let _guard = match fan.changed.wait(runtime) {
                            Ok(guard) => guard,
                            Err(poisoned) => {
                                error(format!("{}: recovering poisoned condition-variable lock", fan.config.label));
                                poisoned.into_inner()
                            }
                        };
                    }
                }
                continue;
            }
        }

        match action {
            Action::Stop => {
                fan.shutdown_to_low();
                info(format!("{}: worker stopped", fan.config.label));
                break;
            }
            Action::Attempt { sequence } | Action::Reconfigure { sequence } => {
                observed_revision = snapshot.revision;
                observed_kickstart = observed_kickstart.max(sequence);
                let (value, new_phase) = output_for(snapshot.mode, snapshot.throttle, fan.config.low, fan.config.high);
                let delay = if snapshot.mode == FanMode::Percent && snapshot.throttle > 0 && snapshot.throttle < 100 {
                    Some(phase_duration(fan.frequency_hz, snapshot.throttle, true))
                } else {
                    None
                };
                let result = fan.write_control(value);
                let completed_sequence = match action {
                    Action::Attempt { sequence } => Some(sequence),
                    _ => None,
                };
                let result_current = fan.record_control_result(
                    &result,
                    completed_sequence,
                    snapshot.revision,
                    snapshot.kickstart_seq,
                );
                match result {
                    Ok(path) => {
                        phase_high = new_phase;
                        next_transition = delay.map(|value| Instant::now() + value);
                        next_health_check = Instant::now() + HEALTH_CHECK_INTERVAL;
                        if result_current {
                            let label = if snapshot.mode == FanMode::Percent {
                                format!("percent (throttle {})", snapshot.throttle)
                            } else {
                                snapshot.mode.as_str().to_owned()
                            };
                            info(format!("{}: mode applied: {} via {}", fan.config.label, label, path.display()));
                        }
                    }
                    Err(_) => {
                        next_transition = None;
                        next_health_check = Instant::now() + HEALTH_CHECK_INTERVAL;
                    }
                }
            }
            Action::Toggle => {
                let new_phase = !phase_high;
                let value = if new_phase { fan.config.high } else { fan.config.low };
                match fan.write_control(value) {
                    Ok(_) => {
                        phase_high = new_phase;
                        next_transition = Some(Instant::now() + phase_duration(fan.frequency_hz, snapshot.throttle, phase_high));
                    }
                    Err(reason) => {
                        fan.mark_inactive(reason);
                        next_transition = None;
                    }
                }
            }
            Action::HealthCheck => {
                match fan.check_control_file() {
                    Ok(_) => next_health_check = Instant::now() + HEALTH_CHECK_INTERVAL,
                    Err(reason) => {
                        fan.mark_inactive(reason);
                        next_transition = None;
                    }
                }
            }
        }
    }
}

