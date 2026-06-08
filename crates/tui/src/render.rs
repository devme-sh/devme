//! Ratatui renderer for [`TuiState`]. Pure layout + styling — no I/O, no
//! event loop. The runtime wires this to a real terminal; tests wire it to
//! a [`ratatui::backend::TestBackend`].
//!
//! Layout (lazygit-inspired, see ADR-0010):
//!
//! ```text
//!  stacks        ╭─ devme v0.1 • 2/3 running ─────────────────────────────╮
//!  ✗ kpi-dash    │  ● db │ ◌ backend │ ✗ proxy                            │
//!  ○ portal      │ ╭─ logs ──────────────────────────────────────────╮   │
//!  ○ worker      │ │ 12:34:01 listening on :8080                      │   │
//!                │ │ 12:34:02 GET /api/health 200                     │   │
//!  ─────────     │ │ ...                                              │   │
//!  tools         │ ╰──────────────────────────────────────────────────╯   │
//!  ✓ gcloud      │  backend · starting · pid 12345 · 0 restarts          │
//!  · uv          ╰────────────────────────────────────────────────────────╯
//!  ? help  hl svc  jk stack  S/s/r start/stop/restart  o open  q quit
//! ```
//!
//! The sidebar is borderless, herdr-style: a dim section header and a filled
//! highlight for the selected row, with the main pane's own left border
//! acting as the divider. Each stack carries an aggregate status dot. The top
//! section is stacks (worktrees), the bottom the dependency checks ("tools" —
//! uv, gcloud, …) that gate startup; those are repo-level, so they persist
//! across stack switches. Services live in the tabs row of the main pane, not
//! the sidebar (which would duplicate them).

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Tabs, Wrap,
};

use crate::state::TuiState;
use crate::theme;
use devme_core::{ServiceState, StepState};

/// Render `state` into `frame`'s full area.
pub fn render(frame: &mut Frame<'_>, state: &mut TuiState) {
    let area = frame.area();

    if state.copy_mode() {
        render_copy_mode(frame, area, state);
        return;
    }

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let outer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(22), Constraint::Min(0)])
        .split(vertical[0]);

    render_sidebar(frame, outer[0], state);
    render_main(frame, outer[1], state);
    render_footer(frame, vertical[1], state);

    // The stale-skill prompt takes modal priority over help.
    if let Some(dlg) = state.skill_dialog() {
        render_skill_dialog(frame, area, dlg);
    } else if state.help_visible() {
        render_help_overlay(frame, area);
    }
}

/// Modal asking the human to install the AI skill, or to refresh an
/// out-of-date one. Both flavours share the framing and styling.
fn render_skill_dialog(frame: &mut Frame<'_>, area: Rect, dlg: &crate::state::SkillDialog) {
    use crate::state::SkillPrompt;

    let w = 56u16.min(area.width.saturating_sub(4));
    let h = 9u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, modal);

    let title = match dlg.kind {
        SkillPrompt::Install => " AI skill ",
        SkillPrompt::Update => " AI skill update ",
    };
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let key = |k: &'static str| {
        Span::styled(
            format!(" {k} "),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )
    };
    let desc = |d: String| Span::styled(d, Style::default().fg(Color::Gray));
    let dim = |d: &'static str| Span::styled(d, Style::default().fg(Color::DarkGray));

    let lines: Vec<Line> = match dlg.kind {
        SkillPrompt::Install => vec![
            Line::from(desc(
                "devme ships an AI coding skill that teaches agents".into(),
            )),
            Line::from(desc("to drive it. Install it for this repo?".into())),
            Line::default(),
            Line::from(vec![key("i"), desc(" install (.claude/skills/devme)".into())]),
            Line::from(vec![key("g"), desc(" install globally (~/.claude/...)".into())]),
            Line::from(vec![key("n"), dim(" not now")]),
        ],
        SkillPrompt::Update => {
            let where_ = if dlg.count > 1 {
                format!("{} installs", dlg.count)
            } else {
                "this project".to_string()
            };
            vec![
                Line::from(desc(format!(
                    "devme's AI skill is out of date (v{} \u{2192} v{}).",
                    dlg.from, dlg.to
                ))),
                Line::from(desc(format!("Refresh {where_} from this binary?"))),
                Line::default(),
                Line::from(vec![key("u"), desc(" update now".into())]),
                Line::from(vec![key("a"), desc(" always (auto-update from now on)".into())]),
                Line::from(vec![key("n"), dim(" not now")]),
            ]
        }
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Full-screen log view for copy mode. No borders, sidebar, or tabs —
/// just the log text so terminal-native text selection works cleanly.
fn render_copy_mode(frame: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    let svc_name = state.selected_service().map(|s| s.name.clone());
    let title = svc_name.as_deref().unwrap_or("logs");

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let header_area = layout[0];
    let log_area = layout[1];
    let footer_area = layout[2];

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " COPY MODE ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {title} "),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            "— select text with mouse ",
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    frame.render_widget(header, header_area);

    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(" y ", key),
        Span::styled("copy visible  ", dim),
        Span::styled("Y ", key),
        Span::styled("copy all  ", dim),
        Span::styled("jk ", key),
        Span::styled("scroll  ", dim),
        Span::styled("g/G ", key),
        Span::styled("top/bottom  ", dim),
        Span::styled("Esc ", key),
        Span::styled("exit", dim),
    ]));
    frame.render_widget(footer, footer_area);

    let Some(name) = &svc_name else {
        return;
    };
    let viewport = log_area.height as usize;
    state.set_viewport_height(viewport);
    let offset = state.log_scroll_offset();
    let logs = state.service_logs(name);
    let end = logs.len().saturating_sub(offset);
    let start = end.saturating_sub(viewport);

    let mut text = Text::default();
    for line in logs.iter().skip(start).take(end - start) {
        let parsed = line
            .as_bytes()
            .into_text()
            .unwrap_or_else(|_| Text::raw(line.clone()));
        for parsed_line in parsed.lines {
            text.lines.push(parsed_line);
        }
    }
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), log_area);
}

