use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn read(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

pub fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read(path)?.trim().parse().ok()
}

/// Reads a cgroup limit file where "max" means unlimited.
pub fn read_limit(path: impl AsRef<Path>) -> Option<Option<u64>> {
    let s = read(path)?;
    let s = s.trim();
    if s == "max" {
        Some(None)
    } else {
        s.parse().ok().map(Some)
    }
}

pub fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn now_nanos() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

pub fn clk_tck() -> f64 {
    // SAFETY: sysconf has no preconditions.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 {
        v as f64
    } else {
        100.0
    }
}

pub fn regex(pattern: &str) -> Option<regex_lite::Regex> {
    if pattern.is_empty() {
        return None;
    }
    match regex_lite::Regex::new(pattern) {
        Ok(r) => Some(r),
        Err(e) => {
            log::error!("invalid pattern {pattern:?}: {e}");
            None
        }
    }
}

pub fn matches(re: &Option<regex_lite::Regex>, s: &str) -> bool {
    re.as_ref().is_some_and(|r| r.is_match(s))
}

/// Per-second rate between two counter readings; None across resets.
pub fn rate(prev: u64, cur: u64, secs: f64) -> Option<f64> {
    if cur < prev || secs <= 0.0 {
        return None;
    }
    Some((cur - prev) as f64 / secs)
}

pub mod shutdown {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    pub fn request() {
        REQUESTED.store(true, Ordering::SeqCst);
    }

    pub fn requested() -> bool {
        REQUESTED.load(Ordering::SeqCst)
    }

    /// Sleeps for `d`, returning early (true) when shutdown is requested.
    pub fn sleep(d: Duration) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if requested() {
                return true;
            }
            std::thread::sleep((end - Instant::now()).min(Duration::from_millis(100)));
        }
        requested()
    }
}

pub mod log {
    use std::sync::atomic::{AtomicU8, Ordering};

    static LEVEL: AtomicU8 = AtomicU8::new(2);

    pub fn set_level(s: &str) {
        let l = match s {
            "error" => 0,
            "warn" => 1,
            "info" => 2,
            _ => 3,
        };
        LEVEL.store(l, Ordering::Relaxed);
    }

    pub fn enabled(l: u8) -> bool {
        l <= LEVEL.load(Ordering::Relaxed)
    }

    pub fn emit(level: &str, msg: std::fmt::Arguments) {
        let ts = crate::util::now_secs();
        eprintln!("{{\"ts\":{ts:.3},\"level\":\"{level}\",\"msg\":{}}}", serde_json::Value::String(msg.to_string()));
    }

    macro_rules! error { ($($t:tt)*) => { if $crate::util::log::enabled(0) { $crate::util::log::emit("error", format_args!($($t)*)) } } }
    macro_rules! warn_ { ($($t:tt)*) => { if $crate::util::log::enabled(1) { $crate::util::log::emit("warn", format_args!($($t)*)) } } }
    macro_rules! info { ($($t:tt)*) => { if $crate::util::log::enabled(2) { $crate::util::log::emit("info", format_args!($($t)*)) } } }
    macro_rules! debug { ($($t:tt)*) => { if $crate::util::log::enabled(3) { $crate::util::log::emit("debug", format_args!($($t)*)) } } }
    pub(crate) use {debug, error, info, warn_ as warn};
}
