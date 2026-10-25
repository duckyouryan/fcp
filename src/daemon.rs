//! One worker thread per fan does the software PWM; the main thread answers
//! requests on a unix socket (one line in, text out, connection closed).

use crate::config::{eval, Config, FanCfg, Pwm, Src};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

const SENSOR_EVERY: Duration = Duration::from_secs(1);

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
}
extern "C" fn on_signal(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    /// Not ours: the chip does whatever it did before we came.
    System,
    /// Fixed percent.
    Manual,
    /// Percent follows the temperature curve.
    Auto,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::System => "system",
            Mode::Manual => "manual",
            Mode::Auto => "auto",
        }
    }
    fn parse(s: &str) -> Option<Mode> {
        [Mode::System, Mode::Manual, Mode::Auto].into_iter().find(|m| m.name() == s)
    }
}

struct Ctl {
    mode: Mode,
    /// Manual percent.
    pct: u8,
    /// Percent being driven right now (for `fan list`).
    eff: Option<u8>,
    /// The pwm_enable value the system had before we took over; given back on release.
    saved: Option<i64>,
    /// Bumped on every change; the worker restarts its cycle when it moves.
    gen: u64,
    stop: bool,
}

struct Fan {
    cfg: FanCfg,
    ctl: Mutex<Ctl>,
    cv: Condvar,
}

/// What we know about a fan from the state file.
#[derive(Clone)]
struct St {
    mode: Mode,
    pct: u8,
    saved: Option<i64>,
}

impl Fan {
    fn new(cfg: FanCfg, st: Option<St>) -> Fan {
        let st = st.filter(|s| s.mode != Mode::System && cfg.pwm.is_some());
        let (mode, pct, saved) = st.map_or((Mode::System, 0, None), |s| (s.mode, s.pct, s.saved));
        let ctl = Ctl { mode, pct, eff: (mode == Mode::Manual).then_some(pct), saved, gen: 0, stop: false };
        Fan { cfg, ctl: Mutex::new(ctl), cv: Condvar::new() }
    }

    fn lock(&self) -> MutexGuard<'_, Ctl> {
        self.ctl.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Daemon start: a fan that was ours before keeps its saved system state; if we
    /// have none (clean stop), remember what the system has right now.
    fn resume(&self) {
        let Some(pwm) = &self.cfg.pwm else { return };
        let mut g = self.lock();
        if g.mode == Mode::System {
            return;
        }
        let name = &self.cfg.name;
        let saved = g.saved;
        match saved {
            Some(v) => eprintln!("{name}: resuming {} (system state to give back: {v})", g.mode.name()),
            None => match read_num(&pwm.src.path) {
                Ok(v) => {
                    g.saved = Some(v);
                    eprintln!("{name}: resuming {} (system state is {v})", g.mode.name());
                }
                Err(e) => {
                    eprintln!("{name}: {e}; staying in system mode");
                    g.mode = Mode::System;
                }
            },
        }
    }

    /// `fcp fan NAME VALUE`. Returns a one-line description of what happened.
    fn set(&self, value: &str) -> Result<String, String> {
        let Some(pwm) = &self.cfg.pwm else {
            return Err(format!("{} has no pwm configured", self.cfg.name));
        };
        match value {
            "system" => self.release(pwm),
            "auto" if self.cfg.curve.is_empty() => {
                Err(format!("{} has no sensor/curve configured", self.cfg.name))
            }
            "auto" => self.take(pwm, Mode::Auto, 0),
            n => match n.parse::<u8>() {
                Ok(p) if p <= 100 => self.take(pwm, Mode::Manual, p),
                _ => Err(format!("'{n}': expected 0-100, auto or system")),
            },
        }
    }

    /// Switch to manual/auto. Coming from system, first remember the system's value.
    fn take(&self, pwm: &Pwm, mode: Mode, pct: u8) -> Result<String, String> {
        let mut g = self.lock();
        let mut note = String::new();
        if g.mode == Mode::System {
            let current = read_num(&pwm.src.path)?;
            g.saved = Some(current);
            note = format!(" (system state was {current})");
        }
        g.mode = mode;
        g.pct = pct;
        g.eff = (mode == Mode::Manual).then_some(pct);
        g.gen += 1;
        self.cv.notify_all();
        let what = if mode == Mode::Auto { "auto".to_string() } else { format!("manual {pct}%") };
        Ok(format!("{}: {what}{note}", self.cfg.name))
    }

