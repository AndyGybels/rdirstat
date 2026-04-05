use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ignore::WalkBuilder;

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
const FLUSH_INTERVAL: u64 = 5000;

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

/// Thread-local state for each parallel walker thread.
/// Flushes remaining data to shared ScanState on drop.
struct ThreadLocalState {
    state: Arc<ScanState>,
    root: PathBuf,
    local_dir_sizes: HashMap<PathBuf, u64>,
    local_ext_stats: HashMap<String, (u64, u64)>,
    local_count: u64,
    local_deepest: (PathBuf, usize),
    errors: u64,
}

impl ThreadLocalState {
    fn new(state: Arc<ScanState>, root: PathBuf) -> Self {
        ThreadLocalState {
            state,
            root,
            local_dir_sizes: HashMap::new(),
            local_ext_stats: HashMap::new(),
            local_count: 0,
            local_deepest: (PathBuf::new(), 0),
            errors: 0,
        }
    }

    fn flush_dir_sizes(&mut self) {
        if self.local_dir_sizes.is_empty() {
            return;
        }
        let mut sizes = self.state.dir_sizes.lock().unwrap();
        for (dir, size) in self.local_dir_sizes.drain() {
            *sizes.entry(dir).or_insert(0) += size;
        }
    }

    fn process_entry(&mut self, entry: ignore::DirEntry) {
        let depth = entry.depth();
        let path = strip_unc_prefix(entry.path().to_path_buf());

        if entry.file_type().map_or(false, |ft| ft.is_dir()) {
            self.state.record_dir(depth);
            if depth > self.local_deepest.1 {
                self.local_deepest = (path, depth);
            }
        } else if entry.file_type().map_or(false, |ft| ft.is_file()) {
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);

            self.state.total_bytes.fetch_add(len, Ordering::Relaxed);
            self.state.record_top_file(&path, len);

            // Extension stats — thread-local, no lock
            if let Some(ext) = path.extension() {
                let ext = ext.to_string_lossy().to_lowercase();
                let e = self.local_ext_stats.entry(ext).or_insert((0, 0));
                e.0 += 1;
                e.1 += len;
            }

            // Accumulate size to all ancestor directories up to root
            let mut p = path.parent();
            while let Some(dir) = p {
                *self.local_dir_sizes.entry(dir.to_path_buf()).or_insert(0) += len;
                if dir == self.root.as_path() {
                    break;
                }
                p = dir.parent();
            }

            self.local_count += 1;

            if self.local_count % FLUSH_INTERVAL == 0 {
                self.flush_dir_sizes();
                self.state.files_scanned.fetch_add(FLUSH_INTERVAL, Ordering::Relaxed);
                self.state.merge_ext_stats(&self.local_ext_stats);
                self.local_ext_stats.clear();
                self.state.refresh_top_exts(15);
            }
        }
    }
}

impl Drop for ThreadLocalState {
    fn drop(&mut self) {
        // Flush remaining dir sizes
        self.flush_dir_sizes();
        // Flush remaining file count
        let remainder = self.local_count % FLUSH_INTERVAL;
        if remainder > 0 {
            self.state.files_scanned.fetch_add(remainder, Ordering::Relaxed);
        }
        // Merge remaining ext stats
        if !self.local_ext_stats.is_empty() {
            self.state.merge_ext_stats(&self.local_ext_stats);
        }
        // Update deepest path if this thread found a deeper one
        if self.local_deepest.1 > 0 {
            self.state.set_deepest_path(&self.local_deepest.0, self.local_deepest.1);
        }
        if self.errors > 0 {
            log(&format!("scan: thread finished with {} errors", self.errors));
        }
    }
}

/// Start the background scanner using the `ignore` crate's parallel walker.
pub fn start_scan(root: PathBuf, state: Arc<ScanState>) {
    state.cancel.store(false, Ordering::Relaxed);
    *state.scan_start.lock().unwrap() = Some(std::time::Instant::now());
    state.scanning.store(true, Ordering::Release);
    log(&format!("scan: starting from {}", root.display()));

    thread::spawn(move || {
        let num_threads = num_cpus::get().max(2);

        let walker = WalkBuilder::new(&root)
            .hidden(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .follow_links(false)
            .threads(num_threads)
            .build_parallel();

        walker.run(|| {
            let state = Arc::clone(&state);
            let root = root.clone();
            let mut tls = ThreadLocalState::new(state, root);

            Box::new(move |result| {
                if tls.state.cancel.load(Ordering::Relaxed) {
                    return ignore::WalkState::Quit;
                }

                match result {
                    Ok(entry) => tls.process_entry(entry),
                    Err(e) => {
                        if tls.errors < 5 {
                            log(&format!("scan: walker error: {e}"));
                        }
                        tls.errors += 1;
                    }
                }

                ignore::WalkState::Continue
            })
        });

        // After walk completes: mark all directories as completed, find top dirs
        let entries: Vec<(PathBuf, u64)> = {
            let sizes = state.dir_sizes.lock().unwrap();
            sizes.iter().map(|(p, &s)| (p.clone(), s)).collect()
        };
        {
            let mut comp = state.completed.lock().unwrap();
            for (dir, _) in &entries {
                comp.insert(dir.clone());
            }
        }
        for (dir, size) in &entries {
            state.record_completed_dir(dir, *size);
        }

        state.refresh_top_exts(15);
        state.scanning.store(false, Ordering::Release);
        log(&format!("scan: finished, {} files", state.files_scanned()));
    });
}
