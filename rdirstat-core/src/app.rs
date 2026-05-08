//! [`AppState`] — the controller layer between a frontend and the scan
//! engine.
//!
//! Owns the current directory, the scan state, the navigation history,
//! sort preference, and the shared entry buffer the snapshot thread reads
//! from. Frontends call methods like [`AppState::set_directory`],
//! [`AppState::scan`], [`AppState::stop_scan`], [`AppState::toggle_sort`],
//! and [`AppState::execute_delete`]; they don't manipulate the underlying
//! [`ScanState`] directly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::{fs, thread};

use crate::logging::log;
use crate::model::DirEntry;
use crate::scan::{start_scan, ScanState};
use crate::util::{allocated_size, strip_unc_prefix};

/// Top-level controller state for an `rdirstat` frontend.
///
/// Two construction modes — see [`AppState::new`] (auto-scans on
/// construction, used by the TUI) and [`AppState::new_idle`] (no scan
/// until the user triggers one, used by the GUI).
pub struct AppState {
    pub current_dir: PathBuf,
    pub scan_root: PathBuf,
    pub entries: Vec<DirEntry>,
    pub selected: usize,
    pub scroll: usize,
    pub history: Vec<(PathBuf, usize, usize)>,
    pub sort_by_size: Arc<AtomicBool>,
    pub frame_count: usize,
    pub scan_state: Arc<ScanState>,
    /// Shared entry data for the snapshot thread to read.
    pub entry_source: Arc<std::sync::Mutex<Vec<(String, PathBuf, bool, bool, u64)>>>,
}