    /// Switch to system: write back the saved value, then stop touching the file.
    fn release(&self, pwm: &Pwm) -> Result<String, String> {
        let mut g = self.lock();
        if g.mode == Mode::System {
            return Ok(format!("{}: already system", self.cfg.name));
        }
        let saved = g.saved;
        let note = match saved {
            Some(v) => {
                write_num(&pwm.src.path, v)
                    .map_err(|e| format!("cannot restore {}: {e}", pwm.src.path.display()))?;
                format!(" (restored {v})")
            }
            None => String::new(),
        };
        g.mode = Mode::System;
        g.saved = None;
        g.eff = None;
        g.gen += 1;
        self.cv.notify_all();
        Ok(format!("{}: system{note}", self.cfg.name))
    }

    /// Daemon stop: give the fan back, keep the mode so the next start resumes it.
    fn shutdown(&self) {
        let mut g = self.lock();
        g.stop = true;
        let saved = g.saved;
        if let (Some(pwm), Some(v)) = (&self.cfg.pwm, saved) {
            match write_num(&pwm.src.path, v) {
                Ok(()) => {
                    g.saved = None;
                    eprintln!("{}: restored system state ({v})", self.cfg.name);
                }
                Err(e) => eprintln!("{}: cannot restore system state: {e}", self.cfg.name),
            }
        }
        self.cv.notify_all();
    }

    /// Sleep until `dur` passes (None = until something changes).
    /// Returns true if interrupted by a change or stop.
    fn sleep(&self, gen: u64, dur: Option<Duration>) -> bool {
        let end = dur.map(|d| Instant::now() + d);
        let mut g = self.lock();
        loop {
            if g.gen != gen || g.stop {
                return true;
            }
            g = match end {
                None => self.cv.wait(g).unwrap_or_else(|e| e.into_inner()),
                Some(end) => {
                    let now = Instant::now();
                    if now >= end {
                        return false;
                    }
                    self.cv.wait_timeout(g, end - now).unwrap_or_else(|e| e.into_inner()).0
                }
            };
        }
    }

    /// Write a pwm value, unless nothing changed or the world moved on since the
    /// worker looked (then false: restart the cycle). Done under the lock so a
    /// release can never be followed by a stale write.
    fn put(&self, pwm: &Pwm, gen: u64, value: i64, out: &mut Out) -> bool {
        let g = self.lock();
        if g.gen != gen || g.stop {
            return false;
        }
        if out.last == Some(value) {
            return true;
        }
        match write_num(&pwm.src.path, value) {
            Ok(()) => {
                out.last = Some(value);
                if !out.ok {
                    eprintln!("{}: pwm writes work again", self.cfg.name);
                    out.ok = true;
                }
            }
            Err(e) => {
                out.last = None;
                if out.ok {
                    eprintln!("{}: cannot write {}: {e}", self.cfg.name, pwm.src.path.display());
                    out.ok = false;
                }
            }
        }
        true
    }

    fn read_temp(&self) -> Result<f64, String> {
        let s = self.cfg.sensor.as_ref().ok_or("no sensor configured")?;
        read_num(&s.path).map(|v| v as f64 / 1000.0)
    }
}

struct Out {
    last: Option<i64>,
    ok: bool,
}

