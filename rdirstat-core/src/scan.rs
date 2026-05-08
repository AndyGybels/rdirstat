use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::{fs, thread};

use ignore::WalkBuilder;

use crate::logging::log;
use crate::util::{allocated_size, strip_unc_prefix};

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
    /// Per-directory completion: entries yielded by walker, entries expected.
    /// (yielded_count, expected_count or 0 if unknown)
    dir_completion: Mutex<HashMap<PathBuf, (usize, usize)>>,
    /// (dev, ino) of every counted file, sharded by ino to spread lock contention.
    /// Used to dedup hardlinks and macOS firmlink double-traversal
    /// (e.g. /Users vs /System/Volumes/Data/Users).
    #[cfg(unix)]
    seen_inodes: [Mutex<HashSet<(u64, u64)>>; INODE_SHARDS],
    /// Files skipped as inode-aliases (hardlinks / firmlinks).
    pub aliased_files: AtomicU64,
}

const TOP_N: usize = 10;
const FLUSH_INTERVAL: u64 = 5000;
#[cfg(unix)]
const INODE_SHARDS: usize = 64;

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
            dir_completion: Mutex::new(HashMap::with_capacity(100_000)),
            #[cfg(unix)]
            seen_inodes: std::array::from_fn(|_| Mutex::new(HashSet::with_capacity(16_384))),
            aliased_files: AtomicU64::new(0),
        })
    }

    /// Returns true if this (dev, ino) is new (count it); false if already seen.
    /// Always returns true on non-Unix.
    #[inline]
    fn record_inode(&self, _dev: u64, _ino: u64) -> bool {
        #[cfg(unix)]
        {
            let shard = (_ino as usize) & (INODE_SHARDS - 1);
            self.seen_inodes[shard].lock().unwrap().insert((_dev, _ino))
        }
        #[cfg(not(unix))]
        {
            true
        }
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
        // Update existing entry if path already tracked, otherwise insert
        if let Some(existing) = top.iter_mut().find(|e| e.path == path) {
            existing.size = size;
        } else if top.len() < TOP_N || size > top.last().map(|e| e.size).unwrap_or(0) {
            top.push(SizedEntry { path: path.to_path_buf(), size });
        }
        top.sort_by(|a, b| b.size.cmp(&a.size));
        top.truncate(TOP_N);
        let new_min = if top.len() >= TOP_N {
            top.last().map(|e| e.size).unwrap_or(0)
        } else {
            0
        };
        self.top_dirs_min.store(new_min, Ordering::Relaxed);
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
        self.dir_completion.lock().unwrap().clear();
        #[cfg(unix)]
        for shard in &self.seen_inodes {
            shard.lock().unwrap().clear();
        }
        self.aliased_files.store(0, Ordering::Relaxed);
    }

    /// Apply batched expected/yielded updates and detect newly-complete dirs.
    ///
    /// A directory `D` is **complete** when every immediate file/symlink child
    /// has been emitted by the walker AND every immediate subdirectory child
    /// has itself recursively completed. Concretely, `yielded[D] == expected[D]`
    /// where:
    ///   - `expected[D]` is the total number of entries in D, set when the
    ///      walker first yields D (via `read_dir(D).count()`).
    ///   - `yielded[D]` is incremented by 1 per immediate file/symlink child
    ///      yielded, and by 1 per immediate subdir child completing (cascade).
    ///
    /// `usize::MAX` is the "expected not yet known" sentinel — distinguishes
    /// a legitimately empty dir (`expected == 0`) from one we haven't visited.
    fn process_completions(
        &self,
        yielded: &mut HashMap<PathBuf, usize>,
        pending_expected: &mut Vec<(PathBuf, usize)>,
        root: &Path,
    ) -> Vec<PathBuf> {
        if yielded.is_empty() && pending_expected.is_empty() {
            return Vec::new();
        }

        let mut newly_completed = Vec::new();
        let mut completion = self.dir_completion.lock().unwrap();
        let mut to_check: Vec<PathBuf> = Vec::new();

        // Register newly-known expected counts. Empty dirs (expected = 0) are
        // included so they auto-complete on the very next check below.
        for (path, expected) in pending_expected.drain(..) {
            let entry = completion
                .entry(path.clone())
                .or_insert((0, usize::MAX));
            entry.1 = expected;
            to_check.push(path);
        }

        // Apply yielded counts.
        for (parent, count) in yielded.drain() {
            let entry = completion
                .entry(parent.clone())
                .or_insert((0, usize::MAX));
            entry.0 += count;
            to_check.push(parent);
        }

        // Cascade-aware completion check. Walks up the tree as parents become
        // satisfied by their last child completing.
        while let Some(path) = to_check.pop() {
            if let Some(&(yielded_n, expected_n)) = completion.get(&path) {
                if expected_n != usize::MAX && yielded_n >= expected_n {
                    completion.remove(&path);
                    newly_completed.push(path.clone());

                    // Bubble completion to parent: this subdir is now "done"
                    // from parent's perspective.
                    if path != root {
                        if let Some(parent) = path.parent() {
                            let parent_entry = completion
                                .entry(parent.to_path_buf())
                                .or_insert((0, usize::MAX));
                            parent_entry.0 += 1;
                            to_check.push(parent.to_path_buf());
                        }
                    }
                }
            }
        }

        newly_completed
    }
}

