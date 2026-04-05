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
