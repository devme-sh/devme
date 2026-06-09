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

use crate::keymap;
use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Wrap,
};

use crate::state::{ClickTarget, TuiState};
use crate::theme::{self, Palette};
use devme_core::{ServiceState, StepState};

/// Render `state` into `frame`'s full area.
pub fn render(frame: &mut Frame<'_>, state: &mut TuiState) {
    let area = frame.area();

    // Rebuild the click hit-map from scratch each frame. Modes that don't draw
    // the sidebar/tabs (copy, zoom) simply leave it empty, so clicks no-op.
    state.begin_frame_hits();

    if state.copy_mode() {
        render_copy_mode(frame, area, state);
        return;
    }

    if state.zoom() {
        render_zoom(frame, area, state);
        return;
    }

    // Parked after an external `devme down`: a full-screen "all stopped" hero
    // instead of the live dashboard. Stays until a `devme up` reattaches.
    if state.stopped() {
        render_stopped(frame, area, state);
        return;
    }

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    // The sidebar can be collapsed (`\``) to give the log pane full width.
    let main_area = if state.sidebar_collapsed() {
        vertical[0]
    } else {
        let outer = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(state.sidebar_width()), Constraint::Min(0)])
            .split(vertical[0]);
        render_sidebar(frame, outer[0], state);
        // The main pane's left border (first column of outer[1]) is the visual
        // divider; record it so a click/drag there resizes the sidebar.
        state.set_sidebar_divider(outer[1].x, vertical[0].y, vertical[0].height, area.width);
        outer[1]
    };
    render_main(frame, main_area, state);
    render_footer(frame, vertical[1], state);

    // Transient corner notifications, above the main pane.
    render_toasts(frame, main_area, state);

    // Modal priority: port conflict > skill prompt > quit confirm > settings
    // > help. A crash-on-bind needs the user's attention before anything else.
    if let Some(dlg) = state.port_conflict() {
        render_port_conflict_dialog(frame, area, dlg);
    } else if let Some(dlg) = state.skill_dialog() {
        render_skill_dialog(frame, area, dlg);
    } else if state.quit_confirm_visible() {
        render_quit_confirm(frame, area, state);
    } else if state.settings_visible() {
        render_settings_overlay(frame, area, state);
    } else if state.stack_info_visible() {
        render_stack_info_overlay(frame, area, state);
    } else if state.notifications_visible() {
        render_notifications_overlay(frame, area, state);
    } else if state.help_visible() {
        render_help_overlay(frame, area);
    }
}

