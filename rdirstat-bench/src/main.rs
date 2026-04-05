//! Benchmark: isolates what's causing scan slowdown.
//!
//! Runs 4 modes on the same directory:
//!   1. walkdir-only    — just iterate walkdir, count files. Pure I/O baseline.
//!   2. walkdir+metadata — iterate + read file sizes. I/O + metadata cost.
//!   3. walkdir+hashmap  — iterate + insert every dir path into a HashMap. Data structure cost.
//!   4. full-scan        — everything the real scanner does (dir_sizes, completed, top_files, ext_stats).
//!
//! Usage: rdirstat-bench <path>
//!   e.g.  rdirstat-bench C:\

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

fn main() {
    let root = env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: rdirstat-bench <path>");
        std::process::exit(1);
    });
    let root = PathBuf::from(&root);

    if !root.is_dir() {
        eprintln!("Error: {} is not a directory", root.display());
        std::process::exit(1);
    }

    println!("Benchmarking scan of: {}", root.display());
    println!("Threads: {}", num_cpus::get().max(2));
    println!();

    // Collect top-level dirs
    let child_dirs: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();

    println!("Top-level directories: {}", child_dirs.len());
    println!();

    // ── Mode 1: walkdir only ─────────────────────────────────────────────
    println!("=== Mode 1: walkdir-only (pure I/O) ===");
    let r = run_parallel(&child_dirs, |path, cancel| {
        let mut files = 0u64;
        let mut dirs = 0u64;
        for entry in walkdir::WalkDir::new(path).follow_links(false) {
            if cancel.load(Ordering::Relaxed) { break; }
            if let Ok(e) = entry {
                if e.file_type().is_file() { files += 1; }
                else if e.file_type().is_dir() { dirs += 1; }
            }
        }
        (files, dirs, 0u64)
    });
    print_result(&r);

    // ── Mode 2: walkdir + metadata ───────────────────────────────────────
    println!("=== Mode 2: walkdir + metadata (I/O + file sizes) ===");
    let r = run_parallel(&child_dirs, |path, cancel| {
        let mut files = 0u64;
        let mut dirs = 0u64;
        let mut bytes = 0u64;
        for entry in walkdir::WalkDir::new(path).follow_links(false) {
            if cancel.load(Ordering::Relaxed) { break; }
            if let Ok(e) = entry {
                if e.file_type().is_file() {
                    files += 1;
                    bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                } else if e.file_type().is_dir() {
                    dirs += 1;
                }
            }
        }
        (files, dirs, bytes)
    });
    print_result(&r);

    // ── Mode 3: walkdir + HashMap inserts ────────────────────────────────
    println!("=== Mode 3: walkdir + shared HashMap (data structure cost) ===");
    let shared_map: Arc<Mutex<HashMap<PathBuf, u64>>> =
        Arc::new(Mutex::new(HashMap::with_capacity(100_000)));
    let shared_set: Arc<Mutex<HashSet<PathBuf>>> =
        Arc::new(Mutex::new(HashSet::with_capacity(100_000)));

    let r = run_parallel_with_state(
        &child_dirs,
        (shared_map.clone(), shared_set.clone()),
        |path, cancel, (map, set)| {
            let mut files = 0u64;
            let mut dirs = 0u64;
            let mut bytes = 0u64;
            let mut dir_stack: Vec<(PathBuf, u64)> = Vec::new();
            let mut batch: Vec<(PathBuf, u64)> = Vec::new();

            for entry in walkdir::WalkDir::new(path).follow_links(false) {
                if cancel.load(Ordering::Relaxed) { break; }
                if let Ok(e) = entry {
                    let depth = e.depth();

                    while dir_stack.len() > depth {
                        if let Some((dir, size)) = dir_stack.pop() {
                            batch.push((dir, size));
                            if let Some(parent) = dir_stack.last_mut() {
                                parent.1 += size;
                            }
                        }
                    }

                    if !batch.is_empty() {
                        let mut m = map.lock().unwrap();
                        let mut s = set.lock().unwrap();
                        for (dir, size) in batch.drain(..) {
                            m.insert(dir.clone(), size);
                            s.insert(dir);
                        }
                    }

                    if e.file_type().is_dir() {
                        dirs += 1;
                        dir_stack.push((e.path().to_path_buf(), 0));
                    } else if e.file_type().is_file() {
                        files += 1;
                        let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                        bytes += len;
                        if let Some(parent) = dir_stack.last_mut() {
                            parent.1 += len;
                        }

                        if files % 5000 == 0 {
                            if !dir_stack.is_empty() {
                                let total: u64 = dir_stack.iter().map(|(_, s)| s).sum();
                                map.lock().unwrap().insert(dir_stack[0].0.clone(), total);
                            }
                        }
                    }
                }
            }

            // flush remaining
            {
                let mut m = map.lock().unwrap();
                let mut s = set.lock().unwrap();
                while let Some((dir, size)) = dir_stack.pop() {
                    m.insert(dir.clone(), size);
                    s.insert(dir);
                    if let Some(parent) = dir_stack.last_mut() {
                        parent.1 += size;
                    }
                }
            }

            (files, dirs, bytes)
        },
    );
    let map_entries = shared_map.lock().unwrap().len();
    let set_entries = shared_set.lock().unwrap().len();
    print_result(&r);
    println!("  HashMap entries: {}, HashSet entries: {}", map_entries, set_entries);
    println!();

    // ── Mode 4: walkdir + HashMap + FxHashMap comparison ─────────────────
    // (uses a simple custom hasher to show hashing cost)
    println!("=== Mode 4: walkdir + integer-keyed Vec (no hashing) ===");
    let counter = Arc::new(AtomicU64::new(0));
    let sizes_vec: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::with_capacity(100_000)));

    let r = run_parallel_with_state(
        &child_dirs,
        (counter.clone(), sizes_vec.clone()),
        |path, cancel, (counter, sizes_vec)| {
            let mut files = 0u64;
            let mut dirs = 0u64;
            let mut bytes = 0u64;
            let mut dir_stack: Vec<(usize, u64)> = Vec::new(); // (id, size)

            for entry in walkdir::WalkDir::new(path).follow_links(false) {
                if cancel.load(Ordering::Relaxed) { break; }
                if let Ok(e) = entry {
                    let depth = e.depth();

                    while dir_stack.len() > depth {
                        if let Some((id, size)) = dir_stack.pop() {
                            let mut v = sizes_vec.lock().unwrap();
                            if id < v.len() { v[id] = size; }
                            drop(v);
                            if let Some(parent) = dir_stack.last_mut() {
                                parent.1 += size;
                            }
                        }
                    }

                    if e.file_type().is_dir() {
                        dirs += 1;
                        let id = counter.fetch_add(1, Ordering::Relaxed) as usize;
                        let mut v = sizes_vec.lock().unwrap();
                        if id >= v.len() { v.resize(id + 1024, 0); }
                        drop(v);
                        dir_stack.push((id, 0));
                    } else if e.file_type().is_file() {
                        files += 1;
                        let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                        bytes += len;
                        if let Some(parent) = dir_stack.last_mut() {
                            parent.1 += len;
                        }
                    }
                }
            }

            // flush remaining
            {
                let mut v = sizes_vec.lock().unwrap();
                while let Some((id, size)) = dir_stack.pop() {
                    if id < v.len() { v[id] = size; }
                    if let Some(parent) = dir_stack.last_mut() {
                        parent.1 += size;
                    }
                }
            }

            (files, dirs, bytes)
        },
    );
    print_result(&r);
    println!("  Vec entries: {}", sizes_vec.lock().unwrap().len());
    println!();

    println!("Done. Compare the files/sec between modes to identify the bottleneck.");
}