// ── footer / sidebar ────────────────────────────────────────────────────────

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    // Three-region status bar:
    //   left  — focus breadcrumb (stack > service)
    //   centre — terse key hints (only the most-used)
    //   right — aggregate health summary (counts of each state)
    let dim = Style::default().fg(theme::OVERLAY0);
    let key = Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD);

    let stack = if state.shared_selected() { "shared" } else { state.instance_label() };
    let svc = state.selected_service().map(|s| s.name.as_str()).unwrap_or("—");
    let breadcrumb = format!(" {stack} › {svc} ");
    let left = Paragraph::new(Line::from(vec![
        Span::styled(breadcrumb, Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
    ]));

    let centre_line = if state.show_skill_hint() {
        Line::from(vec![
            Span::styled("hint: ", Style::default().fg(Color::DarkGray)),
            Span::styled("devme skill install", Style::default().fg(Color::Yellow)),
            Span::styled("  (suppress: ", Style::default().fg(Color::DarkGray)),
            Span::styled("devme config set hints.skills false", Style::default().fg(Color::DarkGray)),
            Span::styled(")", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::styled("? ", key),
            Span::styled("help  ", dim),
            Span::styled("hl ", key),
            Span::styled("svc  ", dim),
            Span::styled("jk ", key),
            Span::styled("stack  ", dim),
            Span::styled("S/s/r ", key),
            Span::styled("start/stop/restart  ", dim),
            Span::styled("o ", key),
            Span::styled("open  ", dim),
            Span::styled("q ", key),
            Span::styled("quit", dim),
        ])
    };
    let centre = Paragraph::new(centre_line)
        .alignment(ratatui::layout::Alignment::Center);

    let (running, starting, stopped, failed) = aggregate_states(state);
    let right_spans = vec![
        Span::styled(format!(" ●{running} "), Style::default().fg(theme::GREEN)),
        Span::styled(format!("◌{starting} "), Style::default().fg(theme::YELLOW)),
        Span::styled(format!("○{stopped} "), Style::default().fg(theme::OVERLAY0)),
        Span::styled(format!("✗{failed}"), Style::default().fg(theme::RED)),
        // Right-edge margin so the glyphs don't kiss the terminal border.
        Span::raw("  "),
    ];
    let right = Paragraph::new(Line::from(right_spans))
        .alignment(ratatui::layout::Alignment::Right);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(24),
            Constraint::Min(0),
            Constraint::Length(22),
        ])
        .split(area);
    frame.render_widget(left, cols[0]);
    frame.render_widget(centre, cols[1]);
    frame.render_widget(right, cols[2]);
}