/// The software PWM. One cycle = `max` for pct% of the period, then `min` for the rest.
fn worker(fan: Arc<Fan>, period: Duration) {
    let Some(pwm) = &fan.cfg.pwm else { return };
    let name = &fan.cfg.name;
    let mut out = Out { last: None, ok: true };
    let (mut seen, mut last_read) = (u64::MAX, None::<Instant>);
    let (mut auto_pct, mut sensor_ok) = (100u8, true);
    loop {
        let (mode, manual, gen) = {
            let g = fan.lock();
            if g.stop {
                return;
            }
            (g.mode, g.pct, g.gen)
        };
        if gen != seen {
            (seen, out.last, last_read) = (gen, None, None);
        }
        let pct = match mode {
            Mode::System => {
                fan.sleep(gen, None);
                continue;
            }
            Mode::Manual => manual,
            Mode::Auto => {
                if last_read.map_or(true, |t| t.elapsed() >= SENSOR_EVERY) {
                    last_read = Some(Instant::now());
                    match fan.read_temp() {
                        Ok(t) => {
                            auto_pct = eval(&fan.cfg.curve, t);
                            if !sensor_ok {
                                eprintln!("{name}: sensor works again");
                                sensor_ok = true;
                            }
                        }
                        Err(e) => {
                            auto_pct = 100;
                            if sensor_ok {
                                eprintln!("{name}: {e}; running at 100%");
                                sensor_ok = false;
                            }
                        }
                    }
                    let mut g = fan.lock();
                    if g.gen == gen {
                        g.eff = Some(auto_pct);
                    }
                }
                auto_pct
            }
        };

        if pct == 0 || pct == 100 {
            let v = if pct == 100 { pwm.max } else { pwm.min };
            if !fan.put(pwm, gen, v, &mut out) {
                continue;
            }
            fan.sleep(gen, Some(period));
        } else {
            let high = period.mul_f64(f64::from(pct) / 100.0);
            if !fan.put(pwm, gen, pwm.max, &mut out) || fan.sleep(gen, Some(high)) {
                continue;
            }
            if !fan.put(pwm, gen, pwm.min, &mut out) {
                continue;
            }
            fan.sleep(gen, Some(period - high));
        }
    }
}

fn read_num(path: &Path) -> Result<i64, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    text.trim()
        .parse()
        .map_err(|_| format!("{} contains '{}', not a number", path.display(), text.trim()))
}

fn write_num(path: &Path, value: i64) -> io::Result<()> {
    // No truncate: sysfs files are not regular files.
    let mut f = OpenOptions::new().write(true).open(path)?;
    f.write_all(format!("{value}\n").as_bytes())
}

// ---- state file: one line per fan: NAME MODE PERCENT SAVED ("-" = none) ----

fn format_state(fans: &[Arc<Fan>]) -> String {
    let mut out = String::new();
    for f in fans.iter().filter(|f| f.cfg.pwm.is_some()) {
        let g = f.lock();
        let saved = g.saved.map_or("-".to_string(), |v| v.to_string());
        out += &format!("{} {} {} {saved}\n", f.cfg.name, g.mode.name(), g.pct);
    }
    out
}

fn parse_state(text: &str) -> HashMap<String, St> {
    let mut map = HashMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let w: Vec<&str> = line.split_whitespace().collect();
        let parsed = (|| {
            if w.len() != 4 {
                return None;
            }
            let saved = if w[3] == "-" { None } else { Some(w[3].parse().ok()?) };
            let pct: u8 = w[2].parse().ok().filter(|p| *p <= 100)?;
            Some(St { mode: Mode::parse(w[1])?, pct, saved })
        })();
        match parsed {
            Some(st) => {
                map.insert(w[0].to_string(), st);
            }
            None => eprintln!("fcp: ignoring bad state line '{line}'"),
        }
    }
    map
}

fn persist(fans: &[Arc<Fan>], path: &Path) {
    let result = (|| -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, format_state(fans))?;
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        eprintln!("fcp: cannot save state to {}: {e}", path.display());
    }
}

// ---- requests ----

fn command(fans: &[Arc<Fan>], line: &str, state: &Path) -> Result<String, String> {
    match line.split_whitespace().collect::<Vec<_>>().as_slice() {
        ["list"] => Ok(list(fans)),
        ["set", name, value] => {
            let fan = fans
                .iter()
                .find(|f| f.cfg.name == *name)
                .ok_or_else(|| format!("unknown fan '{name}'"))?;
            let msg = fan.set(value)?;
            eprintln!("fcp: {msg}");
            persist(fans, state);
            Ok(msg + "\n")
        }
        _ => Err("bad request".into()),
    }
}

