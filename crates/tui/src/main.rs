//! `devme-tui` — opens the supervisor socket and drives the TUI.
//!
//! Wires together:
//! - `devme_client::Client` for the IPC subscription stream
//! - `crossterm` for raw-mode terminal input
//! - `ratatui` + `devme_tui::render` for drawing
//!
//! Quit on `q`, `Esc`, or `Ctrl-C`. Navigate tabs with `↑`/`↓` or `h`/`l`.

use std::io::Stdout;
use std::path::Path;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use devme_client::Client;
use devme_core::ClientMessage;
use devme_tui::render::render;
use devme_tui::state::TuiState;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

fn main() {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("devme-tui: tokio init failed: {e}");
            std::process::exit(1);
        }
    };
    let exit_code = match runtime.block_on(real_main()) {
        Ok(()) => 0,
        Err(e) => {
            // Ensure we leave the terminal in a sane state before printing.
            let _ = disable_raw_mode();
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
            eprintln!("devme-tui: {e}");
            1
        }
    };
    // Force exit so the spawn_blocking thread reading from crossterm's
    // (blocking) event stream doesn't keep the process alive — its blocking
    // syscall ignores tokio runtime shutdown, so otherwise `q` would render
    // cleanup but leave the user staring at a hung terminal until Ctrl-C.
    std::process::exit(exit_code);
}

async fn real_main() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let sock = devme_config::paths::supervisor_socket(&cwd)?;

    let mut client = Client::connect(&sock).await?;
    client
        .send(ClientMessage::Subscribe { services: vec![] })
        .await?;
    // Opening the TUI implies "bring this stack up." Start is idempotent —
    // services already running stay running; only newly-eligible ones spawn.
    // Services the user has explicitly stopped this session stay stopped
    // because the executor still has them tracked as Stopped.
    client
        .send(ClientMessage::Start {
            service: String::new(),
            skip_deps: false,
        })
        .await?;

    let mut state = TuiState::default();
    if let Some(name) = cwd.file_name().and_then(|s| s.to_str()) {
        state.set_instance_label(name);
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &mut state, &mut client).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut TuiState,
    client: &mut Client,
) -> anyhow::Result<()> {
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<Event>();
    tokio::task::spawn_blocking(move || {
        while let Ok(evt) = crossterm::event::read() {
            if key_tx.send(evt).is_err() {
                break;
            }
        }
    });

    loop {
        terminal.draw(|f| render(f, state))?;

        tokio::select! {
            evt = key_rx.recv() => match evt {
                Some(Event::Key(k)) => {
                    if matches!(k.kind, KeyEventKind::Release) {
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(());
                        }
                        // Vertical = instance list (sidebar).
                        KeyCode::Down | KeyCode::Char('j') => state.select_next_instance(),
                        KeyCode::Up | KeyCode::Char('k') => state.select_prev_instance(),
                        // Horizontal = service tabs.
                        KeyCode::Right | KeyCode::Char('l') => state.select_next_service(),
                        KeyCode::Left | KeyCode::Char('h') => state.select_prev_service(),
                        KeyCode::Char('S') => {
                            if let Some(name) = state.selected_service().map(|s| s.name.clone()) {
                                let _ = client
                                    .send(ClientMessage::Start { service: name, skip_deps: false })
                                    .await;
                            }
                        }
                        KeyCode::Char('s') => {
                            if let Some(name) = state.selected_service().map(|s| s.name.clone()) {
                                let _ = client
                                    .send(ClientMessage::Stop { service: name })
                                    .await;
                            }
                        }
                        KeyCode::Char('r') => {
                            if let Some(name) = state.selected_service().map(|s| s.name.clone()) {
                                let _ = client
                                    .send(ClientMessage::Restart { service: name })
                                    .await;
                            }
                        }
                        _ => {}
                    }
                }
                Some(_) => {} // resize, mouse — handled by redraw on next loop
                None => return Ok(()),
            },
            msg = client.next_event() => match msg? {
                Some(m) => state.apply(m),
                None => return Ok(()),
            },
        }
    }
}

#[allow(dead_code)]
fn _suppress_unused(_p: &Path) {}