fn aggregate_states(state: &TuiState) -> (usize, usize, usize, usize) {
    let mut running = 0;
    let mut starting = 0;
    let mut stopped = 0;
    let mut failed = 0;
    for s in state.services() {
        match &s.state {
            ServiceState::Running { .. } | ServiceState::External { healthy: true } => {
                running += 1
            }
            ServiceState::Starting
            | ServiceState::Restarting { .. }
            | ServiceState::WaitingOnDependency { .. } => starting += 1,
            ServiceState::Failed { .. } | ServiceState::CrashLoop { .. } => failed += 1,
            ServiceState::Stopped | ServiceState::External { healthy: false } => stopped += 1,
        }
    }
    (running, starting, stopped, failed)
}

fn render_help_overlay(frame: &mut Frame<'_>, area: Rect) {
    // Centered modal, sized to the longest key-binding line. Wide enough
    // to read at a glance, narrow enough that the underlying layout is
    // still partly visible around it.
    let w = 56u16.min(area.width.saturating_sub(4));
    let h = 26u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect { x, y, width: w, height: h };

    // Clear erases anything behind the modal so the overlay text isn't
    // tangled with the layout underneath.
    frame.render_widget(Clear, modal);

    let block = Block::default()
        .title(Span::styled(
            " help ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let key = |k: &'static str| {
        Span::styled(
            format!(" {k:<10}"),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    };
    let desc = |d: &'static str| Span::styled(d, Style::default().fg(Color::Gray));
    let section = |title: &'static str| {
        Line::from(vec![Span::styled(
            title,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )])
    };

    let lines: Vec<Line> = vec![
        section("navigation"),
        Line::from(vec![key("←→ / hl"), desc("service tab")]),
        Line::from(vec![key("↑↓ / jk"), desc("stack")]),
        Line::default(),
        section("log viewport"),
        Line::from(vec![key("b / space"), desc("page up / down")]),
        Line::from(vec![key("J / K"), desc("scroll one line")]),
        Line::from(vec![key("g / G"), desc("top / live tail")]),
        Line::from(vec![key("y / Y"), desc("copy visible / all logs")]),
        Line::from(vec![key("p"), desc("copy debug prompt to clipboard")]),
        Line::from(vec![key("v"), desc("copy mode (select text)")]),
        Line::from(vec![key("wheel"), desc("scroll")]),
        Line::default(),
        section("service actions"),
        Line::from(vec![key("S"), desc("start selected")]),
        Line::from(vec![key("s"), desc("stop selected")]),
        Line::from(vec![key("r"), desc("restart selected")]),
        Line::default(),
        section("session"),
        Line::from(vec![key("D"), desc("detach (keep services running)")]),
        Line::from(vec![key("q / Esc"), desc("quit (stops everything)")]),
        Line::from(vec![key("?"), desc("toggle this overlay")]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_sidebar(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    // Borderless, herdr-style: no boxes. The two sections (stacks + tools)
    // are set off by a dim header label and a horizontal divider, and
    // selection is a filled row rather than an arrow. The main pane's own
    // left border serves as the divider between sidebar and main, so we keep
    // a one-column blank gutter on the right instead of drawing a second
    // rule (which would read as a doubled border).
    //
    // Services are deliberately NOT listed here — they live in the tabs row
    // of the main pane. The sidebar's top section is stacks (worktrees), the
    // bottom is the dependency checks ("tools" — uv, gcloud, …) that gate
    // startup. Steps are repo-level, so they persist across stack switches
    // (see `TuiState::steps`).
    if area.width == 0 || area.height == 0 {
        return;
    }
    let content = Rect { width: area.width - 1, ..area };

    // Tools section = divider row + header row + one row per step. It claims
    // a fixed slice off the bottom; stacks take the rest.
    let step_count = state.steps().len() as u16;
    let tools_height = if step_count == 0 {
        0
    } else {
        (step_count + 2).min(content.height.saturating_sub(2))
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(tools_height)])
        .split(content);

    render_stacks_pane(frame, chunks[0], state);
    if tools_height > 0 {
        render_tools_pane(frame, chunks[1], state);
    }
}

/// A single status dot (glyph + colour) summarising a stack's health.
fn health_dot(health: crate::state::StackHealth) -> (&'static str, Color) {
    use crate::state::StackHealth as H;
    match health {
        H::AllRunning => ("●", theme::GREEN),
        H::SomeRunning => ("◐", theme::YELLOW),
        H::Idle => ("○", theme::OVERLAY0),
        H::Failed => ("✗", theme::RED),
        H::Placeholder => ("○", theme::SURFACE1),
    }
}

/// Header label for a sidebar section — a quiet lowercase tag, herdr-style.
fn section_header(frame: &mut Frame<'_>, area: Rect, label: &str) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {label}"),
            Style::default().fg(theme::OVERLAY0).add_modifier(Modifier::BOLD),
        )),
        Rect { height: 1, ..area },
    );
}

