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
    env, fs,
    io::{self, stdout},
    path::PathBuf,
    sync::atomic::Ordering,
    sync::mpsc::Receiver,
};

use rdirstat_core::{
    init_logger, list_mounts, format_size, strip_unc_prefix,
    spawn_snapshot_thread, AppState, MountPoint, UiSnapshot,
};

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Explorer,
    Overview,
}

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

struct TuiApp {
    state: AppState,
    dialog: Dialog,
    tab: Tab,
    snapshot: UiSnapshot,
    snapshot_rx: Receiver<UiSnapshot>,
    selected: usize,
    scroll: usize,
    history: Vec<(PathBuf, usize, usize)>,
    frame_count: usize,
    overview_scroll: usize,
}

impl TuiApp {
    fn new(root: &std::path::Path) -> Self {
        let state = AppState::new(root);

        let rx = spawn_snapshot_thread(
            state.scan_state.clone(),
            state.entry_source.clone(),
            state.sort_by_size.clone(),
        );

        TuiApp {
            state,
            dialog: Dialog::None,
            tab: Tab::Explorer,
            snapshot: UiSnapshot::empty(),
            snapshot_rx: rx,
            selected: 0,
            scroll: 0,
            history: Vec::new(),
            frame_count: 0,
            overview_scroll: 0,
        }
    }

    fn poll_snapshot(&mut self) {
        while let Ok(snap) = self.snapshot_rx.try_recv() {
            self.snapshot = snap;
        }
        if !self.snapshot.entries.is_empty() {
            self.selected = self.selected.min(self.snapshot.entries.len() - 1);
        } else {
            self.selected = 0;
        }
    }

    fn move_selection(&mut self, delta: isize, visible_rows: usize) {
        let len = self.snapshot.entries.len();
        if len == 0 {
            return;
        }
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

    fn enter_selected(&mut self) {
        let entries = &self.snapshot.entries;
        if self.selected >= entries.len() {
            return;
        }
        let entry = &entries[self.selected];
        if !entry.is_dir {
            return;
        }
        if entry.is_parent {
            self.go_up();
            return;
        }
        let new_dir = entry.path.clone();
        self.history.push((self.state.current_dir.clone(), self.selected, self.scroll));
        self.selected = 0;
        self.scroll = 0;
        self.state.set_directory(new_dir);
    }

    fn go_up(&mut self) {
        if let Some((dir, sel, scroll)) = self.history.pop() {
            self.selected = sel;
            self.scroll = scroll;
            self.state.set_directory(dir);
        } else if let Some(parent) = self.state.current_dir.parent() {
            let parent = parent.to_path_buf();
            if parent != self.state.current_dir {
                self.selected = 0;
                self.scroll = 0;
                self.state.set_directory(parent);
            }
        }
    }

    fn prompt_delete(&mut self) {
        let entries = &self.snapshot.entries;
        if self.selected >= entries.len() {
            return;
        }
        let entry = &entries[self.selected];
        if entry.is_parent {
            return;
        }
        self.dialog = Dialog::ConfirmDelete {
            name: entry.name.clone(),
            path: entry.path.clone(),
            is_dir: entry.is_dir,
            size: entry.size,
        };
    }

    fn execute_delete(&mut self) {
        let (path, is_dir) = match &self.dialog {
            Dialog::ConfirmDelete { path, is_dir, .. } => (path.clone(), *is_dir),
            _ => return,
        };

        match self.state.execute_delete(&path, is_dir) {
            Ok(()) => {
                self.dialog = Dialog::None;
            }
            Err(message) => {
                self.dialog = Dialog::DeleteResult {
                    message,
                    success: false,
                };
            }
        }
    }

    fn open_drive_picker(&mut self) {
        let mounts = list_mounts();
        let selected = mounts
            .iter()
            .position(|m| self.state.scan_root.starts_with(&m.path))
            .unwrap_or(0);
        self.dialog = Dialog::DrivePicker { mounts, selected };
    }

    fn switch_drive(&mut self, path: PathBuf) {
        self.history.clear();
        self.selected = 0;
        self.scroll = 0;
        self.state.switch_drive(path);
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

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, root: &std::path::Path) -> io::Result<()> {
    let mut app = TuiApp::new(root);
    let mut visible_rows: usize = 10;

    loop {
        app.frame_count = app.frame_count.wrapping_add(1);
        app.poll_snapshot();

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
                                app.dialog = Dialog::None;
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

                // Tab switching
                if key.code == KeyCode::Tab {
                    app.tab = match app.tab {
                        Tab::Explorer => Tab::Overview,
                        Tab::Overview => Tab::Explorer,
                    };
                    continue;
                }

                // Global keys
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('r') => {
                        if app.snapshot.scanning {
                            app.state.stop_scan();
                        } else {
                            app.state.scan();
                        }
                    }
                    KeyCode::Char('g') => app.open_drive_picker(),
                    _ => {}
                }

                // Tab-specific keys
                match app.tab {
                    Tab::Explorer => match key.code {
                        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1, visible_rows),
                        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1, visible_rows),
                        KeyCode::PageDown => app.move_selection(visible_rows as isize, visible_rows),
                        KeyCode::PageUp => app.move_selection(-(visible_rows as isize), visible_rows),
                        KeyCode::Home => {
                            app.selected = 0;
                            app.scroll = 0;
                        }
                        KeyCode::End => {
                            if !app.snapshot.entries.is_empty() {
                                app.selected = app.snapshot.entries.len() - 1;
                            }
                        }
                        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => app.enter_selected(),
                        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => app.go_up(),
                        KeyCode::Char('s') => app.state.toggle_sort(),
                        KeyCode::Char('d') | KeyCode::Delete => app.prompt_delete(),
                        _ => {}
                    },
                    Tab::Overview => match key.code {
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.overview_scroll = app.overview_scroll.saturating_add(1);
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.overview_scroll = app.overview_scroll.saturating_sub(1);
                        }
                        KeyCode::PageDown => {
                            app.overview_scroll = app.overview_scroll.saturating_add(visible_rows);
                        }
                        KeyCode::PageUp => {
                            app.overview_scroll = app.overview_scroll.saturating_sub(visible_rows);
                        }
                        KeyCode::Home => { app.overview_scroll = 0; }
                        _ => {}
                    },
                }
            }
        }
    }
    Ok(())
}

