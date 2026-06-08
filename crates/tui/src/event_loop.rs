use std::io::Stdout;

use base64::Engine;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use devme_core::ClientMessage;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::discovery::Registry;
use crate::render::render;
use crate::state::TuiState;
use crate::worktree::{AutoSpawner, WorktreeEvent};

const LOG_PAGE: usize = 20;
const MOUSE_SCROLL_LINES: usize = 3;

fn enable_mouse(w: &mut impl std::io::Write) -> std::io::Result<()> {
    w.write_all(b"\x1b[?1000h\x1b[?1006h")
}

fn disable_mouse(w: &mut impl std::io::Write) -> std::io::Result<()> {
    w.write_all(b"\x1b[?1000l\x1b[?1006l")
}

/// Launch the TUI. Must be called from within a tokio runtime.
/// When `no_shutdown` is true, quitting detaches without stopping daemons.
pub async fn launch(no_shutdown: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let repo_dir = devme_config::paths::repo_socket_dir(&cwd)?;
    let home_id = devme_config::paths::instance_id(&cwd);

    let mut registry = Registry::bind(&repo_dir).await?;
    let (wt_tx, wt_rx) = mpsc::unbounded_channel::<WorktreeEvent>();
    let _spawner = AutoSpawner::bind(&cwd, wt_tx).await?;
    let mut state = TuiState::default();
    // Queue a skill prompt before entering the alt-screen loop: offer to
    // install when nothing's there, or to refresh a stale devme-managed copy
    // (or silently refresh when auto-update is on).
    state.check_skill_prompt();

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    enable_mouse(&mut stdout)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(
        &mut terminal,
        &mut state,
        &mut registry,
        wt_rx,
        &home_id,
        no_shutdown,
    )
    .await;

    disable_raw_mode()?;
    disable_mouse(terminal.backend_mut())?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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

    let mut started: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut home_selected = false;

    loop {
        terminal.draw(|f| render(f, state))?;

        if !home_selected && state.has_instance(home_id) {
            state.select_instance_by_id(home_id);
            home_selected = true;
        }
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
                        KeyCode::Esc if state.copy_mode() => {
                            state.exit_copy_mode();
                            enable_mouse(terminal.backend_mut())?;
                        }
                        KeyCode::Char('v') if !state.copy_mode() => {
                            state.enter_copy_mode();
                            disable_mouse(terminal.backend_mut())?;
                        }
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
                                enable_mouse(terminal.backend_mut())?;
                            }
                            KeyCode::Char('Y') => {
                                copy_to_clipboard(&state.all_log_lines());
                                state.exit_copy_mode();
                                enable_mouse(terminal.backend_mut())?;
                            }
                            KeyCode::Char('q') => {
                                state.exit_copy_mode();
                                enable_mouse(terminal.backend_mut())?;
                            }
                            _ => {}
                        }
                        // Skill prompt is modal: capture its keys, swallow the
                        // rest so the choice is deliberate. Install offers
                        // i/g/n; update offers u/a/n.
                        _ if state.skill_dialog_visible() => {
                            use crate::state::SkillPrompt;
                            let kind = state.skill_dialog().map(|d| d.kind);
                            match (kind, k.code) {
                                (Some(SkillPrompt::Install), KeyCode::Char('i')) => {
                                    state.apply_skill_install(false)
                                }
                                (Some(SkillPrompt::Install), KeyCode::Char('g')) => {
                                    state.apply_skill_install(true)
                                }
                                (Some(SkillPrompt::Update), KeyCode::Char('u')) => {
                                    state.apply_skill_update(false)
                                }
                                (Some(SkillPrompt::Update), KeyCode::Char('a')) => {
                                    state.apply_skill_update(true)
                                }
                                (_, KeyCode::Char('n') | KeyCode::Esc) => {
                                    state.dismiss_skill_dialog()
                                }
                                _ => {}
                            }
                        }
                        KeyCode::Char('?') => state.toggle_help(),
                        KeyCode::Esc if state.help_visible() => state.hide_help(),
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
                        KeyCode::Char('o') => {
                            // Open the focused service's local URL in the
                            // browser. No-op for services without a port.
                            if let Some(port) = state.selected_service().and_then(|s| s.port) {
                                let _ = devme_config::browser::open_url(
                                    &format!("http://localhost:{port}"),
                                );
                            }
                        }
                        KeyCode::Char('y') => {
                            copy_to_clipboard(&state.visible_log_lines());
                        }
                        KeyCode::Char('Y') => {
                            copy_to_clipboard(&state.all_log_lines());
                        }
                        KeyCode::Char('p') => {
                            copy_to_clipboard(&[&build_debug_prompt(state)]);
                        }
                        _ => {}
                    }
                }
                Some(Event::Mouse(me)) => match me.kind {
                    MouseEventKind::ScrollUp => state.log_scroll_up(MOUSE_SCROLL_LINES),
                    MouseEventKind::ScrollDown => state.log_scroll_down(MOUSE_SCROLL_LINES),
                    _ => {}
                }
                Some(_) => {}
                None => return Ok(()),
            },
            tagged = registry.recv() => match tagged {
                Some(t) => state.apply_from(&t.instance_id, t.message),
                None => return Ok(()),
            },
            wt = wt_rx.recv() => match wt {
                Some(WorktreeEvent::Discovered { id, label, cwd }) => {
                    state.add_placeholder_instance(id, label, cwd);
                }
                None => {}
            },
        }
    }
}

