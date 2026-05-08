#![windows_subsystem = "windows"]

mod theme;

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::sync::mpsc::Receiver;
use std::{env, fs, path::PathBuf};

use rdirstat_core::{
    init_logger, list_mounts, format_size, strip_unc_prefix,
    spawn_snapshot_thread, AppState, MountPoint, UiSnapshot,
};

fn main() -> eframe::Result<()> {
    init_logger();

    let root = env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        let cwd = env::current_dir().unwrap();
        cwd.ancestors().last().unwrap_or(&cwd).to_path_buf()
    });

    let root = strip_unc_prefix(fs::canonicalize(&root).expect("failed to canonicalize root"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "rdirstat",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(GuiApp::new(&root)))
        }),
    )
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Explorer,
    Overview,
}

enum GuiDialog {
    None,
    ConfirmDelete {
        name: String,
        path: PathBuf,
        is_dir: bool,
        size: u64,
    },
    DeleteResult {
        message: String,
    },
    DrivePicker {
        mounts: Vec<MountPoint>,
        selected: usize,
    },
}

struct GuiApp {
    state: AppState,
    dialog: GuiDialog,
    tab: Tab,
    snapshot: UiSnapshot,
    snapshot_rx: Receiver<UiSnapshot>,
    // GUI owns selection/scroll/history — independent of AppState
    selected: usize,
    scroll: usize,
    history: Vec<(PathBuf, usize, usize)>,
}

impl GuiApp {
    fn new(root: &std::path::Path) -> Self {
        let state = AppState::new_idle(root);

        let rx = spawn_snapshot_thread(
            state.scan_state.clone(),
            state.entry_source.clone(),
            state.sort_by_size.clone(),
        );

        GuiApp {
            state,
            dialog: GuiDialog::None,
            tab: Tab::Explorer,
            snapshot: UiSnapshot::empty(),
            snapshot_rx: rx,
            selected: 0,
            scroll: 0,
            history: Vec::new(),
        }
    }

