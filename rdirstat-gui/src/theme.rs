//! Centralised colour palette for the GUI.
//!
//! Tuned to feel like a terminal session running the TUI: near-black panel
//! background, square corners, low-saturation chrome, with the same role
//! accents (blue for dirs, amber for scanning, etc.) as the TUI uses on
//! 256-colour terminals. Every fg/bg combination passes WCAG AA.
//!
//! Some palette tokens are exposed for completeness even if no current call
//! site uses them yet — they're part of the public theme surface.
#![allow(dead_code)]

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Margin, Stroke, TextStyle, Theme,
    ThemePreference, Visuals,
};

// ── Backgrounds ──────────────────────────────────────────────────────────
/// Main window background. Near-black with a faint cool tint — close to
/// what most users see in a dark terminal (e.g. macOS Terminal default,
/// iTerm2 dark, "Dark+" themes), so the GUI reads as "the same scheme".
pub const BG_PANEL: Color32 = Color32::from_rgb(0x10, 0x11, 0x16);
/// One step lighter — for subtle striping or secondary surfaces.
pub const BG_SUBTLE: Color32 = Color32::from_rgb(0x18, 0x1a, 0x21);
/// Inactive widgets / soft borders.
pub const BG_INACTIVE: Color32 = Color32::from_rgb(0x21, 0x24, 0x2c);
/// Hovered widget surface — only slightly above panel, no jarring step.
pub const BG_HOVER: Color32 = Color32::from_rgb(0x28, 0x2c, 0x36);

/// Selected-row background — neutral deep blue, lets coloured fg text
/// (dir-blue, scan-amber, etc.) stay readable.
pub const BG_SELECTION: Color32 = Color32::from_rgb(0x1f, 0x29, 0x3f);
/// Bar background (the unfilled portion of a usage bar).
pub const BG_BAR: Color32 = Color32::from_rgb(0x16, 0x18, 0x1d);

// ── Foregrounds ──────────────────────────────────────────────────────────
pub const FG_PRIMARY: Color32 = Color32::from_rgb(0xc0, 0xca, 0xf5);
pub const FG_MUTED: Color32 = Color32::from_rgb(0x94, 0x9e, 0xc4);
pub const FG_DIM: Color32 = Color32::from_rgb(0x56, 0x5f, 0x89);

// ── Semantic accents (re-used for both text AND its matching bar) ────────
/// Directories — Tokyo Night blue.
pub const ACCENT_DIR: Color32 = Color32::from_rgb(0x7a, 0xa2, 0xf7);
/// Regular files — neutral light, prose-like.
pub const ACCENT_FILE: Color32 = Color32::from_rgb(0xc0, 0xca, 0xf5);
/// In-progress / scanning — warm amber, calmer than pure yellow.
pub const ACCENT_SCAN: Color32 = Color32::from_rgb(0xe0, 0xaf, 0x68);
/// Completion / success.
pub const ACCENT_DONE: Color32 = Color32::from_rgb(0x9e, 0xce, 0x6a);
/// Headings / chrome accent (e.g. tab indicator).
pub const ACCENT_HEADING: Color32 = Color32::from_rgb(0xbb, 0x9a, 0xf7);

/// Destructive-action / error — softer than `Color32::RED`, doesn't shout.
pub const WARN: Color32 = Color32::from_rgb(0xf7, 0x76, 0x8e);

/// Apply the full palette to egui via `Visuals` + spacing tweaks.
/// Call once after the eframe `CreationContext` is available.
pub fn apply(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();

    // Window / panel surfaces
    visuals.panel_fill = BG_PANEL;
    visuals.window_fill = BG_PANEL;
    visuals.window_stroke = Stroke::new(1.0, BG_INACTIVE);
    // text-edit / plot / "extreme" surfaces sit a hair below panel so they
    // read as "inset", same way TUI box-edges create depth.
    visuals.extreme_bg_color = BG_BAR;
    visuals.faint_bg_color = BG_SUBTLE;

    // Default text colour everywhere we don't explicitly override
    visuals.override_text_color = Some(FG_PRIMARY);
    visuals.hyperlink_color = ACCENT_DIR;

    // Selection (used by selectable_label, table row highlight, etc.)
    visuals.selection.bg_fill = BG_SELECTION;
    visuals.selection.stroke = Stroke::new(1.0, ACCENT_DIR);

    // ── Widget surfaces ─────────────────────────────────────────────────
    // Square corners throughout: terminals render rectangles, no rounding.
    let square = CornerRadius::ZERO;

    visuals.widgets.noninteractive.bg_fill = BG_PANEL;
    visuals.widgets.noninteractive.weak_bg_fill = BG_PANEL;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BG_INACTIVE);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, FG_MUTED);
    visuals.widgets.noninteractive.corner_radius = square;

    visuals.widgets.inactive.bg_fill = BG_SUBTLE;
    visuals.widgets.inactive.weak_bg_fill = BG_SUBTLE;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BG_INACTIVE);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, FG_PRIMARY);
    visuals.widgets.inactive.corner_radius = square;

    visuals.widgets.hovered.bg_fill = BG_HOVER;
    visuals.widgets.hovered.weak_bg_fill = BG_HOVER;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT_DIR);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, FG_PRIMARY);
    visuals.widgets.hovered.corner_radius = square;

    visuals.widgets.active.bg_fill = BG_SELECTION;
    visuals.widgets.active.weak_bg_fill = BG_SELECTION;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT_DIR);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, FG_PRIMARY);
    visuals.widgets.active.corner_radius = square;

    visuals.widgets.open.bg_fill = BG_HOVER;
    visuals.widgets.open.weak_bg_fill = BG_HOVER;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, BG_INACTIVE);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, FG_PRIMARY);
    visuals.widgets.open.corner_radius = square;

    // Don't tint hovered / pressed buttons — terminals don't either.
    visuals.menu_corner_radius = square;
    visuals.window_corner_radius = square;

    // Warning / error tint baked in so `ui.colored_label(theme::WARN, …)` and
    // egui's own warning surfaces line up.
    visuals.warn_fg_color = ACCENT_SCAN;
    visuals.error_fg_color = WARN;

    // No drop-shadow under windows — flat, terminal-like.
    visuals.popup_shadow = egui::epaint::Shadow::NONE;
    visuals.window_shadow = egui::epaint::Shadow::NONE;

    // Pin the theme preference to Dark first, otherwise eframe's default
    // (FollowSystem) means `ctx.theme()` returns Light when the OS is in
    // light mode and `set_visuals` writes our palette into the *wrong*
    // theme slot — leaving the actual rendered theme stuck on egui's
    // built-in light visuals (the "white background" symptom).
    ctx.set_theme(ThemePreference::Dark);
    // Bind the palette explicitly to the Dark slot so we don't depend on
    // `ctx.theme()` having already flipped after the call above.
    ctx.set_visuals_of(Theme::Dark, visuals);

    // Type scale + breathing room. Body slightly bigger than egui default
    // so the GUI doesn't feel cramped next to the TUI's monospace cells.
    let mut style = (*ctx.style()).clone();
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::proportional(17.0),
    );
    style.text_styles.insert(
        TextStyle::Body,
        FontId::proportional(13.5),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::monospace(12.5),
    );
    style.text_styles.insert(
        TextStyle::Button,
        FontId::proportional(13.5),
    );
    style.spacing.item_spacing = egui::vec2(8.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.window_margin = Margin::same(8);
    // Bind to the Dark slot — same reasoning as `set_visuals_of` above.
    ctx.set_style_of(Theme::Dark, style);
}