/// Stack-info modal (`i`): the focused stack's identity (branch, worktree
/// path, slot, instance id) and status, one field per row. Copyable rows are
/// selectable; `c`/Enter copy the highlighted one, `b`/`w` jump-copy the
/// branch / path. The status row is info-only (dimmer, skipped by the cursor).
fn render_stack_info_overlay(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let Some(info) = state.stack_info() else {
        return;
    };
    let p = *state.palette();

    // Width fits the longest "  label  value" line *and* the footer hint
    // below (whichever is wider), clamped to the frame.
    let label_w = info.fields.iter().map(|f| f.label.len()).max().unwrap_or(0);
    let widest_field = info
        .fields
        .iter()
        .map(|f| 3 + label_w + 2 + f.value.chars().count())
        .max()
        .unwrap_or(20);
    // Keep in sync with the footer hint rendered at the bottom.
    const FOOTER_W: usize = 46;
    let content_w = widest_field.max(FOOTER_W);
    let w = (content_w as u16 + 2).clamp(28, area.width.saturating_sub(4));
    // Title + fields + divider + footer.
    let h = (info.fields.len() as u16 + 4).min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .title(Span::styled(
            format!(" {} ", info.title),
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(p.accent))
        .style(Style::default().bg(p.panel_bg));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.height < 2 {
        return;
    }

    let mut row = inner.y;
    let bottom = inner.y + inner.height;
    for (i, field) in info.fields.iter().enumerate() {
        if row + 1 >= bottom {
            break;
        }
        let selected = field.copyable && i == info.cursor;
        let fill = selected.then(|| Style::default().bg(p.surface0));
        let label_style = if field.copyable {
            Style::default().fg(p.subtext0)
        } else {
            Style::default().fg(p.overlay0)
        };
        let value_style = if selected {
            Style::default().fg(p.text).add_modifier(Modifier::BOLD)
        } else if field.copyable {
            Style::default().fg(p.text)
        } else {
            Style::default().fg(p.overlay0)
        };
        let marker = if selected { "▸ " } else { "  " };
        let spans = vec![
            Span::styled(marker, Style::default().fg(p.accent)),
            Span::styled(format!("{:<width$}", field.label, width = label_w), label_style),
            Span::raw("  "),
            Span::styled(field.value.clone(), value_style),
        ];
        render_filled(
            frame,
            Rect { x: inner.x, y: row, width: inner.width, height: 1 },
            Line::from(spans),
            fill,
        );
        row += 1;
    }

    // Footer hint, pinned to the last row.
    if bottom > inner.y {
        let hint = Line::from(vec![
            Span::styled(" ↑↓ ", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("move  ", Style::default().fg(p.overlay0)),
            Span::styled("c/⏎ ", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("copy  ", Style::default().fg(p.overlay0)),
            Span::styled("b/w ", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("branch/path  ", Style::default().fg(p.overlay0)),
            Span::styled("esc ", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("close", Style::default().fg(p.overlay0)),
        ]);
        frame.render_widget(
            Paragraph::new(hint),
            Rect { x: inner.x, y: bottom - 1, width: inner.width, height: 1 },
        );
    }
}

/// Notifications-history modal (`n`): the durable scrollback of every toast
/// raised this session, newest first, each with a relative timestamp. The
/// corner toasts auto-expire, so this lets the user catch up on anything they
/// missed — an open/copy result, a crash, a config warning.
fn render_notifications_overlay(frame: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    use crate::state::ToastKind;
    let p = *state.palette();
    let len = state.notifications().len();

    let w = 64u16.min(area.width.saturating_sub(4));
    let h = 18u16.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .title(Span::styled(
            " notifications ",
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(p.accent))
        .style(Style::default().bg(p.panel_bg));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.height == 0 {
        return;
    }

    if len == 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " no notifications yet",
                Style::default().fg(p.subtext0),
            ))),
            inner,
        );
        return;
    }

    // Reserve the last row for a footer; render the rest newest first, with a
    // scroll window derived from the cursor so the selection is always visible.
    let list_rows = (inner.height.saturating_sub(1) as usize).max(1);
    let cursor = state.notif_cursor().min(len - 1);
    let offset = if cursor < list_rows { 0 } else { cursor - list_rows + 1 };

    // Build owned rows (`(line, display_index, selected)`) in one immutable
    // borrow of the history, so the borrow ends before we push click regions.
    let body_budget_base = inner.width as usize;
    let rows: Vec<(Line<'static>, usize, bool)> = {
        let history = state.notifications();
        history
            .iter()
            .rev()
            .enumerate()
            .skip(offset)
            .take(list_rows)
            .map(|(d, toast)| {
                let selected = d == cursor;
                let dot_color = match toast.kind {
                    ToastKind::Failed => p.red,
                    ToastKind::Ready => p.green,
                    ToastKind::Info => p.accent,
                };
                let age = toast.age_label();
                let title = theme::truncate(&toast.title, 12);
                let body_budget =
                    body_budget_base.saturating_sub(title.chars().count() + age.chars().count() + 5);
                let title_style = Style::default().fg(p.text).add_modifier(Modifier::BOLD);
                let line = Line::from(vec![
                    Span::styled("● ", Style::default().fg(dot_color)),
                    Span::styled(format!("{age:>4} "), Style::default().fg(p.overlay0)),
                    Span::styled(title, title_style),
                    Span::styled(
                        format!(" {}", theme::truncate(&toast.body, body_budget)),
                        Style::default().fg(p.subtext0),
                    ),
                ]);
                (line, d, selected)
            })
            .collect()
    };

    // Paint each row into its own rect (full-width highlight for the cursor)
    // and register it as a click-to-copy target.
    for (i, (line, d, selected)) in rows.into_iter().enumerate() {
        let ry = inner.y + i as u16;
        let row = Rect { x: inner.x, y: ry, width: inner.width, height: 1 };
        let para = if selected {
            Paragraph::new(line).style(Style::default().bg(p.surface0))
        } else {
            Paragraph::new(line)
        };
        frame.render_widget(para, row);
        state.push_click_region(inner.x, ry, inner.width, 1, ClickTarget::Notif(d));
    }

    // Footer: cursor position + the keys this modal owns.
    let footer = Line::from(vec![Span::styled(
        format!(" {} of {}   j/k select · c copy · Y all · n/esc close", cursor + 1, len),
        Style::default().fg(p.overlay0),
    )]);
    frame.render_widget(
        Paragraph::new(footer),
        Rect { y: inner.y + inner.height - 1, height: 1, ..inner },
    );
}


/// Stack of auto-expiring toasts in the top-right of the main pane.
fn render_toasts(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    use crate::state::ToastKind;
    let p = *state.palette();
    let toasts = state.toasts();
    if toasts.is_empty() || area.width < 24 || area.height < 4 {
        return;
    }

    let width = 36u16.min(area.width.saturating_sub(4));
    let x = area.x + area.width.saturating_sub(width + 2);
    // Anchor to the bottom-right (clear of the tab row), newest at the bottom
    // and older ones stacked upward.
    let mut y = area.y + area.height.saturating_sub(2);
    for toast in toasts.iter().rev() {
        if y < area.y + 3 {
            break;
        }
        y -= 3;
        let rect = Rect { x, y, width, height: 3 };
        let dot_color = match toast.kind {
            ToastKind::Failed => p.red,
            ToastKind::Ready => p.green,
            ToastKind::Info => p.accent,
        };
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(p.surface1))
            .style(Style::default().bg(p.panel_bg));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let title = theme::truncate(&toast.title, 14);
        let body_budget = (inner.width as usize).saturating_sub(title.chars().count() + 3);
        let line = Line::from(vec![
            Span::styled("● ", Style::default().fg(dot_color)),
            Span::styled(title, Style::default().fg(p.text).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {}", theme::truncate(&toast.body, body_budget)),
                Style::default().fg(p.subtext0),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), inner);
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

/// Modal shown when a running service crash-loops on `address already in
/// use`. Mirrors the pre-launch picker: a radio list of remediations
/// (Stop / Compose-down / Kill / Skip), navigated with ↑↓ / j,k and Enter.
fn render_port_conflict_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    dlg: &crate::state::PortConflictDialog,
) {
    let n = dlg.options.len() as u16;
    let w = 60u16.min(area.width.saturating_sub(4));
    let h = (7 + n).min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, modal);

    let block = Block::default()
        .title(Span::styled(
            " Port conflict ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Red));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let desc = |d: String| Span::styled(d, Style::default().fg(Color::Gray));

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            desc(format!("{} couldn't bind port ", dlg.service)),
            Span::styled(
                dlg.port.to_string(),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(desc(format!("held by {}", dlg.holder_desc))),
        Line::default(),
    ];
    for (i, opt) in dlg.options.iter().enumerate() {
        let selected = i == dlg.selected;
        let (glyph, glyph_style, label_style) = if selected {
            (
                "●",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )
        } else {
            (
                "○",
                Style::default().fg(Color::DarkGray),
                Style::default().fg(Color::Gray),
            )
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {glyph} "), glyph_style),
            Span::styled(opt.label.clone(), label_style),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " ↑↓ move · enter choose · esc skip",
        Style::default().fg(Color::DarkGray),
    )));

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

/// Fullscreen "zoom" view: the selected service's logs fill the screen with
/// just a thin header and footer. Unlike copy mode this keeps live tail,
/// scrollback and the scrollbar — it's for *reading* one service closely, not
/// for native text selection.
fn render_zoom(frame: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    let p = *state.palette();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let (name, svc_state) = match state.selected_service() {
        Some(s) => (s.name.clone(), Some(s.state.clone())),
        None => ("logs".to_string(), None),
    };
    let mut header = vec![
        Span::styled(
            " ⛶ zoom ",
            Style::default().fg(p.panel_bg).bg(p.mauve).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {name} "), Style::default().fg(p.text).add_modifier(Modifier::BOLD)),
    ];
    if let Some(st) = &svc_state {
        header.push(Span::styled(
            state_label(st),
            Style::default().fg(service_color(&p, st)).add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header)), layout[0]);

    // Reuse the standard log viewport (scroll, scrollbar, PAUSED pill).
    render_log_viewport(frame, layout[1], state);

    let dim = Style::default().fg(p.overlay0);
    let key = Style::default().fg(p.accent).add_modifier(Modifier::BOLD);
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(" jk ", key),
        Span::styled("scroll  ", dim),
        Span::styled("g/G ", key),
        Span::styled("top/tail  ", dim),
        Span::styled("hl ", key),
        Span::styled("service  ", dim),
        Span::styled("z/Esc ", key),
        Span::styled("exit zoom", dim),
    ]));
    frame.render_widget(footer, layout[2]);
}

/// Full-screen "everything stopped" hero, shown after an external `devme
/// down` (or a quit elsewhere) drains every daemon. Rather than exit, the TUI
/// stays as a durable dashboard and parks here until a `devme up` repopulates
/// it. A centred card states the situation and the two ways forward — `u` to
/// bring the stack back up in place, `q` to leave — and toasts still surface
/// on top so the `u` → "starting…" feedback is visible during the relaunch.
fn render_stopped(frame: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    let p = *state.palette();

    // Dim full-screen backdrop so the card reads as the single focus.
    frame.render_widget(
        Block::default().style(Style::default().bg(p.surface_dim)),
        area,
    );

    let w = 54u16.min(area.width.saturating_sub(4));
    let h = 16u16.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let card = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, card);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(p.surface1))
        .style(Style::default().bg(p.panel_bg));
    let inner = block.inner(card);
    frame.render_widget(block, card);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let repo = state.stopped_repo().unwrap_or("This stack").to_string();
    let badge = Style::default().fg(p.mauve);
    let dim = |s: String| Span::styled(s, Style::default().fg(p.overlay0));
    let chip = |k: &'static str| {
        Span::styled(
            format!(" {k} "),
            Style::default()
                .fg(p.panel_bg)
                .bg(p.accent)
                .add_modifier(Modifier::BOLD),
        )
    };

    // Key hints render as their own centred block (below) rather than as part
    // of the centred paragraph: centring each line independently would stagger
    // the `u`/`q` chips, since the labels differ in width. Pad every label to
    // the widest so the rows share a width — and thus a common left edge.
    let hints: [(&str, &str); 2] = [("u", "start the stack again"), ("q", "quit devme")];
    let label_w = hints.iter().map(|(_, l)| l.chars().count()).max().unwrap_or(0);

    // A small framed power badge — a deliberate "off" mark, not an error.
    let lines: Vec<Line> = vec![
        Line::default(),
        Line::from(Span::styled("╭─────╮", badge)),
        Line::from(vec![
            Span::styled("│  ", badge),
            Span::styled("⏻", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("  │", badge),
        ]),
        Line::from(Span::styled("╰─────╯", badge)),
        Line::default(),
        Line::from(Span::styled(
            "All services stopped",
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        )),
        Line::default(),
        Line::from(dim(format!("{repo} was shut down via devme down."))),
        Line::from(dim("The dashboard is still live.".into())),
        Line::default(),
    ];
    let head_lines = lines.len() as u16;

    frame.render_widget(
        Paragraph::new(lines)
            .alignment(ratatui::layout::Alignment::Center)
            .wrap(Wrap { trim: true }),
        Rect { height: head_lines.min(inner.height), ..inner },
    );

    // The hint rows as a left-aligned block, centred as a unit. Block width =
    // chip (3 cols) + gap (2) + the padded label, so both rows align exactly.
    let block_w = (3 + 2 + label_w) as u16;
    let hint_x = inner.x + inner.width.saturating_sub(block_w) / 2;
    let mut hint_y = inner.y + head_lines;
    for (k, label) in hints {
        if hint_y >= inner.y + inner.height {
            break;
        }
        let row = Line::from(vec![
            chip(k),
            Span::styled(format!("  {label:<label_w$}"), Style::default().fg(p.subtext0)),
        ]);
        frame.render_widget(
            Paragraph::new(row),
            Rect { x: hint_x, y: hint_y, width: block_w.min(inner.width), height: 1 },
        );
        hint_y += 1;
    }

    // Toasts on top so the "starting…" ack (and any late crash notice) shows.
    render_toasts(frame, area, state);
}

