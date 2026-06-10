//! `devme-ui` — the single source of truth for devme's terminal output
//! (ADR-0017). Every user-facing surface renders through this crate so the
//! whole CLI speaks one visual language and honors one set of rules:
//!
//! - **stdout is the data contract, stderr is commentary** (ADR-0008).
//!   Tables, URLs, JSON, and log lines go to stdout; progress, narration,
//!   warnings, and errors go to stderr via the one-liner helpers here.
//! - **One-liners** are `devme: <msg>` (optionally scoped:
//!   `devme remote: <msg>`). `info`/`success`/`hint` are quiet-gated;
//!   `warn`/`error` always print.
//! - **Sections** are the clack-style tree (`◆ │ ◇ └`) — the only
//!   multi-line progress style. Items are `◇` done / `⚠` attention /
//!   `✗` failed, with `↳` hint continuations.
//! - **Tables and standalone summaries** use `✔ ⚠ ✗` plus the service-state
//!   dots (`● ◐ ◌ ○ ↻`).
//! - **Color** is resolved once per stream (flag → `NO_COLOR` →
//!   `FORCE_COLOR`/`CLICOLOR_FORCE` → is-a-tty) and carried as a [`Style`];
//!   renderers never probe the environment themselves.
//! - **JSON** goes through [`json`]: pretty-printed, stdout. Streaming
//!   output stays NDJSON at the call site.

use std::io::{IsTerminal, Write};
use std::sync::OnceLock;

/// ANSI escape codes. Always pair with [`Style::paint`] (or a `Style`
/// convenience wrapper) so color stays gated in one place.
pub mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BLUE: &str = "\x1b[34m";
    pub const CYAN: &str = "\x1b[36m";
    pub const BRIGHT_RED: &str = "\x1b[91m";
}

/// The devme glyph vocabulary. Two registers, on purpose:
///
/// - **Trees** ([`Section`]) use `DONE`/`WARN`/`FAIL` items under a
///   `SECTION` header with `BAR`/`BAR_END` structure.
/// - **Tables and one-line summaries** use `OK`/`WARN`/`FAIL` plus the
///   service-state dots, which are all single-cell so columns stay aligned.
///
/// `✓` and `▲` are retired — use `OK` and `WARN`.
pub mod glyph {
    /// Success in tables, checklists, and one-line summaries.
    pub const OK: &str = "✔";
    /// Failure, everywhere.
    pub const FAIL: &str = "✗";
    /// Attention/degraded, everywhere.
    pub const WARN: &str = "⚠";
    /// Hint / fix continuation line.
    pub const HINT: &str = "↳";
    /// List bullet.
    pub const BULLET: &str = "•";

    /// Section header (active step).
    pub const SECTION: &str = "◆";
    /// Completed/neutral item inside a section tree.
    pub const DONE: &str = "◇";
    /// Section gutter bar.
    pub const BAR: &str = "│";
    /// Section footer corner.
    pub const BAR_END: &str = "└";

    /// Service running (healthy).
    pub const RUNNING: &str = "●";
    /// Service starting / running degraded.
    pub const PARTIAL: &str = "◐";
    /// Service waiting on a dependency.
    pub const WAITING: &str = "◌";
    /// Service stopped.
    pub const STOPPED: &str = "○";
    /// Service restarting (also the restart-count marker).
    pub const RESTART: &str = "↻";
    /// External (devme-observed, not devme-owned) service.
    pub const EXTERNAL: &str = "◆";

    /// Selected option in an interactive radio prompt.
    pub const RADIO_ON: &str = "●";
    /// Unselected option in an interactive radio prompt.
    pub const RADIO_OFF: &str = "○";
}

/// Per-stream color decision, resolved once and passed down. Renderers take
/// a `Style` instead of probing flags/env/tty themselves, which is what lets
/// `--no-color` reach every surface (the supervisor's preflight tree used to
/// miss it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
}