    /// Drain the channel and keep the latest snapshot.
    fn poll_snapshot(&mut self) {
        while let Ok(snap) = self.snapshot_rx.try_recv() {
            self.snapshot = snap;
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

    fn navigate_to(&mut self, path: PathBuf) {
        if path == self.state.current_dir {
            return;
        }
        self.history.clear();
        self.selected = 0;
        self.scroll = 0;
        self.state.set_directory(path);
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
        self.dialog = GuiDialog::ConfirmDelete {
            name: entry.name.clone(),
            path: entry.path.clone(),
            is_dir: entry.is_dir,
            size: entry.size,
        };
    }

    fn open_drive_picker(&mut self) {
        let mounts = list_mounts();
        let selected = mounts
            .iter()
            .position(|m| self.state.scan_root.starts_with(&m.path))
            .unwrap_or(0);
        self.dialog = GuiDialog::DrivePicker { mounts, selected };
    }

    fn switch_drive(&mut self, path: PathBuf) {
        self.history.clear();
        self.selected = 0;
        self.scroll = 0;
        self.state.switch_drive_idle(path);
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_snapshot();

        if self.snapshot.scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Clamp selection to current snapshot size
        if !self.snapshot.entries.is_empty() {
            self.selected = self.selected.min(self.snapshot.entries.len() - 1);
        } else {
            self.selected = 0;
        }

        // Keyboard shortcuts
        let has_dialog = !matches!(self.dialog, GuiDialog::None);

        if !has_dialog {
            let want_quit = ctx.input(|i| {
                i.key_pressed(egui::Key::Q) || i.key_pressed(egui::Key::Escape)
            });
            if want_quit {
                // Signal the walker to stop so it doesn't keep doing I/O for
                // the few ms between our exit and the OS reaping the process.
                self.state.stop_scan();
                // ViewportCommand::Close on the root viewport stalls on macOS:
                // eframe waits for the OS close handshake while wgpu/Metal is
                // still draining frames AND the background scan thread is
                // holding file handles open, producing the "Q locks up" symptom.
                // The TUI just `break`s out and lets the OS reap everything,
                // which is what we want here too. There's no persisted state.
                std::process::exit(0);
            }
        }

        enum KeyAction {
            None, MoveUp, MoveDown, PageUp, PageDown, Home, End,
            Enter, Back, ToggleSort, Scan, Delete, Drives,
            DialogConfirmYes, DialogCancel,
            DrivePickerUp, DrivePickerDown, DrivePickerSelect,
        }

        let key_action = ctx.input(|i| {
            match &self.dialog {
                GuiDialog::ConfirmDelete { .. } => {
                    if i.key_pressed(egui::Key::Y) { KeyAction::DialogConfirmYes }
                    else if i.key_pressed(egui::Key::N) || i.key_pressed(egui::Key::Escape) { KeyAction::DialogCancel }
                    else { KeyAction::None }
                }
                GuiDialog::DeleteResult { .. } => {
                    if i.events.iter().any(|e| matches!(e, egui::Event::Key { pressed: true, .. })) { KeyAction::DialogCancel }
                    else { KeyAction::None }
                }
                GuiDialog::DrivePicker { .. } => {
                    if i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::J) { KeyAction::DrivePickerDown }
                    else if i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::K) { KeyAction::DrivePickerUp }
                    else if i.key_pressed(egui::Key::Enter) { KeyAction::DrivePickerSelect }
                    else if i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::G) { KeyAction::DialogCancel }
                    else { KeyAction::None }
                }
                GuiDialog::None => {
                    if i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::J) { KeyAction::MoveDown }
                    else if i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::K) { KeyAction::MoveUp }
                    else if i.key_pressed(egui::Key::PageDown) { KeyAction::PageDown }
                    else if i.key_pressed(egui::Key::PageUp) { KeyAction::PageUp }
                    else if i.key_pressed(egui::Key::Home) { KeyAction::Home }
                    else if i.key_pressed(egui::Key::End) { KeyAction::End }
                    else if i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::L) { KeyAction::Enter }
                    else if i.key_pressed(egui::Key::Backspace) || i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::H) { KeyAction::Back }
                    else if i.key_pressed(egui::Key::S) { KeyAction::ToggleSort }
                    else if i.key_pressed(egui::Key::R) { KeyAction::Scan }
                    else if i.key_pressed(egui::Key::D) || i.key_pressed(egui::Key::Delete) { KeyAction::Delete }
                    else if i.key_pressed(egui::Key::G) { KeyAction::Drives }
                    else { KeyAction::None }
                }
            }
        });

        let visible_rows = 20;
        match key_action {
            KeyAction::MoveDown => self.move_selection(1, visible_rows),
            KeyAction::MoveUp => self.move_selection(-1, visible_rows),
            KeyAction::PageDown => self.move_selection(visible_rows as isize, visible_rows),
            KeyAction::PageUp => self.move_selection(-(visible_rows as isize), visible_rows),
            KeyAction::Home => { self.selected = 0; self.scroll = 0; }
            KeyAction::End => {
                if !self.snapshot.entries.is_empty() {
                    self.selected = self.snapshot.entries.len() - 1;
                }
            }
            KeyAction::Enter => self.enter_selected(),
            KeyAction::Back => self.go_up(),
            KeyAction::ToggleSort => {
                self.state.toggle_sort();
            }
            KeyAction::Scan => {
                if self.snapshot.scanning {
                    self.state.stop_scan();
                } else {
                    self.state.scan();
                }
            }
            KeyAction::Delete => self.prompt_delete(),
            KeyAction::Drives => self.open_drive_picker(),
            KeyAction::DialogConfirmYes => {
                if let GuiDialog::ConfirmDelete { path, is_dir, .. } = &self.dialog {
                    let path = path.clone();
                    let is_dir = *is_dir;
                    match self.state.execute_delete(&path, is_dir) {
                        Ok(()) => self.dialog = GuiDialog::None,
                        Err(msg) => self.dialog = GuiDialog::DeleteResult { message: msg },
                    }
                }
            }
            KeyAction::DialogCancel => self.dialog = GuiDialog::None,
            KeyAction::DrivePickerUp => {
                if let GuiDialog::DrivePicker { selected, .. } = &mut self.dialog {
                    *selected = selected.saturating_sub(1);
                }
            }
            KeyAction::DrivePickerDown => {
                if let GuiDialog::DrivePicker { selected, mounts } = &mut self.dialog {
                    *selected = (*selected + 1).min(mounts.len() - 1);
                }
            }
            KeyAction::DrivePickerSelect => {
                if let GuiDialog::DrivePicker { mounts, selected } = &self.dialog {
                    let path = mounts[*selected].path.clone();
                    self.dialog = GuiDialog::None;
                    self.switch_drive(path);
                }
            }
            KeyAction::None => {}
        }

        // Top panel: header with breadcrumbs + tabs
        let mut breadcrumb_nav: Option<PathBuf> = None;
        let sort_by_size = self.state.sort_by_size.load(std::sync::atomic::Ordering::Relaxed);
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("rdirstat");
                ui.separator();

                let path = &self.state.current_dir;
                let mut ancestors: Vec<PathBuf> = path.ancestors().map(|a| a.to_path_buf()).collect();
                ancestors.reverse();
                ancestors.retain(|a| !a.as_os_str().is_empty());

                for (i, ancestor) in ancestors.iter().enumerate() {
                    let label = if i == 0 {
                        ancestor.to_string_lossy().to_string()
                    } else {
                        ancestor.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| ancestor.to_string_lossy().to_string())
                    };
                    if i > 0 { ui.label("/"); }
                    if *ancestor == *path {
                        ui.strong(&label);
                    } else if ui.link(&label).clicked() {
                        breadcrumb_nav = Some(ancestor.clone());
                    }
                }

                ui.separator();
                ui.label(format!("{} items", self.snapshot.entries.len()));
                ui.separator();
                ui.label(format!("{} total", format_size(self.snapshot.total_entry_size)));
                if self.snapshot.scanning {
                    ui.separator();
                    ui.spinner();
                    let rate = self.snapshot.scan_start.map(|start| {
                        let secs = start.elapsed().as_secs_f64();
                        if secs > 0.1 { (self.snapshot.files_scanned as f64 / secs * 60.0) as u64 } else { 0 }
                    }).unwrap_or(0);
                    ui.label(format!("{} files scanned ({}/min)", self.snapshot.files_scanned, rate));
                }
            });

            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Explorer, "Explorer");
                ui.selectable_value(&mut self.tab, Tab::Overview, "Overview");
            });
        });

        if let Some(path) = breadcrumb_nav {
            self.navigate_to(path);
        }

        // Bottom panel: toolbar
        egui::TopBottomPanel::bottom("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Back (h)").clicked() { self.go_up(); }
                if ui.button(if sort_by_size { "Sort: Size (s)" } else { "Sort: Name (s)" }).clicked() {
                    self.state.toggle_sort();
                }
                let scan_label = if self.snapshot.scanning { "Stop (r)" } else { "Scan (r)" };
                if ui.button(scan_label).clicked() {
                    if self.snapshot.scanning { self.state.stop_scan(); } else { self.state.scan(); }
                }
                if ui.button("Delete (d)").clicked() { self.prompt_delete(); }
                if ui.button("Drives (g)").clicked() { self.open_drive_picker(); }

                // Right-aligned Quit — same exit path as the Q / Esc shortcut.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Quit (q)").clicked() {
                        self.state.stop_scan();
                        std::process::exit(0);
                    }
                });
            });
        });

        // Central panel
        match self.tab {
            Tab::Explorer => self.draw_explorer(ctx),
            Tab::Overview => self.draw_overview(ctx),
        }

        self.draw_dialogs(ctx);
    }
}

