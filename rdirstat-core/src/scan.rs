use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::{fs, thread};

use crate::logging::log;
use crate::util::strip_unc_prefix;

/// A tracked large file or directory.
#[derive(Clone)]
pub struct SizedEntry {
    pub path: PathBuf,
    pub size: u64,
}

/// Per-extension statistics.
#[derive(Clone)]
pub struct ExtensionStat {
    pub extension: String,
    pub count: u64,
    pub total_size: u64,
}

/// Shared state between the background scanner and the UI.
pub struct ScanState {
    /// Accumulated size per directory (updated live during scan).
    pub dir_sizes: Mutex<HashMap<PathBuf, u64>>,
    /// Directories whose subtrees have been fully walked.
    pub completed: Mutex<HashSet<PathBuf>>,
    /// Whether the scan is currently running.
    pub scanning: AtomicBool,
    /// Total files processed so far.
    pub files_scanned: AtomicU64,
    /// Set to true to request the scanner to stop.
    pub cancel: AtomicBool,
    /// Top N biggest files found during scan (maintained incrementally).
    pub top_files: Mutex<Vec<SizedEntry>>,
    /// Minimum size to qualify for top_files — avoids locking for small files.
    top_files_min: AtomicU64,
    /// Top N biggest directories (maintained incrementally on dir completion).
    pub top_dirs: Mutex<Vec<SizedEntry>>,
    /// Minimum size to qualify for top_dirs.
    top_dirs_min: AtomicU64,
    /// Extension stats: extension -> (count, total_size).
    ext_stats: Mutex<HashMap<String, (u64, u64)>>,
    /// Cached sorted top extensions (refreshed periodically by scanner).
    pub top_exts_cache: Mutex<Vec<ExtensionStat>>,
    /// Total size of all files scanned.
    pub total_bytes: AtomicU64,
    /// Total directory count.
    pub dirs_scanned: AtomicU64,
    /// Deepest depth encountered (atomic for fast check).
    deepest_depth: AtomicUsize,
    /// Deepest path encountered (only locked when depth record broken).
    pub deepest_path: Mutex<(PathBuf, usize)>,
    /// When the current scan started.
    pub scan_start: Mutex<Option<std::time::Instant>>,
}

const TOP_N: usize = 10;

impl ScanState {
    pub fn new() -> Arc<Self> {
        Arc::new(ScanState {
            dir_sizes: Mutex::new(HashMap::with_capacity(100_000)),
            completed: Mutex::new(HashSet::with_capacity(100_000)),
            scanning: AtomicBool::new(false),
            files_scanned: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            top_files: Mutex::new(Vec::new()),
            top_files_min: AtomicU64::new(0),
            top_dirs: Mutex::new(Vec::new()),
            top_dirs_min: AtomicU64::new(0),
            ext_stats: Mutex::new(HashMap::new()),
            top_exts_cache: Mutex::new(Vec::new()),
            total_bytes: AtomicU64::new(0),
            dirs_scanned: AtomicU64::new(0),
            deepest_depth: AtomicUsize::new(0),
            deepest_path: Mutex::new((PathBuf::new(), 0)),
            scan_start: Mutex::new(None),
        })
    }

    pub fn get_size(&self, path: &Path) -> Option<u64> {
        self.dir_sizes.lock().unwrap().get(path).copied()
    }

    pub fn is_completed(&self, path: &Path) -> bool {
        self.completed.lock().unwrap().contains(path)
    }

    pub fn is_scanning(&self) -> bool {
        self.scanning.load(Ordering::Relaxed)
    }

    pub fn files_scanned(&self) -> u64 {
        self.files_scanned.load(Ordering::Relaxed)
    }

    /// Track a file for top-N. Only locks if the file is big enough.
    pub fn record_top_file(&self, path: &Path, size: u64) {
        let min = self.top_files_min.load(Ordering::Relaxed);
        if size <= min {
            return;
        }
        let mut top = self.top_files.lock().unwrap();
        if top.len() < TOP_N || size > top.last().map(|e| e.size).unwrap_or(0) {
            top.push(SizedEntry { path: path.to_path_buf(), size });
            top.sort_by(|a, b| b.size.cmp(&a.size));
            top.truncate(TOP_N);
            let new_min = if top.len() >= TOP_N {
                top.last().map(|e| e.size).unwrap_or(0)
            } else {
                0
            };
            self.top_files_min.store(new_min, Ordering::Relaxed);
        }
    }

    pub fn record_dir(&self, depth: usize) {
        self.dirs_scanned.fetch_add(1, Ordering::Relaxed);
        // Only update deepest_depth atomically; path is set below if needed
        loop {
            let current = self.deepest_depth.load(Ordering::Relaxed);
            if depth <= current {
                break;
            }
            if self.deepest_depth.compare_exchange_weak(
                current, depth, Ordering::Relaxed, Ordering::Relaxed
            ).is_ok() {
                break;
            }
        }
    }

    /// Set the deepest path. Called separately from record_dir to avoid
    /// passing the path when depth isn't a record.
    pub fn set_deepest_path(&self, path: &Path, depth: usize) {
        let mut deepest = self.deepest_path.lock().unwrap();
        if depth > deepest.1 {
            *deepest = (path.to_path_buf(), depth);
        }
    }