/// Small centred quit modal offering a choice: stop every service and quit,
/// or detach (leave services running — the remote stack stays up under
/// `devme remote`). Shown on `q` when `tui.confirm_quit` is on (the default).
fn render_quit_confirm(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let p = *state.palette();
    let w = 52u16.min(area.width.saturating_sub(4));
    let h = 7u16.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .title(Span::styled(
            " quit ",
            Style::default().fg(p.panel_bg).bg(p.red).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(p.red))
        .style(Style::default().bg(p.panel_bg));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let bold = |c| Style::default().fg(c).add_modifier(Modifier::BOLD);
    let lines = vec![
        Line::from(Span::styled("Quit the TUI — and the services?", Style::default().fg(p.text))),
        Line::default(),
        Line::from(vec![
            Span::styled(" q ", bold(p.red)),
            Span::styled("stop all & quit", Style::default().fg(p.overlay0)),
        ]),
        Line::from(vec![
            Span::styled(" d ", bold(p.accent)),
            Span::styled("detach — leave services running", Style::default().fg(p.overlay0)),
        ]),
        Line::from(vec![
            Span::styled(" Esc ", bold(p.accent)),
            Span::styled("cancel", Style::default().fg(p.overlay0)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

// ── footer / sidebar ────────────────────────────────────────────────────────

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    // Three-region status bar:
    //   left  — focus breadcrumb (stack > service)
    //   centre — terse key hints (only the most-used)
    //   right — aggregate health summary (counts of each state)
    let p = *state.palette();
    let dim = Style::default().fg(p.overlay0);
    let key = Style::default().fg(p.accent).add_modifier(Modifier::BOLD);

    let stack = if state.shared_selected() { "shared" } else { state.instance_label() };
    let svc = state.selected_service().map(|s| s.name.as_str()).unwrap_or("—");
    let breadcrumb = format!(" {stack} › {svc} ");
    let left = Paragraph::new(Line::from(vec![
        Span::styled(breadcrumb, Style::default().fg(p.text).add_modifier(Modifier::BOLD)),
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
        // Generated from the keymap: every binding flagged `footer` shows here,
        // in declaration order, so the bar can't drift from the bindings.
        let mut spans = Vec::new();
        for (i, hint) in keymap::footer_hints().enumerate() {
            let sep = if i + 1 < keymap::footer_hints().count() { "  " } else { "" };
            spans.push(Span::styled(format!("{} ", hint.keys), key));
            spans.push(Span::styled(format!("{}{sep}", hint.label), dim));
        }
        Line::from(spans)
    };
    let centre = Paragraph::new(centre_line)
        .alignment(ratatui::layout::Alignment::Center);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(24), Constraint::Min(0)])
        .split(area);
    frame.render_widget(left, cols[0]);
    frame.render_widget(centre, cols[1]);
}

fn render_help_overlay(frame: &mut Frame<'_>, area: Rect) {
    let key = |k: &str| {
        Span::styled(
            format!(" {k:<13}"),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    };
    let desc = |d: &str| Span::styled(d.to_string(), Style::default().fg(Color::Gray));
    let section = |title: &str| {
        Line::from(vec![Span::styled(
            title.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )])
    };

    // Generated from the keymap so every bound action is documented and the
    // overlay can't drift from what the event loop actually dispatches.
    let mut lines: Vec<Line> = Vec::new();
    for (i, sec) in keymap::Section::ORDER.iter().enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        lines.push(section(sec.title()));
        for b in keymap::BINDINGS.iter().filter(|b| b.section == *sec) {
            lines.push(Line::from(vec![key(b.keys), desc(b.desc)]));
        }
    }

    // Mouse behaviors have no keybinding (the terminal/emulator handles them),
    // so they live in their own section sourced from the keymap rather than
    // among the action-backed bindings above.
    lines.push(Line::default());
    lines.push(section("mouse"));
    for note in keymap::MOUSE_NOTES {
        lines.push(Line::from(vec![key(note.label), desc(note.desc)]));
    }

    // Centered modal, sized to fit the generated content (so adding a binding
    // never silently clips a row), clamped to the available area. Wide enough
    // to read at a glance, narrow enough that the layout shows around it. The
    // height keeps a small margin when the content fits, but gives that margin
    // up on short terminals so a full keymap still renders its last row.
    let w = 56u16.min(area.width.saturating_sub(4));
    let h = (lines.len() as u16 + 2).min(area.height);
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

    frame.render_widget(Paragraph::new(lines), inner);
}

/// In-app settings overlay: a centered modal listing the editable config
/// keys with their current values, each change persisted to `global.toml`
/// and applied live. Mirrors herdr's settings screen, scaled to devme's
/// handful of keys (so a flat list rather than tabbed sections).
fn render_settings_overlay(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    use crate::state::{SettingControl, SETTINGS};
    let Some(settings) = state.settings() else {
        return;
    };
    let p = *state.palette();

    let w = 60u16.min(area.width.saturating_sub(4));
    // Two rows per setting (value + description) + title, divider, footer.
    let h = (SETTINGS.len() as u16 * 2 + 5).min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .title(Span::styled(
            " settings ",
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(p.accent))
        .style(Style::default().bg(p.panel_bg));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.height < 3 {
        return;
    }

    let mut row = inner.y;
    let bottom = inner.y + inner.height;
    for (i, def) in SETTINGS.iter().enumerate() {
        if row + 2 > bottom {
            break;
        }
        let selected = i == settings.cursor;
        let value = settings.values.get(i).map(String::as_str).unwrap_or(def.default);
        let fill = selected.then(|| Style::default().bg(p.surface0));

        // Row 1: label on the left, control value on the right.
        let label_style = if selected {
            Style::default().fg(p.text).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.subtext0)
        };
        let control = match def.control {
            SettingControl::Toggle if value == "true" => {
                vec![Span::styled("● on", Style::default().fg(p.green))]
            }
            SettingControl::Toggle => {
                vec![Span::styled("○ off", Style::default().fg(p.overlay0))]
            }
            SettingControl::Choice(_) => vec![
                Span::styled("‹ ", Style::default().fg(p.overlay0)),
                Span::styled(value.to_string(), Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
                Span::styled(" ›", Style::default().fg(p.overlay0)),
            ],
        };
        let control_w: usize = control.iter().map(|s| s.content.chars().count()).sum();
        let label = Span::styled(format!(" {}", def.label), label_style);
        let pad = (inner.width as usize)
            .saturating_sub(1 + def.label.chars().count() + control_w + 1);
        let mut spans = vec![label, Span::raw(" ".repeat(pad))];
        spans.extend(control);
        render_filled(frame, Rect { x: inner.x, y: row, width: inner.width, height: 1 }, Line::from(spans), fill);
        row += 1;

        // Row 2: description.
        let desc = Line::from(Span::styled(
            format!("   {}", def.desc),
            Style::default().fg(p.overlay0),
        ));
        render_filled(frame, Rect { x: inner.x, y: row, width: inner.width, height: 1 }, desc, fill);
        row += 1;
    }

    // Footer hint, pinned to the last row.
    if bottom > inner.y {
        let hint = Line::from(vec![
            Span::styled(" ↑↓ ", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("move  ", Style::default().fg(p.overlay0)),
            Span::styled("←→/space ", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("change  ", Style::default().fg(p.overlay0)),
            Span::styled("esc ", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("close", Style::default().fg(p.overlay0)),
        ]);
        frame.render_widget(
            Paragraph::new(hint),
            Rect { x: inner.x, y: bottom - 1, width: inner.width, height: 1 },
        );
    }
}

fn render_sidebar(frame: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    let p = *state.palette();
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

    render_stacks_pane(&p, frame, chunks[0], state);
    if tools_height > 0 {
        render_tools_pane(&p, frame, chunks[1], state);
    }
}

/// A single status dot (glyph + colour) summarising a stack's health.
fn health_dot(p: &Palette, health: crate::state::StackHealth) -> (&'static str, Color) {
    use crate::state::StackHealth as H;
    match health {
        H::AllRunning => ("●", p.green),
        H::SomeRunning => ("◐", p.yellow),
        H::Idle => ("○", p.overlay0),
        H::Failed => ("✗", p.red),
        H::Placeholder => ("○", p.surface1),
    }
}

/// Shorten a remote host for the sidebar badge: drop any `user@`, keep the
/// first DNS label (`vps.tail069899.ts.net` → `vps`), capped so a long name
/// can't crowd the sidebar.
fn short_host(host: &str) -> String {
    let after_at = host.rsplit('@').next().unwrap_or(host);
    let first = after_at.split('.').next().unwrap_or(after_at);
    theme::truncate(first, 14)
}

/// The stacks-section header with a right-aligned remote badge (`⇅ host`),
/// shown when the TUI is attached to a remote stack so the whole sidebar
/// clearly reads as living on another host. The `⇅` echoes the sync that
/// `devme remote` runs; the host names which box. Falls back to the plain
/// `section_header` locally.
fn render_stacks_header_remote(p: &Palette, frame: &mut Frame<'_>, area: Rect, host: &str) {
    if area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let badge = format!("⇅ {} ", short_host(host));
    let left = " stacks";
    let pad = width.saturating_sub(left.chars().count() + badge.chars().count());
    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(p.overlay0).add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(pad)),
        Span::styled(badge, Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
    ]);
    frame.render_widget(Paragraph::new(line), Rect { height: 1, ..area });
}

/// Header label for a sidebar section — a quiet lowercase tag, herdr-style.
fn section_header(p: &Palette, frame: &mut Frame<'_>, area: Rect, label: &str) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {label}"),
            Style::default().fg(p.overlay0).add_modifier(Modifier::BOLD),
        )),
        Rect { height: 1, ..area },
    );
}

/// Render a paragraph, optionally filling its row with a selection bg.
fn render_filled(frame: &mut Frame<'_>, area: Rect, line: Line<'static>, fill: Option<Style>) {
    let para = match fill {
        Some(s) => Paragraph::new(line).style(s),
        None => Paragraph::new(line),
    };
    frame.render_widget(para, area);
}

/// Render a sidebar stack/shared row: a status dot + name on the first line,
/// and a dim secondary line (service summary, git ahead/behind) on the
/// second. The whole row fills with a highlight when selected.
fn render_stack_row(
    p: &Palette,
    frame: &mut Frame<'_>,
    area: Rect,
    dot: (&'static str, Color),
    name: &str,
    secondary: Vec<Span<'static>>,
    selected: bool,
) {
    if area.height == 0 {
        return;
    }
    let name_style = if selected {
        Style::default().fg(p.text).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(p.subtext0)
    };
    // Prefix is 3 columns (" " + dot + " "); reserve only those so the name
    // can fill to the sidebar's edge instead of clipping a char early.
    let max_name = (area.width as usize).saturating_sub(3);
    let name_line = Line::from(vec![
        Span::raw(" "),
        Span::styled(dot.0, Style::default().fg(dot.1)),
        Span::raw(" "),
        Span::styled(theme::truncate(name, max_name), name_style),
    ]);
    let fill = selected.then(|| Style::default().bg(p.surface0));
    render_filled(frame, Rect { y: area.y, height: 1, ..area }, name_line, fill);
    if area.height > 1 && !secondary.is_empty() {
        render_filled(
            frame,
            Rect { y: area.y + 1, height: 1, ..area },
            Line::from(secondary),
            fill,
        );
    }
}

/// The dim secondary line for stack `i`: "2/3 up" plus git ↑ahead ↓behind.
fn stack_secondary(p: &Palette, state: &TuiState, i: usize) -> Vec<Span<'static>> {
    use crate::state::StackSummary;
    let mut spans = vec![Span::raw("   ")];
    match state.instance_service_summary(i) {
        StackSummary::Counted { up, total } => {
            let color = if up == total {
                p.green
            } else if up > 0 {
                p.yellow
            } else {
                p.overlay0
            };
            spans.push(Span::styled(format!("{up}/{total} up"), Style::default().fg(color)));
        }
        StackSummary::SharedOnly => {
            spans.push(Span::styled("shared only", Style::default().fg(p.overlay0)));
        }
        StackSummary::NoDaemon => {
            // Steady-state, a worktree has no daemon because it has no
            // devme.toml — say so plainly rather than the jargon "no daemon".
            let label = if state.instance_is_placeholder(i) {
                "no devme.toml"
            } else {
                "no daemon"
            };
            spans.push(Span::styled(label, Style::default().fg(p.surface1)));
        }
    }
    if let Some((ahead, behind)) = state.instance_ahead_behind(i) {
        if ahead > 0 {
            spans.push(Span::styled(format!(" ↑{ahead}"), Style::default().fg(p.green)));
        }
        if behind > 0 {
            spans.push(Span::styled(format!(" ↓{behind}"), Style::default().fg(p.peach)));
        }
    }
    spans
}

fn render_stacks_pane(p: &Palette, frame: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    if area.height == 0 {
        return;
    }
    // When attached to a remote stack, badge the header so it's unmistakable
    // the whole sidebar lives on another host; otherwise a plain header.
    match state.remote_host() {
        Some(host) => render_stacks_header_remote(p, frame, area, host),
        None => section_header(p, frame, area, "stacks"),
    }

    let selected = state.selected_instance_index();
    let shared_active = state.shared_selected();
    let total = state.instances().len();
    let has_shared = !state.shared_services().is_empty();

    let content_top = area.y + 1;
    let content_h = area.height.saturating_sub(1);
    // Reserve the bottom for the shared section when present: a divider rule,
    // a "shared" header, and the 2-line shared row.
    let shared_reserve: u16 = if has_shared { 4 } else { 0 };
    let stack_h = content_h.saturating_sub(shared_reserve);
    let visible = ((stack_h / 2) as usize).max(1);

    state.ensure_stack_visible(visible);
    let scroll = state.sidebar_scroll();

    // Clickable rows are accumulated here and flushed to `state` after the
    // immutable `labels` borrow ends (can't call `&mut` methods mid-loop).
    let mut regions: Vec<(u16, ClickTarget)> = Vec::new();

    let labels = state.instances();
    let mut y = content_top;
    let stack_bottom = content_top + stack_h;
    for (i, label) in labels.iter().enumerate().skip(scroll) {
        if y + 2 > stack_bottom {
            break;
        }
        let is_selected = !shared_active && selected == Some(i);
        let dot = health_dot(p, state.instance_health(i));
        let label = label.to_string();
        let secondary = stack_secondary(p, state, i);
        render_stack_row(p, frame, Rect { y, height: 2, ..area }, dot, &label, secondary, is_selected);
        regions.push((y, ClickTarget::Stack(i)));
        y += 2;
    }

    // "▾ N more" hint when the list overflows the pane.
    let shown = (scroll..total).take(((stack_bottom - content_top) / 2) as usize).count();
    if scroll + shown < total && y < stack_bottom {
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("  ▾ {} more", total - scroll - shown),
                Style::default().fg(p.overlay0),
            )),
            Rect { y, height: 1, ..area },
        );
        y += 1;
    }

    if has_shared {
        // The shared section flows directly beneath the stacks (a divider rule
        // + "shared" header set it off — mirroring the tabs' "shared" separator
        // and the "tools" section). A one-row gap keeps it off the last stack.
        // `stack_h`'s reserve guarantees these 4 rows fit above `tools`.
        let sy = (y + 1).min(stack_bottom);
        // Dotted rule (echoing the `┊` in the tabs' shared separator) so the
        // shared section reads as distinct from the solid-ruled `tools` below.
        let divider = "┈".repeat(area.width as usize);
        frame.render_widget(
            Paragraph::new(Span::styled(divider, Style::default().fg(p.surface1))),
            Rect { y: sy, height: 1, ..area },
        );
        section_header(p, frame, Rect { y: sy + 1, height: 1, ..area }, "shared");

        let svcs = state.shared_services();
        let label = svcs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ");
        let dot = health_dot(p, state.shared_health());
        let up = svcs
            .iter()
            .filter(|s| {
                matches!(
                    s.state,
                    ServiceState::Running { .. } | ServiceState::External { healthy: true }
                )
            })
            .count();
        let color = if up == svcs.len() && !svcs.is_empty() {
            p.green
        } else {
            p.overlay0
        };
        let secondary = vec![Span::styled(
            format!("   {up}/{} up", svcs.len()),
            Style::default().fg(color),
        )];
        render_stack_row(p, frame, Rect { y: sy + 2, height: 2, ..area }, dot, &label, secondary, shared_active);
        regions.push((sy + 2, ClickTarget::Shared));
    }

    // `labels`/`svcs` borrows are done — flush the recorded rows. Each stack
    // row is the full sidebar width, two rows tall.
    for (ry, target) in regions {
        state.push_click_region(area.x, ry, area.width, 2, target);
    }
}

fn render_tools_pane(p: &Palette, frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    if area.height < 2 {
        return;
    }
    // Divider rule across the top, then a dim header, then the steps.
    let divider = "─".repeat(area.width as usize);
    frame.render_widget(
        Paragraph::new(Span::styled(divider, Style::default().fg(p.surface1))),
        Rect { height: 1, ..area },
    );
    section_header(p, frame, Rect { y: area.y + 1, height: 1, ..area }, "tools");

    let bottom = area.y + area.height;
    let mut y = area.y + 2;
    for s in state.steps() {
        if y >= bottom {
            return;
        }
        // Row prefix is 3 columns (" " + glyph + " "), so the label may use the
        // remaining width — reserving 4 left a blank trailing column and clipped
        // names one char early.
        let max_name = (area.width as usize).saturating_sub(3);
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                step_glyph(s.state).to_string(),
                Style::default().fg(step_color(p, s.state)),
            ),
            Span::raw(" "),
            Span::styled(theme::truncate(s.name.as_str(), max_name), step_text_style(p, s.state)),
        ]);
        frame.render_widget(Paragraph::new(line), Rect { y, height: 1, ..area });
        y += 1;
    }
}