/// Render one sidebar row: a status dot, then a name, optionally filled with
/// a selection background across the full width.
fn render_row(
    frame: &mut Frame<'_>,
    area: Rect,
    dot: (&'static str, Color),
    name: &str,
    selected: bool,
) {
    if area.height == 0 {
        return;
    }
    let row = Rect { height: 1, ..area };
    let name_style = if selected {
        Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::SUBTEXT0)
    };
    // Two leading columns (" ● "), one trailing for breathing room.
    let max_name = (row.width as usize).saturating_sub(4);
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(dot.0, Style::default().fg(dot.1)),
        Span::raw(" "),
        Span::styled(theme::truncate(name, max_name), name_style),
    ]);
    let para = if selected {
        Paragraph::new(line).style(Style::default().bg(theme::SURFACE0))
    } else {
        Paragraph::new(line)
    };
    frame.render_widget(para, row);
}

fn render_stacks_pane(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    if area.height == 0 {
        return;
    }
    section_header(frame, area, "stacks");

    let selected = state.selected_instance_index();
    let shared_active = state.shared_selected();
    let bottom = area.y + area.height;
    let mut y = area.y + 1;

    for (i, label) in state.instances().iter().enumerate() {
        if y >= bottom {
            return;
        }
        let is_selected = !shared_active && selected == Some(i);
        let dot = health_dot(state.instance_health(i));
        render_row(
            frame,
            Rect { y, height: 1, ..area },
            dot,
            label,
            is_selected,
        );
        y += 1;
    }

    // Shared (repo-level) services row, set off by a blank line.
    if !state.shared_services().is_empty() {
        y += 1;
        if y >= bottom {
            return;
        }
        let svc_names: Vec<&str> = state.shared_services().iter().map(|s| s.name.as_str()).collect();
        let label = format!("shared ({})", svc_names.join(", "));
        let dot = health_dot(state.shared_health());
        render_row(frame, Rect { y, height: 1, ..area }, dot, &label, shared_active);
    }
}

fn render_tools_pane(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    if area.height < 2 {
        return;
    }
    // Divider rule across the top, then a dim header, then the steps.
    let divider = "─".repeat(area.width as usize);
    frame.render_widget(
        Paragraph::new(Span::styled(divider, Style::default().fg(theme::SURFACE1))),
        Rect { height: 1, ..area },
    );
    section_header(frame, Rect { y: area.y + 1, height: 1, ..area }, "tools");

    let bottom = area.y + area.height;
    let mut y = area.y + 2;
    for s in state.steps() {
        if y >= bottom {
            return;
        }
        let max_name = (area.width as usize).saturating_sub(4);
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                step_glyph(s.state).to_string(),
                Style::default().fg(step_color(s.state)),
            ),
            Span::raw(" "),
            Span::styled(theme::truncate(s.name.as_str(), max_name), step_text_style(s.state)),
        ]);
        frame.render_widget(Paragraph::new(line), Rect { y, height: 1, ..area });
        y += 1;
    }
}

// ── main pane: tabs + viewport + meta ──────────────────────────────────────

fn render_main(frame: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    let header = format_main_title(state);
    let main_block = Block::default()
        .title(header)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::SURFACE1));
    let inner = main_block.inner(area);
    frame.render_widget(main_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // tabs row + spacer
            Constraint::Min(0),    // log viewport
            Constraint::Length(1), // meta line
        ])
        .split(inner);

    render_tabs(frame, chunks[0], state);
    render_log_viewport(frame, chunks[1], state);
    render_service_meta(frame, chunks[2], state);
}

