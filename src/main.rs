use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, stdout, Write as _},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
};

// ── Logging ──────────────────────────────────────────────────────────────────

struct Logger {
    file: Mutex<fs::File>,
}

impl Logger {
    fn init() -> Arc<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("rdiskstat.log")
            .expect("failed to open log file");
        let logger = Arc::new(Logger {
            file: Mutex::new(file),
        });
        logger.log("--- session start ---");
        logger
    }

    fn log(&self, msg: &str) {
        let timestamp = chrono::Local::now().format("%H:%M:%S%.3f");
        let mut f = self.file.lock().unwrap();
        let _ = writeln!(f, "[{timestamp}] {msg}");
        let _ = f.flush();
    }
}

static mut LOGGER: Option<Arc<Logger>> = None;

fn init_logger() {
    unsafe { LOGGER = Some(Logger::init()) }
}

fn log(msg: &str) {
    unsafe {
        if let Some(ref logger) = LOGGER {
            logger.log(msg);
        }
    }
}

fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        path
    }
}

// ── Global scan state ────────────────────────────────────────────────────────

/// Shared state between the background scanner and the UI.
/// The scanner walks the entire tree from root and accumulates
/// sizes for *every* directory it encounters.
struct ScanState {
    /// Accumulated size per directory (updated live during scan).
    dir_sizes: Mutex<HashMap<PathBuf, u64>>,
    /// Directories whose subtrees have been fully walked.
    completed: Mutex<HashSet<PathBuf>>,
    /// Whether the scan is currently running.
    scanning: AtomicBool,
    /// Total files processed so far.
    files_scanned: AtomicU64,
    /// Set to true to request the scanner to stop.
    cancel: AtomicBool,
}

impl ScanState {
    fn new() -> Arc<Self> {
        Arc::new(ScanState {
            dir_sizes: Mutex::new(HashMap::new()),
            completed: Mutex::new(HashSet::new()),
            scanning: AtomicBool::new(false),
            files_scanned: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
        })
    }

    fn get_size(&self, path: &Path) -> Option<u64> {
        self.dir_sizes.lock().unwrap().get(path).copied()
    }

    fn is_completed(&self, path: &Path) -> bool {
        self.completed.lock().unwrap().contains(path)
    }

    fn is_scanning(&self) -> bool {
        self.scanning.load(Ordering::Relaxed)
    }

    fn files_scanned(&self) -> u64 {
        self.files_scanned.load(Ordering::Relaxed)
    }

    fn clear(&self) {
        self.dir_sizes.lock().unwrap().clear();
        self.completed.lock().unwrap().clear();
        self.files_scanned.store(0, Ordering::Relaxed);
    }
}

