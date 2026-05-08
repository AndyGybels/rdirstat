//! Lock-free, clone-on-tick view of the live [`ScanState`] for the UI.
//!
//! Frontends never read [`ScanState`] directly during rendering. Instead a
//! background thread (see [`spawn_snapshot_thread`]) polls the live state
//! every 100 ms, builds a [`UiSnapshot`], and pushes it through an
//! `mpsc::Sender`. The UI's event loop drains the channel each frame and
//! renders off the latest snapshot — no scanner-locks held during draw,
//! no jitter as the walker grows the data structures behind it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::scan::{ExtensionStat, ScanState, SizedEntry};

/// One resolved row in a directory listing, suitable for direct rendering.
///
/// Built by [`build_snapshot`] from the entries the frontend wants to show.
/// The `..` parent-nav row uses `is_parent = true`, in which case `size`
/// is forced to 0 (it's a navigation affordance, not a real member of the
/// current directory).
#[derive(Clone)]
pub struct EntrySnapshot {
    /// Display name (e.g. `"src"`, `".."`).
    pub name: String,
    /// Absolute path. For `is_parent` entries this is the parent directory.
    pub path: PathBuf,
    pub is_dir: bool,
    /// True if this is the synthetic `..` parent-nav entry.
    pub is_parent: bool,
    /// Size in bytes. Always 0 for parent-nav entries.
    pub size: u64,
    /// Whether this directory is still being walked. Always false for files
    /// and for `is_parent` entries.
    pub scanning: bool,
}

/// Everything the UI needs to render one frame.
///
/// `Clone` is cheap-ish (a few `Vec`s and `String`s) — the snapshot thread
/// produces a fresh one every 100 ms and pushes it through a channel.
#[derive(Clone)]
pub struct UiSnapshot {
    /// Whether the scan is currently in progress.
    pub scanning: bool,
    /// Total unique files counted (after inode dedup).
    pub files_scanned: u64,
    /// Total directories visited.
    pub dirs_scanned: u64,
    /// Total bytes counted across all unique files.
    pub total_bytes: u64,
    /// The current directory's listing, sorted as the UI requested.
    pub entries: Vec<EntrySnapshot>,
    /// Sum of `entries[i].size` across non-parent rows. Excludes `..`'s
    /// (zeroed) size, so this is the *current directory*'s total — never
    /// inflated by the parent.
    pub total_entry_size: u64,
    /// Running top-N biggest files in the whole subtree.
    pub top_files: Vec<SizedEntry>,
    /// Running top-N biggest directories.
    pub top_dirs: Vec<SizedEntry>,
    /// Top-N extensions by total size.
    pub top_exts: Vec<ExtensionStat>,
    /// `(path, depth)` of the deepest directory encountered so far.
    pub deepest: (PathBuf, usize),
    /// When the current scan started (for elapsed-time / rate displays).
    pub scan_start: Option<std::time::Instant>,
}

impl UiSnapshot {
    /// A snapshot with all counters at 0 and all lists empty. Useful as an
    /// initial value for the UI before the snapshot thread has produced
    /// anything.
    pub fn empty() -> Self {
        UiSnapshot {
            scanning: false,
            files_scanned: 0,
            dirs_scanned: 0,
            total_bytes: 0,
            entries: Vec::new(),
            total_entry_size: 0,
            top_files: Vec::new(),
            top_dirs: Vec::new(),
            top_exts: Vec::new(),
            deepest: (PathBuf::new(), 0),
            scan_start: None,
        }
    }
}