struct BenchResult {
    files: u64,
    dirs: u64,
    bytes: u64,
    elapsed: Duration,
}

fn print_result(r: &BenchResult) {
    let secs = r.elapsed.as_secs_f64();
    let files_per_sec = r.files as f64 / secs;
    let files_per_min = files_per_sec * 60.0;
    println!("  Files: {}, Dirs: {}, Bytes: {}", r.files, r.dirs, format_size(r.bytes));
    println!("  Time:  {:.2}s", secs);
    println!("  Rate:  {:.0} files/sec ({:.0}/min)", files_per_sec, files_per_min);
    println!();
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_099_511_627_776 {
        format!("{:.2} TB", bytes as f64 / 1_099_511_627_776.0)
    } else if bytes >= 1_073_741_824 {
        format!("{:.2} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn run_parallel<F>(dirs: &[PathBuf], worker_fn: F) -> BenchResult
where
    F: Fn(&PathBuf, &AtomicBool) -> (u64, u64, u64) + Send + Sync + 'static,
{
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_fn = Arc::new(worker_fn);
    let total_files = Arc::new(AtomicU64::new(0));
    let total_dirs = Arc::new(AtomicU64::new(0));
    let total_bytes = Arc::new(AtomicU64::new(0));

    let num_threads = num_cpus::get().max(2);
    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
    let rx = Arc::new(Mutex::new(rx));

    let start = Instant::now();

    let mut handles = Vec::new();
    for _ in 0..num_threads {
        let rx = Arc::clone(&rx);
        let cancel = Arc::clone(&cancel);
        let worker_fn = Arc::clone(&worker_fn);
        let tf = Arc::clone(&total_files);
        let td = Arc::clone(&total_dirs);
        let tb = Arc::clone(&total_bytes);
        handles.push(thread::spawn(move || {
            loop {
                let path = {
                    let lock = rx.lock().unwrap();
                    match lock.recv() {
                        Ok(p) => p,
                        Err(_) => break,
                    }
                };
                let (f, d, b) = worker_fn(&path, &cancel);
                tf.fetch_add(f, Ordering::Relaxed);
                td.fetch_add(d, Ordering::Relaxed);
                tb.fetch_add(b, Ordering::Relaxed);
            }
        }));
    }

    for dir in dirs {
        let _ = tx.send(dir.clone());
    }
    drop(tx);

    for h in handles {
        let _ = h.join();
    }

    let elapsed = start.elapsed();

    BenchResult {
        files: total_files.load(Ordering::Relaxed),
        dirs: total_dirs.load(Ordering::Relaxed),
        bytes: total_bytes.load(Ordering::Relaxed),
        elapsed,
    }
}

fn run_parallel_with_state<S, F>(dirs: &[PathBuf], state: S, worker_fn: F) -> BenchResult
where
    S: Clone + Send + 'static,
    F: Fn(&PathBuf, &AtomicBool, &S) -> (u64, u64, u64) + Send + Sync + 'static,
{
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_fn = Arc::new(worker_fn);
    let total_files = Arc::new(AtomicU64::new(0));
    let total_dirs = Arc::new(AtomicU64::new(0));
    let total_bytes = Arc::new(AtomicU64::new(0));

    let num_threads = num_cpus::get().max(2);
    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
    let rx = Arc::new(Mutex::new(rx));

    let start = Instant::now();

    let mut handles = Vec::new();
    for _ in 0..num_threads {
        let rx = Arc::clone(&rx);
        let cancel = Arc::clone(&cancel);
        let worker_fn = Arc::clone(&worker_fn);
        let state = state.clone();
        let tf = Arc::clone(&total_files);
        let td = Arc::clone(&total_dirs);
        let tb = Arc::clone(&total_bytes);
        handles.push(thread::spawn(move || {
            loop {
                let path = {
                    let lock = rx.lock().unwrap();
                    match lock.recv() {
                        Ok(p) => p,
                        Err(_) => break,
                    }
                };
                let (f, d, b) = worker_fn(&path, &cancel, &state);
                tf.fetch_add(f, Ordering::Relaxed);
                td.fetch_add(d, Ordering::Relaxed);
                tb.fetch_add(b, Ordering::Relaxed);
            }
        }));
    }

    for dir in dirs {
        let _ = tx.send(dir.clone());
    }
    drop(tx);

    for h in handles {
        let _ = h.join();
    }

    let elapsed = start.elapsed();

    BenchResult {
        files: total_files.load(Ordering::Relaxed),
        dirs: total_dirs.load(Ordering::Relaxed),
        bytes: total_bytes.load(Ordering::Relaxed),
        elapsed,
    }
}