/// Walk a single subtree rooted at `subtree_root`, tracking sizes for
/// every directory encountered. Reports into shared `state`.
fn scan_subtree(subtree_root: PathBuf, state: &ScanState, cancel: &AtomicBool) -> u64 {
    let mut dir_stack: Vec<(PathBuf, u64)> = Vec::new();
    let mut local_count = 0u64;
    let mut errors = 0u64;
    let flush_interval = 1000;

    for entry in walkdir::WalkDir::new(&subtree_root).follow_links(false) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        match entry {
            Ok(e) => {
                let depth = e.depth();

                while dir_stack.len() > depth {
                    if let Some((done_dir, size)) = dir_stack.pop() {
                        state.dir_sizes.lock().unwrap().insert(done_dir.clone(), size);
                        state.completed.lock().unwrap().insert(done_dir);
                        if let Some(parent) = dir_stack.last_mut() {
                            parent.1 += size;
                        }
                    }
                }

                if e.file_type().is_dir() {
                    let path = strip_unc_prefix(e.path().to_path_buf());
                    dir_stack.push((path, 0));
                } else if e.file_type().is_file() {
                    let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                    if let Some(parent) = dir_stack.last_mut() {
                        parent.1 += len;
                    }
                    local_count += 1;

                    if local_count % flush_interval == 0 {
                        // Flush in-progress sizes. Each stack entry already
                        // includes completed children (propagated on pop).
                        // But the currently-in-progress descendant chain
                        // hasn't been propagated yet, so we accumulate
                        // bottom-up: each dir's live total is its own size
                        // plus all deeper in-progress descendants.
                        let mut sizes = state.dir_sizes.lock().unwrap();
                        let mut in_progress_below = 0u64;
                        for (dir, size) in dir_stack.iter().rev() {
                            in_progress_below += size;
                            sizes.insert(dir.clone(), in_progress_below);
                        }
                        drop(sizes);
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

    // Flush remaining
    let mut total = 0u64;
    {
        let mut sizes = state.dir_sizes.lock().unwrap();
        let mut completed = state.completed.lock().unwrap();
        while let Some((done_dir, size)) = dir_stack.pop() {
            sizes.insert(done_dir.clone(), size);
            completed.insert(done_dir);
            if let Some(parent) = dir_stack.last_mut() {
                parent.1 += size;
            } else {
                total = size; // this is the subtree root's total
            }
        }
    }
    state.files_scanned.fetch_add(local_count % flush_interval, Ordering::Relaxed);
    if errors > 0 {
        log(&format!("scan: subtree {} done with {errors} errors", subtree_root.display()));
    }
    total
}

/// Start the background scanner. Enumerates top-level children of `root`,
/// then spawns parallel walker threads (one per child, capped by CPU count)
/// via a thread pool. Each walker tracks sizes for every directory in its subtree.
fn start_scan(root: PathBuf, state: Arc<ScanState>) {
    state.cancel.store(false, Ordering::Relaxed);
    state.scanning.store(true, Ordering::Release);
    log(&format!("scan: starting from {}", root.display()));

    thread::spawn(move || {
        // Enumerate top-level entries
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

        // Use a channel-based thread pool to scan children in parallel
        let num_threads = num_cpus::get().max(2);
        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let rx = Arc::new(Mutex::new(rx));

        // Track how many workers are still running
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

        // Feed work
        for dir in child_dirs {
            if state.cancel.load(Ordering::Relaxed) {
                break;
            }
            let _ = tx.send(dir);
        }
        drop(tx); // close channel so workers exit

        // Wait for all workers to finish
        for h in handles {
            let _ = h.join();
        }

        // Compute and store root directory size (sum of all children)
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

// ── App ──────────────────────────────────────────────────────────────────────

// ── Drive/mount listing ──────────────────────────────────────────────────────

#[derive(Clone)]
struct MountPoint {
    path: PathBuf,
    label: String,
}

/// List available drives/mount points for the current platform.
fn list_mounts() -> Vec<MountPoint> {
    #[cfg(target_os = "windows")]
    {
        list_mounts_windows()
    }
    #[cfg(not(target_os = "windows"))]
    {
        list_mounts_unix()
    }
}

#[cfg(target_os = "windows")]
fn list_mounts_windows() -> Vec<MountPoint> {
    let mut mounts = Vec::new();
    // Check drive letters A-Z
    for letter in b'A'..=b'Z' {
        let drive = format!("{}:\\", letter as char);
        let path = PathBuf::from(&drive);
        if path.exists() {
            mounts.push(MountPoint {
                path,
                label: drive,
            });
        }
    }
    mounts
}

#[cfg(not(target_os = "windows"))]
fn list_mounts_unix() -> Vec<MountPoint> {
    let mut mounts = Vec::new();

    // Always include root
    mounts.push(MountPoint {
        path: PathBuf::from("/"),
        label: "/".to_string(),
    });

    // Parse /proc/mounts on Linux, /etc/mtab as fallback
    let mount_files = ["/proc/mounts", "/etc/mtab"];
    for mf in &mount_files {
        if let Ok(content) = fs::read_to_string(mf) {
            for line in content.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let mount_path = parts[1];
                    // Skip virtual filesystems
                    if mount_path == "/"
                        || mount_path.starts_with("/proc")
                        || mount_path.starts_with("/sys")
                        || mount_path.starts_with("/dev")
                        || mount_path.starts_with("/run")
                        || mount_path.starts_with("/snap")
                    {
                        continue;
                    }
                    let path = PathBuf::from(mount_path);
                    if path.exists() {
                        mounts.push(MountPoint {
                            path,
                            label: format!("{} ({})", mount_path, parts[0]),
                        });
                    }
                }
            }
            break; // use first file that works
        }
    }

    // macOS: check /Volumes
    if let Ok(read_dir) = fs::read_dir("/Volumes") {
        for entry in read_dir.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !mounts.iter().any(|m| m.path == path) {
                mounts.push(MountPoint {
                    path,
                    label: format!("/Volumes/{name}"),
                });
            }
        }
    }

    mounts
}

// ── Dialog ───────────────────────────────────────────────────────────────────

enum Dialog {
    None,
    ConfirmDelete {
        name: String,
        path: PathBuf,
        is_dir: bool,
        size: u64,
    },
    DeleteResult {
        message: String,
        success: bool,
    },
    DrivePicker {
        mounts: Vec<MountPoint>,
        selected: usize,
    },
}

struct App {
    current_dir: PathBuf,
    scan_root: PathBuf,
    entries: Vec<DirEntry>,
    selected: usize,
    scroll: usize,
    history: Vec<(PathBuf, usize, usize)>,
    sort_by_size: bool,
    frame_count: usize,
    dialog: Dialog,
    scan_state: Arc<ScanState>,
}

struct DirEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    file_size: u64, // only for files
}

impl DirEntry {
    fn current_size(&self, scan: &ScanState) -> u64 {
        if self.is_dir {
            scan.get_size(&self.path).unwrap_or(0)
        } else {
            self.file_size
        }
    }

    fn is_scanning(&self, scan: &ScanState) -> bool {
        if !self.is_dir {
            return false;
        }
        // A directory is "scanning" if the global scan is running
        // and this directory hasn't been fully walked yet
        scan.is_scanning() && !scan.is_completed(&self.path)
    }
}

impl App {
    fn new(root: &Path) -> Self {
        let scan_state = ScanState::new();
        start_scan(root.to_path_buf(), Arc::clone(&scan_state));

        let mut app = App {
            current_dir: root.to_path_buf(),
            scan_root: root.to_path_buf(),
            entries: Vec::new(),
            selected: 0,
            scroll: 0,
            history: Vec::new(),
            sort_by_size: true,
            frame_count: 0,
            dialog: Dialog::None,
            scan_state,
        };
        app.load_entries();
        app
    }

    /// Read the directory listing (names + types). Sizes come from scan_state.
    fn load_entries(&mut self) {
        log(&format!("load_entries: {}", self.current_dir.display()));
        self.entries.clear();
        let Ok(read_dir) = fs::read_dir(&self.current_dir) else {
            log("load_entries: failed to read dir");
            return;
        };

        for entry in read_dir.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
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
            });
        }

        self.sort_entries();
    }

    fn sort_entries(&mut self) {
        let scan = &self.scan_state;
        if self.sort_by_size {
            self.entries
                .sort_by(|a, b| b.current_size(scan).cmp(&a.current_size(scan)));
        } else {
            self.entries
                .sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        }
    }

    fn total_size(&self) -> u64 {
        self.entries
            .iter()
            .map(|e| e.current_size(&self.scan_state))
            .sum()
    }

    fn enter_dir(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let entry = &self.entries[self.selected];
        if !entry.is_dir {
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

    fn go_up(&mut self) {
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

    fn move_selection(&mut self, delta: isize, visible_rows: usize) {
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

    fn toggle_sort(&mut self) {
        self.sort_by_size = !self.sort_by_size;
        self.sort_entries();
    }

    fn rescan(&mut self) {
        log(&format!("rescan: from {}", self.current_dir.display()));
        // Cancel any in-progress scan
        self.scan_state.cancel.store(true, Ordering::Relaxed);
        // Wait briefly for the scanner to notice
        while self.scan_state.is_scanning() {
            thread::sleep(std::time::Duration::from_millis(10));
        }
        // Clear all data and restart from current directory
        self.scan_state.clear();
        self.scan_root = self.current_dir.clone();
        start_scan(self.scan_root.clone(), Arc::clone(&self.scan_state));
        self.load_entries();
    }

    fn prompt_delete(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let entry = &self.entries[self.selected];
        self.dialog = Dialog::ConfirmDelete {
            name: entry.name.clone(),
            path: entry.path.clone(),
            is_dir: entry.is_dir,
            size: entry.current_size(&self.scan_state),
        };
    }

    fn execute_delete(&mut self) {
        let (path, is_dir) = match &self.dialog {
            Dialog::ConfirmDelete { path, is_dir, .. } => (path.clone(), *is_dir),
            _ => return,
        };

        log(&format!("delete: {}", path.display()));

        let result = if is_dir {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };

        match result {
            Ok(()) => {
                log(&format!("delete: success {}", path.display()));
                self.dialog = Dialog::None;
                self.load_entries();
            }
            Err(e) => {
                log(&format!("delete: error {} -> {e}", path.display()));
                self.dialog = Dialog::DeleteResult {
                    message: format!("Failed to delete: {e}"),
                    success: false,
                };
            }
        }
    }

    fn open_drive_picker(&mut self) {
        let mounts = list_mounts();
        log(&format!("drive picker: {} mounts found", mounts.len()));
        // Try to select the current drive
        let selected = mounts
            .iter()
            .position(|m| self.scan_root.starts_with(&m.path))
            .unwrap_or(0);
        self.dialog = Dialog::DrivePicker { mounts, selected };
    }

    fn switch_drive(&mut self, path: PathBuf) {
        log(&format!("switch_drive: {}", path.display()));
        self.dialog = Dialog::None;

        // Cancel current scan
        self.scan_state.cancel.store(true, Ordering::Relaxed);
        while self.scan_state.is_scanning() {
            thread::sleep(std::time::Duration::from_millis(10));
        }

        // Reset everything for the new root
        self.scan_state.clear();
        self.scan_root = path.clone();
        self.current_dir = path.clone();
        self.history.clear();
        self.selected = 0;
        self.scroll = 0;

        start_scan(self.scan_root.clone(), Arc::clone(&self.scan_state));
        self.load_entries();
    }
}

// ── Main loop ────────────────────────────────────────────────────────────────

fn main() -> io::Result<()> {
    init_logger();

    let root = env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        let cwd = env::current_dir().unwrap();
        cwd.ancestors().last().unwrap_or(&cwd).to_path_buf()
    });

    let root = strip_unc_prefix(fs::canonicalize(&root)?);
    log(&format!("root: {}", root.display()));

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let result = run(&mut terminal, &root);

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    if let Err(e) = result {
        eprintln!("Error: {e}");
    }
    Ok(())
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, root: &Path) -> io::Result<()> {
    let mut app = App::new(root);
    let mut visible_rows: usize = 10;

    loop {
        app.frame_count = app.frame_count.wrapping_add(1);

        // Re-sort while scan is in progress so sizes update live
        if app.scan_state.is_scanning() && app.sort_by_size {
            app.sort_entries();
        }

        terminal.draw(|f| {
            visible_rows = draw_ui(f, &app);
        })?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match &app.dialog {
                    Dialog::ConfirmDelete { .. } => {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => app.execute_delete(),
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                app.dialog = Dialog::None;
                            }
                            _ => {}
                        }
                        continue;
                    }
                    Dialog::DeleteResult { .. } => {
                        app.dialog = Dialog::None;
                        continue;
                    }
                    Dialog::DrivePicker { mounts, selected } => {
                        let sel = *selected;
                        match key.code {
                            KeyCode::Down | KeyCode::Char('j') => {
                                if let Dialog::DrivePicker { selected, mounts } = &mut app.dialog {
                                    *selected = (*selected + 1).min(mounts.len() - 1);
                                }
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if let Dialog::DrivePicker { selected, .. } = &mut app.dialog {
                                    *selected = selected.saturating_sub(1);
                                }
                            }
                            KeyCode::Enter => {
                                let path = mounts[sel].path.clone();
                                app.switch_drive(path);
                            }
                            KeyCode::Esc | KeyCode::Char('g') => {
                                app.dialog = Dialog::None;
                            }
                            _ => {}
                        }
                        continue;
                    }
                    Dialog::None => {}
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Down | KeyCode::Char('j') => app.move_selection(1, visible_rows),
                    KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1, visible_rows),
                    KeyCode::PageDown => app.move_selection(visible_rows as isize, visible_rows),
                    KeyCode::PageUp => app.move_selection(-(visible_rows as isize), visible_rows),
                    KeyCode::Home => {
                        app.selected = 0;
                        app.scroll = 0;
                    }
                    KeyCode::End => {
                        if !app.entries.is_empty() {
                            app.selected = app.entries.len() - 1;
                        }
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => app.enter_dir(),
                    KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => app.go_up(),
                    KeyCode::Char('s') => app.toggle_sort(),
                    KeyCode::Char('r') => app.rescan(),
                    KeyCode::Char('d') | KeyCode::Delete => app.prompt_delete(),
                    KeyCode::Char('g') => app.open_drive_picker(),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

// ── Drawing ──────────────────────────────────────────────────────────────────

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn draw_ui(f: &mut Frame, app: &App) -> usize {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    let visible = draw_file_list(f, app, chunks[1]);
    draw_help(f, chunks[2]);

    match &app.dialog {
        Dialog::ConfirmDelete {
            name,
            is_dir,
            size,
            ..
        } => {
            let kind = if *is_dir { "directory" } else { "file" };
            let lines = vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    format!("  Delete {kind}: {name}"),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )]),
                Line::from(format!("  Size: {}", format_size(*size))),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  This action cannot be undone!",
                    Style::default().fg(Color::Red),
                )]),
                Line::from(""),
                Line::from("  [y] Yes, delete    [n] Cancel"),
                Line::from(""),
            ];
            let height = lines.len() as u16 + 2;
            let dialog = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Confirm Delete ")
                    .style(Style::default().fg(Color::Red)),
            );
            let area = centered_rect(50, height, f.area());
            f.render_widget(ratatui::widgets::Clear, area);
            f.render_widget(dialog, area);
        }
        Dialog::DeleteResult { message, success } => {
            let color = if *success { Color::Green } else { Color::Red };
            let lines = vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    format!("  {message}"),
                    Style::default().fg(color),
                )]),
                Line::from(""),
                Line::from("  Press any key to continue"),
                Line::from(""),
            ];
            let height = lines.len() as u16 + 2;
            let dialog = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Result ")
                    .style(Style::default().fg(color)),
            );
            let area = centered_rect(50, height, f.area());
            f.render_widget(ratatui::widgets::Clear, area);
            f.render_widget(dialog, area);
        }
        Dialog::DrivePicker { mounts, selected } => {
            let mut lines = vec![Line::from("")];
            for (i, mount) in mounts.iter().enumerate() {
                let marker = if i == *selected { "> " } else { "  " };
                let style = if i == *selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                lines.push(Line::from(vec![Span::styled(
                    format!("{marker}{}", mount.label),
                    style,
                )]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                "  j/k:Navigate  Enter:Select  Esc:Cancel",
                Style::default().fg(Color::DarkGray),
            )]));
            lines.push(Line::from(""));

            let height = (lines.len() as u16 + 2).min(f.area().height.saturating_sub(4));
            let dialog = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Select Drive ")
                    .style(Style::default().fg(Color::Cyan)),
            );
            let area = centered_rect(40, height, f.area());
            f.render_widget(ratatui::widgets::Clear, area);
            f.render_widget(dialog, area);
        }
        Dialog::None => {}
    }

    visible
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let popup_width = area.width * percent_x / 100;
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    Rect::new(
        area.x + x,
        area.y + y,
        popup_width.min(area.width),
        height.min(area.height),
    )
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let total = format_size(app.total_size());
    let count = app.entries.len();
    let sort_label = if app.sort_by_size { "size" } else { "name" };
    let scanning = if app.scan_state.is_scanning() {
        let files = app.scan_state.files_scanned();
        format!(" [scanning... {files} files]")
    } else {
        String::new()
    };
    let title = format!(
        " {} | {} items | {} total | sort: {}{} ",
        app.current_dir.display(),
        count,
        total,
        sort_label,
        scanning
    );
    let header = Paragraph::new(title).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" rdiskstat ")
            .style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(header, area);
}