impl AppState {
    /// Construct an `AppState` and immediately start scanning `root`.
    /// Used by frontends that scan-on-launch (e.g. the TUI).
    pub fn new(root: &Path) -> Self {
        let scan_state = ScanState::new();
        start_scan(root.to_path_buf(), Arc::clone(&scan_state));

        let mut app = AppState {
            current_dir: root.to_path_buf(),
            scan_root: root.to_path_buf(),
            entries: Vec::new(),
            selected: 0,
            scroll: 0,
            history: Vec::new(),
            sort_by_size: Arc::new(AtomicBool::new(true)),
            frame_count: 0,
            scan_state,
            entry_source: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        app.load_entries();
        app
    }

    /// Create without starting a scan automatically.
    pub fn new_idle(root: &Path) -> Self {
        let scan_state = ScanState::new();

        let mut app = AppState {
            current_dir: root.to_path_buf(),
            scan_root: root.to_path_buf(),
            entries: Vec::new(),
            selected: 0,
            scroll: 0,
            history: Vec::new(),
            sort_by_size: Arc::new(AtomicBool::new(true)),
            frame_count: 0,
            scan_state,
            entry_source: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        app.load_entries();
        app
    }

    /// Sync the shared entry source so the snapshot thread can read it.
    fn sync_entry_source(&self) {
        let data: Vec<_> = self.entries.iter().map(|e| {
            (e.name.clone(), e.path.clone(), e.is_dir, e.is_parent, e.file_size)
        }).collect();
        *self.entry_source.lock().unwrap() = data;
    }

    /// Whether a scan has ever been started.
    pub fn has_scanned(&self) -> bool {
        self.scan_state.is_scanning() || self.scan_state.files_scanned() > 0
    }

    /// Start scanning if not already scanning.
    pub fn scan(&mut self) {
        if !self.scan_state.is_scanning() {
            self.scan_state.clear();
            self.scan_root = self.current_dir.clone();
            start_scan(self.scan_root.clone(), Arc::clone(&self.scan_state));
        }
    }

    /// Stop the current scan.
    pub fn stop_scan(&mut self) {
        self.scan_state.cancel.store(true, Ordering::Relaxed);
    }

    pub fn load_entries(&mut self) {
        log(&format!("load_entries: {}", self.current_dir.display()));
        self.entries.clear();

        // Add ".." entry if not at root
        if let Some(parent) = self.current_dir.parent() {
            if parent != self.current_dir {
                self.entries.push(DirEntry {
                    name: "..".to_string(),
                    path: parent.to_path_buf(),
                    is_dir: true,
                    file_size: 0,
                    is_parent: true,
                });
            }
        }

        let Ok(read_dir) = fs::read_dir(&self.current_dir) else {
            log("load_entries: failed to read dir");
            return;
        };

        for entry in read_dir.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            // Skip symlinks and junction points (e.g. "Documents and Settings")
            if ft.is_symlink() {
                continue;
            }
            let path = strip_unc_prefix(entry.path());
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = ft.is_dir();
            let file_size = if is_dir {
                0
            } else {
                entry.metadata().map(|m| allocated_size(&m)).unwrap_or(0)
            };

            self.entries.push(DirEntry {
                name,
                path,
                is_dir,
                file_size,
                is_parent: false,
            });
        }

        self.sort_entries();
    }

    pub fn sort_entries(&mut self) {
        if self.sort_by_size.load(Ordering::Relaxed) {
            // Batch-read dir_sizes once to avoid locking the mutex per comparison.
            let sizes = self.scan_state.dir_sizes.lock().unwrap();
            self.entries.sort_by(|a, b| {
                a.is_parent.cmp(&b.is_parent).reverse()
                    .then_with(|| {
                        let sa = if a.is_dir { sizes.get(&a.path).copied().unwrap_or(0) } else { a.file_size };
                        let sb = if b.is_dir { sizes.get(&b.path).copied().unwrap_or(0) } else { b.file_size };
                        sb.cmp(&sa)
                    })
            });
            drop(sizes);
        } else {
            self.entries.sort_by(|a, b| {
                a.is_parent.cmp(&b.is_parent).reverse()
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
        }
        self.sync_entry_source();
    }

    pub fn total_size(&self) -> u64 {
        self.entries
            .iter()
            .map(|e| e.current_size(&self.scan_state))
            .sum()
    }

    pub fn enter_dir(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let entry = &self.entries[self.selected];
        if !entry.is_dir {
            return;
        }
        if entry.is_parent {
            self.go_up();
            return;
        }
        log(&format!("enter_dir: {}", entry.path.display()));
        let new_dir = entry.path.clone();
        self.history
            .push((self.current_dir.clone(), self.selected, self.scroll));
        self.current_dir = new_dir;
        self.selected = 0;
        self.scroll = 0;
        self.load_entries();
    }

    /// Navigate directly to a specific directory path, clearing history.
    pub fn navigate_to(&mut self, path: PathBuf) {
        if path == self.current_dir {
            return;
        }
        log(&format!("navigate_to: {}", path.display()));
        self.history.clear();
        self.current_dir = path;
        self.selected = 0;
        self.scroll = 0;
        self.load_entries();
    }

    /// Change directory and reload entries. Does not manage history or selection.
    pub fn set_directory(&mut self, path: PathBuf) {
        log(&format!("set_directory: {}", path.display()));
        self.current_dir = path;
        self.load_entries();
    }

    pub fn go_up(&mut self) {
        log(&format!("go_up: from {}", self.current_dir.display()));
        if let Some((dir, sel, scroll)) = self.history.pop() {
            self.current_dir = dir;
            self.selected = sel;
            self.scroll = scroll;
            self.load_entries();
        } else if let Some(parent) = self.current_dir.parent() {
            let parent = parent.to_path_buf();
            if parent != self.current_dir {
                let old = self.current_dir.clone();
                self.current_dir = parent;
                self.selected = 0;
                self.scroll = 0;
                self.load_entries();
                if let Some(idx) = self.entries.iter().position(|e| e.path == old) {
                    self.selected = idx;
                }
            }
        }
    }

    pub fn move_selection(&mut self, delta: isize, visible_rows: usize) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len();
        if delta > 0 {
            self.selected = (self.selected + delta as usize).min(len - 1);
        } else {
            self.selected = self.selected.saturating_sub((-delta) as usize);
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
        if self.selected >= self.scroll + visible_rows {
            self.scroll = self.selected - visible_rows + 1;
        }
    }

    pub fn toggle_sort(&mut self) {
        let old = self.sort_by_size.load(Ordering::Relaxed);
        self.sort_by_size.store(!old, Ordering::Relaxed);
        self.sort_entries();
    }

    pub fn rescan(&mut self) {
        log(&format!("rescan: from {}", self.current_dir.display()));
        self.scan_state.cancel.store(true, Ordering::Relaxed);
        while self.scan_state.is_scanning() {
            thread::sleep(std::time::Duration::from_millis(10));
        }
        self.scan_state.clear();
        self.scan_root = self.current_dir.clone();
        start_scan(self.scan_root.clone(), Arc::clone(&self.scan_state));
        self.load_entries();
    }

    pub fn delete_selected(&mut self) -> Result<(String, PathBuf, bool, u64), String> {
        if self.entries.is_empty() {
            return Err("No entries".to_string());
        }
        let entry = &self.entries[self.selected];
        Ok((
            entry.name.clone(),
            entry.path.clone(),
            entry.is_dir,
            entry.current_size(&self.scan_state),
        ))
    }

    pub fn execute_delete(&mut self, path: &Path, is_dir: bool) -> Result<(), String> {
        log(&format!("delete: {}", path.display()));
        let result = if is_dir {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        match result {
            Ok(()) => {
                log(&format!("delete: success {}", path.display()));
                self.load_entries();
                Ok(())
            }
            Err(e) => {
                log(&format!("delete: error {} -> {e}", path.display()));
                Err(format!("Failed to delete: {e}"))
            }
        }
    }

    pub fn switch_drive(&mut self, path: PathBuf) {
        self.switch_drive_inner(path, true);
    }

    pub fn switch_drive_idle(&mut self, path: PathBuf) {
        self.switch_drive_inner(path, false);
    }

    fn switch_drive_inner(&mut self, path: PathBuf, auto_scan: bool) {
        log(&format!("switch_drive: {}", path.display()));
        self.scan_state
            .cancel
            .store(true, Ordering::Relaxed);
        while self.scan_state.is_scanning() {
            thread::sleep(std::time::Duration::from_millis(10));
        }
        self.scan_state.clear();
        self.scan_root = path.clone();
        self.current_dir = path.clone();
        self.history.clear();
        self.selected = 0;
        self.scroll = 0;
        if auto_scan {
            start_scan(self.scan_root.clone(), Arc::clone(&self.scan_state));
        }
        self.load_entries();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicU64;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn setup_test_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tid = std::thread::current().id();
        let tmp = std::env::temp_dir().join(format!("rdirstat_test_app_{id}_{tid:?}"));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("alpha")).unwrap();
        fs::create_dir_all(tmp.join("beta")).unwrap();
        fs::write(tmp.join("file_a.txt"), "aaaa").unwrap();
        fs::write(tmp.join("file_b.txt"), "bb").unwrap();
        fs::write(tmp.join("alpha/inner.txt"), "inside").unwrap();
        tmp
    }

    fn cleanup(tmp: &Path) {
        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn new_idle_does_not_scan() {
        let tmp = setup_test_dir();
        let app = AppState::new_idle(&tmp);
        assert!(!app.has_scanned());
        assert!(!app.scan_state.is_scanning());
        assert_eq!(app.current_dir, tmp);
        assert_eq!(app.scan_root, tmp);
        assert!(!app.entries.is_empty());
        cleanup(&tmp);
    }

    #[test]
    fn load_entries_has_parent_and_children() {
        let tmp = setup_test_dir();
        let app = AppState::new_idle(&tmp);
        // Should have: "..", "alpha", "beta", "file_a.txt", "file_b.txt"
        assert!(app.entries.len() >= 5);
        // First entry should be ".." (parent)
        assert!(app.entries[0].is_parent);
        assert_eq!(app.entries[0].name, "..");
        cleanup(&tmp);
    }

    #[test]
    fn sort_entries_by_name() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.sort_by_size.store(false, Ordering::Relaxed);
        app.sort_entries();
        // Skip parent "..", rest should be alphabetical
        let names: Vec<&str> = app.entries.iter().skip(1).map(|e| e.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
        assert_eq!(names, sorted);
        cleanup(&tmp);
    }

    #[test]
    fn total_size_sums_entries() {
        let tmp = setup_test_dir();
        let app = AppState::new_idle(&tmp);
        let total = app.total_size();
        // file_a.txt = 4, file_b.txt = 2, dirs = 0 (no scan data)
        assert!(total >= 6);
        cleanup(&tmp);
    }

    #[test]
    fn enter_dir_navigates() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        // Find "alpha" directory
        let alpha_idx = app.entries.iter().position(|e| e.name == "alpha").unwrap();
        app.selected = alpha_idx;
        app.enter_dir();
        assert_eq!(app.current_dir, tmp.join("alpha"));
        assert_eq!(app.history.len(), 1);
        cleanup(&tmp);
    }

    #[test]
    fn enter_dir_on_file_does_nothing() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        let file_idx = app.entries.iter().position(|e| e.name == "file_a.txt").unwrap();
        app.selected = file_idx;
        let old_dir = app.current_dir.clone();
        app.enter_dir();
        assert_eq!(app.current_dir, old_dir);
        cleanup(&tmp);
    }

    #[test]
    fn enter_dir_empty_entries() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.entries.clear();
        app.enter_dir(); // should not panic
        cleanup(&tmp);
    }

    #[test]
    fn enter_dir_parent_goes_up() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        // Navigate into alpha
        let idx = app.entries.iter().position(|e| e.name == "alpha").unwrap();
        app.selected = idx;
        app.enter_dir();
        assert_eq!(app.current_dir, tmp.join("alpha"));
        // Now select ".." and enter
        app.selected = 0; // ".." is always first
        assert!(app.entries[0].is_parent);
        app.enter_dir();
        assert_eq!(app.current_dir, tmp);
        cleanup(&tmp);
    }

