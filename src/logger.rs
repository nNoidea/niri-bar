use chrono::Local;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::net::UnixDatagram;
use std::sync::Mutex;

/// Persistent log file — written alongside stderr and journal so logs are always available
static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);

/// Systemd journal socket — sends structured log datagrams directly to systemd-journald
static JOURNAL_SOCKET: Mutex<Option<UnixDatagram>> = Mutex::new(None);

/// Syslog priority for a log `level` (`ERROR`/`FATAL PANIC` -> 3, `WARN` -> 4,
/// `INFO` -> 6, `DEBUG` -> 7).
pub fn priority_for_level(level: &str) -> &'static str {
    match level {
        "ERROR" | "FATAL PANIC" => "3",
        "WARN" => "4",
        "INFO" => "6",
        "DEBUG" => "7",
        _ => "6",
    }
}

/// Format one human-readable log line: `[ts] [LEVEL] [tag] message`.
pub fn format_log_line(ts: &str, level: &str, tag: &str, content: &str) -> String {
    format!("[{}] [{: <5}] [{}] {}", ts, level, tag, content)
}

/// Format a systemd-journal datagram payload for `tag`/`content`/`priority`.
pub fn format_journal_payload(tag: &str, content: &str, priority: &str) -> String {
    format!(
        "MESSAGE=[{}] {}\nSYSLOG_IDENTIFIER=niri-bar\nPRIORITY={}\nNIRI_TAG={}\n",
        tag, content, priority, tag
    )
}

/// True when debug logging is enabled via `RUST_LOG` (contains
/// `debug`/`trace` or equals `1`) or `NIRI_BAR_DEBUG=1`/`true`.
pub fn parse_debug_flag(rust_log: Option<&str>, niri_debug: Option<&str>) -> bool {
    if let Some(v) = rust_log {
        let l = v.to_lowercase();
        if l.contains("debug") || l.contains("trace") || l == "1" {
            return true;
        }
    }
    if let Some(v) = niri_debug {
        if v == "1" || v.eq_ignore_ascii_case("true") {
            return true;
        }
    }
    false
}

/// Write a pre-formatted log line to stderr, file, and systemd journal.
pub fn emit(level: &str, tag: &str, content: &str) {
    let ts = timestamp();
    let formatted = format_log_line(&ts, level, tag, content);

    // 1. Output to stderr
    eprintln!("{}", formatted);

    // 2. Output to log file
    if let Ok(mut guard) = LOG_FILE.lock() {
        if let Some(ref mut file) = *guard {
            let _ = writeln!(file, "{}", formatted);
            let _ = file.flush();
        }
    }

    // 3. Output to systemd journal
    if let Ok(guard) = JOURNAL_SOCKET.lock() {
        if let Some(ref sock) = *guard {
            let priority = priority_for_level(level);
            let payload = format_journal_payload(tag, content, priority);
            let _ = sock.send(payload.as_bytes());
        }
    }
}