/// Build a [`UiSnapshot`] from the current scan state and the directory
/// listing the frontend wants to show.
///
/// `entries` is the frontend's pre-built list of rows
/// `(name, path, is_dir, is_parent, file_size)` — the function only locks
/// the [`ScanState`] briefly to look up directory sizes and the
/// `completed` set, then drops the locks before sorting and assembling the
/// snapshot. Sort by size if `sort_by_size`, otherwise by name; the
/// parent-nav row is always pinned to the top regardless.
pub fn build_snapshot(
    scan: &ScanState,
    entries: &[(String, PathBuf, bool, bool, u64)], // (name, path, is_dir, is_parent, file_size)
    sort_by_size: bool,
) -> UiSnapshot {
    let scanning = scan.is_scanning();
    let files_scanned = scan.files_scanned();
    let dirs_scanned = scan.dirs_scanned.load(Ordering::Relaxed);
    let total_bytes = scan.total_bytes.load(Ordering::Relaxed);

    // Targeted lookups: lock once, read only the entries we need, unlock.
    let mut entry_snapshots: Vec<EntrySnapshot> = {
        let dir_sizes = scan.dir_sizes.lock().unwrap();
        let completed = scan.completed.lock().unwrap();

        entries
            .iter()
            .map(|(name, path, is_dir, is_parent, file_size)| {
                // The ".." parent-nav entry is a navigation affordance, not a
                // member of the current directory. Reporting `dir_sizes[parent]`
                // here would (a) display the entire parent's size next to ".."
                // and (b) inflate `total_entry_size` by exactly that amount,
                // making the header read "current dir + parent dir".
                let size = if *is_parent {
                    0
                } else if *is_dir {
                    dir_sizes.get(path).copied().unwrap_or(0)
                } else {
                    *file_size
                };
                let entry_scanning = *is_dir && scanning && !completed.contains(path);
                EntrySnapshot {
                    name: name.clone(),
                    path: path.clone(),
                    is_dir: *is_dir,
                    is_parent: *is_parent,
                    size,
                    scanning: entry_scanning,
                }
            })
            .collect()
        // dir_sizes and completed locks dropped here
    };

    // Sort entries: parent ".." always first, then by size or name
    if sort_by_size {
        entry_snapshots.sort_by(|a, b| {
            a.is_parent.cmp(&b.is_parent).reverse()
                .then_with(|| b.size.cmp(&a.size))
        });
    } else {
        entry_snapshots.sort_by(|a, b| {
            a.is_parent.cmp(&b.is_parent).reverse()
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    }

    let total_entry_size: u64 = entry_snapshots.iter().map(|e| e.size).sum();

    let top_files = scan.top_files.lock().unwrap().clone();
    let top_dirs = scan.top_dirs.lock().unwrap().clone();
    let top_exts = scan.top_exts_cache.lock().unwrap().clone();
    let deepest = scan.deepest_path.lock().unwrap().clone();
    let scan_start = *scan.scan_start.lock().unwrap();

    UiSnapshot {
        scanning,
        files_scanned,
        dirs_scanned,
        total_bytes,
        entries: entry_snapshots,
        total_entry_size,
        top_files,
        top_dirs,
        top_exts,
        deepest,
        scan_start,
    }
}

/// Spawn the snapshot thread.
///
/// Every 100 ms the thread:
///
/// 1. Reads the current entry list from `entry_source` (frontend updates
///    this whenever the user navigates).
/// 2. Reads the sort preference atomic.
/// 3. Calls [`build_snapshot`] and pushes the result through the returned
///    `Receiver`.
/// 4. Exits when the receiver has been dropped.
///
/// The UI's event loop should `try_recv` (non-blocking) on each frame and
/// keep the latest [`UiSnapshot`]. Don't `recv` (blocking) — that defeats
/// the whole architecture.
pub fn spawn_snapshot_thread(
    scan_state: Arc<ScanState>,
    entry_source: Arc<std::sync::Mutex<Vec<(String, PathBuf, bool, bool, u64)>>>,
    sort_by_size: Arc<AtomicBool>,
) -> std::sync::mpsc::Receiver<UiSnapshot> {
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));

            let entries = entry_source.lock().unwrap().clone();
            let sort = sort_by_size.load(Ordering::Relaxed);
            let snapshot = build_snapshot(&scan_state, &entries, sort);
            if tx.send(snapshot).is_err() {
                break; // receiver dropped, UI closed
            }
        }
    });

    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::Ordering;

    #[test]
    fn empty_snapshot() {
        let snap = UiSnapshot::empty();
        assert!(!snap.scanning);
        assert_eq!(snap.files_scanned, 0);
        assert_eq!(snap.dirs_scanned, 0);
        assert_eq!(snap.total_bytes, 0);
        assert!(snap.entries.is_empty());
        assert_eq!(snap.total_entry_size, 0);
        assert!(snap.top_files.is_empty());
        assert!(snap.top_dirs.is_empty());
        assert!(snap.top_exts.is_empty());
        assert_eq!(snap.deepest.1, 0);
        assert!(snap.scan_start.is_none());
    }

    #[test]
    fn build_snapshot_empty_entries() {
        let state = ScanState::new();
        let entries: Vec<(String, PathBuf, bool, bool, u64)> = vec![];
        let snap = build_snapshot(&state, &entries, true);
        assert!(!snap.scanning);
        assert!(snap.entries.is_empty());
        assert_eq!(snap.total_entry_size, 0);
    }

    #[test]
    fn build_snapshot_with_files() {
        let state = ScanState::new();
        let entries = vec![
            ("b.txt".to_string(), PathBuf::from("/b.txt"), false, false, 200u64),
            ("a.txt".to_string(), PathBuf::from("/a.txt"), false, false, 100u64),
        ];
        let snap = build_snapshot(&state, &entries, false);
        assert_eq!(snap.entries.len(), 2);
        assert_eq!(snap.total_entry_size, 300);
        // Sorted by name
        assert_eq!(snap.entries[0].name, "a.txt");
        assert_eq!(snap.entries[1].name, "b.txt");
    }

    #[test]
    fn build_snapshot_sort_by_size() {
        let state = ScanState::new();
        let entries = vec![
            ("small.txt".to_string(), PathBuf::from("/small"), false, false, 10u64),
            ("big.txt".to_string(), PathBuf::from("/big"), false, false, 1000u64),
            ("mid.txt".to_string(), PathBuf::from("/mid"), false, false, 500u64),
        ];
        let snap = build_snapshot(&state, &entries, true);
        assert_eq!(snap.entries[0].name, "big.txt");
        assert_eq!(snap.entries[1].name, "mid.txt");
        assert_eq!(snap.entries[2].name, "small.txt");
    }

    #[test]
    fn build_snapshot_parent_always_first() {
        let state = ScanState::new();
        let entries = vec![
            ("a.txt".to_string(), PathBuf::from("/a"), false, false, 9999u64),
            ("..".to_string(), PathBuf::from("/parent"), true, true, 0u64),
        ];
        // Sort by size — parent should still be first despite having size 0
        let snap = build_snapshot(&state, &entries, true);
        assert_eq!(snap.entries[0].name, "..");
        assert!(snap.entries[0].is_parent);
    }

    #[test]
    fn build_snapshot_dir_uses_scan_size() {
        let state = ScanState::new();
        state.dir_sizes.lock().unwrap().insert(PathBuf::from("/sub"), 5000);
        let entries = vec![
            ("sub".to_string(), PathBuf::from("/sub"), true, false, 0u64),
        ];
        let snap = build_snapshot(&state, &entries, true);
        assert_eq!(snap.entries[0].size, 5000);
        assert_eq!(snap.total_entry_size, 5000);
    }

    #[test]
    fn build_snapshot_scanning_state() {
        let state = ScanState::new();
        state.scanning.store(true, Ordering::Relaxed);
        let entries = vec![
            ("sub".to_string(), PathBuf::from("/sub"), true, false, 0u64),
        ];
        let snap = build_snapshot(&state, &entries, true);
        assert!(snap.scanning);
        assert!(snap.entries[0].scanning); // dir not completed while scanning
    }

    #[test]
    fn build_snapshot_completed_dir_not_scanning() {
        let state = ScanState::new();
        state.scanning.store(true, Ordering::Relaxed);
        state.completed.lock().unwrap().insert(PathBuf::from("/sub"));
        let entries = vec![
            ("sub".to_string(), PathBuf::from("/sub"), true, false, 0u64),
        ];
        let snap = build_snapshot(&state, &entries, true);
        assert!(!snap.entries[0].scanning);
    }

    #[test]
    fn build_snapshot_includes_top_files() {
        let state = ScanState::new();
        state.record_top_file(Path::new("/big"), 9999);
        let snap = build_snapshot(&state, &[], true);
        assert_eq!(snap.top_files.len(), 1);
        assert_eq!(snap.top_files[0].size, 9999);
    }

    #[test]
    fn build_snapshot_includes_deepest() {
        let state = ScanState::new();
        state.set_deepest_path(Path::new("/a/b/c/d"), 4);
        let snap = build_snapshot(&state, &[], true);
        assert_eq!(snap.deepest.1, 4);
    }

    #[test]
    fn build_snapshot_includes_scan_start() {
        let state = ScanState::new();
        let now = std::time::Instant::now();
        *state.scan_start.lock().unwrap() = Some(now);
        let snap = build_snapshot(&state, &[], true);
        assert!(snap.scan_start.is_some());
    }

    #[test]
    fn spawn_snapshot_thread_produces_snapshots() {
        let state = ScanState::new();
        state.files_scanned.store(42, Ordering::Relaxed);
        let entry_source = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sort_by_size = Arc::new(AtomicBool::new(true));

        let rx = spawn_snapshot_thread(
            Arc::clone(&state),
            Arc::clone(&entry_source),
            sort_by_size,
        );

        // Wait for at least one snapshot
        let snap = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(snap.files_scanned, 42);
        // Drop receiver to stop the thread
    }
}
