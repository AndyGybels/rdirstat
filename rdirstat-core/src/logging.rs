//! Append-only file logger used by the `rdirstat` and `rdirstat-gui`
//! binaries.
//!
//! Writes time-stamped lines to `./rdirstat.log` in the current working
//! directory. Library callers shouldn't normally use this — it's hard-coded
//! to a relative path and aimed at the binaries' diagnostics. Build your
//! own `tracing` subscriber if you're embedding `rdirstat-core` in a larger
//! application.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::fs;

struct Logger {
    file: Mutex<fs::File>,
}

impl Logger {
    fn init() -> Arc<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("rdirstat.log")
            .expect("failed to open log file");
        let logger = Arc::new(Logger {
            file: Mutex::new(file),
        });
        logger.write("--- session start ---");
        logger
    }

    fn write(&self, msg: &str) {
        let timestamp = chrono::Local::now().format("%H:%M:%S%.3f");
        let mut f = self.file.lock().unwrap();
        let _ = writeln!(f, "[{timestamp}] {msg}");
        let _ = f.flush();
    }
}

static mut LOGGER: Option<Arc<Logger>> = None;

/// Initialise the global file logger. Opens `rdirstat.log` (creating it if
/// needed) and writes a `--- session start ---` marker. Subsequent
/// [`log`] calls write through this handle. Calling this multiple times
/// rotates the underlying file handle but doesn't truncate.
pub fn init_logger() {
    unsafe { LOGGER = Some(Logger::init()) }
}

/// Append a time-stamped line to the log file, if [`init_logger`] has been
/// called. No-op otherwise — safe to call from libraries that don't know
/// whether the host has set up logging.
pub fn log(msg: &str) {
    unsafe {
        if let Some(ref logger) = LOGGER {
            logger.write(msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_without_init_does_not_panic() {
        // log() should be a no-op when logger isn't initialized
        log("test message that should not panic");
    }

    #[test]
    fn init_and_log() {
        init_logger();
        log("unit test log entry");
        // Verify log file was created
        assert!(std::path::Path::new("rdirstat.log").exists());
    }
}