const SPINNER: &[char] = &['|', '/', '-', '\\'];

fn draw_file_list(f: &mut Frame, app: &App, area: Rect) -> usize {
    let inner_height = area.height.saturating_sub(2) as usize;
    if app.entries.is_empty() {
        let empty = Paragraph::new("  (empty directory)").block(
            Block::default()
                .borders(Borders::ALL)
                .style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(empty, area);
        return inner_height;
    }

    let total_size = app.total_size().max(1);
    let col_width = (area.width.saturating_sub(2)) as usize;
    let scan = &app.scan_state;

    let lines: Vec<Line> = app
        .entries
        .iter()
        .enumerate()
        .skip(app.scroll)
        .take(inner_height)
        .map(|(idx, entry)| {
            let is_selected = idx == app.selected;
            let scanning = entry.is_scanning(scan);
            let current = entry.current_size(scan);

            let icon = if entry.is_dir { "/" } else { " " };

            let size_str = format!("{:>9}", format_size(current));
            let pct = current as f64 / total_size as f64 * 100.0;
            let pct_str = format!("{:>5.1}%", pct);

            let status = if scanning {
                let spin_char = SPINNER[app.frame_count % SPINNER.len()];
                format!(" {spin_char}")
            } else {
                "  ".to_string()
            };

            // Layout: icon(1) + name(55%) + status(2) + size(9) + space(1) + pct(6) + space(1) + bar(rest)
            let fixed_cols = 1 + 2 + 9 + 1 + 6 + 1;
            let flexible = col_width.saturating_sub(fixed_cols);
            let name_width = flexible * 55 / 100;
            let bar_max = flexible.saturating_sub(name_width);

            let display_name = if entry.name.len() > name_width {
                format!("{}...", &entry.name[..name_width.saturating_sub(3)])
            } else {
                format!("{:<width$}", entry.name, width = name_width)
            };

            let bar_fill = (current as f64 / total_size as f64 * bar_max as f64) as usize;
            let bar: String = "\u{2588}".repeat(bar_fill)
                + &"\u{2591}".repeat(bar_max.saturating_sub(bar_fill));

            let line_text =
                format!("{icon}{display_name}{status} {size_str} {pct_str} {bar}");

            let color = if scanning {
                Color::Yellow
            } else if entry.is_dir {
                Color::Blue
            } else {
                Color::White
            };

            let style = if is_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(if scanning { Color::Yellow } else { Color::Cyan })
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            };

            Line::from(vec![Span::styled(line_text, style)])
        })
        .collect();

    let list = Paragraph::new(lines).block(Block::default().borders(Borders::ALL));
    f.render_widget(list, area);

    inner_height
}

fn draw_help(f: &mut Frame, area: Rect) {
    let help = Paragraph::new(
        " q:Quit  j/k:Nav  Enter:Open  Bksp:Back  s:Sort  r:Rescan  d:Delete  g:Drives  PgUp/Dn",
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(help, area);
}
