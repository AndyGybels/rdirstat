use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::{fs, thread};

use crate::logging::log;
use crate::model::DirEntry;
use crate::scan::{start_scan, ScanState};
use crate::util::strip_unc_prefix;

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
                entry.metadata().map(|m| m.len()).unwrap_or(0)
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