/// Thread-local state for each parallel walker thread.
/// Flushes remaining data to shared ScanState on drop.
struct ThreadLocalState {
    state: Arc<ScanState>,
    root: PathBuf,
    local_dir_sizes: HashMap<PathBuf, u64>,
    local_ext_stats: HashMap<String, (u64, u64)>,
    /// Entries yielded per parent directory (for completion tracking).
    /// Only file/symlink children contribute here; subdirs propagate via
    /// cascade when they themselves complete.
    local_yielded: HashMap<PathBuf, usize>,
    /// (dir, expected_child_count) pairs gathered this batch; flushed and
    /// turned into completion-map entries on the next flush.
    pending_expected: Vec<(PathBuf, usize)>,
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
            local_yielded: HashMap::new(),
            pending_expected: Vec::new(),
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

    fn flush_completions(&mut self) {
        // process_completions now atomically applies expected + yielded under
        // a single dir_completion lock and walks the cascade. Empty dirs and
        // newly-registered expected paths are queued for the completion check
        // on this same call.
        let newly_completed = self.state.process_completions(
            &mut self.local_yielded,
            &mut self.pending_expected,
            &self.root,
        );

        if newly_completed.is_empty() {
            return;
        }

        // Snapshot final sizes for newly-complete dirs. This is correct because
        // `flush_dir_sizes` ran before us in `flush()`, and any size for a
        // recursively-completed subtree must already have been flushed by its
        // contributing thread (each thread flushes sizes before yielded, and
        // a dir cannot complete until every contributing thread has flushed
        // its yielded count).
        let completed_with_sizes: Vec<(PathBuf, u64)> = {
            let sizes = self.state.dir_sizes.lock().unwrap();
            newly_completed
                .iter()
                .map(|p| (p.clone(), sizes.get(p).copied().unwrap_or(0)))
                .collect()
        };

        {
            let mut comp = self.state.completed.lock().unwrap();
            for (path, _) in &completed_with_sizes {
                comp.insert(path.clone());
            }
        }

        for (path, size) in &completed_with_sizes {
            self.state.record_completed_dir(path, *size);
        }
    }

    fn flush(&mut self) {
        self.flush_dir_sizes();
        self.flush_completions();
    }

