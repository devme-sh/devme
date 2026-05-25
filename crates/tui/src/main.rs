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

use base64::Engine;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};

/// Lines per PgUp / PgDn step. A "screen" worth of scroll.
const LOG_PAGE: usize = 20;
const MOUSE_SCROLL_LINES: usize = 3;
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use devme_core::ClientMessage;
use devme_tui::discovery::Registry;
use devme_tui::render::render;
use devme_tui::state::TuiState;
use devme_tui::worktree::{AutoSpawner, WorktreeEvent};
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
    let no_shutdown = std::env::args().any(|a| a == "--no-shutdown");

    let cwd = std::env::current_dir()?;
    let repo_dir = devme_config::paths::repo_socket_dir(&cwd)?;
    let home_id = devme_config::paths::instance_id(&cwd);

    let mut registry = Registry::bind(&repo_dir).await?;
    let (wt_tx, wt_rx) = mpsc::unbounded_channel::<WorktreeEvent>();
    // Hold the spawner so its watchers stay alive for the TUI's lifetime.
    let _spawner = AutoSpawner::bind(&cwd, wt_tx).await?;
    let mut state = TuiState::default();

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &mut state, &mut registry, wt_rx, &home_id, no_shutdown).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut TuiState,
    registry: &mut Registry,
    mut wt_rx: mpsc::UnboundedReceiver<WorktreeEvent>,
    home_id: &str,
    no_shutdown: bool,
) -> anyhow::Result<()> {
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<Event>();
    tokio::task::spawn_blocking(move || {
        while let Ok(evt) = crossterm::event::read() {
            if key_tx.send(evt).is_err() {
                break;
            }
        }
    });

    // Auto-Start every attached daemon on its first `Subscribed`. Opens
    // the TUI implying "bring everything up" — matches docker compose
    // `up` UX. Tracked per id so reconnects don't re-Start (which is
    // idempotent server-side anyway, but keeps the log noise down).
    let mut started: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut home_selected = false;

    loop {
        terminal.draw(|f| render(f, state))?;

        if !home_selected && state.has_instance(home_id) {
            // Auto-select the home stack on first appearance.
            state.select_instance_by_id(home_id);
            home_selected = true;
        }
        // Send Start to any daemon whose Subscribed has populated services
        // but which we haven't sent Start to yet. Skips placeholders (no
        // services) since their daemon isn't bound.
        for id in state.attached_instance_ids() {
            if started.insert(id.clone()) {
                registry
                    .send_to(
                        &id,
                        ClientMessage::Start {
                            service: String::new(),
                            skip_deps: false,
                        },
                    )
                    .await;
            }
        }

        tokio::select! {
            evt = key_rx.recv() => match evt {
                Some(Event::Key(k)) => {
                    if matches!(k.kind, KeyEventKind::Release) {
                        continue;
                    }
                    match k.code {
                        // Copy mode: full-screen log view with mouse capture
                        // disabled for native text selection.
                        KeyCode::Esc if state.copy_mode() => {
                            state.exit_copy_mode();
                            execute!(terminal.backend_mut(), EnableMouseCapture)?;
                        }
                        KeyCode::Char('v') if !state.copy_mode() => {
                            state.enter_copy_mode();
                            execute!(terminal.backend_mut(), DisableMouseCapture)?;
                        }
                        // Scrolling still works in copy mode.
                        _ if state.copy_mode() => match k.code {
                            KeyCode::Char('j') | KeyCode::Down => state.log_scroll_down(1),
                            KeyCode::Char('k') | KeyCode::Up => state.log_scroll_up(1),
                            KeyCode::Char('g') => state.log_scroll_top(),
                            KeyCode::Char('G') => state.log_scroll_bottom(),
                            KeyCode::PageUp | KeyCode::Char('b') => state.log_page_up(LOG_PAGE),
                            KeyCode::PageDown | KeyCode::Char(' ') => state.log_page_down(LOG_PAGE),
                            KeyCode::Char('y') => {
                                copy_to_clipboard(&state.visible_log_lines());
                                state.exit_copy_mode();
                                execute!(terminal.backend_mut(), EnableMouseCapture)?;
                            }
                            KeyCode::Char('Y') => {
                                copy_to_clipboard(&state.all_log_lines());
                                state.exit_copy_mode();
                                execute!(terminal.backend_mut(), EnableMouseCapture)?;
                            }
                            KeyCode::Char('q') => {
                                state.exit_copy_mode();
                                execute!(terminal.backend_mut(), EnableMouseCapture)?;
                            }
                            _ => {}
                        }
                        // `?` toggles help; Esc inside the overlay dismisses.
                        KeyCode::Char('?') => state.toggle_help(),
                        KeyCode::Esc if state.help_visible() => state.hide_help(),
                        // `q` / Esc / Ctrl-C — shut down every attached daemon
                        // (unless --no-shutdown, which detaches instead).
                        KeyCode::Char('q') | KeyCode::Esc => {
                            if !no_shutdown {
                                registry.broadcast(ClientMessage::Shutdown).await;
                            }
                            return Ok(());
                        }
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !no_shutdown {
                                registry.broadcast(ClientMessage::Shutdown).await;
                            }
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
                        KeyCode::Char('y') => {
                            copy_to_clipboard(&state.visible_log_lines());
                        }
                        KeyCode::Char('Y') => {
                            copy_to_clipboard(&state.all_log_lines());
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
            wt = wt_rx.recv() => match wt {
                Some(WorktreeEvent::Discovered { id, label, cwd }) => {
                    // Idempotent: if a daemon has already attached and
                    // populated this instance, add_placeholder_instance is
                    // a no-op (id collision short-circuits).
                    state.add_placeholder_instance(id, label, cwd);
                }
                None => {} // sender dropped; ignore
            },
        }
    }
}

fn copy_to_clipboard(lines: &[&str]) {
    if lines.is_empty() {
        return;
    }
    let text = lines.join("\n");
    // OSC 52 escape sequence — works in most modern terminals (iTerm2,
    // Ghostty, kitty, tmux with set-clipboard on) regardless of OS.
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let _ = std::io::Write::write_all(
        &mut std::io::stdout(),
        format!("\x1b]52;c;{encoded}\x07").as_bytes(),
    );
}

