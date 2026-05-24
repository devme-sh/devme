//! Ratatui renderer for [`TuiState`]. Pure layout + styling — no I/O, no
//! event loop. The runtime wires this to a real terminal; tests wire it to
//! a [`ratatui::backend::TestBackend`].

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};

use crate::state::TuiState;
use devme_core::{ServiceSnapshot, ServiceState, StepSnapshot, StepState};

/// Render `state` into `frame`'s full area. Lazygit-style layout: sidebar
/// on the left, tabs + viewport on the right, single-line footer at the
/// bottom with the active key bindings.
pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let outer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Min(0)])
        .split(vertical[0]);

    render_sidebar(frame, outer[0], state);
    render_main(frame, outer[1], state);
    render_footer(frame, vertical[1]);
}

fn render_footer(frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    let hints = " q quit  ↑↓ navigate  enter select  r restart  s stop  ? help ";
    frame.render_widget(Paragraph::new(hints), area);
}

fn render_sidebar(frame: &mut Frame<'_>, area: ratatui::layout::Rect, state: &TuiState) {
    let block = Block::default().title("Instances").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let label = state.instance_label();
    if !label.is_empty() {
        let line = Line::from(vec![
            Span::styled("▸ ", Style::default().fg(Color::Cyan)),
            Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
        ]);
        frame.render_widget(Paragraph::new(line), inner);
    }
}

fn render_main(frame: &mut Frame<'_>, area: ratatui::layout::Rect, state: &TuiState) {
    let main_block = Block::default().title("devme").borders(Borders::ALL);
    let inner = main_block.inner(area);
    frame.render_widget(main_block, area);

    let steps_line = render_steps_line(state.steps());
    let steps_height = if state.steps().is_empty() { 0 } else { 1 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(steps_height),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(inner);

    if steps_height > 0 {
        frame.render_widget(Paragraph::new(steps_line), chunks[0]);
    }

    let titles: Vec<Line> = state
        .services()
        .iter()
        .map(|s| Line::from(Span::styled(s.name.as_str(), service_tab_style(&s.state))))
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
        .highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED))
        .divider(" │ ")
        .padding("", "");
    frame.render_widget(tabs, chunks[1]);

    // Viewport: for now just compact status of the selected service.
    let body = match state.selected_service() {
        Some(svc) => render_service_status(svc),
        None => "no service selected".to_string(),
    };
    frame.render_widget(Paragraph::new(body), chunks[2]);
}

fn render_steps_line<'a>(steps: &'a [StepSnapshot]) -> Line<'a> {
    if steps.is_empty() {
        return Line::default();
    }
    let mut spans = vec![Span::styled(
        "setup:  ",
        Style::default().fg(Color::DarkGray),
    )];
    for (i, s) in steps.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(
            step_glyph(s.state),
            Style::default().fg(step_color(s.state)),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::raw(s.name.as_str()));
    }
    Line::from(spans)
}

fn step_color(state: StepState) -> Color {
    match state {
        StepState::Passed | StepState::SkippedThisRun => Color::Green,
        StepState::Overridden => Color::Yellow,
        StepState::Failed | StepState::ProvisionFailed => Color::Red,
        StepState::Unknown => Color::DarkGray,
    }
}

fn service_tab_style(state: &ServiceState) -> Style {
    let base = Style::default();
    match state {
        ServiceState::Running { degraded: false, .. } => base.fg(Color::Green),
        ServiceState::Running { degraded: true, .. } => base.fg(Color::Yellow),
        ServiceState::Starting | ServiceState::Restarting { .. } => base.fg(Color::Yellow),
        ServiceState::Failed { .. } | ServiceState::CrashLoop { .. } => base.fg(Color::Red),
        ServiceState::External { healthy: true } => base.fg(Color::Cyan),
        ServiceState::External { healthy: false } => base.fg(Color::Red),
        ServiceState::Stopped | ServiceState::WaitingOnDependency { .. } => {
            base.fg(Color::DarkGray)
        }
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

fn render_service_status(svc: &ServiceSnapshot) -> String {
    let state_label = state_label(&svc.state);
    let mut line = format!("{}  •  {}", svc.name, state_label);
    if let Some(pid) = svc.pid {
        line.push_str(&format!("  pid {pid}"));
    }
    if let Some(port) = svc.port {
        line.push_str(&format!("  port {port}"));
    }
    if svc.restart_count > 0 {
        line.push_str(&format!("  restarts {}", svc.restart_count));
    }
    line
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
    use devme_core::{ServerMessage, ServiceSnapshot, ServiceState};
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
        // A real Tabs widget renders names with a divider between them; a
        // bare space-joined Paragraph wouldn't include this.
        assert!(
            text.contains("a │ b") || text.contains("a | b"),
            "expected tab divider between 'a' and 'b':\n{text}"
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

        let text = render_to_text(&state, 80, 12);
        // The tab row is row index 1 (just below the main pane's top border).
        // Asserting all three names appear *anywhere* in the rendered output
        // would be too lax — we want them on the same row.
        let lines: Vec<&str> = text.lines().collect();
        let tab_line = lines
            .iter()
            .find(|l| l.contains("db") && l.contains("backend") && l.contains("frontend"))
            .unwrap_or_else(|| panic!("no row had all three service names:\n{text}"));
        // Sanity: they appear in the right order on that row.
        let i_db = tab_line.find("db").unwrap();
        let i_be = tab_line.find("backend").unwrap();
        let i_fe = tab_line.find("frontend").unwrap();
        assert!(i_db < i_be && i_be < i_fe);
    }

    #[test]
    fn steps_render_as_checklist_with_status_glyphs() {
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
        let text = render_to_text(&state, 80, 12);
        // Passed shows a check, unknown a circle/question, failed an X.
        assert!(text.contains("✓") && text.contains("gcloud_auth"), "passed glyph or name missing:\n{text}");
        assert!(text.contains("uv") && (text.contains("·") || text.contains("?")), "unknown glyph or name missing:\n{text}");
        assert!(text.contains("✗") && text.contains("redis"), "failed glyph or name missing:\n{text}");
    }

    #[test]
    fn footer_lists_basic_key_bindings() {
        let state = TuiState::default();
        let text = render_to_text(&state, 80, 12);
        let last = text.lines().last().unwrap_or("");
        // Footer shows the bare-minimum nav + action keys.
        assert!(last.contains("quit"), "footer missing 'quit':\n{text}");
        assert!(
            last.contains("navigate") || last.contains("nav"),
            "footer missing navigation hint:\n{text}"
        );
    }

    #[test]
    fn selected_service_shows_compact_state_label() {
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

        let text = render_to_text(&state, 80, 12);
        // Compact label, not a Rust Debug dump.
        assert!(
            text.contains("running"),
            "expected 'running' label:\n{text}"
        );
        assert!(
            !text.contains("started_without"),
            "Debug-formatting leaked:\n{text}"
        );
        assert!(text.contains("1234"), "pid missing:\n{text}");
        assert!(text.contains("5432"), "port missing:\n{text}");
    }

    #[test]
    fn render_shows_service_name_in_main_pane() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            services: vec![ServiceSnapshot {
                name: "backend".into(),
                state: ServiceState::Stopped,
                pid: None,
                port: None,
                restart_count: 0,
            }],
            steps: vec![],
        });

        let text = render_to_text(&state, 60, 12);
        assert!(text.contains("backend"), "missing 'backend':\n{text}");
    }
}