fn list(fans: &[Arc<Fan>]) -> String {
    let w = fans.iter().map(|f| f.cfg.name.len()).max().unwrap_or(0).max(3);
    let mut out = format!("{:<w$}  {:<6}  {:>4}  {:>9}  {:>6}\n", "FAN", "MODE", "SET", "SPEED", "TEMP");
    for f in fans {
        let (mode, set) = match &f.cfg.pwm {
            None => ("-", "-".to_string()),
            Some(_) => {
                let g = f.lock();
                let set = match (g.mode, g.eff) {
                    (Mode::System, _) | (_, None) => "-".to_string(),
                    (_, Some(p)) => format!("{p}%"),
                };
                (g.mode.name(), set)
            }
        };
        let rpm = read_or(&f.cfg.tach, |v| format!("{v} rpm"));
        let temp = read_or(&f.cfg.sensor, |v| format!("{:.0} °C", v as f64 / 1000.0));
        out += &format!("{:<w$}  {mode:<6}  {set:>4}  {rpm:>9}  {temp:>6}\n", f.cfg.name);
    }
    out
}

fn read_or(src: &Option<Src>, show: impl Fn(i64) -> String) -> String {
    match src {
        None => "-".into(),
        Some(s) => read_num(&s.path).map(show).unwrap_or_else(|_| "err".into()),
    }
}

fn handle(mut s: UnixStream, fans: &[Arc<Fan>], state: &Path) {
    let _ = s.set_nonblocking(false);
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    let _ = BufReader::new((&s).take(256)).read_line(&mut line);
    let reply = match command(fans, line.trim(), state) {
        Ok(text) => text,
        Err(e) => format!("error: {e}\n"),
    };
    let _ = s.write_all(reply.as_bytes());
}

fn bind(path: &Path) -> Result<UnixListener, String> {
    if UnixStream::connect(path).is_ok() {
        return Err(format!("{} is in use; is fcp already running?", path.display()));
    }
    let _ = fs::remove_file(path);
    let err = |e: io::Error| format!("socket {}: {e}", path.display());
    let listener = UnixListener::bind(path).map_err(err)?;
    // Anyone may talk to the daemon, no root needed.
    fs::set_permissions(path, fs::Permissions::from_mode(0o666)).map_err(err)?;
    listener.set_nonblocking(true).map_err(err)?;
    Ok(listener)
}

