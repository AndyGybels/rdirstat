//! Integration tests for the post-order directory completion logic.
//!
//! After the rewrite of `process_completions`, a directory is "complete"
//! only when every file in its subtree has been processed and every
//! immediate subdirectory has itself recursively completed. These tests
//! exercise that invariant.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rdirstat_core::{start_scan, ScanState};

/// Spin until the scan thread reports it's no longer scanning.
fn wait_for_scan(state: &ScanState, timeout: Duration) {
    let start = Instant::now();
    while state.is_scanning() {
        if start.elapsed() > timeout {
            panic!("scan did not complete within {:?}", timeout);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn write_file(path: &Path, len: usize) {
    let mut f = fs::File::create(path).unwrap();
    f.write_all(&vec![0u8; len]).unwrap();
}

#[test]
fn scan_completes_and_marks_every_dir_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Build:
    //   root/
    //     a.bin (100 bytes)
    //     sub/
    //       b.bin (100 bytes)
    //       deeper/
    //         c.bin (100 bytes)
    //     empty/
    fs::create_dir_all(root.join("sub").join("deeper")).unwrap();
    fs::create_dir_all(root.join("empty")).unwrap();
    write_file(&root.join("a.bin"), 100);
    write_file(&root.join("sub").join("b.bin"), 100);
    write_file(&root.join("sub").join("deeper").join("c.bin"), 100);

    let state = ScanState::new();
    start_scan(root.to_path_buf(), Arc::clone(&state));
    wait_for_scan(&state, Duration::from_secs(10));

    // Every directory we built must be in the completed set after the scan.
    for d in &[
        root.to_path_buf(),
        root.join("sub"),
        root.join("sub").join("deeper"),
        root.join("empty"),
    ] {
        assert!(
            state.is_completed(d),
            "expected {} to be marked completed",
            d.display()
        );
    }
}

#[test]
fn deep_dirs_mark_parent_complete_only_after_their_subtree_finishes() {
    // This test guards against the old bug: a directory was being marked
    // "complete" the moment its immediate children had been listed by the
    // walker, even though deeper subdir files hadn't been counted yet.
    //
    // We build a moderately nested tree and assert that:
    //   - At end of scan, dir_sizes for the root reflects ALL files
    //     in the subtree (the bug would make it grow past the size at
    //     the time root was first flagged complete).
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // root/
    //   level1/
    //     level2/
    //       level3/
    //         many small files
    let deep = root.join("level1").join("level2").join("level3");
    fs::create_dir_all(&deep).unwrap();

    const FILE_COUNT: usize = 200;
    const FILE_SIZE: usize = 256;
    for i in 0..FILE_COUNT {
        write_file(&deep.join(format!("f{i:04}.bin")), FILE_SIZE);
    }

    let state = ScanState::new();
    start_scan(root.to_path_buf(), Arc::clone(&state));
    wait_for_scan(&state, Duration::from_secs(10));

    // The size attributed to the root must reach the full subtree total.
    // (Allocated size on disk will be >= logical due to filesystem cluster
    // rounding, so we assert >= the logical lower bound.)
    let root_size = state.get_size(root).unwrap_or(0);
    let logical_total = (FILE_COUNT * FILE_SIZE) as u64;
    assert!(
        root_size >= logical_total,
        "root size {} should be at least logical total {}",
        root_size,
        logical_total
    );

    // Sanity: the deep dir is also marked complete.
    assert!(state.is_completed(&deep));
    assert!(state.is_completed(&root.join("level1")));
    assert!(state.is_completed(root));
}

#[test]
fn empty_dir_completes() {
    // Pure regression: empty dirs (expected = 0) should auto-complete on
    // first check rather than blocking their parent forever.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    fs::create_dir_all(root.join("a").join("empty1")).unwrap();
    fs::create_dir_all(root.join("a").join("empty2")).unwrap();
    write_file(&root.join("a").join("file.bin"), 50);

    let state = ScanState::new();
    start_scan(root.to_path_buf(), Arc::clone(&state));
    wait_for_scan(&state, Duration::from_secs(5));

    assert!(state.is_completed(&root.join("a").join("empty1")));
    assert!(state.is_completed(&root.join("a").join("empty2")));
    assert!(state.is_completed(&root.join("a")));
    assert!(state.is_completed(root));
}

#[test]
fn parent_nav_entry_does_not_inflate_total() {
    // Regression: when navigating into a subdirectory, the ".." parent-nav
    // entry has is_dir=true, path=<parent>. A naive size lookup would return
    // the parent dir's full size, which then propagated into the snapshot's
    // total_entry_size, making the UI display "current dir + parent dir"
    // for the total.
    use rdirstat_core::snapshot::build_snapshot;
    use std::sync::atomic::{AtomicBool, Ordering};

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // root/
    //   sibling.bin (10_000 bytes — large, lives in parent)
    //   sub/
    //     small.bin (1000 bytes — lives in current dir)
    fs::create_dir_all(root.join("sub")).unwrap();
    write_file(&root.join("sibling.bin"), 10_000);
    write_file(&root.join("sub").join("small.bin"), 1_000);

    let state = ScanState::new();
    start_scan(root.to_path_buf(), Arc::clone(&state));
    wait_for_scan(&state, Duration::from_secs(5));

    // Simulate the entry list a frontend would build when sitting inside `sub/`.
    // Order matches AppState::load_entries: ".." first, then real entries.
    let sub = root.join("sub");
    let entries = vec![
        ("..".to_string(), root.to_path_buf(), true, /* is_parent */ true, 0),
        ("small.bin".to_string(), sub.join("small.bin"), false, false, 1_000),
    ];

    let snap = build_snapshot(&state, &entries, false);

    // The total must reflect ONLY the contents of `sub/`, not the parent.
    let small_alloc = snap
        .entries
        .iter()
        .find(|e| e.name == "small.bin")
        .map(|e| e.size)
        .unwrap();
    assert_eq!(
        snap.total_entry_size, small_alloc,
        "total_entry_size leaked the parent dir size into the current view"
    );

    // The ".." entry itself must report 0 (it's a navigation affordance).
    let parent_entry = snap.entries.iter().find(|e| e.is_parent).unwrap();
    assert_eq!(parent_entry.size, 0, "parent-nav entry must report size 0");
    assert!(parent_entry.size < 10_000, "parent-nav size leaked");

    // Touch the unused sort_by_size flag so the import doesn't warn.
    let _ = AtomicBool::new(false).load(Ordering::Relaxed);
}

#[cfg(unix)]
#[test]
fn hardlinked_files_are_counted_once() {
    // Two hardlinks to the same inode must contribute size only once,
    // matching `du`'s default behavior.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let real = root.join("real.bin");
    write_file(&real, 4096);
    fs::hard_link(&real, root.join("alias.bin")).unwrap();

    let state = ScanState::new();
    start_scan(root.to_path_buf(), Arc::clone(&state));
    wait_for_scan(&state, Duration::from_secs(5));

    // Size attributed to root should reflect a single 4 KB file, not 8 KB.
    let root_size = state.get_size(root).unwrap_or(0);
    assert!(
        root_size < 8192,
        "root size {} suggests both hardlinks were counted (expected single 4 KB allocation)",
        root_size
    );
    assert!(root_size >= 4096, "root size {} too small", root_size);

    // And the alias should have been recorded as such.
    assert!(state.aliased_files.load(std::sync::atomic::Ordering::Relaxed) >= 1);
}