    /// Update top dirs when a directory is completed with its final size.
    pub fn record_completed_dir(&self, path: &Path, size: u64) {
        let min = self.top_dirs_min.load(Ordering::Relaxed);
        if size <= min {
            return;
        }
        let mut top = self.top_dirs.lock().unwrap();
        if top.len() < TOP_N || size > top.last().map(|e| e.size).unwrap_or(0) {
            top.push(SizedEntry { path: path.to_path_buf(), size });
            top.sort_by(|a, b| b.size.cmp(&a.size));
            top.truncate(TOP_N);
            let new_min = if top.len() >= TOP_N {
                top.last().map(|e| e.size).unwrap_or(0)
            } else {
                0
            };
            self.top_dirs_min.store(new_min, Ordering::Relaxed);
        }
    }

    /// Merge thread-local extension stats into the shared map.
    pub fn merge_ext_stats(&self, local: &HashMap<String, (u64, u64)>) {
        let mut stats = self.ext_stats.lock().unwrap();
        for (ext, (count, size)) in local {
            let entry = stats.entry(ext.clone()).or_insert((0, 0));
            entry.0 += count;
            entry.1 += size;
        }
    }

    /// Rebuild the cached top extensions list from ext_stats.
    pub fn refresh_top_exts(&self, n: usize) {
        let stats = self.ext_stats.lock().unwrap();
        let mut exts: Vec<ExtensionStat> = stats.iter()
            .map(|(ext, &(count, total_size))| ExtensionStat {
                extension: ext.clone(),
                count,
                total_size,
            })
            .collect();
        drop(stats);
        exts.sort_by(|a, b| b.total_size.cmp(&a.total_size));
        exts.truncate(n);
        *self.top_exts_cache.lock().unwrap() = exts;
    }

    pub fn clear(&self) {
        self.dir_sizes.lock().unwrap().clear();
        self.completed.lock().unwrap().clear();
        self.files_scanned.store(0, Ordering::Relaxed);
        self.top_files.lock().unwrap().clear();
        self.top_files_min.store(0, Ordering::Relaxed);
        self.top_dirs.lock().unwrap().clear();
        self.top_dirs_min.store(0, Ordering::Relaxed);
        self.ext_stats.lock().unwrap().clear();
        *self.top_exts_cache.lock().unwrap() = Vec::new();
        self.total_bytes.store(0, Ordering::Relaxed);
        self.dirs_scanned.store(0, Ordering::Relaxed);
        self.deepest_depth.store(0, Ordering::Relaxed);
        *self.deepest_path.lock().unwrap() = (PathBuf::new(), 0);
        *self.scan_start.lock().unwrap() = None;
    }
}

/// Walk a single subtree, tracking sizes for every directory encountered.
fn scan_subtree(subtree_root: PathBuf, state: &ScanState, cancel: &AtomicBool) -> u64 {
    let mut dir_stack: Vec<(PathBuf, u64)> = Vec::new();
    let mut local_count = 0u64;
    let mut errors = 0u64;
    let flush_interval = 5000;

    // Thread-local extension stats — merged once at the end
    let mut local_ext_stats: HashMap<String, (u64, u64)> = HashMap::new();

    // Batch completed dirs, flush under one lock
    let mut completed_batch: Vec<(PathBuf, u64)> = Vec::new();

    // Track deepest path locally, write once at end
    let mut local_deepest: (PathBuf, usize) = (PathBuf::new(), 0);

    for entry in walkdir::WalkDir::new(&subtree_root).follow_links(false) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        match entry {
            Ok(e) => {
                let depth = e.depth();

                // Complete directories that we've moved past — batch them
                while dir_stack.len() > depth {
                    if let Some((done_dir, size)) = dir_stack.pop() {
                        completed_batch.push((done_dir, size));
                        if let Some(parent) = dir_stack.last_mut() {
                            parent.1 += size;
                        }
                    }
                }

                // Flush completed batch if it has entries
                if !completed_batch.is_empty() {
                    {
                        let mut sizes = state.dir_sizes.lock().unwrap();
                        let mut comp = state.completed.lock().unwrap();
                        for (dir, size) in &completed_batch {
                            sizes.insert(dir.clone(), *size);
                            comp.insert(dir.clone());
                        }
                    }
                    for (dir, size) in completed_batch.drain(..) {
                        state.record_completed_dir(&dir, size);
                    }
                }

                if e.file_type().is_dir() {
                    let path = strip_unc_prefix(e.path().to_path_buf());
                    state.record_dir(depth);
                    if depth > local_deepest.1 {
                        local_deepest = (path.clone(), depth);
                    }
                    dir_stack.push((path, 0));
                } else if e.file_type().is_file() {
                    let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                    let file_path = strip_unc_prefix(e.path().to_path_buf());

                    state.total_bytes.fetch_add(len, Ordering::Relaxed);
                    state.record_top_file(&file_path, len);

                    // Extension stats — thread-local, no lock
                    if let Some(ext) = file_path.extension() {
                        let ext = ext.to_string_lossy().to_lowercase();
                        let entry = local_ext_stats.entry(ext).or_insert((0, 0));
                        entry.0 += 1;
                        entry.1 += len;
                    }

                    if let Some(parent) = dir_stack.last_mut() {
                        parent.1 += len;
                    }
                    local_count += 1;

                    if local_count % flush_interval == 0 {
                        // Only write the subtree root's cumulative total — 1 insert
                        if !dir_stack.is_empty() {
                            let subtree_total: u64 = dir_stack.iter().map(|(_, s)| s).sum();
                            state.dir_sizes.lock().unwrap()
                                .insert(dir_stack[0].0.clone(), subtree_total);
                        }
                        state.files_scanned.fetch_add(flush_interval, Ordering::Relaxed);
                    }
                }
            }
            Err(e) => {
                if errors < 5 {
                    log(&format!("scan: walkdir error in {}: {e}", subtree_root.display()));
                }
                errors += 1;
            }
        }
    }

    // Flush remaining directories — one lock for all
    let mut total = 0u64;
    {
        let mut sizes = state.dir_sizes.lock().unwrap();
        let mut comp = state.completed.lock().unwrap();
        while let Some((done_dir, size)) = dir_stack.pop() {
            sizes.insert(done_dir.clone(), size);
            comp.insert(done_dir.clone());
            // Can't call record_completed_dir while holding locks, so batch
            completed_batch.push((done_dir, size));
            if let Some(parent) = dir_stack.last_mut() {
                parent.1 += size;
            } else {
                total = size;
            }
        }
    }
    for (dir, size) in completed_batch.drain(..) {
        state.record_completed_dir(&dir, size);
    }

    state.files_scanned.fetch_add(local_count % flush_interval, Ordering::Relaxed);

    // Merge thread-local stats — one lock each
    state.merge_ext_stats(&local_ext_stats);
    state.refresh_top_exts(15);

    // Update deepest path if this subtree had a deeper one
    if local_deepest.1 > 0 {
        state.set_deepest_path(&local_deepest.0, local_deepest.1);
    }

    if errors > 0 {
        log(&format!("scan: subtree {} done with {errors} errors", subtree_root.display()));
    }
    total
}