impl Style {
    /// No color — for pipes and tests.
    pub const PLAIN: Style = Style { color: false };
    /// Forced color — for tests of the colored path.
    pub const COLOR: Style = Style { color: true };

    /// Wrap `s` in `code` + reset, or pass it through untouched. Apply
    /// padding *before* painting so escape bytes don't skew columns.
    pub fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("{code}{s}{}", ansi::RESET)
        } else {
            s.to_string()
        }
    }

    pub fn bold(&self, s: &str) -> String {
        self.paint(ansi::BOLD, s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint(ansi::DIM, s)
    }
    pub fn ok(&self, s: &str) -> String {
        self.paint(ansi::GREEN, s)
    }
    pub fn warn(&self, s: &str) -> String {
        self.paint(ansi::YELLOW, s)
    }
    pub fn err(&self, s: &str) -> String {
        self.paint(ansi::RED, s)
    }
    pub fn accent(&self, s: &str) -> String {
        self.paint(ansi::CYAN, s)
    }
}

/// Resolve whether a stream should get ANSI color. Precedence: an explicit
/// `--no-color` wins, then `NO_COLOR` (https://no-color.org), then
/// `FORCE_COLOR`/`CLICOLOR_FORCE` (opt back *in* when piped), then the
/// stream's own tty-ness — stderr styling must not key off stdout's.
fn detect_color(no_color_flag: bool, is_tty: bool) -> bool {
    if no_color_flag || std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var_os("FORCE_COLOR").is_some()
        || std::env::var_os("CLICOLOR_FORCE").is_some()
    {
        return true;
    }
    is_tty
}

struct Ui {
    quiet: bool,
    out: Style,
    err: Style,
}

static UI: OnceLock<Ui> = OnceLock::new();

fn ui() -> &'static Ui {
    UI.get_or_init(|| Ui {
        quiet: false,
        out: Style::PLAIN,
        err: Style::PLAIN,
    })
}

/// Resolve quiet + per-stream color once, at process start. Calling it more
/// than once is a no-op (first call wins) so libraries can never re-decide
/// presentation out from under `main`.
pub fn init(quiet: bool, no_color_flag: bool) {
    let _ = UI.set(Ui {
        quiet,
        out: Style {
            color: detect_color(no_color_flag, std::io::stdout().is_terminal()),
        },
        err: Style {
            color: detect_color(no_color_flag, std::io::stderr().is_terminal()),
        },
    });
}

/// Was `-q` passed? For callers with bespoke output that still must gate.
pub fn quiet() -> bool {
    ui().quiet
}

/// Style for stdout (tables, data).
pub fn out_style() -> Style {
    ui().out
}

/// Style for stderr (one-liners, sections rendered to stderr).
pub fn err_style() -> Style {
    ui().err
}

// --- one-liners (stderr) -----------------------------------------------------