pub fn run() -> Result<(), String> {
    let config_path = crate::config_path();
    let text = fs::read_to_string(&config_path)
        .map_err(|e| format!("cannot read {}: {e}", config_path.display()))?;
    let cfg = Config::parse(&text).map_err(|e| format!("{}: {e}", config_path.display()))?;
    cfg.check()?;

    let (state_path, socket_path) = (crate::state_path(), crate::socket_path());
    let saved = fs::read_to_string(&state_path).map(|t| parse_state(&t)).unwrap_or_default();
    let period = Duration::from_secs_f64(1.0 / cfg.frequency);

    // SAFETY: the handler only stores into an atomic.
    unsafe {
        signal(2, on_signal);
        signal(15, on_signal);
    }
    let listener = bind(&socket_path)?;

    let fans: Vec<Arc<Fan>> = cfg
        .fans
        .into_iter()
        .map(|c| {
            let st = saved.get(&c.name).cloned();
            Arc::new(Fan::new(c, st))
        })
        .collect();
    for f in &fans {
        f.resume();
    }
    persist(&fans, &state_path);
    let workers: Vec<_> = fans
        .iter()
        .filter(|f| f.cfg.pwm.is_some())
        .map(|f| {
            let f = Arc::clone(f);
            thread::spawn(move || worker(f, period))
        })
        .collect();
    eprintln!("fcp: {} fans, {} Hz, config {}", fans.len(), cfg.frequency, config_path.display());

    while !STOP.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => handle(stream, &fans, &state_path),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                eprintln!("fcp: accept failed: {e}");
                break;
            }
        }
    }

    eprintln!("fcp: stopping");
    for f in &fans {
        f.shutdown();
    }
    for w in workers {
        let _ = w.join();
    }
    persist(&fans, &state_path);
    let _ = fs::remove_file(&socket_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fcp-test-{}-{name}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn fan_for(file: &Path, curve: bool, st: Option<St>) -> Fan {
        let cfg = FanCfg {
            name: "T".into(),
            pwm: Some(Pwm { src: Src { path: file.to_path_buf(), chip: None }, min: 2, max: 0 }),
            tach: None,
            sensor: curve.then(|| Src { path: file.to_path_buf(), chip: None }),
            curve: if curve { vec![(40.0, 0.0), (75.0, 100.0)] } else { vec![] },
        };
        Fan::new(cfg, st)
    }

    fn content(file: &Path) -> String {
        fs::read_to_string(file).unwrap().trim().to_string()
    }

    #[test]
    fn taking_remembers_and_system_gives_back() {
        let file = tmpdir("cassette").join("pwm_enable");
        fs::write(&file, "2\n").unwrap();
        let fan = fan_for(&file, false, None);

        fan.set("40").unwrap();
        assert_eq!(fan.lock().saved, Some(2));
        assert_eq!(fan.lock().mode, Mode::Manual);

        write_num(&file, 0).unwrap(); // pretend the worker wrote something
        fan.set("system").unwrap();
        assert_eq!(content(&file), "2");
        assert_eq!(fan.lock().saved, None);
        assert_eq!(fan.lock().mode, Mode::System);
    }

    #[test]
    fn switching_manual_to_auto_keeps_the_original_state() {
        let file = tmpdir("keep").join("pwm_enable");
        fs::write(&file, "1\n").unwrap();
        let fan = fan_for(&file, true, None);
        fan.set("30").unwrap();
        write_num(&file, 0).unwrap();
        fan.set("auto").unwrap();
        assert_eq!(fan.lock().saved, Some(1)); // not 0
        fan.set("system").unwrap();
        assert_eq!(content(&file), "1");
    }

    #[test]
    fn bad_requests_are_refused() {
        let file = tmpdir("bad").join("pwm_enable");
        fs::write(&file, "2\n").unwrap();
        let fan = fan_for(&file, false, None);
        assert!(fan.set("auto").is_err()); // no curve configured
        assert!(fan.set("101").is_err());
        assert!(fan.set("-1").is_err());
        assert!(fan.set("fast").is_err());
        assert_eq!(fan.lock().mode, Mode::System);
        assert_eq!(content(&file), "2"); // nothing was written
    }

    #[test]
    fn restart_after_crash_keeps_the_saved_state() {
        let file = tmpdir("crash").join("pwm_enable");
        fs::write(&file, "0\n").unwrap(); // left over from the crashed daemon
        let st = St { mode: Mode::Manual, pct: 40, saved: Some(2) };
        let fan = fan_for(&file, false, Some(st));
        fan.resume();
        assert_eq!(fan.lock().saved, Some(2));
        fan.set("system").unwrap();
        assert_eq!(content(&file), "2");
    }

    #[test]
    fn restart_after_clean_stop_remembers_the_system_state_again() {
        let file = tmpdir("clean").join("pwm_enable");
        fs::write(&file, "1\n").unwrap();
        let st = St { mode: Mode::Auto, pct: 0, saved: None };
        let fan = fan_for(&file, true, Some(st));
        fan.resume();
        assert_eq!(fan.lock().saved, Some(1));
        assert_eq!(fan.lock().mode, Mode::Auto);
    }

    #[test]
    fn state_file_roundtrip() {
        let file = tmpdir("state").join("pwm_enable");
        fs::write(&file, "2\n").unwrap();
        let fan = Arc::new(fan_for(&file, false, None));
        fan.set("40").unwrap();
        let text = format_state(&[fan]);
        assert_eq!(text, "T manual 40 2\n");
        let parsed = parse_state(&format!("{text}garbage\nX system 0 -\n"));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["T"].saved, Some(2));
        assert_eq!(parsed["T"].mode, Mode::Manual);
    }

    #[test]
    fn worker_hits_the_requested_duty_and_stop_restores() {
        let file = tmpdir("duty").join("pwm_enable");
        fs::write(&file, "2\n").unwrap();
        let fan = Arc::new(fan_for(&file, false, None));
        let w = {
            let f = Arc::clone(&fan);
            thread::spawn(move || worker(f, Duration::from_millis(100)))
        };
        fan.set("30").unwrap();
        thread::sleep(Duration::from_millis(300));
        let (mut high, mut total) = (0u32, 0u32);
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end {
            if content(&file) == "0" {
                high += 1;
            }
            total += 1;
        }
        fan.shutdown();
        w.join().unwrap();
        let duty = f64::from(high) / f64::from(total);
        assert!((0.2..0.4).contains(&duty), "duty was {duty}");
        assert_eq!(content(&file), "2");
    }
}