/// Start the background scanner. Enumerates top-level children of `root`,
/// then spawns parallel walker threads via a thread pool.
pub fn start_scan(root: PathBuf, state: Arc<ScanState>) {
    state.cancel.store(false, Ordering::Relaxed);
    *state.scan_start.lock().unwrap() = Some(std::time::Instant::now());
    state.scanning.store(true, Ordering::Release);
    log(&format!("scan: starting from {}", root.display()));

    thread::spawn(move || {
        let mut child_dirs: Vec<PathBuf> = Vec::new();
        let mut root_file_size = 0u64;

        if let Ok(read_dir) = fs::read_dir(&root) {
            for entry in read_dir.flatten() {
                if let Ok(ft) = entry.file_type() {
                    let path = strip_unc_prefix(entry.path());
                    if ft.is_dir() {
                        child_dirs.push(path);
                    } else if ft.is_file() {
                        root_file_size += entry.metadata().map(|m| m.len()).unwrap_or(0);
                    }
                }
            }
        }

        log(&format!("scan: {} top-level dirs to scan", child_dirs.len()));

        let num_threads = num_cpus::get().max(2);
        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let rx = Arc::new(Mutex::new(rx));
        let workers_active = Arc::new(AtomicU64::new(num_threads as u64));

        let mut handles = Vec::new();
        for _ in 0..num_threads {
            let rx = Arc::clone(&rx);
            let state = Arc::clone(&state);
            let active = Arc::clone(&workers_active);
            handles.push(thread::spawn(move || {
                loop {
                    let path = {
                        let lock = rx.lock().unwrap();
                        match lock.recv() {
                            Ok(p) => p,
                            Err(_) => break,
                        }
                    };
                    if state.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    log(&format!("worker: scanning {}", path.display()));
                    scan_subtree(path, &state, &state.cancel);
                }
                active.fetch_sub(1, Ordering::Relaxed);
            }));
        }

        for dir in child_dirs {
            if state.cancel.load(Ordering::Relaxed) {
                break;
            }
            let _ = tx.send(dir);
        }
        drop(tx);

        for h in handles {
            let _ = h.join();
        }

        // Compute root directory size
        {
            let sizes = state.dir_sizes.lock().unwrap();
            let mut root_total = root_file_size;
            if let Ok(read_dir) = fs::read_dir(&root) {
                for entry in read_dir.flatten() {
                    if let Ok(ft) = entry.file_type() {
                        if ft.is_dir() {
                            let path = strip_unc_prefix(entry.path());
                            root_total += sizes.get(&path).copied().unwrap_or(0);
                        }
                    }
                }
            }
            drop(sizes);
            state.dir_sizes.lock().unwrap().insert(root.clone(), root_total);
            state.completed.lock().unwrap().insert(root);
        }

        state.scanning.store(false, Ordering::Release);
        log(&format!("scan: finished, {} files", state.files_scanned()));
    });
}