fn copy_to_clipboard(lines: &[&str]) {
    if lines.is_empty() {
        return;
    }
    let text = lines.join("\n");
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let _ = std::io::Write::write_all(
        &mut std::io::stdout(),
        format!("\x1b]52;c;{encoded}\x07").as_bytes(),
    );
}

fn build_debug_prompt(state: &TuiState) -> String {
    let cwd = state.current_instance_cwd();
    let label = if state.shared_selected() { "shared" } else { state.instance_label() };
    let services = state.services();

    let mut prompt = format!("My devme dev environment ({label}).\n\nWorking directory: {cwd}\n\n");

    if services.is_empty() {
        let has_toml = std::path::Path::new(cwd).join("devme.toml").exists();
        if !has_toml {
            prompt.push_str("No devme.toml found in this directory.\n\n");
        } else {
            prompt.push_str("devme.toml exists but no services are running (daemon may not have started yet).\n\n");
        }
    } else {
        prompt.push_str("## Service states\n\n");
        for svc in &services {
            prompt.push_str(&format!("- **{}**: {:?}", svc.name, svc.state));
            if let Some(pid) = svc.pid {
                prompt.push_str(&format!(" (pid {})", pid));
            }
            if let Some(port) = svc.port {
                prompt.push_str(&format!(" (port {})", port));
            }
            if svc.restart_count > 0 {
                prompt.push_str(&format!(" ({} restarts)", svc.restart_count));
            }
            prompt.push('\n');
        }
        prompt.push('\n');

        for svc in &services {
            let logs = state.service_logs(&svc.name);
            if logs.is_empty() {
                continue;
            }
            let tail: Vec<&str> = if logs.len() > 30 {
                logs.iter().skip(logs.len() - 30).map(|s| s.as_str()).collect()
            } else {
                logs.iter().map(|s| s.as_str()).collect()
            };
            prompt.push_str(&format!("## {} logs (last {})\n\n```\n", svc.name, tail.len()));
            for line in &tail {
                prompt.push_str(line);
                prompt.push('\n');
            }
            prompt.push_str("```\n\n");
        }
    }

    for step in state.steps() {
        if matches!(step.state, devme_core::StepState::Failed | devme_core::StepState::ProvisionFailed) {
            prompt.push_str(&format!("Step `{}` is {:?}.\n", step.name, step.state));
            let logs = state.service_logs(&step.name);
            if !logs.is_empty() {
                prompt.push_str("\n```\n");
                for line in logs.iter() {
                    prompt.push_str(line);
                    prompt.push('\n');
                }
                prompt.push_str("```\n\n");
            }
        }
    }

    if let Ok(toml) = std::fs::read_to_string(
        std::path::Path::new(cwd).join("devme.toml"),
    ) {
        prompt.push_str("## devme.toml\n\n```toml\n");
        prompt.push_str(&toml);
        prompt.push_str("```\n\n");
    }

    prompt.push_str("Inspect the environment and help me diagnose any issues.");
    prompt
}