// ── main pane: tabs + viewport + meta ──────────────────────────────────────

fn render_main(frame: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    let p = *state.palette();
    let header = format_main_title(state);
    let main_block = Block::default()
        .title(header)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(p.surface1));
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
    let p = *state.palette();
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
        Style::default().fg(p.mauve).add_modifier(Modifier::BOLD),
    )];
    // A stack that owns no services has nothing to count — the placeholder tab
    // already explains why, so "0/0 running" would just be noise.
    if count > 0 {
        spans.push(Span::styled(
            format!("• {running}/{count} running"),
            Style::default().fg(if running == count {
                p.green
            } else if running > 0 {
                p.yellow
            } else {
                p.overlay0
            }),
        ));
    }
    if failed > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("• {failed} failed"),
            Style::default().fg(p.red),
        ));
    }
    // Remote marker in the always-visible title bar so remoteness survives a
    // collapsed sidebar (where the `⇅ host` header badge is hidden).
    if let Some(host) = state.remote_host() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("⇅ {}", short_host(host)),
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn render_tabs(frame: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    let p = *state.palette();
    let tabs = state.tab_services();
    if tabs.is_empty() {
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
            Style::default().fg(p.overlay0).italic(),
        )));
        frame.render_widget(msg, area);
        return;
    }
    let spinner = state.spinner_frame();
    let sel_idx = state.selected_tab_index();
    // The first repo-scoped service starts the "shared" group; the divider in
    // front of it is labelled so the trailing tabs read as not-owned.
    let first_shared = tabs.iter().position(|t| t.is_shared());

    // Build the row left-to-right, tracking each tab's column span so we can
    // scroll the row horizontally to keep the selected tab on screen.
    let mut spans: Vec<Span> = Vec::new();
    let mut col: usize = 0;
    let mut sel_range: Option<(usize, usize)> = None;
    // (tab index, start col, end col) in content coordinates, before the
    // horizontal scroll is applied — turned into screen click regions below.
    let mut tab_spans: Vec<(usize, usize, usize)> = Vec::new();
    let push = |spans: &mut Vec<Span>, col: &mut usize, text: String, style: Style| {
        *col += text.chars().count();
        spans.push(Span::styled(text, style));
    };

    for (i, t) in tabs.iter().enumerate() {
        if i > 0 {
            if Some(i) == first_shared {
                let div = Style::default().fg(p.surface1);
                push(&mut spans, &mut col, "  ┊ ".into(), div);
                push(
                    &mut spans,
                    &mut col,
                    "shared".into(),
                    Style::default().fg(p.overlay0).add_modifier(Modifier::ITALIC),
                );
                push(&mut spans, &mut col, " ┊  ".into(), div);
            } else {
                push(&mut spans, &mut col, " │ ".into(), Style::default().fg(p.surface1));
            }
        }
        let is_sel = i == sel_idx;
        // Placeholder has no backing service: a warn glyph + dim italic label.
        let (dot, dot_color, name_fg, italic) = match &t.snapshot {
            Some(s) => (
                service_dot(&s.state, spinner).to_string(),
                service_color(&p, &s.state),
                // Shared tabs read dimmer than owned ones so the eye groups them.
                if t.is_shared() { p.subtext0 } else { p.text },
                false,
            ),
            None => ("⚠".to_string(), p.yellow, p.overlay0, true),
        };
        let (pad, dot_style, mut name_style) = if is_sel {
            (
                Style::default().bg(p.surface0),
                Style::default().fg(dot_color).bg(p.surface0).add_modifier(Modifier::BOLD),
                Style::default().fg(p.text).bg(p.surface0).add_modifier(Modifier::BOLD),
            )
        } else {
            (
                Style::default(),
                Style::default().fg(dot_color),
                Style::default().fg(name_fg),
            )
        };
        if italic {
            name_style = name_style.add_modifier(Modifier::ITALIC);
        }
        let start = col;
        push(&mut spans, &mut col, " ".into(), pad);
        push(&mut spans, &mut col, dot, dot_style);
        push(&mut spans, &mut col, " ".into(), pad);
        push(&mut spans, &mut col, t.label.clone(), name_style);
        push(&mut spans, &mut col, " ".into(), pad);
        if is_sel {
            sel_range = Some((start, col));
        }
        tab_spans.push((i, start, col));
    }

    // Horizontal scroll. The offset is manual (mouse wheel over the row), but
    // when the *selection* moves we scroll it back into view — so keyboard nav
    // and clicks always reveal the focused tab, while free wheel-scrolling
    // between selections is left untouched. We detect a selection change by
    // comparing this frame's tab context (stack + selected tab) to last
    // frame's; only then do we "scroll into view".
    let total = col;
    let avail = area.width as usize;
    let max_scroll = total.saturating_sub(avail);

    let stack_sig = if state.shared_selected() {
        usize::MAX
    } else {
        state.selected_instance_index().unwrap_or(usize::MAX)
    };
    let ctx = (stack_sig, sel_idx);
    let selection_moved = state.tab_ctx() != Some(ctx);
    state.set_tab_ctx(ctx);

    let mut scroll_x = state.tab_scroll().min(max_scroll);
    if selection_moved
        && let Some((start, end)) = sel_range
    {
        // Reveal the selected tab: pull left if it's clipped off the right
        // edge, push right if it's off the left.
        if end > scroll_x + avail {
            scroll_x = end - avail;
        }
        if start < scroll_x {
            scroll_x = start;
        }
    }
    scroll_x = scroll_x.min(max_scroll);
    state.set_tab_scroll(scroll_x);
    // Record the row so a mouse wheel over it scrolls the tabs, not the logs.
    state.set_tab_row(area.x, area.y, area.width, area.height);

    frame.render_widget(
        Paragraph::new(Line::from(spans)).scroll((0, scroll_x as u16)),
        area,
    );

    // Edge markers when the row is clipped, so it's clear it continues — and
    // clickable: each pages the row sideways. Drawn in the accent colour (and a
    // hand cursor on hover, since they're click regions) so they read as
    // controls. Registered *before* the tab regions below so they win the
    // hit-test on the cell they share with an edge tab (click_at takes the
    // first matching region).
    let marker = Style::default().fg(p.accent).add_modifier(Modifier::BOLD);
    let show_left = scroll_x > 0;
    let show_right = total.saturating_sub(scroll_x) > avail;
    if show_left {
        frame.render_widget(
            Paragraph::new(Span::styled("‹", marker)),
            Rect { width: 1, ..area },
        );
        state.push_click_region(area.x, area.y, 1, area.height, ClickTarget::TabScrollLeft);
    }
    if show_right {
        let rx = area.x + area.width.saturating_sub(1);
        frame.render_widget(
            Paragraph::new(Span::styled("›", marker)),
            Rect { x: rx, width: 1, ..area },
        );
        state.push_click_region(rx, area.y, 1, area.height, ClickTarget::TabScrollRight);
    }

    // Record each visible tab as a click region. Content cols are shifted left
    // by `scroll_x` and clipped to the pane, mirroring the rendered Paragraph —
    // and kept clear of the one-cell edge arrows so those stay clickable.
    let left_guard = if show_left { area.x + 1 } else { area.x };
    let right_guard = if show_right {
        area.x + area.width.saturating_sub(1)
    } else {
        area.x + area.width
    };
    for (i, s, e) in tab_spans {
        let vs = s.max(scroll_x);
        let ve = e.min(scroll_x + avail);
        if vs >= ve {
            continue; // scrolled out of view
        }
        let sx = (area.x + (vs - scroll_x) as u16).max(left_guard);
        let ex = (area.x + (ve - scroll_x) as u16).min(right_guard);
        if sx >= ex {
            continue; // entirely under an edge arrow
        }
        state.push_click_region(sx, area.y, ex - sx, area.height, ClickTarget::Tab(i));
    }
}

