//! Tiny custom logger for QIMS.
//!
//! Emits lines in the shape:
//! ```text
//! [14:03:22] [qims_backend::main.rs:143] [INFO] ⋆.˚ QIMS backend listening on ...
//! ```
//!
//! The level is read from the `QIMS_LOG` environment variable (e.g.
//! `QIMS_LOG=debug`), defaulting to `info`.

use log::{Level, LevelFilter, Metadata, Record};

/// Environment variable controlling the maximum log level.
const LOG_ENV: &str = "QIMS_LOG";

/// Crate whose logs honor the full `QIMS_LOG` level; everything else
/// (SurrealDB, SurrealKV, tower, …) is capped at [`EXTERNAL_LEVEL`].
const OWN_CRATE: &str = "qims_backend";
/// Maximum level shown for dependency crates, to keep our own logs readable.
const EXTERNAL_LEVEL: LevelFilter = LevelFilter::Warn;

struct QimsLogger {
    level: LevelFilter,
}

impl QimsLogger {
    /// The effective threshold for a given log target.
    fn threshold(&self, target: &str) -> LevelFilter {
        if target.starts_with(OWN_CRATE) {
            self.level
        } else {
            EXTERNAL_LEVEL
        }
    }
}

impl log::Log for QimsLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.threshold(metadata.target())
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let time = chrono::Local::now().format("%H:%M:%S");

        // Compact `::parent::file.rs:line` location. The module path's last
        // segment usually equals the file stem (`…::update` + `update.rs`), so
        // it is folded into the file name instead of printed twice.
        let file = record
            .file()
            .map(|f| f.rsplit('/').next().unwrap_or(f))
            .unwrap_or("?");
        let line = record.line().unwrap_or(0);
        let location = if record.target().starts_with(OWN_CRATE) {
            let stem = file.strip_suffix(".rs").unwrap_or(file);
            let mut segments: Vec<&str> = record.target().split("::").collect();
            if segments.first() == Some(&OWN_CRATE) {
                segments.remove(0);
            }
            if segments.last() == Some(&stem) {
                segments.pop();
            }
            match segments.last() {
                Some(parent) => format!("::{parent}::{file}:{line}"),
                None => format!("::{file}:{line}"),
            }
        } else {
            // Dependency crates keep their full target for context.
            format!("{}::{}:{}", record.target(), file, line)
        };

        let color = level_color(record.level());
        let reset = "\x1b[0m";
        let dim = "\x1b[2m";

        let line = format!(
            "{dim}[{time}]{reset} {dim}[{location}]{reset} [{color}{level}{reset}] ⋆.˚ {message}",
            level = record.level(),
            message = record.args(),
        );

        // Warnings and errors go to stderr, everything else to stdout.
        if record.level() <= Level::Warn {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }

    fn flush(&self) {}
}

/// Human duration for query logs: microseconds under 1 ms, else milliseconds.
pub fn elapsed_str(d: std::time::Duration) -> String {
    if d.as_millis() < 1 {
        format!("{}µs", d.as_micros())
    } else {
        format!("{:.2}ms", d.as_secs_f64() * 1000.0)
    }
}

/// Await a database query future and log how long it took, from the caller's
/// location (so the log line points at the handler, not this macro).
#[macro_export]
macro_rules! db_timed {
    ($op:expr, $fut:expr) => {{
        let __start = ::std::time::Instant::now();
        let __out = $fut.await;
        log::info!(
            "db {} in {}",
            $op,
            $crate::logger::elapsed_str(__start.elapsed())
        );
        __out
    }};
}

/// ANSI color for a given level.
fn level_color(level: Level) -> &'static str {
    match level {
        Level::Error => "\x1b[31m", // red
        Level::Warn => "\x1b[33m",  // yellow
        Level::Info => "\x1b[32m",  // green
        Level::Debug => "\x1b[36m", // cyan
        Level::Trace => "\x1b[35m", // magenta
    }
}

/// Install the logger. Reads the level from `QIMS_LOG` (default `info`).
pub fn init() {
    let level = std::env::var(LOG_ENV)
        .ok()
        .and_then(|v| v.parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::Info);

    log::set_boxed_logger(Box::new(QimsLogger { level }))
        .expect("logger already initialised");
    // The global max must let both our level and external WARN records through.
    log::set_max_level(level.max(EXTERNAL_LEVEL));
}
