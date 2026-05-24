//! Ratatui renderer for [`TuiState`]. Pure layout + styling — no I/O, no
//! event loop. The runtime wires this to a real terminal; tests wire it to
//! a [`ratatui::backend::TestBackend`].
//!
//! Layout (lazygit-inspired, see ADR-0010):
//!
//! ```text
//! ╭─ instance ──╮╭─ header: stack name • [tab1 │ tab2 │ tab3] • selected meta ─╮
//! │ ▸ kpi-...   ││╭─ logs ──────────────────────────────────────────────────╮ │
//! │             │││ 12:34:01 listening on :8080                              │ │
//! │ steps:      │││ 12:34:02 GET /api/health 200                             │ │
//! │  ✓ tools    │││ ...                                                       │ │
//! │  · uv       │││                                                           │ │
//! │             │││                                                           │ │
//! │             │││                                                           │ │
//! ╰─────────────╯│╰───────────────────────────────────────────────────────────╯ │
//!                ╰── status: tick • running • pid 12345 • 0 restarts ───────────╯
//!  q quit  ↑↓/jk navigate  s stop  r restart  S start  ? help
//! ```

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Tabs, Wrap};

use crate::state::TuiState;
use devme_core::{ServiceState, StepState};

/// Render `state` into `frame`'s full area.
pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    let area = frame.area();
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
    render_footer(frame, vertical[1]);
}

// ── footer / sidebar ────────────────────────────────────────────────────────

fn render_footer(frame: &mut Frame<'_>, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let spans = vec![
        Span::styled(" q", key),
        Span::styled(" quit  ", dim),
        Span::styled("↑↓/jk", key),
        Span::styled(" nav  ", dim),
        Span::styled("S", key),
        Span::styled(" start  ", dim),
        Span::styled("s", key),
        Span::styled(" stop  ", dim),
        Span::styled("r", key),
        Span::styled(" restart  ", dim),
        Span::styled("?", key),
        Span::styled(" help", dim),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_sidebar(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let block = Block::default()
        .title(Span::styled(
            " instance ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let label = state.instance_label();
    let mut lines: Vec<Line> = Vec::new();
    if !label.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("▸ ", Style::default().fg(Color::Cyan)),
            Span::styled(
                label.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::default());
    }

    if !state.steps().is_empty() {
        lines.push(Line::from(Span::styled(
            "steps",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )));
        for s in state.steps() {
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    step_glyph(s.state).to_string(),
                    Style::default().fg(step_color(s.state)),
                ),
                Span::raw(" "),
                Span::styled(s.name.as_str().to_string(), step_text_style(s.state)),
            ]));
        }
        lines.push(Line::default());
    }

    if !state.services().is_empty() {
        lines.push(Line::from(Span::styled(
            "services",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )));
        for s in state.services() {
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    service_dot(&s.state).to_string(),
                    Style::default().fg(service_color(&s.state)),
                ),
                Span::raw(" "),
                Span::styled(s.name.clone(), Style::default()),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// ── main pane: tabs + viewport + meta ──────────────────────────────────────

fn render_main(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let header = format_main_title(state);
    let main_block = Block::default()
        .title(header)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
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

    let mut spans = vec![Span::styled(
        " devme ",
        Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::styled(
        format!("• {running}/{count} running"),
        Style::default().fg(if running == count && count > 0 {
            Color::Green
        } else if running > 0 {
            Color::Yellow
        } else {
            Color::DarkGray
        }),
    ));
    if failed > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("• {failed} failed"),
            Style::default().fg(Color::Red),
        ));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn render_tabs(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    if state.services().is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "no services declared in devme.toml",
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
                .fg(Color::White)
                .bg(Color::Indexed(238))
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::styled(" │ ", Style::default().fg(Color::DarkGray)))
        .padding(" ", " ");
    frame.render_widget(tabs, area);
}

