//! Colour palette for the TUI.
//!
//! Raw 16-colour ANSI names (`Color::Gray`, `Color::DarkGray`, …) render
//! inconsistently across terminals and read as harsh on a dark background.
//! A [`Palette`] is a set of softer truecolor values, and gives the sidebar
//! its quiet, herdr-style look: dim section headers, a faint separator rule,
//! and a filled highlight for the selected row.
//!
//! Three flavours are available via config (`tui.theme`):
//!   * `mocha` — Catppuccin Mocha (dark), the default,
//!   * `latte` — Catppuccin Latte (light),
//!   * `auto`  — query the terminal's background colour (OSC 11) and pick
//!     mocha or latte by its luminance.

use ratatui::style::Color;

/// A full set of UI colours. Field names mirror herdr's so the two are easy
/// to cross-reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Primary accent (blue). Active markers, focused separators.
    pub accent: Color,
    /// Selected-row background fill.
    pub surface0: Color,
    /// A slightly brighter fill (drag / secondary highlight).
    pub surface1: Color,
    /// Separator rules and section dividers, and the dim active-row fill.
    pub surface_dim: Color,
    /// Dim labels — section headers, placeholders, secondary text.
    pub overlay0: Color,
    /// Unselected row names.
    pub subtext0: Color,
    /// Selected / active row names.
    pub text: Color,
    /// Branch / worktree accents.
    pub mauve: Color,
    pub green: Color,
    pub yellow: Color,
    pub red: Color,
    pub teal: Color,
    pub peach: Color,
    /// Panel background (used behind toasts so they read as opaque).
    pub panel_bg: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self::mocha()
    }
}

impl Palette {
    /// Catppuccin Mocha — the dark default.
    pub const fn mocha() -> Self {
        Self {
            accent: Color::Rgb(137, 180, 250),
            surface0: Color::Rgb(49, 50, 68),
            surface1: Color::Rgb(69, 71, 90),
            surface_dim: Color::Rgb(30, 30, 46),
            overlay0: Color::Rgb(108, 112, 134),
            subtext0: Color::Rgb(166, 173, 200),
            text: Color::Rgb(205, 214, 244),
            mauve: Color::Rgb(203, 166, 247),
            green: Color::Rgb(166, 227, 161),
            yellow: Color::Rgb(249, 226, 175),
            red: Color::Rgb(243, 139, 168),
            teal: Color::Rgb(148, 226, 213),
            peach: Color::Rgb(250, 179, 135),
            panel_bg: Color::Rgb(24, 24, 37),
        }
    }

    /// Catppuccin Latte — for light terminals.
    pub const fn latte() -> Self {
        Self {
            accent: Color::Rgb(30, 102, 245),
            surface0: Color::Rgb(204, 208, 218),
            surface1: Color::Rgb(188, 192, 204),
            surface_dim: Color::Rgb(220, 224, 232),
            overlay0: Color::Rgb(156, 160, 176),
            subtext0: Color::Rgb(108, 111, 133),
            text: Color::Rgb(76, 79, 105),
            mauve: Color::Rgb(136, 57, 239),
            green: Color::Rgb(64, 160, 43),
            yellow: Color::Rgb(223, 142, 29),
            red: Color::Rgb(210, 15, 57),
            teal: Color::Rgb(23, 146, 153),
            peach: Color::Rgb(254, 100, 11),
            panel_bg: Color::Rgb(239, 241, 245),
        }
    }

    /// Resolve a `tui.theme` config value to a palette. `auto` queries the
    /// terminal background and falls back to mocha if detection fails.
    pub fn resolve(name: &str) -> Self {
        match name {
            "latte" | "light" => Self::latte(),
            "auto" => detect_terminal_background()
                .map(|bg| {
                    if is_dark(bg) {
                        Self::mocha()
                    } else {
                        Self::latte()
                    }
                })
                .unwrap_or_else(Self::mocha),
            _ => Self::mocha(),
        }
    }