    #[test]
    fn go_up_with_history() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        let idx = app.entries.iter().position(|e| e.name == "alpha").unwrap();
        app.selected = idx;
        app.enter_dir();
        app.go_up();
        assert_eq!(app.current_dir, tmp);
        assert_eq!(app.selected, idx); // restored from history
        cleanup(&tmp);
    }

    #[test]
    fn go_up_without_history() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        let parent = tmp.parent().unwrap().to_path_buf();
        app.go_up();
        assert_eq!(app.current_dir, parent);
        cleanup(&tmp);
    }

    #[test]
    fn navigate_to_clears_history() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.history.push((PathBuf::from("/old"), 0, 0));
        app.navigate_to(tmp.join("alpha"));
        assert_eq!(app.current_dir, tmp.join("alpha"));
        assert!(app.history.is_empty());
        assert_eq!(app.selected, 0);
        cleanup(&tmp);
    }

    #[test]
    fn navigate_to_same_dir_noop() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.history.push((PathBuf::from("/old"), 0, 0));
        app.navigate_to(tmp.clone());
        // History should not be cleared since we didn't actually navigate
        assert_eq!(app.history.len(), 1);
        cleanup(&tmp);
    }

    #[test]
    fn set_directory_changes_dir() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.set_directory(tmp.join("beta"));
        assert_eq!(app.current_dir, tmp.join("beta"));
        cleanup(&tmp);
    }

    #[test]
    fn move_selection_forward() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.selected = 0;
        app.move_selection(2, 10);
        assert_eq!(app.selected, 2);
        cleanup(&tmp);
    }

    #[test]
    fn move_selection_clamps_max() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        let len = app.entries.len();
        app.move_selection(999, 10);
        assert_eq!(app.selected, len - 1);
        cleanup(&tmp);
    }

    #[test]
    fn move_selection_backward() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.selected = 3;
        app.move_selection(-2, 10);
        assert_eq!(app.selected, 1);
        cleanup(&tmp);
    }

    #[test]
    fn move_selection_clamps_min() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.selected = 1;
        app.move_selection(-999, 10);
        assert_eq!(app.selected, 0);
        cleanup(&tmp);
    }

    #[test]
    fn move_selection_empty() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.entries.clear();
        app.move_selection(1, 10); // should not panic
        cleanup(&tmp);
    }

    #[test]
    fn move_selection_scrolls_down() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.scroll = 0;
        // Move past visible area (visible_rows=2)
        app.move_selection(3, 2);
        assert!(app.scroll > 0);
        cleanup(&tmp);
    }

    #[test]
    fn move_selection_scrolls_up() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.selected = 3;
        app.scroll = 3;
        app.move_selection(-2, 10);
        assert_eq!(app.scroll, 1); // scroll follows selection
        cleanup(&tmp);
    }

    #[test]
    fn toggle_sort_switches_mode() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        assert!(app.sort_by_size.load(Ordering::Relaxed));
        app.toggle_sort();
        assert!(!app.sort_by_size.load(Ordering::Relaxed));
        app.toggle_sort();
        assert!(app.sort_by_size.load(Ordering::Relaxed));
        cleanup(&tmp);
    }

    #[test]
    fn delete_selected_returns_info() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        let idx = app.entries.iter().position(|e| e.name == "file_a.txt").unwrap();
        app.selected = idx;
        let result = app.delete_selected().unwrap();
        assert_eq!(result.0, "file_a.txt");
        assert!(!result.2); // not a dir
        cleanup(&tmp);
    }

    #[test]
    fn delete_selected_empty() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.entries.clear();
        assert!(app.delete_selected().is_err());
        cleanup(&tmp);
    }

    #[test]
    fn execute_delete_file() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        let target = tmp.join("file_b.txt");
        assert!(target.exists());
        app.execute_delete(&target, false).unwrap();
        assert!(!target.exists());
        cleanup(&tmp);
    }

    #[test]
    fn execute_delete_dir() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        let target = tmp.join("beta");
        assert!(target.exists());
        app.execute_delete(&target, true).unwrap();
        assert!(!target.exists());
        cleanup(&tmp);
    }

    #[test]
    fn execute_delete_nonexistent() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        let result = app.execute_delete(Path::new("/nonexistent_path_xyz"), false);
        assert!(result.is_err());
        cleanup(&tmp);
    }

    #[test]
    fn switch_drive_idle_no_scan() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        let new_root = tmp.join("alpha");
        app.switch_drive_idle(new_root.clone());
        assert_eq!(app.current_dir, new_root);
        assert_eq!(app.scan_root, new_root);
        assert!(app.history.is_empty());
        assert!(!app.scan_state.is_scanning());
        cleanup(&tmp);
    }

    #[test]
    fn has_scanned_false_initially() {
        let tmp = setup_test_dir();
        let app = AppState::new_idle(&tmp);
        assert!(!app.has_scanned());
        cleanup(&tmp);
    }

    #[test]
    fn scan_starts_scan() {
        let tmp = setup_test_dir();
        let mut app = AppState::new_idle(&tmp);
        app.scan();
        // Scan should start
        // Wait a bit for it to register
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(app.has_scanned());
        app.stop_scan();
        cleanup(&tmp);
    }

    #[test]
    fn sync_entry_source_matches_entries() {
        let tmp = setup_test_dir();
        let app = AppState::new_idle(&tmp);
        let source = app.entry_source.lock().unwrap();
        assert_eq!(source.len(), app.entries.len());
        for (i, (name, path, is_dir, is_parent, _size)) in source.iter().enumerate() {
            assert_eq!(name, &app.entries[i].name);
            assert_eq!(path, &app.entries[i].path);
            assert_eq!(*is_dir, app.entries[i].is_dir);
            assert_eq!(*is_parent, app.entries[i].is_parent);
        }
        cleanup(&tmp);
    }
}