/// A message source: `devme` or `devme <scope>`. All one-liners go to
/// stderr — they are commentary, never the command's data.
#[derive(Debug, Clone, Copy)]
pub struct Scope(&'static str);

/// The default `devme:` scope.
pub const fn root() -> Scope {
    Scope("")
}

/// A subcommand scope: `scoped("remote")` → `devme remote: …`.
pub const fn scoped(name: &'static str) -> Scope {
    Scope(name)
}

impl Scope {
    fn prefix(&self) -> String {
        if self.0.is_empty() {
            "devme:".to_string()
        } else {
            format!("devme {}:", self.0)
        }
    }

    fn format(&self, glyph: &str, glyph_color: &str, msg: &str, style: Style) -> String {
        if glyph.is_empty() {
            format!("{} {msg}", self.prefix())
        } else {
            format!("{} {} {msg}", self.prefix(), style.paint(glyph_color, glyph))
        }
    }

    /// Progress/narration. Quiet-gated.
    pub fn info(&self, msg: impl std::fmt::Display) {
        if !ui().quiet {
            eprintln!("{}", self.format("", "", &msg.to_string(), ui().err));
        }
    }

    /// A completed outcome worth a glyph. Quiet-gated.
    pub fn success(&self, msg: impl std::fmt::Display) {
        if !ui().quiet {
            eprintln!(
                "{}",
                self.format(glyph::OK, ansi::GREEN, &msg.to_string(), ui().err)
            );
        }
    }

    /// Something off but not fatal. Always prints — quiet suppresses
    /// information, not warnings.
    pub fn warn(&self, msg: impl std::fmt::Display) {
        eprintln!(
            "{}",
            self.format(glyph::WARN, ansi::YELLOW, &msg.to_string(), ui().err)
        );
    }

    /// A failure. Always prints.
    pub fn error(&self, msg: impl std::fmt::Display) {
        eprintln!(
            "{}",
            self.format(glyph::FAIL, ansi::RED, &msg.to_string(), ui().err)
        );
    }
}

/// `devme: <msg>` — progress/narration, quiet-gated, stderr.
pub fn info(msg: impl std::fmt::Display) {
    root().info(msg);
}

/// `devme: ✔ <msg>` — quiet-gated, stderr.
pub fn success(msg: impl std::fmt::Display) {
    root().success(msg);
}

/// `devme: ⚠ <msg>` — always prints, stderr.
pub fn warn(msg: impl std::fmt::Display) {
    root().warn(msg);
}

/// `devme: ✗ <msg>` — always prints, stderr.
pub fn error(msg: impl std::fmt::Display) {
    root().error(msg);
}

/// `  ↳ <msg>` — a dim follow-up action under the previous line.
/// Quiet-gated, stderr.
pub fn hint(msg: impl std::fmt::Display) {
    if !ui().quiet {
        let style = ui().err;
        eprintln!("  {}", style.dim(&format!("{} {msg}", glyph::HINT)));
    }
}

/// An unprefixed, quiet-gated stderr line — only for annotations woven into
/// another stream (e.g. `[service] Stopped` markers between live log lines),
/// where a `devme:` prefix on every line would drown the stream. Everything
/// else uses [`info`]/[`warn`]/[`error`].
pub fn note(msg: impl std::fmt::Display) {
    if !ui().quiet {
        eprintln!("{msg}");
    }
}

/// Emit a command's JSON result: pretty-printed, stdout. The one JSON style
/// for whole-document output (streaming stays NDJSON at the call site).
pub fn json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    );
}

// --- sections (clack-style tree) ----------------------------------------------

/// The kind of a section item — decides glyph and color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// Done / fine: green `◇`.
    Ok,
    /// Needs attention: yellow `⚠`, label bolded.
    Warn,
    /// Failed: red `✗`, label bolded.
    Fail,
    /// Deliberately not done: dim `◇`, dim label.
    Skip,
}

/// A clack-style tree block — devme's one multi-line progress style:
///
/// ```text
///
///   ◆  Check dependencies
///   │
///   │  ◇ tools
///   │  ⚠ gcloud_adc  not found
///   │    ↳ gcloud auth application-default login
///   │
///   └  All dependencies satisfied
///
/// ```
///
/// Renders into any `Write` (the supervisor's testability pattern), with a
/// [`Style`] supplied by the caller. Interactive flows that must write their
/// own bytes inside the tree (prompts) use [`Section::line`] /
/// [`Section::bar_prefix`].
pub struct Section<'a, W: Write> {
    out: &'a mut W,
    style: Style,
}

impl<'a, W: Write> Section<'a, W> {
    /// Open a section: blank line, `◆ <title>` header, then a bar gutter.
    pub fn begin(out: &'a mut W, style: Style, title: &str) -> std::io::Result<Self> {
        Self::begin_noted(out, style, title, None)
    }

