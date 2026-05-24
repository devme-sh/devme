//! `devme-tui` — discovers every supervisor on this host, multiplexes their
//! event streams, and drives the TUI.
//!
//! Wires together:
//! - [`devme_tui::discovery::Registry`] — one client per running daemon,
//!   with a directory watcher that picks up new worktrees while the TUI
//!   is open
//! - `crossterm` for raw-mode terminal input
//! - `ratatui` + [`devme_tui::render`] for drawing
//!
//! Quit on `q`, `Esc`, or `Ctrl-C`. Navigate stacks with `j`/`k`, services
//! with `h`/`l`.

use std::io::Stdout;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};

/// Lines per PgUp / PgDn step. A "screen" worth of scroll.
const LOG_PAGE: usize = 20;
/// Lines per mouse-wheel notch. Trackpads emit many events; 3 is the
/// terminal-emulator-typical value (matches xterm, iterm2).
const MOUSE_SCROLL_LINES: usize = 3;
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use devme_core::ClientMessage;
use devme_tui::discovery::Registry;
use devme_tui::render::render;
use devme_tui::state::TuiState;
use devme_tui::worktree::AutoSpawner;
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
            let _ = disable_raw_mode();
            let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
            eprintln!("devme-tui: {e}");
            1
        }
    };
    // Force exit so the spawn_blocking thread reading from crossterm's
    // (blocking) event stream doesn't keep the process alive.
    std::process::exit(exit_code);
}

async fn real_main() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let runtime_dir = devme_config::paths::runtime_dir()?;
    let home_id = devme_config::paths::instance_id(&cwd);

    let mut registry = Registry::bind(&runtime_dir).await?;
    // Hold the spawner so its watcher stays alive for the TUI's lifetime.
    let _spawner = AutoSpawner::bind(&cwd).await?;
    let mut state = TuiState::default();

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // EnableMouseCapture lets us see scroll-wheel events. Trade-off: it
    // also captures clicks/drags, so the terminal's native text selection
    // is intercepted — on macOS Terminal/iTerm, hold Option to bypass.
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &mut state, &mut registry, &home_id).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut TuiState,
    registry: &mut Registry,
    home_id: &str,
) -> anyhow::Result<()> {
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<Event>();
    tokio::task::spawn_blocking(move || {
        while let Ok(evt) = crossterm::event::read() {
            if key_tx.send(evt).is_err() {
                break;
            }
        }
    });

    // Send Start to the cwd daemon (if it attaches) so opening the TUI
    // implies "bring this stack up" — matches docker compose `up` UX.
    // We do this once when the home daemon first appears, not eagerly,
    // because the daemon may not have finished binding yet.
    let mut home_started = false;

    loop {
        terminal.draw(|f| render(f, state))?;

        if !home_started && state.has_instance(home_id) {
            registry
                .send_to(
                    home_id,
                    ClientMessage::Start {
                        service: String::new(),
                        skip_deps: false,
                    },
                )
                .await;
            // Auto-select the home stack on first appearance.
            state.select_instance_by_id(home_id);
            home_started = true;
        }

        tokio::select! {
            evt = key_rx.recv() => match evt {
                Some(Event::Key(k)) => {
                    if matches!(k.kind, KeyEventKind::Release) {
                        continue;
                    }
                    match k.code {
                        // `?` toggles help; Esc inside the overlay dismisses.
                        KeyCode::Char('?') => state.toggle_help(),
                        KeyCode::Esc if state.help_visible() => state.hide_help(),
                        // `q` / Esc / Ctrl-C — shut down every attached daemon.
                        KeyCode::Char('q') | KeyCode::Esc => {
                            registry.broadcast(ClientMessage::Shutdown).await;
                            return Ok(());
                        }
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            registry.broadcast(ClientMessage::Shutdown).await;
                            return Ok(());
                        }
                        // Detach: leave the TUI but keep services running.
                        KeyCode::Char('D') => return Ok(()),
                        KeyCode::Down | KeyCode::Char('j') => state.select_next_instance(),
                        KeyCode::Up | KeyCode::Char('k') => state.select_prev_instance(),
                        KeyCode::Right | KeyCode::Char('l') => state.select_next_service(),
                        KeyCode::Left | KeyCode::Char('h') => state.select_prev_service(),
                        KeyCode::PageUp | KeyCode::Char('b') => state.log_page_up(LOG_PAGE),
                        KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => {
                            state.log_page_down(LOG_PAGE)
                        }
                        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            state.log_scroll_up(LOG_PAGE / 2);
                        }
                        KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            state.log_scroll_down(LOG_PAGE / 2);
                        }
                        KeyCode::Char('J') => state.log_scroll_down(1),
                        KeyCode::Char('K') => state.log_scroll_up(1),
                        KeyCode::Char('g') => state.log_scroll_top(),
                        KeyCode::Char('G') => state.log_scroll_bottom(),
                        KeyCode::Char('S') => {
                            if let Some((id, name)) = state.selected_instance_and_service() {
                                registry
                                    .send_to(&id, ClientMessage::Start { service: name, skip_deps: false })
                                    .await;
                            }
                        }
                        KeyCode::Char('s') => {
                            if let Some((id, name)) = state.selected_instance_and_service() {
                                registry.send_to(&id, ClientMessage::Stop { service: name }).await;
                            }
                        }
                        KeyCode::Char('r') => {
                            if let Some((id, name)) = state.selected_instance_and_service() {
                                registry.send_to(&id, ClientMessage::Restart { service: name }).await;
                            }
                        }
                        _ => {}
                    }
                }
                Some(Event::Mouse(me)) => match me.kind {
                    MouseEventKind::ScrollUp => state.log_scroll_up(MOUSE_SCROLL_LINES),
                    MouseEventKind::ScrollDown => state.log_scroll_down(MOUSE_SCROLL_LINES),
                    _ => {}
                }
                Some(_) => {} // resize — handled by redraw on next loop
                None => return Ok(()),
            },
            tagged = registry.recv() => match tagged {
                Some(t) => state.apply_from(&t.instance_id, t.message),
                None => return Ok(()),
            },
        }
    }
}