fn render_log_viewport(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let inner_area = area;
    let svc = match state.selected_service() {
        Some(s) => s,
        None => {
            let msg = Paragraph::new(Line::from(Span::styled(
                "no service selected",
                Style::default().fg(Color::DarkGray).italic(),
            )));
            frame.render_widget(msg, inner_area);
            return;
        }
    };
    let logs = state.service_logs(&svc.name);
    if logs.is_empty() {
        let placeholder = match &svc.state {
            ServiceState::Stopped => "stopped — press S to start",
            ServiceState::Starting => "starting…",
            _ => "no output yet",
        };
        let msg = Paragraph::new(Line::from(Span::styled(
            placeholder,
            Style::default().fg(Color::DarkGray).italic(),
        )));
        frame.render_widget(msg, inner_area);
        return;
    }

    // Show the most-recent lines that fit. ANSI-aware parsing colors them.
    let take = (inner_area.height as usize).max(1);
    let start = logs.len().saturating_sub(take);
    let mut text = Text::default();
    for line in logs.iter().skip(start) {
        let parsed = line
            .as_bytes()
            .into_text()
            .unwrap_or_else(|_| Text::raw(line.clone()));
        for parsed_line in parsed.lines {
            text.lines.push(parsed_line);
        }
    }
    frame.render_widget(Paragraph::new(text), inner_area);
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
        StepState::Passed | StepState::SkippedThisRun => Color::Green,
        StepState::Overridden => Color::Yellow,
        StepState::Failed | StepState::ProvisionFailed => Color::Red,
        StepState::Unknown => Color::DarkGray,
    }
}

fn step_text_style(state: StepState) -> Style {
    match state {
        StepState::Unknown => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
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
        S::Running { degraded: false, .. } => Color::Green,
        S::Running { degraded: true, .. } => Color::Yellow,
        S::Starting | S::Restarting { .. } => Color::Yellow,
        S::Failed { .. } | S::CrashLoop { .. } => Color::Red,
        S::External { healthy: true } => Color::Cyan,
        S::External { healthy: false } => Color::Red,
        S::Stopped | S::WaitingOnDependency { .. } => Color::DarkGray,
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
    use devme_core::{ServerMessage, ServiceSnapshot, ServiceState, StepSnapshot};
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

    fn render_to_text(state: &TuiState, w: u16, h: u16) -> String {
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
            services: vec![
                svc("a", ServiceState::Stopped),
                svc("b", ServiceState::Stopped),
            ],
            steps: vec![],
        });
        let text = render_to_text(&state, 80, 12);
        assert!(
            text.contains("│"),
            "expected tab divider somewhere:\n{text}"
        );
    }

    #[test]
    fn tabs_row_shows_every_service_name() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            services: vec![
                svc("db", ServiceState::Stopped),
                svc("backend", ServiceState::Stopped),
                svc("frontend", ServiceState::Stopped),
            ],
            steps: vec![],
        });

        let text = render_to_text(&state, 100, 14);
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
        let text = render_to_text(&state, 80, 14);
        assert!(text.contains("✓"), "passed glyph missing:\n{text}");
        assert!(text.contains("✗"), "failed glyph missing:\n{text}");
        assert!(text.contains("·"), "unknown glyph missing:\n{text}");
        assert!(text.contains("gcloud"), "step name missing:\n{text}");
    }

    #[test]
    fn footer_lists_basic_key_bindings() {
        let state = TuiState::default();
        let text = render_to_text(&state, 80, 12);
        let last = text.lines().last().unwrap_or("");
        assert!(last.contains("quit"), "footer missing 'quit':\n{text}");
        assert!(
            last.contains("nav") || last.contains("navigate"),
            "footer missing navigation hint:\n{text}"
        );
    }

    #[test]
    fn selected_service_meta_shows_state_and_pid_and_port() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
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

        let text = render_to_text(&state, 80, 14);
        assert!(text.contains("running"), "expected 'running':\n{text}");
        assert!(text.contains("1234"), "pid missing:\n{text}");
        assert!(text.contains("5432"), "port missing:\n{text}");
    }

    #[test]
    fn log_lines_appear_in_viewport_for_selected_service() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
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

        let text = render_to_text(&state, 100, 20);
        assert!(text.contains("listening on :8080"), "missing first log line:\n{text}");
        assert!(text.contains("GET /health 200"), "missing second log line:\n{text}");
    }

    #[test]
    fn header_shows_running_count() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
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
        let text = render_to_text(&state, 100, 14);
        assert!(text.contains("1/2 running"), "header count missing:\n{text}");
    }

    #[test]
    fn header_shows_failed_count_when_nonzero() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            services: vec![
                svc("boom", ServiceState::Failed { exit_code: Some(7) }),
                svc("tick", ServiceState::Running { degraded: false, started_without: vec![] }),
            ],
            steps: vec![],
        });
        let text = render_to_text(&state, 100, 14);
        assert!(text.contains("1 failed"), "failed count missing:\n{text}");
    }
}