    /// [`Section::begin`] with a dim note after the title
    /// (`◆ Configure environment  2 variables`).
    pub fn begin_noted(
        out: &'a mut W,
        style: Style,
        title: &str,
        note: Option<&str>,
    ) -> std::io::Result<Self> {
        writeln!(out)?;
        let note = match note {
            Some(n) if !n.is_empty() => format!("  {}", style.dim(n)),
            _ => String::new(),
        };
        writeln!(
            out,
            "  {}  {}{note}",
            style.accent(glyph::SECTION),
            style.bold(title)
        )?;
        let mut s = Section { out, style };
        s.gutter()?;
        Ok(s)
    }

    /// An empty `│` spacer row.
    pub fn gutter(&mut self) -> std::io::Result<()> {
        writeln!(self.out, "  {}", self.style.dim(glyph::BAR))
    }

    /// One item row. `detail` rides dim after the label.
    pub fn item(&mut self, kind: Item, label: &str, detail: Option<&str>) -> std::io::Result<()> {
        let (g, label) = match kind {
            Item::Ok => (self.style.ok(glyph::DONE), label.to_string()),
            Item::Warn => (self.style.warn(glyph::WARN), self.style.bold(label)),
            Item::Fail => (self.style.err(glyph::FAIL), self.style.bold(label)),
            Item::Skip => (self.style.dim(glyph::DONE), self.style.dim(label)),
        };
        let detail = match detail {
            Some(d) if !d.is_empty() => format!("  {}", self.style.dim(d)),
            _ => String::new(),
        };
        writeln!(
            self.out,
            "  {}  {g} {label}{detail}",
            self.style.dim(glyph::BAR)
        )
    }

    pub fn ok(&mut self, label: &str) -> std::io::Result<()> {
        self.item(Item::Ok, label, None)
    }

    pub fn warn_item(&mut self, label: &str, detail: Option<&str>) -> std::io::Result<()> {
        self.item(Item::Warn, label, detail)
    }

    pub fn fail(&mut self, label: &str, detail: Option<&str>) -> std::io::Result<()> {
        self.item(Item::Fail, label, detail)
    }

    /// A dim `↳ <fix>` continuation under the previous item.
    pub fn hint(&mut self, msg: &str) -> std::io::Result<()> {
        writeln!(
            self.out,
            "  {}    {}",
            self.style.dim(glyph::BAR),
            self.style.dim(&format!("{} {msg}", glyph::HINT))
        )
    }

    /// A free-form bar-prefixed row — the escape hatch for prompt text and
    /// streamed command output inside the tree.
    pub fn line(&mut self, raw: &str) -> std::io::Result<()> {
        writeln!(self.out, "  {}  {raw}", self.style.dim(glyph::BAR))
    }

    /// A glyphed sub-status under the previous item (`│    ◇ installed`),
    /// for provision outcomes and other per-item progress.
    pub fn sub(&mut self, kind: Item, text: &str) -> std::io::Result<()> {
        let painted = match kind {
            Item::Ok => self.style.ok(&format!("{} {text}", glyph::DONE)),
            Item::Warn => self.style.warn(&format!("{} {text}", glyph::WARN)),
            Item::Fail => self.style.err(&format!("{} {text}", glyph::FAIL)),
            Item::Skip => self.style.dim(&format!("{} {text}", glyph::DONE)),
        };
        writeln!(self.out, "  {}    {painted}", self.style.dim(glyph::BAR))
    }

    /// A wizard field header: accent+bold name, then an optional dim help
    /// line, both at item depth.
    pub fn field(&mut self, name: &str, help: Option<&str>) -> std::io::Result<()> {
        let code = format!("{}{}", ansi::CYAN, ansi::BOLD);
        writeln!(
            self.out,
            "  {}  {}",
            self.style.dim(glyph::BAR),
            self.style.paint(&code, name)
        )?;
        if let Some(h) = help.filter(|h| !h.is_empty()) {
            writeln!(
                self.out,
                "  {}  {}",
                self.style.dim(glyph::BAR),
                self.style.dim(h)
            )?;
        }
        Ok(())
    }

    /// A dim, glyph-less sub-line under the previous item (`│    running…`).
    pub fn sub_note(&mut self, text: &str) -> std::io::Result<()> {
        writeln!(
            self.out,
            "  {}    {}",
            self.style.dim(glyph::BAR),
            self.style.dim(text)
        )
    }

