//! The [`DirEntry`] row type used by [`crate::AppState`] to represent a
//! single line in a directory listing.

use std::path::PathBuf;
use crate::scan::ScanState;

/// One row in a directory listing.
///
/// Includes file size for files (frozen at the time the listing was built)
/// but *not* directory size — directory sizes change as the scan
/// progresses, so they're looked up live via [`Self::current_size`] against
/// a [`ScanState`].
pub struct DirEntry {
    /// Display name. `".."` for the parent-nav row.
    pub name: String,
    /// Absolute path. For parent-nav rows this is the parent directory.
    pub path: PathBuf,
    pub is_dir: bool,
    /// For files: the file's allocated size in bytes. For directories:
    /// always 0 — call [`Self::current_size`] for live values.
    pub file_size: u64,
    /// True for the synthetic `..` parent-nav row.
    pub is_parent: bool,
}

impl DirEntry {
    /// Current size to display, as of the moment of the call.
    ///
    /// - Parent-nav rows always return 0 (they're navigation, not content).
    /// - Files return their frozen [`Self::file_size`].
    /// - Directories look up the live accumulated size from `scan`, or 0
    ///   if the scanner hasn't visited them yet.
    pub fn current_size(&self, scan: &ScanState) -> u64 {
        // The ".." parent-nav entry has `path = <parent>`, so a naive size
        // lookup returns the entire parent directory's size and double-counts
        // it in any sum across `entries`. It's a navigation affordance, not
        // a member of the current directory — report 0.
        if self.is_parent {
            0
        } else if self.is_dir {
            scan.get_size(&self.path).unwrap_or(0)
        } else {
            self.file_size
        }
    }

    /// Whether this entry is currently being scanned — only ever true for
    /// directories whose subtree hasn't yet been recursively completed.
    /// Files and the parent-nav row always return false.
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
