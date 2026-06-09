//! Render a populated TUI state once into an in-memory TestBackend and
//! dump the buffer to stdout. Useful for eyeballing layout changes
//! without leaving the editor — `cargo run --example preview -p devme-tui`.

use base64::Engine;
use devme_core::{ServerMessage, ServiceSnapshot, ServiceState, StepSnapshot, StepState};
use devme_tui::render::render;
use devme_tui::state::TuiState;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

fn main() {
    let mut state = TuiState::default();
    state.add_instance("kpi-dashboard");
    state.add_instance("internal-portal");
    state.add_instance("ingest-worker");
    // Subscribe the *first* stack so its services/steps land on the selected
    // row; the other two stay placeholders (dim dots), which shows off the
    // sidebar's per-stack health dots.
    state.apply(ServerMessage::Subscribed {
        instance: devme_core::InstanceInfo {
            id: "local::kpi-dashboard".into(),
            label: "kpi-dashboard".into(),
            cwd: "/tmp/preview".into(),
        },
        services: vec![
            ServiceSnapshot {
                name: "db".into(),
                state: ServiceState::Running {
                    degraded: false,
                    started_without: vec![],
                },
                pid: Some(12345),
                port: Some(5432),
                url: Some("{host}:{port}".into()),
                restart_count: 0,
            },
            ServiceSnapshot {
                name: "backend".into(),
                state: ServiceState::Starting,
                pid: Some(12346),
                port: Some(8080),
                url: Some("http://{host}:{port}".into()),
                restart_count: 0,
            },
            ServiceSnapshot {
                name: "frontend".into(),
                state: ServiceState::Stopped,
                pid: None,
                port: None,
                url: None,
                restart_count: 0,
            },
            ServiceSnapshot {
                name: "proxy".into(),
                state: ServiceState::Failed {
                    exit_code: Some(137),
                },
                pid: None,
                port: None,
                url: Some("{host}:{port}".into()),
                restart_count: 3,
            },
        ],
        steps: vec![
            StepSnapshot {
                name: "gcloud_auth".into(),
                state: StepState::Passed,
            },
            StepSnapshot {
                name: "uv".into(),
                state: StepState::Unknown,
            },
        ],
    });

    let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
    for (svc, line) in [
        ("backend", "INFO  starting uv server on :8080"),
        ("backend", "INFO  GET  /api/health         200  1.2ms"),
        ("backend", "INFO  POST /api/login          200  18ms"),
        ("backend", "\x1b[33mWARN \x1b[0m queue depth high (n=137)"),
        ("backend", "INFO  GET  /api/dashboards     200  4.8ms"),
        (
            "backend",
            "\x1b[31mERROR\x1b[0m upstream timeout on /api/billing",
        ),
        ("backend", "INFO  GET  /api/users/42       200  2.1ms"),
        ("backend", "INFO  GET  /api/dashboards/9   200  3.0ms"),
        ("db", "LOG: database system is ready to accept connections"),
        ("db", "LOG: checkpoint starting: time"),
    ] {
        state.apply(ServerMessage::LogChunk {
            stream: devme_core::LogStream::Stdout,
            service: svc.into(),
            bytes: enc(line),
            ts: 0,
        });
    }
    // Select the second tab so the preview shows the busier service.
    state.select_next_service();

    // Background git status for the secondary sidebar line.
    state.set_git_ahead_behind("local::kpi-dashboard", 2, 1);

    // A state transition spawns a toast (frontend Stopped → Failed).
    state.apply(ServerMessage::StatusUpdate {
        service: "frontend".into(),
        state: ServiceState::Failed { exit_code: Some(1) },
        pid: None,
        port: None,
        restart_count: 1,
    });

    let (w, h) = (110, 30);
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render(f, &mut state)).unwrap();
    print_buffer(terminal.backend().buffer());
}

fn print_buffer(buf: &Buffer) {
    use ratatui::style::{Color, Modifier, Style};

    fn ansi_for(style: Style) -> String {
        let mut codes = vec!["0".to_string()];
        if let Some(c) = style.fg {
            if let Color::Rgb(r, g, b) = c {
                codes.push(format!("38;2;{r};{g};{b}"));
            } else if let Some(n) = fg_code(c) {
                codes.push(n.to_string());
            }
        }
        if let Some(c) = style.bg {
            if let Color::Rgb(r, g, b) = c {
                codes.push(format!("48;2;{r};{g};{b}"));
            } else if let Some(n) = bg_code(c) {
                codes.push(n.to_string());
            }
        }
        if style.add_modifier.contains(Modifier::BOLD) {
            codes.push("1".into());
        }
        if style.add_modifier.contains(Modifier::REVERSED) {
            codes.push("7".into());
        }
        if style.add_modifier.contains(Modifier::DIM) {
            codes.push("2".into());
        }
        format!("\x1b[{}m", codes.join(";"))
    }

    fn fg_code(c: Color) -> Option<u32> {
        Some(match c {
            Color::Black => 30,
            Color::Red => 31,
            Color::Green => 32,
            Color::Yellow => 33,
            Color::Blue => 34,
            Color::Magenta => 35,
            Color::Cyan => 36,
            Color::Gray => 37,
            Color::DarkGray => 90,
            Color::LightRed => 91,
            Color::LightGreen => 92,
            Color::LightYellow => 93,
            Color::LightBlue => 94,
            Color::LightMagenta => 95,
            Color::LightCyan => 96,
            Color::White => 97,
            _ => return None,
        })
    }

    fn bg_code(c: Color) -> Option<u32> {
        fg_code(c).map(|n| n + 10)
    }

    println!("┌{}┐", "─".repeat(buf.area.width as usize));
    for y in 0..buf.area.height {
        print!("│");
        let mut prev = Style::default();
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            let style = cell.style();
            if style != prev {
                print!("{}", ansi_for(style));
                prev = style;
            }
            print!("{}", cell.symbol());
        }
        println!("\x1b[0m│");
    }
    println!("└{}┘", "─".repeat(buf.area.width as usize));
}
