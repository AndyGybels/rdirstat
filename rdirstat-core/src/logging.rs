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

pub fn init_logger() {
    unsafe { LOGGER = Some(Logger::init()) }
}

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