    /// An inline question under the previous item — written without a
    /// trailing newline and flushed, so the caller can read the answer from
    /// its own input. The caller owns echoing/newlines from here.
    pub fn prompt(&mut self, text: &str) -> std::io::Result<()> {
        write!(
            self.out,
            "  {}    {} ",
            self.style.dim(glyph::BAR),
            self.style.dim(text)
        )?;
        self.out.flush()
    }

    /// Terminate an unanswered [`Section::prompt`] line (EOF on input, no
    /// echoed Enter).
    pub fn newline(&mut self) -> std::io::Result<()> {
        writeln!(self.out)
    }

    /// Lend the underlying writer to a nested renderer (an interactive menu
    /// drawn inside the tree). The borrower owns its own bar-prefixing.
    pub fn writer(&mut self) -> &mut W {
        self.out
    }

    /// The `  │  ` prefix, for interactive code that writes without a
    /// trailing newline (inline prompts) and still wants to sit in the tree.
    pub fn bar_prefix(&self) -> String {
        format!("  {}  ", self.style.dim(glyph::BAR))
    }

    /// Close the section: gutter, `└ <summary>`, blank line. The summary is
    /// painted by `kind` (Ok green / Warn yellow / Fail red / Skip dim).
    pub fn end(mut self, kind: Item, summary: &str) -> std::io::Result<()> {
        self.gutter()?;
        let painted = match kind {
            Item::Ok => self.style.ok(summary),
            Item::Warn => self.style.warn(summary),
            Item::Fail => self.style.err(summary),
            Item::Skip => self.style.dim(summary),
        };
        writeln!(self.out, "  {}  {painted}", glyph::BAR_END)?;
        writeln!(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(style: Style, f: impl FnOnce(&mut Section<Vec<u8>>)) -> String {
        let mut buf = Vec::new();
        {
            let mut sec = Section::begin(&mut buf, style, "Title").unwrap();
            f(&mut sec);
            sec.end(Item::Ok, "done").unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn paint_gates_on_color() {
        assert_eq!(Style::COLOR.ok("x"), "\x1b[32mx\x1b[0m");
        assert_eq!(Style::PLAIN.ok("x"), "x");
        assert_eq!(Style::PLAIN.bold("x"), "x");
    }

    #[test]
    fn detect_color_precedence() {
        // The flag always wins; env vars are process-global so only the
        // flag-driven paths are asserted here.
        assert!(!detect_color(true, true));
    }

    #[test]
    fn section_renders_clack_tree_plain() {
        let out = render(Style::PLAIN, |sec| {
            sec.ok("tools").unwrap();
            sec.warn_item("gcloud_adc", Some("not found")).unwrap();
            sec.hint("gcloud auth login").unwrap();
        });
        let expected = "\n  ◆  Title\n  │\n  │  ◇ tools\n  │  ⚠ gcloud_adc  not found\n  │    ↳ gcloud auth login\n  │\n  └  done\n\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn section_colors_only_marks_not_layout() {
        let plain = render(Style::PLAIN, |sec| sec.fail("web", Some("exit 1")).unwrap());
        let color = render(Style::COLOR, |sec| sec.fail("web", Some("exit 1")).unwrap());
        // Stripping ANSI from the colored render gives the plain one.
        let stripped = strip_ansi(&color);
        assert_eq!(stripped, plain);
        assert!(color.contains(ansi::RED));
    }

    #[test]
    fn scope_prefixes() {
        assert_eq!(root().prefix(), "devme:");
        assert_eq!(scoped("remote").prefix(), "devme remote:");
        assert_eq!(
            scoped("remote").format(glyph::WARN, ansi::YELLOW, "sync down", Style::PLAIN),
            "devme remote: ⚠ sync down"
        );
        assert_eq!(
            root().format("", "", "started", Style::PLAIN),
            "devme: started"
        );
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