fn format_main_title(state: &TuiState) -> Line<'_> {
    let count = state.services().len();
    let running = state
        .services()
        .iter()
        .filter(|s| matches!(s.state, ServiceState::Running { .. }))
        .count();
    let failed = state
        .services()
        .iter()
        .filter(|s| {
            matches!(
                s.state,
                ServiceState::Failed { .. } | ServiceState::CrashLoop { .. }
            )
        })
        .count();

    let version = env!("CARGO_PKG_VERSION");
    let mut spans = vec![Span::styled(
        format!(" devme v{version} "),
        Style::default().fg(theme::MAUVE).add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::styled(
        format!("• {running}/{count} running"),
        Style::default().fg(if running == count && count > 0 {
            theme::GREEN
        } else if running > 0 {
            theme::YELLOW
        } else {
            theme::OVERLAY0
        }),
    ));
    if failed > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("• {failed} failed"),
            Style::default().fg(theme::RED),
        ));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn render_tabs(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    if state.services().is_empty() {
        let text = if state.current_instance_is_placeholder() {
            format!(
                "no devme.toml in {} — add one to start services",
                state.current_instance_cwd()
            )
        } else {
            "no services declared in devme.toml".to_string()
        };
        let msg = Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(Color::DarkGray).italic(),
        )));
        frame.render_widget(msg, area);
        return;
    }
    let titles: Vec<Line> = state
        .services()
        .iter()
        .map(|s| {
            Line::from(vec![
                Span::styled(
                    service_dot(&s.state).to_string(),
                    Style::default().fg(service_color(&s.state)),
                ),
                Span::raw(" "),
                Span::styled(s.name.clone(), Style::default()),
            ])
        })
        .collect();
    let selected = state
        .services()
        .iter()
        .position(|s| {
            state
                .selected_service()
                .map(|sel| sel.name == s.name)
                .unwrap_or(false)
        })
        .unwrap_or(0);
    let tabs = Tabs::new(titles)
        .select(selected)
        .highlight_style(
            Style::default()
                .fg(theme::TEXT)
                .bg(theme::SURFACE0)
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::styled(" │ ", Style::default().fg(theme::SURFACE1)))
        .padding(" ", " ");
    frame.render_widget(tabs, area);
}

fn render_log_viewport(frame: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    let (svc_name, svc_state) = match state.selected_service() {
        Some(s) => (s.name.clone(), s.state.clone()),
        None => {
            let msg = Paragraph::new(Line::from(Span::styled(
                "no service selected",
                Style::default().fg(Color::DarkGray).italic(),
            )));
            frame.render_widget(msg, area);
            return;
        }
    };
    if state.service_logs(&svc_name).is_empty() {
        let placeholder = match &svc_state {
            ServiceState::Stopped => "stopped — press S to start",
            ServiceState::Starting => "starting…",
            _ => "no output yet",
        };
        let msg = Paragraph::new(Line::from(Span::styled(
            placeholder,
            Style::default().fg(Color::DarkGray).italic(),
        )));
        frame.render_widget(msg, area);
        return;
    }

    let (text_area, sb_area) = if area.width >= 5 {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        (split[1], Some(split[3]))
    } else if area.width >= 3 {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        (split[0], Some(split[1]))
    } else {
        (area, None)
    };

    let viewport = (text_area.height as usize).max(1);
    state.set_viewport_height(viewport);
    let offset = state.log_scroll_offset();
    let logs = state.service_logs(&svc_name);
    let end = logs.len().saturating_sub(offset);
    let start = end.saturating_sub(viewport);
    let mut text = Text::default();
    for line in logs.iter().skip(start).take(end - start) {
        let parsed = line
            .as_bytes()
            .into_text()
            .unwrap_or_else(|_| Text::raw(line.clone()));
        for parsed_line in parsed.lines {
            text.lines.push(parsed_line);
        }
    }
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), text_area);

    // Scrollbar: track length = full log buffer, thumb position = top of
    // the visible window (i.e., `start`). Ratatui sizes the thumb from the
    // ratio of `viewport / content_length` implicitly through its render.
    if let Some(sb_area) = sb_area {
        let content_length = logs.len();
        let sb_position = if offset == 0 {
            content_length
        } else {
            start
        };
        let mut sb_state = ScrollbarState::new(content_length)
            .position(sb_position)
            .viewport_content_length(viewport);
        let sb = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::default().fg(Color::DarkGray))
            .thumb_style(if offset > 0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Gray)
            })
            // Slim symbols — ratatui defaults to `█`/`║` which are visually
            // heavy for a 1-column gutter. `┃` (heavy vertical) for the
            // thumb pairs with `│` (light vertical) for the track: both
            // characters sit in the cell's centre, so the thumb sliding
            // past the track stays visually aligned column-to-column.
            .thumb_symbol("┃")
            .track_symbol(Some("│"))
            .begin_symbol(None)
            .end_symbol(None);
        frame.render_stateful_widget(sb, sb_area, &mut sb_state);
    }

    // When the user is parked off-tail, show a "PAUSED" pill so it's
    // obvious the viewport is frozen and they're not seeing live output.
    if offset > 0 {
        let indicator = format!(" PAUSED · {offset} behind · G to follow ");
        let style = Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let w = indicator.chars().count() as u16;
        if w <= text_area.width {
            let badge_area = Rect {
                x: text_area.x + text_area.width - w,
                y: text_area.y,
                width: w,
                height: 1,
            };
            frame.render_widget(Paragraph::new(Span::styled(indicator, style)), badge_area);
        }
    }
}