// ── Drawing ──────────────────────────────────────────────────────────────────

fn draw_ui(f: &mut Frame, app: &TuiApp) -> usize {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    let visible = match app.tab {
        Tab::Explorer => draw_file_list(f, app, chunks[1]),
        Tab::Overview => draw_overview(f, app, chunks[1]),
    };
    draw_help(f, app, chunks[2]);

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

fn draw_header(f: &mut Frame, app: &TuiApp, area: Rect) {
    let snap = &app.snapshot;
    let total = format_size(snap.total_entry_size);
    let count = snap.entries.len();
    let sort_label = if app.state.sort_by_size.load(Ordering::Relaxed) { "size" } else { "name" };
    let scanning = if snap.scanning {
        let rate = snap.scan_start.map(|start| {
            let secs = start.elapsed().as_secs_f64();
            if secs > 0.1 { (snap.files_scanned as f64 / secs * 60.0) as u64 } else { 0 }
        }).unwrap_or(0);
        format!(" [scanning... {} files, {}/min]", snap.files_scanned, rate)
    } else {
        String::new()
    };

    let tab_label = match app.tab {
        Tab::Explorer => "Explorer",
        Tab::Overview => "Overview",
    };

    let title = format!(
        " [{}] {} | {} items | {} total | sort: {}{} ",
        tab_label,
        app.state.current_dir.display(),
        count,
        total,
        sort_label,
        scanning
    );
    let header = Paragraph::new(title).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" rdirstat ")
            .style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(header, area);
}

const SPINNER: &[char] = &['|', '/', '-', '\\'];

