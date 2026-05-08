//! Parallel directory scanner and snapshot pipeline behind the
//! [`rdirstat`](https://github.com/AndyGybels/rdirstat) TUI and GUI.
//!
//! `rdirstat-core` walks a filesystem in parallel, accumulates per-directory
//! sizes, and exposes the running state via [`UiSnapshot`]s suitable for
//! consumption by an event-loop-driven UI. Both the terminal frontend
//! ([`rdirstat`](https://crates.io/crates/rdirstat)) and the desktop frontend
//! ([`rdirstat-gui`](https://crates.io/crates/rdirstat-gui)) are built on
//! exactly this engine — no logic is duplicated between them.
//!
//! ## At a glance
//!
//! ```text
//! start_scan() ──► walker thread ──► ScanState (live, atomic + Mutex)
//!                                       │
//!                                       └─► spawn_snapshot_thread()
//!                                              ─► UiSnapshot stream
//!                                                     ─► your UI
//! ```
//!
//! ## Quick start
//!
//! ```no_run
//! use std::path::PathBuf;
//! use std::sync::Arc;
//! use rdirstat_core::{ScanState, start_scan};
//!
//! let state = ScanState::new();
//! start_scan(PathBuf::from("/tmp"), Arc::clone(&state));
//!
//! // Block until the scan finishes (real frontends would poll a snapshot
//! // channel from their event loop instead).
//! while state.is_scanning() {
//!     std::thread::sleep(std::time::Duration::from_millis(100));
//! }
//!
//! println!("scanned {} files, {} bytes",
//!     state.files_scanned(),
//!     state.total_bytes.load(std::sync::atomic::Ordering::Relaxed));
//! ```
//!
//! ## Design properties worth knowing
//!
//! - **Allocated size, not logical size.** [`util::allocated_size`] returns
//!   `m.blocks() * 512` on Unix, matching `du`'s default output. Sparse files
//!   report their real on-disk footprint; tiny files account for cluster
//!   rounding.
//! - **Inode deduplication.** Hardlinks and macOS firmlinks (where `/Users`
//!   and `/System/Volumes/Data/Users` alias the same inodes) are counted
//!   exactly once, regardless of how many paths point at them.
//! - **Honest "complete" status.** [`ScanState::is_completed`] only returns
//!   `true` when every file in a directory's subtree has been counted — not
//!   the moment the walker finishes listing the directory's immediate
//!   children. `dir_sizes[D]` is the final total at the moment `D` is marked
//!   complete.
//! - **Snapshot-driven UI.** Frontends never read [`ScanState`] directly while
//!   rendering. They consume cloned [`UiSnapshot`]s on a 100 ms cadence — see
//!   [`spawn_snapshot_thread`] — which makes the UI loop lock-free.
//!
//! ## Module map
//!
//! | Module | Purpose |
//! |---|---|
//! | [`scan`] | Parallel walker, [`ScanState`], inode dedup, post-order completion |
//! | [`snapshot`] | [`UiSnapshot`] type and the 100 ms snapshot pump |
//! | [`app`] | [`AppState`] — directory navigation + scan controller for frontends |
//! | [`model`] | [`DirEntry`] — one row in a directory listing |
//! | [`mounts`] | [`list_mounts`] for drive-picker UIs |
//! | [`util`] | [`allocated_size`], [`format_size`], [`strip_unc_prefix`] |
//! | [`logging`] | Append-only file logger used by the binaries |

pub mod logging;
pub mod util;
pub mod scan;
pub mod mounts;
pub mod model;
pub mod app;
pub mod snapshot;

pub use logging::{init_logger, log};
pub use util::{allocated_size, format_size, strip_unc_prefix};
pub use scan::{ScanState, SizedEntry, ExtensionStat, start_scan};
pub use mounts::{MountPoint, list_mounts};
pub use model::DirEntry;
pub use app::AppState;
pub use snapshot::{UiSnapshot, EntrySnapshot, spawn_snapshot_thread};