impl GuiApp {
    fn draw_explorer(&mut self, ctx: &egui::Context) {
        let mut action: Option<usize> = None;
        let snap_entries = &self.snapshot.entries;
        let total_size = self.snapshot.total_entry_size.max(1);

        egui::CentralPanel::default().show(ctx, |ui| {
            let row_height = 20.0;
            let num_rows = snap_entries.len();

            let avail_width = ui.available_width();
            let fixed = 20.0 + 70.0 + 50.0;
            let flexible = (avail_width - fixed).max(200.0);
            let name_width = flexible * 0.55;
            let bar_width = flexible * 0.45;

            TableBuilder::new(ui)
                .striped(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(name_width))
                .column(Column::exact(20.0))
                .column(Column::exact(70.0))
                .column(Column::exact(50.0))
                .column(Column::remainder().at_least(bar_width))
                .header(row_height, |mut header| {
                    header.col(|ui| { ui.strong("Name"); });
                    header.col(|_ui| {});
                    header.col(|ui| { ui.strong("Size"); });
                    header.col(|ui| { ui.strong("%"); });
                    header.col(|ui| { ui.strong("Usage"); });
                })
                .body(|body| {
                    body.rows(row_height, num_rows, |mut row| {
                        let idx = row.index();
                        let entry = &snap_entries[idx];
                        let is_selected = idx == self.selected;
                        let pct = entry.size as f64 / total_size as f64;

                        let text_color = if entry.is_parent {
                            theme::FG_MUTED
                        } else if entry.scanning {
                            theme::ACCENT_SCAN
                        } else if entry.is_dir {
                            theme::ACCENT_DIR
                        } else {
                            theme::ACCENT_FILE
                        };

                        let bg = if is_selected {
                            Some(theme::BG_SELECTION)
                        } else {
                            None
                        };

                        // Name
                        row.col(|ui| {
                            if let Some(bg) = bg { ui.painter().rect_filled(ui.max_rect(), 0.0, bg); }
                            let display = if entry.is_parent {
                                "..".to_string()
                            } else {
                                let icon = if entry.is_dir { "/" } else { " " };
                                format!("{icon}{}", entry.name)
                            };
                            let resp = ui.selectable_label(false, egui::RichText::new(display).monospace().color(text_color));
                            if resp.clicked() { self.selected = idx; }
                            if resp.double_clicked() && entry.is_dir { action = Some(idx); }
                        });

                        // Spinner
                        row.col(|ui| {
                            if let Some(bg) = bg { ui.painter().rect_filled(ui.max_rect(), 0.0, bg); }
                            if entry.scanning { ui.spinner(); }
                        });

                        // Size
                        row.col(|ui| {
                            if let Some(bg) = bg { ui.painter().rect_filled(ui.max_rect(), 0.0, bg); }
                            if !entry.is_parent {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(egui::RichText::new(format_size(entry.size)).monospace().color(text_color));
                                });
                            }
                        });

                        // Percent
                        row.col(|ui| {
                            if let Some(bg) = bg { ui.painter().rect_filled(ui.max_rect(), 0.0, bg); }
                            if !entry.is_parent {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(egui::RichText::new(format!("{:.1}%", pct * 100.0)).monospace().color(text_color));
                                });
                            }
                        });

                        // Bar
                        row.col(|ui| {
                            if let Some(bg) = bg { ui.painter().rect_filled(ui.max_rect(), 0.0, bg); }
                            if !entry.is_parent {
                                let avail = ui.available_size();
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(avail.x, (avail.y - 4.0).max(6.0)),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(rect, 2.0, theme::BG_BAR);
                                let fill_w = (rect.width() * pct as f32).max(0.0);
                                if fill_w > 0.5 {
                                    let fill_rect = egui::Rect::from_min_size(rect.min, egui::vec2(fill_w, rect.height()));
                                    // Same hue family as the row's text colour:
                                    // dir-blue, scan-amber, file-neutral.
                                    let bar_color = if entry.scanning {
                                        theme::ACCENT_SCAN
                                    } else if entry.is_dir {
                                        theme::ACCENT_DIR
                                    } else {
                                        theme::ACCENT_FILE
                                    };
                                    ui.painter().rect_filled(fill_rect, 2.0, bar_color);
                                }
                            }
                        });
                    });
                });
        });

        if let Some(idx) = action {
            self.selected = idx;
            self.enter_selected();
        }
    }

    fn draw_overview(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.state.has_scanned() {
                ui.centered_and_justified(|ui| {
                    ui.label("Press 'r' or click Scan to start scanning.");
                });
                return;
            }

            let snap = &self.snapshot;

            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Summary");
                ui.separator();
                egui::Grid::new("summary_grid").num_columns(2).spacing([20.0, 4.0]).show(ui, |ui| {
                    ui.label("Status:");
                    if snap.scanning {
                        ui.horizontal(|ui| { ui.spinner(); ui.label("Scanning..."); });
                    } else {
                        ui.label("Complete");
                    }
                    ui.end_row();

                    ui.label("Scan root:");
                    ui.label(format!("{}", self.state.scan_root.display()));
                    ui.end_row();

                    ui.label("Total size:");
                    ui.strong(format_size(snap.total_bytes));
                    ui.end_row();

                    ui.label("Files:");
                    ui.label(format!("{}", snap.files_scanned));
                    ui.end_row();

                    ui.label("Directories:");
                    ui.label(format!("{}", snap.dirs_scanned));
                    ui.end_row();

                    if snap.deepest.1 > 0 {
                        ui.label("Deepest path:");
                        ui.label(format!("{} (depth {})", snap.deepest.0.display(), snap.deepest.1));
                        ui.end_row();
                    }
                });

                ui.add_space(16.0);

                ui.columns(2, |cols| {
                    cols[0].heading("Biggest Files");
                    cols[0].separator();
                    if snap.top_files.is_empty() {
                        cols[0].label("No files scanned yet.");
                    } else {
                        egui::Grid::new("top_files").num_columns(2).spacing([8.0, 2.0]).show(&mut cols[0], |ui| {
                            for f in &snap.top_files {
                                ui.label(egui::RichText::new(format_size(f.size)).monospace().strong());
                                let name = f.path.file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| f.path.to_string_lossy().to_string());
                                ui.label(&name).on_hover_text(f.path.to_string_lossy());
                                ui.end_row();
                            }
                        });
                    }

                    cols[1].heading("Biggest Directories");
                    cols[1].separator();
                    if snap.top_dirs.is_empty() {
                        cols[1].label("No directories scanned yet.");
                    } else {
                        egui::Grid::new("top_dirs").num_columns(2).spacing([8.0, 2.0]).show(&mut cols[1], |ui| {
                            for d in &snap.top_dirs {
                                ui.label(egui::RichText::new(format_size(d.size)).monospace().strong());
                                let name = d.path.file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| d.path.to_string_lossy().to_string());
                                ui.label(&name).on_hover_text(d.path.to_string_lossy());
                                ui.end_row();
                            }
                        });
                    }
                });

                ui.add_space(16.0);

                ui.heading("Biggest File Types");
                ui.separator();
                if snap.top_exts.is_empty() {
                    ui.label("No file type data yet.");
                } else {
                    let max_ext_size = snap.top_exts.first().map(|e| e.total_size).unwrap_or(1).max(1);
                    egui::Grid::new("ext_stats").num_columns(4).spacing([12.0, 2.0]).show(ui, |ui| {
                        ui.strong("Extension");
                        ui.strong("Files");
                        ui.strong("Total Size");
                        ui.strong("");
                        ui.end_row();

                        for ext in &snap.top_exts {
                            ui.label(format!(".{}", ext.extension));
                            ui.label(format!("{}", ext.count));
                            ui.label(egui::RichText::new(format_size(ext.total_size)).monospace());
                            let pct = ext.total_size as f32 / max_ext_size as f32;
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(150.0, 12.0), egui::Sense::hover());
                            ui.painter().rect_filled(rect, 2.0, theme::BG_BAR);
                            let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * pct, rect.height()));
                            ui.painter().rect_filled(fill, 2.0, theme::ACCENT_HEADING);
                            ui.end_row();
                        }
                    });
                }
            });
        });
    }

    fn draw_dialogs(&mut self, ctx: &egui::Context) {
        let mut close_dialog = false;
        match &self.dialog {
            GuiDialog::ConfirmDelete { name, is_dir, size, .. } => {
                let kind = if *is_dir { "directory" } else { "file" };
                let name = name.clone();
                let size = *size;
                egui::Window::new("Confirm Delete")
                    .collapsible(false).resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label(format!("Delete {kind}: {name}"));
                        ui.label(format!("Size: {}", format_size(size)));
                        ui.colored_label(theme::WARN, "This action cannot be undone!");
                        ui.horizontal(|ui| {
                            if ui.button("Yes, delete (y)").clicked() {
                                if let GuiDialog::ConfirmDelete { path, is_dir, .. } = &self.dialog {
                                    let path = path.clone();
                                    let is_dir = *is_dir;
                                    match self.state.execute_delete(&path, is_dir) {
                                        Ok(()) => close_dialog = true,
                                        Err(msg) => { self.dialog = GuiDialog::DeleteResult { message: msg }; return; }
                                    }
                                }
                            }
                            if ui.button("Cancel (n)").clicked() { close_dialog = true; }
                        });
                    });
            }
            GuiDialog::DeleteResult { message } => {
                let message = message.clone();
                egui::Window::new("Result")
                    .collapsible(false).resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.colored_label(theme::WARN, &message);
                        if ui.button("OK").clicked() { close_dialog = true; }
                    });
            }
            GuiDialog::DrivePicker { mounts, selected } => {
                let mounts_clone: Vec<_> = mounts.clone();
                let sel = *selected;
                egui::Window::new("Select Drive")
                    .collapsible(false).resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        for (i, mount) in mounts_clone.iter().enumerate() {
                            if ui.selectable_label(i == sel, &mount.label).clicked() {
                                let path = mount.path.clone();
                                close_dialog = true;
                                self.switch_drive(path);
                            }
                        }
                        if ui.button("Cancel").clicked() { close_dialog = true; }
                    });
            }
            GuiDialog::None => {}
        }
        if close_dialog { self.dialog = GuiDialog::None; }
    }
}
