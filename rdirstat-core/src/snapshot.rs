use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::scan::{ExtensionStat, ScanState, SizedEntry};

/// A resolved view of one directory entry for the UI.
#[derive(Clone)]
pub struct EntrySnapshot {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_parent: bool,
    pub size: u64,
    pub scanning: bool,
}

/// Everything the UI needs for one frame, produced off the UI thread.
#[derive(Clone)]
pub struct UiSnapshot {
    pub scanning: bool,
    pub files_scanned: u64,
    pub dirs_scanned: u64,
    pub total_bytes: u64,
    pub entries: Vec<EntrySnapshot>,
    pub total_entry_size: u64,
    pub top_files: Vec<SizedEntry>,
    pub top_dirs: Vec<SizedEntry>,
    pub top_exts: Vec<ExtensionStat>,
    pub deepest: (PathBuf, usize),
    pub scan_start: Option<std::time::Instant>,
}

impl UiSnapshot {
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

/// Build a snapshot from the current scan state and entry list.
/// Uses targeted lookups — only reads the data it needs, never clones entire maps.
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
                let size = if *is_dir {
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

/// Spawn a background thread that periodically builds snapshots and sends
/// them through a channel. The UI thread calls `try_recv()` to get the latest.
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