#[macro_export]
macro_rules! log_info {
    ($tag:expr, $($arg:tt)*) => {
        $crate::logger::emit("INFO", $tag, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($tag:expr, $($arg:tt)*) => {
        $crate::logger::emit("WARN", $tag, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($tag:expr, $($arg:tt)*) => {
        $crate::logger::emit("ERROR", $tag, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($tag:expr, $($arg:tt)*) => {
        if $crate::logger::is_debug_enabled() {
            $crate::logger::emit("DEBUG", $tag, &format!($($arg)*));
        }
    };
}

/// Current local timestamp for log lines (`%Y-%m-%d %H:%M:%S%.3f`).
pub fn timestamp() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// True when debug logging is currently enabled (see [`parse_debug_flag`]).
pub fn is_debug_enabled() -> bool {
    let rust_log = std::env::var("RUST_LOG").ok();
    let niri_debug = std::env::var("NIRI_BAR_DEBUG").ok();
    parse_debug_flag(rust_log.as_deref(), niri_debug.as_deref())
}

fn log_file_path() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.trim().is_empty() {
            return Some(std::path::PathBuf::from(dir).join("niri-bar.log"));
        }
    }
    // Per-UID fallback (no predictable /tmp symlink target).
    if let Ok(uid) = std::env::var("UID").or_else(|_| std::env::var("EUID")) {
        return Some(std::path::PathBuf::from(format!("/tmp/niri-bar-{uid}.log")));
    }
    None
}

fn open_log_file() -> Option<File> {
    let log_path = log_file_path()?;
    // Refuse symlinks (same-user hijack → append to victim file).
    if let Ok(meta) = std::fs::symlink_metadata(&log_path) {
        if meta.file_type().is_symlink() {
            eprintln!("niri-bar: refusing to log to symlink {log_path:?}");
            return None;
        }
        if should_rotate_log(meta.len()) {
            let backup = log_path.with_extension("log.1");
            let _ = std::fs::rename(&log_path, &backup);
        }
    }
    // Mode 0600 via OpenOptions (umask applies); no O_NOFOLLOW in std, the
    // symlink check above is the portable guard.
    OpenOptions::new().create(true).append(true).open(&log_path).ok()
}

/// Pure rotation decision for tests: true when `len` exceeds the 5MB cap.
pub fn should_rotate_log(len: u64) -> bool {
    len > 5 * 1024 * 1024
}

fn open_journal_socket() -> Option<UnixDatagram> {
    let sock = UnixDatagram::unbound().ok()?;
    sock.connect("/run/systemd/journal/socket").ok()?;
    Some(sock)
}

/// Initialize stderr + file + journald logging and install the panic hook.
///
/// Log file: `$XDG_RUNTIME_DIR/niri-bar.log`, falling back to
/// per-UID `/tmp/niri-bar-<uid>.log` (symlinks refused). Rotated at 5MB.
/// Journald via `/run/systemd/journal/socket` when present.
/// Panics are reported as `FATAL PANIC` with location.
pub fn init_logger() {
    // Open the persistent log file
    if let Some(file) = open_log_file() {
        if let Ok(mut guard) = LOG_FILE.lock() {
            *guard = Some(file);
        }
    }

    // Connect to systemd journal socket
    if let Some(sock) = open_journal_socket() {
        if let Ok(mut guard) = JOURNAL_SOCKET.lock() {
            *guard = Some(sock);
        }
    }

    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".into());

        let msg = match info.payload().downcast_ref::<&str>() {
            Some(s) => *s,
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => s.as_str(),
                None => "unspecified error payload",
            },
        };

        emit("FATAL PANIC", "panic", &format!("at {}: {}", loc, msg));
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_rotate_log() {
        assert!(!should_rotate_log(0));
        assert!(!should_rotate_log(5 * 1024 * 1024));
        assert!(should_rotate_log(5 * 1024 * 1024 + 1));
    }

    #[test]
    fn test_priority_for_level() {
        assert_eq!(priority_for_level("ERROR"), "3");
        assert_eq!(priority_for_level("FATAL PANIC"), "3");
        assert_eq!(priority_for_level("WARN"), "4");
        assert_eq!(priority_for_level("INFO"), "6");
        assert_eq!(priority_for_level("DEBUG"), "7");
        assert_eq!(priority_for_level("UNKNOWN"), "6");
    }

    #[test]
    fn test_format_log_line() {
        let formatted = format_log_line("2026-09-01 12:00:00.000", "INFO", "taskbar", "Window 42 focused");
        assert_eq!(
            formatted,
            "[2026-09-01 12:00:00.000] [INFO ] [taskbar] Window 42 focused"
        );
    }

    #[test]
    fn test_format_journal_payload() {
        let payload = format_journal_payload("volume", "Volume changed to 80%", "6");
        assert!(payload.contains("MESSAGE=[volume] Volume changed to 80%"));
        assert!(payload.contains("SYSLOG_IDENTIFIER=niri-bar"));
        assert!(payload.contains("PRIORITY=6"));
        assert!(payload.contains("NIRI_TAG=volume"));
    }

    #[test]
    fn test_parse_debug_flag() {
        // RUST_LOG variants
        assert!(parse_debug_flag(Some("debug"), None));
        assert!(parse_debug_flag(Some("niri_bar=trace"), None));
        assert!(parse_debug_flag(Some("1"), None));
        assert!(!parse_debug_flag(Some("info"), None));
        assert!(!parse_debug_flag(Some("warn"), None));

        // NIRI_BAR_DEBUG variants
        assert!(parse_debug_flag(None, Some("1")));
        assert!(parse_debug_flag(None, Some("true")));
        assert!(parse_debug_flag(None, Some("TRUE")));
        assert!(!parse_debug_flag(None, Some("0")));
        assert!(!parse_debug_flag(None, Some("false")));

        // Both None
        assert!(!parse_debug_flag(None, None));
    }

    #[test]
    fn test_timestamp_non_empty() {
        let ts = timestamp();
        assert!(!ts.is_empty());
        assert!(ts.contains('-'));
        assert!(ts.contains(':'));
    }
}