    fn process_entry(&mut self, entry: ignore::DirEntry) {
        let depth = entry.depth();
        let path = strip_unc_prefix(entry.path().to_path_buf());
        let ft = entry.file_type();
        let is_dir = ft.map_or(false, |ft| ft.is_dir());
        let is_file = ft.map_or(false, |ft| ft.is_file());

        // Files and symlinks count as yielded in their parent the moment they
        // appear. Subdirectories do NOT count here; they only contribute when
        // they themselves complete (via the cascade in process_completions).
        // This is what makes "complete" mean "subtree fully processed" rather
        // than "immediate children listed".
        if depth > 0 && !is_dir {
            if let Some(parent) = path.parent() {
                *self.local_yielded
                    .entry(parent.to_path_buf())
                    .or_insert(0) += 1;
            }
        }

        if is_dir {
            self.state.record_dir(depth);
            if depth > self.local_deepest.1 {
                self.local_deepest = (path.clone(), depth);
            }

            // Establish `expected[D]` for every directory we encounter. The
            // count comes from a second `read_dir` on D, which is virtually
            // free here: the kernel just listed D for the walker, so D's
            // entries are hot in the dentry/inode cache. On error (e.g.
            // permission denied) we record 0, which lets the dir auto-complete
            // immediately rather than hanging forever.
            let expected = fs::read_dir(&path)
                .map(|rd| rd.count())
                .unwrap_or(0);
            self.pending_expected.push((path, expected));
        } else if is_file {
            // Stat the file once. Use the result for both dedup and size.
            let metadata = entry.metadata().ok();

            // Inode dedup: handles hardlinks AND macOS firmlink double-traversal
            // (e.g. /Users and /System/Volumes/Data/Users alias the same inodes).
            // On non-Unix this is a no-op.
            #[cfg(unix)]
            if let Some(ref m) = metadata {
                use std::os::unix::fs::MetadataExt;
                if !self.state.record_inode(m.dev(), m.ino()) {
                    self.state
                        .aliased_files
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }

            let len = metadata.as_ref().map(allocated_size).unwrap_or(0);

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
                self.flush();
                self.state
                    .files_scanned
                    .fetch_add(FLUSH_INTERVAL, Ordering::Relaxed);
                self.state.merge_ext_stats(&self.local_ext_stats);
                self.local_ext_stats.clear();
                self.state.refresh_top_exts(15);
            }
        }
    }
}

