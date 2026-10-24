use std::mem::MaybeUninit;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

fn timestamp() -> String {
    let elapsed = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(value) => value,
        Err(_) => return "time-unavailable".to_owned(),
    };
    let seconds = elapsed.as_secs() as libc::time_t;
    let mut broken_down = MaybeUninit::<libc::tm>::uninit();
    // SAFETY: `seconds` is a valid pointer to a time_t and `broken_down` points to writable storage.
    let result = unsafe { libc::localtime_r(&seconds, broken_down.as_mut_ptr()) };
    if result.is_null() {
        return format!("unix:{}.{:03}", elapsed.as_secs(), elapsed.subsec_millis());
    }
    // SAFETY: localtime_r returned a non-null pointer and initialized the tm structure.
    let broken_down = unsafe { broken_down.assume_init() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        broken_down.tm_year + 1900,
        broken_down.tm_mon + 1,
        broken_down.tm_mday,
        broken_down.tm_hour,
        broken_down.tm_min,
        broken_down.tm_sec,
        elapsed.subsec_millis()
    )
}

pub fn log(level: &str, message: impl AsRef<str>) {
    let mut output = std::io::stdout().lock();
    let _ = writeln!(output, "[{}] [{level}] {}", timestamp(), message.as_ref());
}

pub fn info(message: impl AsRef<str>) {
    log("INFO", message);
}

pub fn warn(message: impl AsRef<str>) {
    log("WARN", message);
}

pub fn error(message: impl AsRef<str>) {
    log("ERROR", message);
}
