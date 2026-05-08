//! Centralised palette / styles for the TUI, mirroring the GUI's Tokyo-Night
//! flavour as closely as a 256-colour terminal palette permits.
//!
//! Indexed colours are used (instead of `Color::Rgb`) because (a) some
//! terminals don't support truecolour and (b) indexed values render
//! correctly under terminals that respect users' palette overrides while
//! still matching our intent on default palettes.
//!
//! Some style tokens are exposed for completeness even if no current call
//! site uses them yet — they're part of the public theme surface.
#![allow(dead_code)]

use ratatui::style::{Color, Modifier, Style};

// ── Foreground/accent colours ────────────────────────────────────────────
/// Default text colour — let the terminal palette decide.
pub const FG_PRIMARY: Color = Color::Reset;
/// Muted secondary text. `Gray` is much more legible than `DarkGray` on
/// modern dark terminals where `DarkGray` ≈ #555 (only ~2.4:1 contrast).
pub const FG_MUTED: Color = Color::Gray;
/// Dim hint text — used very sparingly.
pub const FG_DIM: Color = Color::DarkGray;

/// Directories — Tokyo-Night-ish blue (256-colour cube approximation).
pub const ACCENT_DIR: Color = Color::Indexed(111);
/// Regular files — neutral light, blends with body text.
pub const ACCENT_FILE: Color = Color::Reset;
/// Scanning / in-progress — warm amber, less harsh than `Color::Yellow`.
pub const ACCENT_SCAN: Color = Color::Indexed(215);
/// Completion / success.
pub const ACCENT_DONE: Color = Color::Indexed(149);
/// Headings, tab titles, chrome accents.
pub const ACCENT_HEADING: Color = Color::Indexed(141);
/// Error / destructive action — softer than `Color::Red`.
pub const WARN: Color = Color::Indexed(210);

/// Selection background — neutral deep blue so coloured fg text stays distinct.
pub const BG_SELECTION: Color = Color::Indexed(24);

// ── Composed styles (avoid hand-rolling Style::default().fg(...) at sites) ─
pub fn heading() -> Style {
    Style::default().fg(ACCENT_HEADING).add_modifier(Modifier::BOLD)
}

pub fn dir() -> Style {
    Style::default().fg(ACCENT_DIR)
}

pub fn file() -> Style {
    Style::default().fg(ACCENT_FILE)
}

pub fn scanning() -> Style {
    Style::default().fg(ACCENT_SCAN)
}

pub fn done() -> Style {
    Style::default().fg(ACCENT_DONE)
}

pub fn muted() -> Style {
    Style::default().fg(FG_MUTED)
}

pub fn warn() -> Style {
    Style::default().fg(WARN)
}

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// The selection bar (used for the highlighted row in the explorer).
/// Neutral dark-blue background, normal fg, bold — keeps colour-coded fg
/// (dir-blue, scan-amber) readable instead of forcing them to black.
pub fn selection() -> Style {
    Style::default().bg(BG_SELECTION).add_modifier(Modifier::BOLD)
}

/// The selection bar specifically when the row is mid-scan — bg shifts to
/// amber so the user notices, but fg stays readable.
pub fn selection_scanning() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(ACCENT_SCAN)
        .add_modifier(Modifier::BOLD)
}
