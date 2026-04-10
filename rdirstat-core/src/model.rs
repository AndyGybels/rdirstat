use std::path::PathBuf;
use crate::scan::ScanState;

pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub file_size: u64,
    pub is_parent: bool,
}

impl DirEntry {
    pub fn current_size(&self, scan: &ScanState) -> u64 {
        if self.is_dir {
            scan.get_size(&self.path).unwrap_or(0)
        } else {
            self.file_size
        }
    }

    pub fn is_scanning(&self, scan: &ScanState) -> bool {
        if !self.is_dir {
            return false;
        }
        scan.is_scanning() && !scan.is_completed(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn make_file(name: &str, size: u64) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            path: PathBuf::from(format!("/test/{name}")),
            is_dir: false,
            file_size: size,
            is_parent: false,
        }
    }

    fn make_dir(name: &str) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            path: PathBuf::from(format!("/test/{name}")),
            is_dir: true,
            file_size: 0,
            is_parent: false,
        }
    }

    fn make_parent() -> DirEntry {
        DirEntry {
            name: "..".to_string(),
            path: PathBuf::from("/test"),
            is_dir: true,
            file_size: 0,
            is_parent: true,
        }
    }

    #[test]
    fn current_size_file() {
        let state = ScanState::new();
        let entry = make_file("a.txt", 1234);
        assert_eq!(entry.current_size(&state), 1234);
    }

    #[test]
    fn current_size_dir_no_data() {
        let state = ScanState::new();
        let entry = make_dir("sub");
        assert_eq!(entry.current_size(&state), 0);
    }

    #[test]
    fn current_size_dir_with_data() {
        let state = ScanState::new();
        state.dir_sizes.lock().unwrap().insert(PathBuf::from("/test/sub"), 5000);
        let entry = make_dir("sub");
        assert_eq!(entry.current_size(&state), 5000);
    }

    #[test]
    fn is_scanning_file_always_false() {
        let state = ScanState::new();
        state.scanning.store(true, Ordering::Relaxed);
        let entry = make_file("a.txt", 100);
        assert!(!entry.is_scanning(&state));
    }

    #[test]
    fn is_scanning_dir_not_scanning() {
        let state = ScanState::new();
        let entry = make_dir("sub");
        assert!(!entry.is_scanning(&state));
    }

    #[test]
    fn is_scanning_dir_scanning_not_completed() {
        let state = ScanState::new();
        state.scanning.store(true, Ordering::Relaxed);
        let entry = make_dir("sub");
        assert!(entry.is_scanning(&state));
    }

    #[test]
    fn is_scanning_dir_scanning_completed() {
        let state = ScanState::new();
        state.scanning.store(true, Ordering::Relaxed);
        state.completed.lock().unwrap().insert(PathBuf::from("/test/sub"));
        let entry = make_dir("sub");
        assert!(!entry.is_scanning(&state));
    }

    #[test]
    fn parent_entry() {
        let state = ScanState::new();
        let entry = make_parent();
        assert!(entry.is_parent);
        assert!(entry.is_dir);
        assert_eq!(entry.current_size(&state), 0);
    }
}
