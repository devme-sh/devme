//! Render a populated TUI state once into an in-memory TestBackend and
//! dump the buffer to stdout. Useful for eyeballing layout changes
//! without leaving the editor — `cargo run --example preview -p devme-tui`.

use devme_core::{
    ServerMessage, ServiceSnapshot, ServiceState, StepSnapshot, StepState,
};
use devme_tui::render::render;
use devme_tui::state::TuiState;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

fn main() {
    let mut state = TuiState::default();
    state.set_instance_label("kpi-dashboard");
    state.apply(ServerMessage::Subscribed {
        services: vec![
            ServiceSnapshot {
                name: "db".into(),
                state: ServiceState::Running {
                    degraded: false,
                    started_without: vec![],
                },
                pid: Some(12345),
                port: Some(5432),
                restart_count: 0,
            },
            ServiceSnapshot {
                name: "backend".into(),
                state: ServiceState::Starting,
                pid: Some(12346),
                port: Some(8080),
                restart_count: 0,
            },
            ServiceSnapshot {
                name: "frontend".into(),
                state: ServiceState::Stopped,
                pid: None,
                port: None,
                restart_count: 0,
            },
            ServiceSnapshot {
                name: "proxy".into(),
                state: ServiceState::Failed {
                    exit_code: Some(137),
                },
                pid: None,
                port: None,
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

    let (w, h) = (100, 30);
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render(f, &state)).unwrap();
    print_buffer(terminal.backend().buffer());
}

fn print_buffer(buf: &Buffer) {
    use ratatui::style::{Color, Modifier, Style};

    fn ansi_for(style: Style) -> String {
        let mut codes = vec!["0".to_string()];
        if let Some(c) = style.fg
            && let Some(n) = fg_code(c)
        {
            codes.push(n.to_string());
        }
        if let Some(c) = style.bg
            && let Some(n) = bg_code(c)
        {
            codes.push(n.to_string());
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