impl Drop for ThreadLocalState {
    fn drop(&mut self) {
        self.flush();
        let remainder = self.local_count % FLUSH_INTERVAL;
        if remainder > 0 {
            self.state
                .files_scanned
                .fetch_add(remainder, Ordering::Relaxed);
        }
        if !self.local_ext_stats.is_empty() {
            self.state.merge_ext_stats(&self.local_ext_stats);
        }
        if self.local_deepest.1 > 0 {
            self.state
                .set_deepest_path(&self.local_deepest.0, self.local_deepest.1);
        }
        if self.errors > 0 {
            log(&format!(
                "scan: thread finished with {} errors",
                self.errors
            ));
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

        // Mark all remaining directories as completed
        let all_dirs: Vec<(PathBuf, u64)> = {
            let sizes = state.dir_sizes.lock().unwrap();
            sizes.iter().map(|(p, &s)| (p.clone(), s)).collect()
        };
        {
            let mut comp = state.completed.lock().unwrap();
            for (dir, _) in &all_dirs {
                comp.insert(dir.clone());
            }
        }
        for (dir, size) in &all_dirs {
            state.record_completed_dir(dir, *size);
        }

        state.refresh_top_exts(15);
        state.scanning.store(false, Ordering::Release);
        log(&format!("scan: finished, {} files", state.files_scanned()));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // --- ScanState::new ---

    #[test]
    fn new_state_defaults() {
        let state = ScanState::new();
        assert!(!state.is_scanning());
        assert_eq!(state.files_scanned(), 0);
        assert_eq!(state.total_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(state.dirs_scanned.load(Ordering::Relaxed), 0);
        assert!(state.top_files.lock().unwrap().is_empty());
        assert!(state.top_dirs.lock().unwrap().is_empty());
        assert!(state.top_exts_cache.lock().unwrap().is_empty());
        assert_eq!(state.deepest_path.lock().unwrap().1, 0);
        assert!(state.scan_start.lock().unwrap().is_none());
    }

    // --- get_size / dir_sizes ---

    #[test]
    fn get_size_empty() {
        let state = ScanState::new();
        assert_eq!(state.get_size(Path::new("/foo")), None);
    }

    #[test]
    fn get_size_after_insert() {
        let state = ScanState::new();
        state.dir_sizes.lock().unwrap().insert(PathBuf::from("/foo"), 42);
        assert_eq!(state.get_size(Path::new("/foo")), Some(42));
    }

    // --- is_completed ---

    #[test]
    fn is_completed_empty() {
        let state = ScanState::new();
        assert!(!state.is_completed(Path::new("/foo")));
    }

    #[test]
    fn is_completed_after_insert() {
        let state = ScanState::new();
        state.completed.lock().unwrap().insert(PathBuf::from("/foo"));
        assert!(state.is_completed(Path::new("/foo")));
    }

    // --- is_scanning / files_scanned ---

    #[test]
    fn scanning_toggle() {
        let state = ScanState::new();
        assert!(!state.is_scanning());
        state.scanning.store(true, Ordering::Relaxed);
        assert!(state.is_scanning());
        state.scanning.store(false, Ordering::Relaxed);
        assert!(!state.is_scanning());
    }

    #[test]
    fn files_scanned_increment() {
        let state = ScanState::new();
        state.files_scanned.fetch_add(100, Ordering::Relaxed);
        assert_eq!(state.files_scanned(), 100);
        state.files_scanned.fetch_add(50, Ordering::Relaxed);
        assert_eq!(state.files_scanned(), 150);
    }

    // --- record_top_file ---

    #[test]
    fn record_top_file_basic() {
        let state = ScanState::new();
        state.record_top_file(Path::new("/big"), 1000);
        let top = state.top_files.lock().unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].size, 1000);
        assert_eq!(top[0].path, Path::new("/big"));
    }

    #[test]
    fn record_top_file_skips_small() {
        let state = ScanState::new();
        // Fill with TOP_N entries of size 100
        for i in 0..TOP_N {
            state.record_top_file(&PathBuf::from(format!("/f{i}")), 100);
        }
        // A file with size 50 should be skipped (below min threshold)
        state.record_top_file(Path::new("/small"), 50);
        let top = state.top_files.lock().unwrap();
        assert_eq!(top.len(), TOP_N);
        assert!(top.iter().all(|e| e.size == 100));
    }

    #[test]
    fn record_top_file_sorted_descending() {
        let state = ScanState::new();
        state.record_top_file(Path::new("/a"), 10);
        state.record_top_file(Path::new("/b"), 30);
        state.record_top_file(Path::new("/c"), 20);
        let top = state.top_files.lock().unwrap();
        assert_eq!(top[0].size, 30);
        assert_eq!(top[1].size, 20);
        assert_eq!(top[2].size, 10);
    }

    #[test]
    fn record_top_file_truncates_to_top_n() {
        let state = ScanState::new();
        for i in 0..(TOP_N + 5) {
            state.record_top_file(&PathBuf::from(format!("/f{i}")), (i + 1) as u64);
        }
        let top = state.top_files.lock().unwrap();
        assert_eq!(top.len(), TOP_N);
        // Largest should be first
        assert_eq!(top[0].size, (TOP_N + 5) as u64);
    }

    // --- record_dir ---

    #[test]
    fn record_dir_increments_count() {
        let state = ScanState::new();
        state.record_dir(1);
        state.record_dir(2);
        state.record_dir(3);
        assert_eq!(state.dirs_scanned.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn record_dir_tracks_deepest() {
        let state = ScanState::new();
        state.record_dir(5);
        state.record_dir(3);
        state.record_dir(10);
        state.record_dir(7);
        assert_eq!(state.deepest_depth.load(Ordering::Relaxed), 10);
    }

    // --- set_deepest_path ---

    #[test]
    fn set_deepest_path_basic() {
        let state = ScanState::new();
        state.set_deepest_path(Path::new("/a/b/c"), 3);
        let (path, depth) = state.deepest_path.lock().unwrap().clone();
        assert_eq!(path, Path::new("/a/b/c"));
        assert_eq!(depth, 3);
    }

    #[test]
    fn set_deepest_path_only_deeper() {
        let state = ScanState::new();
        state.set_deepest_path(Path::new("/deep"), 10);
        state.set_deepest_path(Path::new("/shallow"), 5);
        let (path, depth) = state.deepest_path.lock().unwrap().clone();
        assert_eq!(path, Path::new("/deep"));
        assert_eq!(depth, 10);
    }

    #[test]
    fn set_deepest_path_updates_deeper() {
        let state = ScanState::new();
        state.set_deepest_path(Path::new("/a"), 5);
        state.set_deepest_path(Path::new("/b"), 15);
        let (path, depth) = state.deepest_path.lock().unwrap().clone();
        assert_eq!(path, Path::new("/b"));
        assert_eq!(depth, 15);
    }

    // --- record_completed_dir ---

    #[test]
    fn record_completed_dir_basic() {
        let state = ScanState::new();
        state.record_completed_dir(Path::new("/big_dir"), 5000);
        let top = state.top_dirs.lock().unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].size, 5000);
    }

    #[test]
    fn record_completed_dir_dedup() {
        let state = ScanState::new();
        state.record_completed_dir(Path::new("/dir"), 100);
        state.record_completed_dir(Path::new("/dir"), 200);
        let top = state.top_dirs.lock().unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].size, 200); // updated, not duplicated
    }

    #[test]
    fn record_completed_dir_sorted_truncated() {
        let state = ScanState::new();
        for i in 0..(TOP_N + 3) {
            state.record_completed_dir(
                &PathBuf::from(format!("/d{i}")),
                (i + 1) as u64 * 100,
            );
        }
        let top = state.top_dirs.lock().unwrap();
        assert_eq!(top.len(), TOP_N);
        assert_eq!(top[0].size, (TOP_N + 3) as u64 * 100);
    }

    // --- merge_ext_stats ---

    #[test]
    fn merge_ext_stats_empty() {
        let state = ScanState::new();
        let local = HashMap::new();
        state.merge_ext_stats(&local);
        assert!(state.ext_stats.lock().unwrap().is_empty());
    }

    #[test]
    fn merge_ext_stats_single() {
        let state = ScanState::new();
        let mut local = HashMap::new();
        local.insert("txt".to_string(), (5, 1000));
        state.merge_ext_stats(&local);
        let stats = state.ext_stats.lock().unwrap();
        assert_eq!(stats.get("txt"), Some(&(5, 1000)));
    }

    #[test]
    fn merge_ext_stats_accumulates() {
        let state = ScanState::new();
        let mut local1 = HashMap::new();
        local1.insert("txt".to_string(), (3, 300));
        state.merge_ext_stats(&local1);

        let mut local2 = HashMap::new();
        local2.insert("txt".to_string(), (2, 200));
        local2.insert("rs".to_string(), (1, 100));
        state.merge_ext_stats(&local2);

        let stats = state.ext_stats.lock().unwrap();
        assert_eq!(stats.get("txt"), Some(&(5, 500)));
        assert_eq!(stats.get("rs"), Some(&(1, 100)));
    }

    // --- refresh_top_exts ---

    #[test]
    fn refresh_top_exts_sorts_and_truncates() {
        let state = ScanState::new();
        {
            let mut stats = state.ext_stats.lock().unwrap();
            stats.insert("a".to_string(), (1, 100));
            stats.insert("b".to_string(), (1, 300));
            stats.insert("c".to_string(), (1, 200));
        }
        state.refresh_top_exts(2);
        let cache = state.top_exts_cache.lock().unwrap();
        assert_eq!(cache.len(), 2);
        assert_eq!(cache[0].extension, "b");
        assert_eq!(cache[0].total_size, 300);
        assert_eq!(cache[1].extension, "c");
        assert_eq!(cache[1].total_size, 200);
    }

    // --- clear ---

    #[test]
    fn clear_resets_everything() {
        let state = ScanState::new();
        // Populate various fields
        state.dir_sizes.lock().unwrap().insert(PathBuf::from("/x"), 10);
        state.completed.lock().unwrap().insert(PathBuf::from("/x"));
        state.files_scanned.store(999, Ordering::Relaxed);
        state.scanning.store(true, Ordering::Relaxed);
        state.record_top_file(Path::new("/big"), 9999);
        state.record_completed_dir(Path::new("/d"), 8888);
        state.total_bytes.store(77777, Ordering::Relaxed);
        state.dirs_scanned.store(555, Ordering::Relaxed);
        state.set_deepest_path(Path::new("/deep"), 20);
        *state.scan_start.lock().unwrap() = Some(std::time::Instant::now());

        state.clear();

        assert!(state.dir_sizes.lock().unwrap().is_empty());
        assert!(state.completed.lock().unwrap().is_empty());
        assert_eq!(state.files_scanned(), 0);
        assert!(state.top_files.lock().unwrap().is_empty());
        assert!(state.top_dirs.lock().unwrap().is_empty());
        assert_eq!(state.total_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(state.dirs_scanned.load(Ordering::Relaxed), 0);
        assert_eq!(state.deepest_path.lock().unwrap().1, 0);
        assert!(state.scan_start.lock().unwrap().is_none());
    }

    // --- process_completions ---

    #[test]
    fn process_completions_empty() {
        let state = ScanState::new();
        let mut yielded = HashMap::new();
        let result = state.process_completions(&mut yielded, Path::new("/root"));
        assert!(result.is_empty());
    }

    #[test]
    fn process_completions_not_ready() {
        let state = ScanState::new();
        // Register a dir with expected=3
        {
            let mut comp = state.dir_completion.lock().unwrap();
            comp.insert(PathBuf::from("/root/a"), (0, 3));
        }
        // Yield only 2
        let mut yielded = HashMap::new();
        yielded.insert(PathBuf::from("/root/a"), 2);
        let result = state.process_completions(&mut yielded, Path::new("/root"));
        assert!(result.is_empty());
    }

    #[test]
    fn process_completions_completes() {
        let state = ScanState::new();
        {
            let mut comp = state.dir_completion.lock().unwrap();
            comp.insert(PathBuf::from("/root/a"), (0, 3));
        }
        let mut yielded = HashMap::new();
        yielded.insert(PathBuf::from("/root/a"), 3);
        let result = state.process_completions(&mut yielded, Path::new("/root"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], PathBuf::from("/root/a"));
    }

    #[test]
    fn process_completions_cascades_to_parent() {
        let state = ScanState::new();
        {
            let mut comp = state.dir_completion.lock().unwrap();
            // /root/a expects 2 entries, has 1 yielded (needs 1 more from child completing)
            comp.insert(PathBuf::from("/root/a"), (1, 2));
            // /root/a/b expects 1 entry, has 0 yielded
            comp.insert(PathBuf::from("/root/a/b"), (0, 1));
        }
        // Yield 1 to /root/a/b → it completes → cascades +1 to /root/a → /root/a completes
        let mut yielded = HashMap::new();
        yielded.insert(PathBuf::from("/root/a/b"), 1);
        let result = state.process_completions(&mut yielded, Path::new("/root"));
        assert!(result.contains(&PathBuf::from("/root/a/b")));
        assert!(result.contains(&PathBuf::from("/root/a")));
    }

    #[test]
    fn process_completions_stops_at_root() {
        let state = ScanState::new();
        {
            let mut comp = state.dir_completion.lock().unwrap();
            comp.insert(PathBuf::from("/root"), (0, 1));
        }
        let mut yielded = HashMap::new();
        yielded.insert(PathBuf::from("/root"), 1);
        let result = state.process_completions(&mut yielded, Path::new("/root"));
        assert_eq!(result, vec![PathBuf::from("/root")]);
        // Should not cascade above root
    }

    // --- start_scan integration ---

    #[test]
    fn start_scan_on_temp_dir() {
        let tmp = std::env::temp_dir().join("rdirstat_test_scan");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("sub")).unwrap();
        fs::write(tmp.join("file.txt"), "hello").unwrap();
        fs::write(tmp.join("sub/inner.txt"), "world!").unwrap();

        let state = ScanState::new();
        start_scan(tmp.clone(), Arc::clone(&state));

        // Wait for scan to finish
        for _ in 0..200 {
            if !state.is_scanning() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!state.is_scanning());
        assert!(state.files_scanned() >= 2);
        assert!(state.total_bytes.load(Ordering::Relaxed) >= 11);
        assert!(state.is_completed(&tmp));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn start_scan_cancel() {
        let tmp = std::env::temp_dir().join("rdirstat_test_cancel");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("f.txt"), "x").unwrap();

        let state = ScanState::new();
        start_scan(tmp.clone(), Arc::clone(&state));
        state.cancel.store(true, Ordering::Relaxed);

        for _ in 0..200 {
            if !state.is_scanning() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!state.is_scanning());

        let _ = fs::remove_dir_all(&tmp);
    }
}
