//! Small std-only helpers: time formatting, random ids, hashing, logging.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_millis() as u64
}

/// RFC 3339 UTC timestamp with millisecond precision.
pub fn now_iso() -> String {
    iso_from_millis(unix_millis())
}

pub fn iso_from_millis(millis: u64) -> String {
    let secs = millis / 1000;
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        millis % 1000
    )
}

/// Calendar date for a day count since 1970-01-01 (Howard Hinnant's algorithm).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub fn today_long() -> String {
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
        "November", "December",
    ];
    const DAYS: [&str; 7] = ["Thursday", "Friday", "Saturday", "Sunday", "Monday", "Tuesday", "Wednesday"];
    let days = (unix_millis() / 1000 / 86_400) as i64;
    let (year, month, day) = civil_from_days(days);
    format!("{}, {} {day}, {year}", DAYS[(days % 7) as usize], MONTHS[(month - 1) as usize])
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 128 random bits as hex, from the kernel CSPRNG with a time/counter fallback.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let filled = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_ok();
    if !filled {
        let mut seed = unix_millis() ^ (std::process::id() as u64) << 32 ^ COUNTER.fetch_add(1, Ordering::Relaxed);
        for byte in &mut buf {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *byte = seed as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", random_hex(12))
}

/// FNV-1a 64-bit; used only for stable, non-security name disambiguation.
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Truncate to at most `max` bytes on a char boundary, with a marker.
pub fn truncate(text: &str, max: usize, marker: &str) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &text[..end])
}

pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

// ---- logging ---------------------------------------------------------------

static LEVEL: AtomicU8 = AtomicU8::new(2);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

pub fn init_logging() {
    let level = match std::env::var("MELCHIOR_LOG").unwrap_or_default().to_ascii_lowercase().as_str() {
        "error" => Level::Error,
        "warn" => Level::Warn,
        "debug" | "trace" => Level::Debug,
        _ => Level::Info,
    };
    LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Log lines go to stderr only: stdout is reserved for the JSON-RPC stream.
pub fn log(level: Level, scope: &str, message: &str) {
    if level as u8 > LEVEL.load(Ordering::Relaxed) {
        return;
    }
    let tag = match level {
        Level::Error => "error",
        Level::Warn => "warn",
        Level::Info => "info",
        Level::Debug => "debug",
    };
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{} {tag} [{scope}] {message}", now_iso());
}

#[macro_export]
macro_rules! log_info { ($scope:expr, $($arg:tt)*) => { $crate::util::log($crate::util::Level::Info, $scope, &format!($($arg)*)) } }
#[macro_export]
macro_rules! log_warn { ($scope:expr, $($arg:tt)*) => { $crate::util::log($crate::util::Level::Warn, $scope, &format!($($arg)*)) } }
#[macro_export]
macro_rules! log_error { ($scope:expr, $($arg:tt)*) => { $crate::util::log($crate::util::Level::Error, $scope, &format!($($arg)*)) } }
#[macro_export]
macro_rules! log_debug { ($scope:expr, $($arg:tt)*) => { $crate::util::log($crate::util::Level::Debug, $scope, &format!($($arg)*)) } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_dates() {
        assert_eq!(iso_from_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_millis(1_793_000_000_123), "2026-10-26T07:33:20.123Z");
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn session_ids_are_path_safe() {
        assert!(valid_session_id("saved-session"));
        assert!(valid_session_id("a.b_c-1"));
        assert!(!valid_session_id(".."));
        assert!(!valid_session_id("a/b"));
        assert!(!valid_session_id(""));
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate("héllo", 2, "…"), "h…");
    }
}