fn render_log_viewport(frame: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    // The placeholder tab has no logs — it explains why the worktree owns no
    // services and points at the shared tabs.
    if state.selected_tab_is_placeholder() {
        let p = *state.palette();
        let msg = Paragraph::new(Text::from(state.placeholder_explanation()))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(p.overlay0).italic());
        frame.render_widget(msg, area);
        return;
    }
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

    // Scrollbar. ratatui (0.29) sizes the thumb as
    // `viewport / ((content_length - 1) + viewport)` of the track. Feeding it
    // the raw line count as `content_length` makes the thumb ~half the track
    // whenever the buffer merely fills the viewport — even though nothing
    // scrolls. So we feed it the number of *scroll steps* instead
    // (`total - viewport`): the thumb then reflects the fraction of the buffer
    // on screen, and the bar disappears entirely when everything fits
    // (ratatui renders nothing for `content_length == 0`). `position` is the
    // index of the top visible line (`start`); offset 0 (live tail) lands it
    // at the bottom. Hit-testing still gets the true total for drag mapping.
    if let Some(sb_area) = sb_area {
        let content_length = logs.len();
        // Record the track so a click/drag on it can scroll (last use of
        // `logs` is here, so the &mut borrow below is free under NLL).
        state.set_scrollbar_hit(sb_area.x, sb_area.y, sb_area.height, content_length, viewport);
        let max_scroll = content_length.saturating_sub(viewport);
        let mut sb_state = ScrollbarState::new(max_scroll).position(start.min(max_scroll));
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
    let p = *state.palette();
    let svc = match state.selected_service() {
        Some(s) => s,
        None => return,
    };
    let mut spans = vec![Span::styled(
        " ".to_string() + &svc.name,
        Style::default().fg(p.text).add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        state_label(&svc.state),
        Style::default()
            .fg(service_color(&p, &svc.state))
            .add_modifier(Modifier::BOLD),
    ));
    if let Some(pid) = svc.pid {
        spans.push(Span::styled("  · pid ", Style::default().fg(p.overlay0)));
        spans.push(Span::raw(pid.to_string()));
    }
    if let Some(port) = svc.port {
        spans.push(Span::styled("  · port ", Style::default().fg(p.overlay0)));
        spans.push(Span::raw(port.to_string()));
    }
    if svc.restart_count > 0 {
        spans.push(Span::styled("  · restarts ", Style::default().fg(p.overlay0)));
        spans.push(Span::styled(
            svc.restart_count.to_string(),
            Style::default().fg(p.yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── style helpers ──────────────────────────────────────────────────────────

fn step_color(p: &Palette, state: StepState) -> Color {
    match state {
        StepState::Passed | StepState::SkippedThisRun => p.green,
        StepState::Overridden => p.yellow,
        StepState::Failed | StepState::ProvisionFailed => p.red,
        StepState::Unknown => p.overlay0,
    }
}

fn step_text_style(p: &Palette, state: StepState) -> Style {
    match state {
        StepState::Unknown => Style::default().fg(p.overlay0),
        _ => Style::default().fg(p.subtext0),
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

/// Status glyph for a service. Starting/restarting services animate with the
/// braille `spinner` frame so the row reads as live.
fn service_dot(state: &ServiceState, spinner: char) -> String {
    use ServiceState as S;
    match state {
        S::Running { degraded: false, .. } => "●".to_string(),
        S::Running { degraded: true, .. } => "◐".to_string(),
        S::Starting | S::Restarting { .. } => spinner.to_string(),
        S::Failed { .. } | S::CrashLoop { .. } => "✗".to_string(),
        S::External { healthy: true } => "◇".to_string(),
        S::External { healthy: false } => "✗".to_string(),
        S::Stopped | S::WaitingOnDependency { .. } => "○".to_string(),
    }
}

fn service_color(p: &Palette, state: &ServiceState) -> Color {
    use ServiceState as S;
    match state {
        S::Running { degraded: false, .. } => p.green,
        S::Running { degraded: true, .. } => p.yellow,
        S::Starting | S::Restarting { .. } => p.yellow,
        S::Failed { .. } | S::CrashLoop { .. } => p.red,
        S::External { healthy: true } => p.teal,
        S::External { healthy: false } => p.red,
        S::Stopped | S::WaitingOnDependency { .. } => p.overlay0,
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
            url: None,
            restart_count: 0,
        }
    }

    #[test]
    fn stopped_state_renders_hero_card() {
        let mut state = TuiState::default();
        state.enter_stopped(Some("kpi-dash".into()));
        let text = render_to_text(&mut state, 80, 24);
        assert!(text.contains("All services stopped"), "missing title:\n{text}");
        assert!(text.contains("kpi-dash"), "missing repo name:\n{text}");
        assert!(text.contains("start the stack again"), "missing start hint:\n{text}");
        assert!(text.contains("quit devme"), "missing quit hint:\n{text}");
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

    /// Many tabs in a narrow pane overflow; the row can be scrolled
    /// horizontally to reveal the ones clipped off the right edge.
    #[test]
    fn tabs_scroll_horizontally_when_overflowing() {
        let mut state = TuiState::default();
        let services: Vec<_> = (0..12)
            .map(|i| svc(&format!("service-{i:02}"), ServiceState::Stopped))
            .collect();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services,
            steps: vec![],
        });

        // Narrow pane → the 12 tabs can't all fit.
        let first = render_to_text(&mut state, 60, 12);
        assert!(first.contains("service-00"), "first tab visible:\n{first}");
        assert!(!first.contains("service-11"), "last tab off-screen initially:\n{first}");

        // Scroll the row right (clamped to the content width) → the tail shows.
        state.scroll_tabs(500);
        let scrolled = render_to_text(&mut state, 60, 12);
        assert!(
            scrolled.contains("service-11"),
            "last tab revealed after scrolling:\n{scrolled}"
        );
    }

    /// Moving the selection scrolls the focused tab back into view even after a
    /// manual scroll parked it off-screen.
    #[test]
    fn selected_tab_scrolls_into_view() {
        let mut state = TuiState::default();
        let services: Vec<_> = (0..12)
            .map(|i| svc(&format!("service-{i:02}"), ServiceState::Stopped))
            .collect();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services,
            steps: vec![],
        });
        let _ = render_to_text(&mut state, 60, 12); // prime the tab context

        for _ in 0..11 {
            state.select_next_service();
        }
        let text = render_to_text(&mut state, 60, 12);
        assert!(
            text.contains("service-11"),
            "selected last tab scrolled into view:\n{text}"
        );
    }

    #[test]
    fn shared_services_render_as_trailing_group_on_a_stack() {
        let mut state = TuiState::default();
        // Shared daemon first so the instance's stubs are recognised as shared.
        state.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "shared::repo".into(),
                label: "shared".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![
                svc("postgres", ServiceState::Stopped),
                svc("redis", ServiceState::Stopped),
            ],
            steps: vec![],
        });
        state.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "inst".into(),
                label: "feature/a".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![
                svc("api", ServiceState::Stopped),
                svc("web", ServiceState::Stopped),
                svc("postgres", ServiceState::Stopped),
                svc("redis", ServiceState::Stopped),
            ],
            steps: vec![],
        });

        let text = render_to_text(&mut state, 100, 14);
        // Owned services, the shared-group label, and shared services all on
        // the tab row — shared sorted last, behind the labelled divider.
        let tab_line = text
            .lines()
            .find(|l| l.contains("api") && l.contains("postgres"))
            .unwrap_or_else(|| panic!("no tab row with owned + shared services:\n{text}"));
        let i_web = tab_line.find("web").unwrap();
        let i_shared = tab_line.find("shared").unwrap();
        let i_pg = tab_line.find("postgres").unwrap();
        let i_redis = tab_line.find("redis").unwrap();
        assert!(
            i_web < i_shared && i_shared < i_pg && i_pg < i_redis,
            "expected owned · 'shared' label · shared services, got:\n{tab_line}"
        );
    }

    #[test]
    fn worktree_without_devme_toml_shows_placeholder_tab_and_explanation() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "shared::repo".into(),
                label: "shared".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![
                svc("proxy", ServiceState::Stopped),
                svc("postgres", ServiceState::Stopped),
            ],
            steps: vec![],
        });
        // Discovered worktree, no devme.toml → placeholder row, no daemon.
        state.add_placeholder_instance("inst", "feature/x", "/tmp/a");

        let text = render_to_text(&mut state, 100, 14);
        let tab_line = text
            .lines()
            .find(|l| l.contains("proxy"))
            .unwrap_or_else(|| panic!("no tab row:\n{text}"));
        let i_label = tab_line
            .find("no devme.toml")
            .unwrap_or_else(|| panic!("placeholder label missing:\n{tab_line}"));
        let i_proxy = tab_line.find("proxy").unwrap();
        let i_pg = tab_line.find("postgres").unwrap();
        assert!(
            i_label < i_proxy && i_proxy < i_pg,
            "placeholder tab should lead, then shared services:\n{tab_line}"
        );
        // The viewport explains the empty state rather than showing logs.
        assert!(
            text.contains("add one to start services"),
            "placeholder explanation missing from viewport:\n{text}"
        );
        // A worktree that owns nothing shows no "N/N running" count in the
        // title — the placeholder tab already explains the empty state.
        assert!(
            !text.contains("running"),
            "title should omit the count for a stack that owns no services:\n{text}"
        );
    }

    #[test]
    fn title_shows_count_when_stack_owns_services() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![svc("api", ServiceState::Stopped)],
            steps: vec![],
        });
        let text = render_to_text(&mut state, 100, 14);
        assert!(text.contains("0/1 running"), "count missing:\n{text}");
    }

    #[test]
    fn sidebar_groups_shared_services_under_a_header() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "shared::repo".into(),
                label: "shared".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![
                svc("proxy", ServiceState::Stopped),
                svc("postgres", ServiceState::Stopped),
            ],
            steps: vec![],
        });
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![svc("api", ServiceState::Stopped)],
            steps: vec![],
        });
        let text = render_to_text(&mut state, 100, 16);
        let lines: Vec<&str> = text.lines().collect();
        // The shared row lists its services joined — unique to the sidebar
        // (the tab row shows them as separate tabs).
        let row_idx = lines
            .iter()
            .position(|l| l.contains("proxy, postgres"))
            .unwrap_or_else(|| panic!("shared services row missing from sidebar:\n{text}"));
        // Directly above: a "shared" header, and a divider rule above that.
        assert!(
            lines[row_idx - 1].contains("shared"),
            "expected a 'shared' header just above the shared row:\n{text}"
        );
        assert!(
            lines[row_idx - 2].contains('┈'),
            "expected a dotted divider rule above the shared header:\n{text}"
        );
        // With a single stack, the shared section flows just beneath it — in
        // the upper half of the sidebar, not pinned to the bottom.
        assert!(
            row_idx < lines.len() / 2,
            "shared section should sit just beneath the stacks, not at the bottom (row {row_idx} of {}):\n{text}",
            lines.len()
        );
    }

    #[test]
    fn tab_row_scrolls_to_keep_selected_tab_visible() {
        let mut state = TuiState::default();
        let names = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
        ];
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: names
                .iter()
                .map(|n| svc(n, ServiceState::Stopped))
                .collect(),
            steps: vec![],
        });
        // Select the last tab — it sits well off the right edge of a narrow pane.
        for _ in 0..names.len() - 1 {
            state.select_next_service();
        }
        let text = render_to_text(&mut state, 60, 14);
        let tab_line = text
            .lines()
            .find(|l| l.contains("hotel"))
            .unwrap_or_else(|| panic!("selected (last) tab must be visible:\n{text}"));
        // Scrolled, so the row shows a left-overflow marker and the first tab
        // has been clipped away.
        assert!(tab_line.contains('‹'), "left overflow marker expected:\n{tab_line}");
        assert!(!tab_line.contains("alpha"), "first tab should be clipped:\n{tab_line}");
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
    fn port_conflict_modal_lists_remediations() {
        use devme_supervisor::port_preflight::Holder;
        let mut state = TuiState::default();
        state.open_port_conflict(
            "inst".into(),
            "postgres".into(),
            5432,
            Holder::Container {
                name: "kpi-shared-db-1".into(),
                project: Some("kpi-shared".into()),
            },
        );
        let text = render_to_text(&mut state, 80, 20);
        assert!(text.contains("Port conflict"), "missing title:\n{text}");
        assert!(text.contains("postgres"), "missing service:\n{text}");
        assert!(text.contains("5432"), "missing port:\n{text}");
        assert!(text.contains("kpi-shared-db-1"), "missing holder:\n{text}");
        assert!(text.contains("Stop container"), "missing stop option:\n{text}");
        assert!(text.contains("Compose down"), "missing compose option:\n{text}");
        assert!(text.contains("Skip"), "missing skip option:\n{text}");
    }

    #[test]
    fn port_conflict_modal_outranks_skill_dialog() {
        use crate::state::SkillPrompt;
        use devme_supervisor::port_preflight::Holder;
        let mut state = TuiState::default();
        state.set_skill_dialog_for_test(SkillPrompt::Install);
        state.open_port_conflict(
            "inst".into(),
            "web".into(),
            3000,
            Holder::Process(vec![(123, Some("node".into()))]),
        );
        let text = render_to_text(&mut state, 80, 20);
        // The port-conflict modal wins; the skill prompt is suppressed.
        assert!(text.contains("Port conflict"), "port modal not on top:\n{text}");
        assert!(text.contains("Kill node (123)"), "missing kill option:\n{text}");
        assert!(!text.contains("Install it"), "skill prompt leaked through:\n{text}");
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
    fn notifications_overlay_renders_history_when_toggled() {
        let mut state = TuiState::default();
        state.notify("open", "https://example.test opened");
        // Use a durable (non-"copy") notification — copy acks are transient and
        // wouldn't land in the history the modal renders.
        state.notify("logs", "copied 12 log lines");

        // The corner toasts still show their bodies while live, so the modal's
        // own footer ("… of N … n/esc close") is the reliable discriminator.
        let text = render_to_text(&mut state, 100, 30);
        assert!(!text.contains("n/esc close"), "modal leaked when hidden:\n{text}");

        state.toggle_notifications();
        let text = render_to_text(&mut state, 100, 30);
        assert!(text.contains("notifications"), "modal title missing:\n{text}");
        assert!(text.contains("copied 12 log lines"), "newest entry missing:\n{text}");
        assert!(text.contains("example.test opened"), "older entry missing:\n{text}");
        assert!(text.contains("of 2"), "scroll footer missing:\n{text}");
        assert!(text.contains("c copy"), "copy affordance missing from footer:\n{text}");

        state.close_notifications();
        let text = render_to_text(&mut state, 100, 30);
        assert!(!text.contains("n/esc close"), "modal should hide again:\n{text}");
    }

    #[test]
    fn notifications_overlay_handles_empty_history() {
        let mut state = TuiState::default();
        state.toggle_notifications();
        let text = render_to_text(&mut state, 100, 30);
        assert!(text.contains("no notifications yet"), "empty state missing:\n{text}");
    }

    #[test]
    fn sidebar_badges_remote_host() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: InstanceInfo { id: "id".into(), label: "main".into(), cwd: "/srv/app".into() },
            services: vec![],
            steps: vec![],
        });

        // Local: a plain header, no badge.
        let text = render_to_text(&mut state, 100, 20);
        assert!(!text.contains("⇅"), "remote badge leaked locally:\n{text}");

        // Remote: the stacks header gains a `⇅ <short host>` badge.
        state.set_remote_host(Some("vps.tail069899.ts.net".into()));
        let text = render_to_text(&mut state, 100, 20);
        assert!(text.contains("⇅"), "remote badge missing:\n{text}");
        assert!(text.contains("vps"), "short host missing from badge:\n{text}");
        // The long DNS suffix is dropped — only the first label shows.
        assert!(!text.contains("tail069899"), "badge should shorten the host:\n{text}");
    }

    #[test]
    fn stack_info_overlay_renders_fields_when_open() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "id".into(),
                label: "feature/x".into(),
                cwd: "/tmp/wt-x".into(),
            },
            services: vec![ServiceSnapshot {
                name: "api".into(),
                state: ServiceState::Stopped,
                pid: None,
                port: None,
                url: None,
                restart_count: 0,
            }],
            steps: vec![],
        });

        let text = render_to_text(&mut state, 100, 30);
        assert!(!text.contains("Instance id"), "modal leaked when hidden:\n{text}");

        state.open_stack_info(Some(2), None);
        let text = render_to_text(&mut state, 100, 30);
        assert!(text.contains("feature/x"), "branch/title missing:\n{text}");
        assert!(text.contains("/tmp/wt-x"), "worktree path missing:\n{text}");
        assert!(text.contains("Slot"), "slot row missing:\n{text}");
        assert!(text.contains("branch/path"), "footer hint missing:\n{text}");

        state.close_stack_info();
        let text = render_to_text(&mut state, 100, 30);
        assert!(!text.contains("Instance id"), "modal should hide again:\n{text}");
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
                url: Some("{host}:{port}".into()),
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
    fn scrollbar_hidden_when_logs_fit_but_shown_when_they_overflow() {
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());

        // A handful of lines fits in the viewport — no thumb should render.
        // (ratatui draws nothing when there's nothing to scroll; a stale model
        // here used to size the thumb at ~half the track regardless.)
        let mut fits = TuiState::default();
        fits.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![svc("api", ServiceState::Stopped)],
            steps: vec![],
        });
        for i in 0..3 {
            fits.apply(ServerMessage::LogChunk {
                service: "api".into(),
                bytes: enc(&format!("line {i}")),
                ts: i as u64,
            });
        }
        let text = render_to_text(&mut fits, 100, 20);
        assert!(!text.contains('┃'), "scrollbar thumb shown when logs fit:\n{text}");

        // Far more lines than the viewport — the thumb must appear.
        let mut overflow = TuiState::default();
        overflow.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![svc("api", ServiceState::Stopped)],
            steps: vec![],
        });
        for i in 0..200 {
            overflow.apply(ServerMessage::LogChunk {
                service: "api".into(),
                bytes: enc(&format!("line {i}")),
                ts: i as u64,
            });
        }
        let text = render_to_text(&mut overflow, 100, 20);
        assert!(text.contains('┃'), "scrollbar thumb missing when logs overflow:\n{text}");
    }

    #[test]
    fn tools_label_uses_reclaimed_prefix_column() {
        // The row prefix is 3 columns (" " + glyph + " "). At the 28-col default
        // sidebar (less a 1-col right gutter → 27 content), the label budget is
        // 27 - 3 = 24. A 24-char name must render whole — reserving 4 used to
        // clip it one char early.
        let name = "abcdefghij_klmnop_qrstuv"; // 24 chars
        assert_eq!(name.chars().count(), 24);
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![svc("api", ServiceState::Stopped)],
            steps: vec![StepSnapshot {
                name: name.into(),
                state: StepState::Passed,
            }],
        });
        let text = render_to_text(&mut state, 100, 24);
        assert!(
            text.contains(name),
            "tool label clipped despite fitting the sidebar:\n{text}"
        );
    }

    #[test]
    fn title_bar_marks_remote() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: InstanceInfo { id: "id".into(), label: "main".into(), cwd: "/srv/app".into() },
            services: vec![],
            steps: vec![],
        });
        // Collapse the sidebar so the header badge is gone — the title must
        // still carry the remote marker.
        state.toggle_sidebar();
        state.set_remote_host(Some("vps.tail069899.ts.net".into()));
        let text = render_to_text(&mut state, 100, 20);
        assert!(text.contains("⇅ vps"), "title-bar remote marker missing:\n{text}");
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
