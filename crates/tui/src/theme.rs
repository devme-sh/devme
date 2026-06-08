//! Colour palette for the TUI — a Catppuccin Mocha subset.
//!
//! Raw 16-colour ANSI names (`Color::Gray`, `Color::DarkGray`, …) render
//! inconsistently across terminals and read as harsh on a dark background.
//! These truecolor values are softer and uniform, and give the sidebar the
//! quiet, herdr-style look: dim section headers, a faint separator rule, and
//! a filled highlight for the selected row instead of an arrow marker.

use ratatui::style::Color;

/// Primary accent (blue). Active stack marker, focused separator.
pub const ACCENT: Color = Color::Rgb(137, 180, 250);
/// Selected-row background fill.
pub const SURFACE0: Color = Color::Rgb(49, 50, 68);
/// Separator rules and section dividers.
pub const SURFACE1: Color = Color::Rgb(69, 71, 90);
/// Active-but-unfocused row background.
pub const SURFACE_DIM: Color = Color::Rgb(30, 30, 46);
/// Dim labels — section headers, placeholders, secondary text.
pub const OVERLAY0: Color = Color::Rgb(108, 112, 134);
/// Unselected row names.
pub const SUBTEXT0: Color = Color::Rgb(166, 173, 200);
/// Selected / active row names.
pub const TEXT: Color = Color::Rgb(205, 214, 244);
/// Branch / worktree accents.
pub const MAUVE: Color = Color::Rgb(203, 166, 247);

pub const GREEN: Color = Color::Rgb(166, 227, 161);
pub const YELLOW: Color = Color::Rgb(249, 226, 175);
pub const RED: Color = Color::Rgb(243, 139, 168);
pub const TEAL: Color = Color::Rgb(148, 226, 213);
pub const PEACH: Color = Color::Rgb(250, 179, 135);

/// Truncate `text` to `max` display columns, appending `…` when clipped.
/// A monospace approximation (one column per char) — good enough for the
/// ASCII branch/service names devme deals in.
pub fn truncate(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        return text.to_string();
    }
    match max {
        0 => String::new(),
        1 => "…".to_string(),
        _ => {
            let prefix: String = text.chars().take(max - 1).collect();
            format!("{prefix}…")
        }
    }
}