    /// Like [`resolve`](Self::resolve) but never queries the terminal — used
    /// when applying a theme change live inside the alt-screen, where the
    /// OSC-11 round-trip would fight the input reader. `auto` previews as
    /// mocha and resolves for real on the next launch.
    pub fn preview(name: &str) -> Self {
        match name {
            "latte" | "light" => Self::latte(),
            _ => Self::mocha(),
        }
    }
}

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

/// Perceived luminance test for an `(r, g, b)` background. Dark backgrounds
/// want the Mocha palette; light ones want Latte.
fn is_dark((r, g, b): (u8, u8, u8)) -> bool {
    // Rec. 601 luma, 0..255. Midpoint split.
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    luma < 128.0
}

/// Query the terminal for its default background colour via OSC 11 and parse
/// the `rgb:RRRR/GGGG/BBBB` reply. Returns `None` if the terminal doesn't
/// answer within a short window (e.g. it's not a TTY, or is piped).
///
/// Must be called *before* the alt screen is entered. We flip raw mode on
/// briefly ourselves so the reply isn't line-buffered, then restore it.
fn detect_terminal_background() -> Option<(u8, u8, u8)> {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return None;
    }

    // Raw mode so the reply arrives byte-by-byte rather than after Enter.
    crossterm::terminal::enable_raw_mode().ok()?;

    let mut out = std::io::stdout();
    if out.write_all(b"\x1b]11;?\x07").is_err() || out.flush().is_err() {
        let _ = crossterm::terminal::disable_raw_mode();
        return None;
    }

    // Read the reply with a deadline. OSC replies look like
    // `\x1b]11;rgb:rrrr/gggg/bbbb\x07` (or ST-terminated `\x1b\\`).
    let deadline = Instant::now() + Duration::from_millis(120);
    let mut buf = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    let mut stdin = std::io::stdin();
    while Instant::now() < deadline {
        match stdin.read(&mut byte) {
            Ok(1) => {
                buf.push(byte[0]);
                // Stop at a terminator once we've seen the payload.
                if byte[0] == 0x07 || (byte[0] == b'\\' && buf.len() > 2) {
                    break;
                }
            }
            _ => break,
        }
        if buf.len() > 64 {
            break;
        }
    }
    let _ = crossterm::terminal::disable_raw_mode();

    parse_osc_background(&String::from_utf8_lossy(&buf))
}

/// Parse the `rgb:rrrr/gggg/bbbb` (or `#rrggbb`) value out of an OSC 11
/// reply. Components may be 1–4 hex digits each; we scale to 8-bit.
fn parse_osc_background(reply: &str) -> Option<(u8, u8, u8)> {
    let idx = reply.find("rgb:")?;
    let rest = &reply[idx + 4..];
    let end = rest.find(['\x07', '\x1b']).unwrap_or(rest.len());
    let body = &rest[..end];
    let mut parts = body.split('/');
    let r = scale_hex(parts.next()?)?;
    let g = scale_hex(parts.next()?)?;
    let b = scale_hex(parts.next()?)?;
    Some((r, g, b))
}

fn scale_hex(component: &str) -> Option<u8> {
    if component.is_empty()
        || component.len() > 4
        || !component.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    let value = u32::from_str_radix(component, 16).ok()?;
    let max = (1u32 << (component.len() * 4)) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_appends_ellipsis() {
        assert_eq!(truncate("feature/long-branch", 10), "feature/l…");
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn parses_osc_background_reply() {
        assert_eq!(
            parse_osc_background("\x1b]11;rgb:1e1e/1e1e/2e2e\x07"),
            Some((0x1e, 0x1e, 0x2e))
        );
        assert_eq!(
            parse_osc_background("\x1b]11;rgb:ffff/ffff/ffff\x1b\\"),
            Some((255, 255, 255))
        );
    }

    #[test]
    fn luminance_split_picks_theme() {
        assert!(is_dark((30, 30, 46))); // mocha base
        assert!(!is_dark((239, 241, 245))); // latte base
    }

    #[test]
    fn resolve_falls_back_to_mocha() {
        assert_eq!(Palette::resolve("nonsense"), Palette::mocha());
        assert_eq!(Palette::resolve("latte"), Palette::latte());
    }
}
