//! Minimal stderr logger behind the `log` facade; level from `NT_LOG`.

use log::{Level, LevelFilter};

const LOGGER: Logger = Logger;

/// Installs the logger once. `NT_LOG` = off|error|warn|info|debug|trace (default error;
/// unknown values fall back to error). Lines are `[level] target: message` on stderr.
pub(crate) fn init() {
    // `LevelFilter::from_str` is case-insensitive and also accepts `off`.
    let level = std::env::var("NT_LOG")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(LevelFilter::Error);
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(level);
}

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        };
        #[expect(
            clippy::print_stderr,
            reason = "the logger is the one place that writes to stderr"
        )]
        {
            eprintln!("[{}] {}: {}", level, record.target(), record.args());
        }
    }

    fn flush(&self) {}
}