fn draw_file_list(f: &mut Frame, app: &TuiApp, area: Rect) -> usize {
    let inner_height = area.height.saturating_sub(2) as usize;
    let snap = &app.snapshot;

    if snap.entries.is_empty() {
        let empty = Paragraph::new("  (empty directory)").block(
            Block::default()
                .borders(Borders::ALL)
                .style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(empty, area);
        return inner_height;
    }

    let total_size = snap.total_entry_size.max(1);
    let col_width = (area.width.saturating_sub(2)) as usize;

    let lines: Vec<Line> = snap
        .entries
        .iter()
        .enumerate()
        .skip(app.scroll)
        .take(inner_height)
        .map(|(idx, entry)| {
            let is_selected = idx == app.selected;

            let icon = if entry.is_dir { "/" } else { " " };

            let size_str = format!("{:>9}", format_size(entry.size));
            let pct = entry.size as f64 / total_size as f64 * 100.0;
            let pct_str = format!("{:>5.1}%", pct);

            let status = if entry.scanning {
                let spin_char = SPINNER[app.frame_count % SPINNER.len()];
                format!(" {spin_char}")
            } else {
                "  ".to_string()
            };

            let fixed_cols = 1 + 2 + 9 + 1 + 6 + 1;
            let flexible = col_width.saturating_sub(fixed_cols);
            let name_width = flexible * 55 / 100;
            let bar_max = flexible.saturating_sub(name_width);

            let display_name = if entry.name.len() > name_width {
                format!("{}...", &entry.name[..name_width.saturating_sub(3)])
            } else {
                format!("{:<width$}", entry.name, width = name_width)
            };

            let bar_fill = (entry.size as f64 / total_size as f64 * bar_max as f64) as usize;
            let bar: String = "\u{2588}".repeat(bar_fill)
                + &"\u{2591}".repeat(bar_max.saturating_sub(bar_fill));

            let line_text =
                format!("{icon}{display_name}{status} {size_str} {pct_str} {bar}");

            let color = if entry.scanning {
                Color::Yellow
            } else if entry.is_dir {
                Color::Blue
            } else {
                Color::White
            };

            let style = if is_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(if entry.scanning { Color::Yellow } else { Color::Cyan })
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

fn draw_overview(f: &mut Frame, app: &TuiApp, area: Rect) -> usize {
    let inner_height = area.height.saturating_sub(2) as usize;
    let snap = &app.snapshot;
    let col_width = (area.width.saturating_sub(4)) as usize;

    if !app.state.has_scanned() {
        let msg = Paragraph::new("  Press 'r' to start scanning.").block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Overview ")
                .style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(msg, area);
        return inner_height;
    }

    let mut lines: Vec<Line> = Vec::new();

    // ── Summary ──
    let status = if snap.scanning { "Scanning..." } else { "Complete" };
    let status_color = if snap.scanning { Color::Yellow } else { Color::Green };

    lines.push(Line::from(vec![
        Span::styled(" Summary", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(vec![
        Span::raw("  Status:      "),
        Span::styled(status, Style::default().fg(status_color)),
    ]));
    lines.push(Line::from(format!("  Scan root:   {}", app.state.scan_root.display())));
    lines.push(Line::from(vec![
        Span::raw("  Total size:  "),
        Span::styled(format_size(snap.total_bytes), Style::default().add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(format!("  Files:       {}", snap.files_scanned)));
    lines.push(Line::from(format!("  Directories: {}", snap.dirs_scanned)));
    if snap.deepest.1 > 0 {
        let path_str = snap.deepest.0.display().to_string();
        let max_len = col_width.saturating_sub(20);
        let truncated = if path_str.len() > max_len {
            format!("...{}", &path_str[path_str.len() - max_len..])
        } else {
            path_str
        };
        lines.push(Line::from(format!("  Deepest:     {} (depth {})", truncated, snap.deepest.1)));
    }
    lines.push(Line::from(""));

    // ── Biggest Files ──
    lines.push(Line::from(vec![
        Span::styled(" Biggest Files", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    ]));
    if snap.top_files.is_empty() {
        lines.push(Line::from("  No files scanned yet."));
    } else {
        for file in &snap.top_files {
            let name = file.path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| file.path.to_string_lossy().to_string());
            let max_name = col_width.saturating_sub(14);
            let truncated = if name.len() > max_name {
                format!("{}...", &name[..max_name.saturating_sub(3)])
            } else {
                name
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {:>9}", format_size(file.size)), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::raw(format!("  {truncated}")),
            ]));
        }
    }
    lines.push(Line::from(""));

    // ── Biggest Directories ──
    lines.push(Line::from(vec![
        Span::styled(" Biggest Directories", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    ]));
    if snap.top_dirs.is_empty() {
        lines.push(Line::from("  No directories scanned yet."));
    } else {
        for dir in &snap.top_dirs {
            let name = dir.path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| dir.path.to_string_lossy().to_string());
            let max_name = col_width.saturating_sub(14);
            let truncated = if name.len() > max_name {
                format!("{}...", &name[..max_name.saturating_sub(3)])
            } else {
                name
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {:>9}", format_size(dir.size)), Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
                Span::raw(format!("  {truncated}")),
            ]));
        }
    }
    lines.push(Line::from(""));

    // ── Biggest File Types ──
    lines.push(Line::from(vec![
        Span::styled(" Biggest File Types", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    ]));
    if snap.top_exts.is_empty() {
        lines.push(Line::from("  No file type data yet."));
    } else {
        let max_ext_size = snap.top_exts.first().map(|e| e.total_size).unwrap_or(1).max(1);
        // Header
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<8} {:>7} {:>9}  ", "Ext", "Files", "Size"), Style::default().fg(Color::DarkGray)),
        ]));
        for ext in &snap.top_exts {
            let bar_max = col_width.saturating_sub(32).min(30);
            let pct = ext.total_size as f64 / max_ext_size as f64;
            let bar_fill = (pct * bar_max as f64) as usize;
            let bar = "\u{2588}".repeat(bar_fill)
                + &"\u{2591}".repeat(bar_max.saturating_sub(bar_fill));
            lines.push(Line::from(vec![
                Span::raw(format!("  .{:<7} {:>7} ", ext.extension, ext.count)),
                Span::styled(format!("{:>9}", format_size(ext.total_size)), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(bar, Style::default().fg(Color::Cyan)),
            ]));
        }
    }

    // Apply scroll and render
    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(app.overview_scroll)
        .take(inner_height)
        .collect();

    let overview = Paragraph::new(visible_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Overview "),
    );
    f.render_widget(overview, area);

    inner_height
}

fn draw_help(f: &mut Frame, app: &TuiApp, area: Rect) {
    let help_text = match app.tab {
        Tab::Explorer =>
            " q:Quit  j/k:Nav  Enter:Open  Bksp:Back  s:Sort  r:Scan  d:Delete  g:Drives  Tab:Overview",
        Tab::Overview =>
            " q:Quit  j/k:Scroll  r:Scan  g:Drives  Tab:Explorer",
    };
    let help = Paragraph::new(help_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .style(Style::default().fg(Color::DarkGray)),
        );
    f.render_widget(help, area);
}