fn render_service_meta(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let svc = match state.selected_service() {
        Some(s) => s,
        None => return,
    };
    let mut spans = vec![Span::styled(
        " ".to_string() + &svc.name,
        Style::default().add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        state_label(&svc.state),
        Style::default()
            .fg(service_color(&svc.state))
            .add_modifier(Modifier::BOLD),
    ));
    if let Some(pid) = svc.pid {
        spans.push(Span::styled("  · pid ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw(pid.to_string()));
    }
    if let Some(port) = svc.port {
        spans.push(Span::styled(
            "  · port ",
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::raw(port.to_string()));
    }
    if svc.restart_count > 0 {
        spans.push(Span::styled(
            "  · restarts ",
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            svc.restart_count.to_string(),
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── style helpers ──────────────────────────────────────────────────────────

fn step_color(state: StepState) -> Color {
    match state {
        StepState::Passed | StepState::SkippedThisRun => theme::GREEN,
        StepState::Overridden => theme::YELLOW,
        StepState::Failed | StepState::ProvisionFailed => theme::RED,
        StepState::Unknown => theme::OVERLAY0,
    }
}

fn step_text_style(state: StepState) -> Style {
    match state {
        StepState::Unknown => Style::default().fg(theme::OVERLAY0),
        _ => Style::default().fg(theme::SUBTEXT0),
    }
}

fn step_glyph(state: StepState) -> &'static str {
    match state {
        StepState::Passed | StepState::SkippedThisRun => "✓",
        StepState::Overridden => "!",
        StepState::Failed | StepState::ProvisionFailed => "✗",
        StepState::Unknown => "·",
    }
}

fn service_dot(state: &ServiceState) -> &'static str {
    use ServiceState as S;
    match state {
        S::Running { degraded: false, .. } => "●",
        S::Running { degraded: true, .. } => "◐",
        S::Starting | S::Restarting { .. } => "◌",
        S::Failed { .. } | S::CrashLoop { .. } => "✗",
        S::External { healthy: true } => "◇",
        S::External { healthy: false } => "✗",
        S::Stopped | S::WaitingOnDependency { .. } => "○",
    }
}

fn service_color(state: &ServiceState) -> Color {
    use ServiceState as S;
    match state {
        S::Running { degraded: false, .. } => theme::GREEN,
        S::Running { degraded: true, .. } => theme::YELLOW,
        S::Starting | S::Restarting { .. } => theme::YELLOW,
        S::Failed { .. } | S::CrashLoop { .. } => theme::RED,
        S::External { healthy: true } => theme::TEAL,
        S::External { healthy: false } => theme::RED,
        S::Stopped | S::WaitingOnDependency { .. } => theme::OVERLAY0,
    }
}

fn state_label(state: &ServiceState) -> &'static str {
    use ServiceState as S;
    match state {
        S::Stopped => "stopped",
        S::Starting => "starting",
        S::Running { degraded: true, .. } => "running (degraded)",
        S::Running { .. } => "running",
        S::WaitingOnDependency { .. } => "waiting on deps",
        S::Restarting { .. } => "restarting",
        S::CrashLoop { .. } => "crash loop",
        S::Failed { .. } => "failed",
        S::External { healthy: true } => "external (healthy)",
        S::External { healthy: false } => "external (unhealthy)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use devme_core::{InstanceInfo, ServerMessage, ServiceSnapshot, ServiceState, StepSnapshot};

    fn test_instance() -> InstanceInfo {
        InstanceInfo {
            id: "test-id".into(),
            label: "test".into(),
            cwd: "/tmp/test".into(),
        }
    }
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn buffer_to_text(buf: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    fn render_to_text(state: &mut TuiState, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, state)).unwrap();
        buffer_to_text(terminal.backend().buffer())
    }

    fn svc(name: &str, state: ServiceState) -> ServiceSnapshot {
        ServiceSnapshot {
            name: name.into(),
            state,
            pid: None,
            port: None,
            restart_count: 0,
        }
    }

    #[test]
    fn tabs_row_has_visual_separators_between_services() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![
                svc("a", ServiceState::Stopped),
                svc("b", ServiceState::Stopped),
            ],
            steps: vec![],
        });
        let text = render_to_text(&mut state, 80, 12);
        assert!(
            text.contains("│"),
            "expected tab divider somewhere:\n{text}"
        );
    }

    #[test]
    fn tabs_row_shows_every_service_name() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![
                svc("db", ServiceState::Stopped),
                svc("backend", ServiceState::Stopped),
                svc("frontend", ServiceState::Stopped),
            ],
            steps: vec![],
        });

        let text = render_to_text(&mut state, 100, 14);
        // All three names must appear on the same row.
        let lines: Vec<&str> = text.lines().collect();
        let tab_line = lines
            .iter()
            .find(|l| l.contains("db") && l.contains("backend") && l.contains("frontend"))
            .unwrap_or_else(|| panic!("no row had all three service names:\n{text}"));
        let i_db = tab_line.find("db").unwrap();
        let i_be = tab_line.find("backend").unwrap();
        let i_fe = tab_line.find("frontend").unwrap();
        assert!(i_db < i_be && i_be < i_fe);
    }

    #[test]
    fn steps_render_in_sidebar_with_status_glyphs() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![],
            steps: vec![
                StepSnapshot {
                    name: "gcloud_auth".into(),
                    state: devme_core::StepState::Passed,
                },
                StepSnapshot {
                    name: "uv".into(),
                    state: devme_core::StepState::Unknown,
                },
                StepSnapshot {
                    name: "redis".into(),
                    state: devme_core::StepState::Failed,
                },
            ],
        });
        let text = render_to_text(&mut state, 80, 14);
        assert!(text.contains("✓"), "passed glyph missing:\n{text}");
        assert!(text.contains("✗"), "failed glyph missing:\n{text}");
        assert!(text.contains("·"), "unknown glyph missing:\n{text}");
        assert!(text.contains("gcloud"), "step name missing:\n{text}");
    }

    #[test]
    fn install_modal_offers_install_keys() {
        use crate::state::SkillPrompt;
        let mut state = TuiState::default();
        state.set_skill_dialog_for_test(SkillPrompt::Install);
        let text = render_to_text(&mut state, 80, 20);
        assert!(text.contains("Install it"), "missing install prompt:\n{text}");
        assert!(text.contains("install globally"), "missing global option:\n{text}");
        assert!(text.contains("not now"), "missing dismiss option:\n{text}");
    }

    #[test]
    fn update_modal_offers_update_keys() {
        use crate::state::SkillPrompt;
        let mut state = TuiState::default();
        state.set_skill_dialog_for_test(SkillPrompt::Update);
        let text = render_to_text(&mut state, 80, 20);
        assert!(text.contains("out of date"), "missing update prompt:\n{text}");
        assert!(text.contains("auto-update"), "missing always option:\n{text}");
    }

    #[test]
    fn footer_lists_basic_key_bindings() {
        let mut state = TuiState::default();
        state.suppress_skill_hint();
        // 140 wide so the centre hints all fit without truncation.
        let text = render_to_text(&mut state, 140, 12);
        let last = text.lines().last().unwrap_or("");
        assert!(last.contains("help"), "footer missing 'help' (was: {last})");
        assert!(last.contains("stack"), "footer missing stack nav (was: {last})");
        assert!(last.contains("svc"), "footer missing svc nav (was: {last})");
        assert!(last.contains("quit"), "footer missing quit (was: {last})");
    }

    #[test]
    fn footer_shows_health_summary_glyphs() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![
                svc("a", ServiceState::Running { degraded: false, started_without: vec![] }),
                svc("b", ServiceState::Running { degraded: false, started_without: vec![] }),
                svc("c", ServiceState::Stopped),
                svc("d", ServiceState::Failed { exit_code: Some(1) }),
            ],
            steps: vec![],
        });
        let text = render_to_text(&mut state, 140, 14);
        let last = text.lines().last().unwrap_or("");
        assert!(last.contains("●2"), "expected 2 running in footer: {last}");
        assert!(last.contains("○1"), "expected 1 stopped in footer: {last}");
        assert!(last.contains("✗1"), "expected 1 failed in footer: {last}");
    }

    #[test]
    fn help_overlay_renders_when_toggled() {
        let mut state = TuiState::default();
        let text = render_to_text(&mut state, 100, 30);
        assert!(!text.contains("toggle this overlay"), "overlay leaked when hidden");
        state.toggle_help();
        let text = render_to_text(&mut state, 100, 30);
        assert!(text.contains("toggle this overlay"), "overlay help text missing");
        state.toggle_help();
        let text = render_to_text(&mut state, 100, 30);
        assert!(!text.contains("toggle this overlay"), "overlay should hide again");
    }

    #[test]
    fn selected_service_meta_shows_state_and_pid_and_port() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![ServiceSnapshot {
                name: "db".into(),
                state: ServiceState::Running {
                    degraded: false,
                    started_without: vec![],
                },
                pid: Some(1234),
                port: Some(5432),
                restart_count: 0,
            }],
            steps: vec![],
        });

        let text = render_to_text(&mut state, 80, 14);
        assert!(text.contains("running"), "expected 'running':\n{text}");
        assert!(text.contains("1234"), "pid missing:\n{text}");
        assert!(text.contains("5432"), "port missing:\n{text}");
    }

    #[test]
    fn log_lines_appear_in_viewport_for_selected_service() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![svc("api", ServiceState::Stopped)],
            steps: vec![],
        });
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
        state.apply(ServerMessage::LogChunk {
            service: "api".into(),
            bytes: enc("listening on :8080"),
            ts: 1,
        });
        state.apply(ServerMessage::LogChunk {
            service: "api".into(),
            bytes: enc("GET /health 200"),
            ts: 2,
        });

        let text = render_to_text(&mut state, 100, 20);
        assert!(text.contains("listening on :8080"), "missing first log line:\n{text}");
        assert!(text.contains("GET /health 200"), "missing second log line:\n{text}");
    }

    #[test]
    fn header_shows_running_count() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![
                svc(
                    "db",
                    ServiceState::Running {
                        degraded: false,
                        started_without: vec![],
                    },
                ),
                svc("api", ServiceState::Stopped),
            ],
            steps: vec![],
        });
        let text = render_to_text(&mut state, 100, 14);
        assert!(text.contains("1/2 running"), "header count missing:\n{text}");
    }

    #[test]
    fn paused_indicator_shows_when_scrolled_off_tail() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![svc("api", ServiceState::Running { degraded: false, started_without: vec![] })],
            steps: vec![],
        });
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
        for i in 0..50 {
            state.apply(ServerMessage::LogChunk {
                service: "api".into(),
                bytes: enc(&format!("line {i}")),
                ts: i,
            });
        }
        // At tail — no PAUSED.
        let text = render_to_text(&mut state, 100, 14);
        assert!(!text.contains("PAUSED"), "PAUSED visible at tail:\n{text}");

        // Scroll up; pill must appear.
        state.log_page_up(10);
        let text = render_to_text(&mut state, 100, 14);
        assert!(text.contains("PAUSED"), "PAUSED missing while scrolled:\n{text}");
        assert!(text.contains("G to follow"), "hint missing:\n{text}");
    }

    #[test]
    fn header_shows_failed_count_when_nonzero() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![
                svc("boom", ServiceState::Failed { exit_code: Some(7) }),
                svc("tick", ServiceState::Running { degraded: false, started_without: vec![] }),
            ],
            steps: vec![],
        });
        let text = render_to_text(&mut state, 100, 14);
        assert!(text.contains("1 failed"), "failed count missing:\n{text}");
    }
}
