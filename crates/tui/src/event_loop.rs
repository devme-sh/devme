use std::io::Stdout;

use base64::Engine;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use devme_core::{ClientMessage, ServiceSnapshot};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::discovery::Registry;
use crate::keymap;
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

    // Load config (surfacing a warning if it failed to parse, rather than
    // silently discarding it) and resolve the colour theme before raw mode
    // is on. `auto` queries the terminal background (OSC 11), which needs to
    // happen while we own stdin and before the alt-screen swallows the reply.
    let (cfg, cfg_warning) = devme_config::GlobalConfig::load_checked();
    let theme_name = cfg.get("tui.theme").unwrap_or_else(|| "mocha".into());
    state.set_palette(crate::theme::Palette::resolve(&theme_name));
    state.set_config(cfg);
    if let Some(warning) = cfg_warning {
        state.push_config_warning(warning);
    }

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

    // Animation/expiry tick — drives the service spinner and toast timeout.
    let mut anim = tokio::time::interval(std::time::Duration::from_millis(120));
    anim.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Background git refresh, fanned out to a detached task so a slow `git`
    // never stalls the UI; results flow back over this channel as
    // `(instance id, current branch, ahead/behind)`. The branch keeps the
    // sidebar label in sync when a worktree checks out a different branch.
    type GitRefresh = (String, Option<String>, Option<(usize, usize)>);
    let (git_tx, mut git_rx) = mpsc::unbounded_channel::<GitRefresh>();
    let mut git_refresh = tokio::time::interval(std::time::Duration::from_secs(5));

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
                        _ if state.copy_mode() => match k.code {
                            KeyCode::Char('j') | KeyCode::Down => state.log_scroll_down(1),
                            KeyCode::Char('k') | KeyCode::Up => state.log_scroll_up(1),
                            KeyCode::Char('g') => state.log_scroll_top(),
                            KeyCode::Char('G') => state.log_scroll_bottom(),
                            KeyCode::PageUp | KeyCode::Char('b') => state.log_page_up(LOG_PAGE),
                            KeyCode::PageDown | KeyCode::Char(' ') => state.log_page_down(LOG_PAGE),
                            KeyCode::Char('y') => {
                                let n = {
                                    let lines = state.visible_log_lines();
                                    copy_to_clipboard(&lines);
                                    lines.len()
                                };
                                state.notify_transient("copy", copied_lines_msg("visible", n));
                                state.exit_copy_mode();
                                enable_mouse(terminal.backend_mut())?;
                            }
                            KeyCode::Char('Y') => {
                                let n = {
                                    let lines = state.all_log_lines();
                                    copy_to_clipboard(&lines);
                                    lines.len()
                                };
                                state.notify_transient("copy", copied_lines_msg("all", n));
                                state.exit_copy_mode();
                                enable_mouse(terminal.backend_mut())?;
                            }
                            KeyCode::Char('q') => {
                                state.exit_copy_mode();
                                enable_mouse(terminal.backend_mut())?;
                            }
                            _ => {}
                        }
                        // Stopped state owns the whole frame after an external
                        // `devme down`: `u`/Enter brings the stack back up in
                        // place (the discovery registry reattaches the fresh
                        // daemons, which clears this state), `q`/Esc/^C quit the
                        // dashboard. Everything else is swallowed — there's
                        // nothing on screen to act on.
                        _ if state.stopped() => match k.code {
                            KeyCode::Char('u')
                            | KeyCode::Char('U')
                            | KeyCode::Char('S')
                            | KeyCode::Enter => {
                                if let Ok(cwd) = std::env::current_dir() {
                                    state.notify("devme", "starting the stack…");
                                    tokio::spawn(async move {
                                        crate::worktree::start_all(&cwd).await;
                                    });
                                }
                            }
                            KeyCode::Char('c')
                                if k.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                return Ok(());
                            }
                            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                                return Ok(());
                            }
                            _ => {}
                        },
                        // Port-conflict modal takes top priority — a service
                        // crashed on bind. ↑↓/jk move, Enter runs the chosen
                        // remediation off-thread then restarts the service,
                        // Esc/n skips.
                        _ if state.port_conflict_visible() => match k.code {
                            KeyCode::Up | KeyCode::Char('k') => state.port_conflict_move(-1),
                            KeyCode::Down | KeyCode::Char('j') => state.port_conflict_move(1),
                            KeyCode::Enter => {
                                if let Some((id, service, action)) =
                                    state.take_port_conflict_choice()
                                {
                                    use crate::state::PortConflictAction as A;
                                    if !matches!(action, A::Skip) {
                                        let svc = service.clone();
                                        let outcome = tokio::task::spawn_blocking(move || {
                                            run_port_remediation(&action)
                                        })
                                        .await;
                                        match outcome {
                                            Ok(Ok(label)) => {
                                                state.push_port_conflict_result(
                                                    true,
                                                    format!("{label} — restarting {svc}"),
                                                );
                                                registry
                                                    .send_to(
                                                        &id,
                                                        ClientMessage::Restart { service },
                                                    )
                                                    .await;
                                            }
                                            Ok(Err(e)) => {
                                                state.push_port_conflict_result(false, e)
                                            }
                                            Err(_) => state.push_port_conflict_result(
                                                false,
                                                "remediation failed to run".to_string(),
                                            ),
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('n') | KeyCode::Esc => state.dismiss_port_conflict(),
                            _ => {}
                        },
                        // Zoom (fullscreen logs) captures navigation so the
                        // hidden sidebar/tabs don't move invisibly. `z`/Esc/q
                        // leave it; h/l still switch service.
                        _ if state.zoom() => match k.code {
                            KeyCode::Char('z') | KeyCode::Esc | KeyCode::Char('q') => {
                                state.exit_zoom()
                            }
                            KeyCode::Char('j') | KeyCode::Down => state.log_scroll_down(1),
                            KeyCode::Char('k') | KeyCode::Up => state.log_scroll_up(1),
                            KeyCode::Char('g') => state.log_scroll_top(),
                            KeyCode::Char('G') => state.log_scroll_bottom(),
                            KeyCode::PageUp | KeyCode::Char('b') => state.log_page_up(LOG_PAGE),
                            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => {
                                state.log_page_down(LOG_PAGE)
                            }
                            KeyCode::Char('h') | KeyCode::Left => state.select_prev_service(),
                            KeyCode::Char('l') | KeyCode::Right => state.select_next_service(),
                            KeyCode::Char('y') => copy_to_clipboard(&state.visible_log_lines()),
                            KeyCode::Char('Y') => copy_to_clipboard(&state.all_log_lines()),
                            _ => {}
                        }
                        // Quit confirmation is modal: y/Enter commits the quit
                        // (sibling-safe shutdown, like `devme down`), anything
                        // else cancels.
                        _ if state.quit_confirm_visible() => match k.code {
                            // Press `q` again (or s/y/Enter) to stop every
                            // service, then quit (like `devme down`).
                            KeyCode::Char('q')
                            | KeyCode::Char('s')
                            | KeyCode::Char('y')
                            | KeyCode::Enter => {
                                if !no_shutdown
                                    && let Ok(cwd) = std::env::current_dir()
                                {
                                    crate::worktree::shutdown_current_and_shared(&cwd).await;
                                }
                                return Ok(());
                            }
                            // Detach: quit the TUI but leave services running —
                            // for `devme remote` this keeps the remote stack up.
                            KeyCode::Char('d') | KeyCode::Char('D') => return Ok(()),
                            KeyCode::Char('n') | KeyCode::Esc => state.cancel_quit_confirm(),
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
                        // Settings overlay is modal: route its keys here and
                        // swallow the rest. Changes persist + apply live.
                        _ if state.settings_visible() => match k.code {
                            KeyCode::Up | KeyCode::Char('k') => state.settings_move(-1),
                            KeyCode::Down | KeyCode::Char('j') => state.settings_move(1),
                            KeyCode::Left | KeyCode::Char('h') => persist_setting(state, -1),
                            KeyCode::Right
                            | KeyCode::Char('l')
                            | KeyCode::Char(' ')
                            | KeyCode::Enter => persist_setting(state, 1),
                            KeyCode::Esc | KeyCode::Char(',') | KeyCode::Char('q') => {
                                state.close_settings()
                            }
                            _ => {}
                        },
                        // Notifications-history modal: j/k (or arrows) move the
                        // cursor; c/Enter copy the selected notification, Y the
                        // whole history; n/Esc/q close it. (Click-to-copy is in
                        // the mouse arm below.)
                        _ if state.notifications_visible() => match k.code {
                            KeyCode::Up | KeyCode::Char('k') => state.notif_cursor_up(1),
                            KeyCode::Down | KeyCode::Char('j') => state.notif_cursor_down(1),
                            KeyCode::Char('c') | KeyCode::Enter => {
                                if let Some(text) = state.notif_selected_text() {
                                    copy_to_clipboard(&[&text]);
                                    state.notify_transient("copy", "notification copied");
                                }
                            }
                            KeyCode::Char('Y') => {
                                let text = state.notif_all_text();
                                if !text.is_empty() {
                                    copy_to_clipboard(&[&text]);
                                    state.notify_transient("copy", "all notifications copied");
                                }
                            }
                            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                                state.close_notifications()
                            }
                            _ => {}
                        },
                        // Esc closes the help overlay first; otherwise it falls
                        // through to `resolve` (→ Quit). Kept out of the keymap
                        // because it's context-dependent on the overlay.
                        KeyCode::Esc if state.help_visible() => state.hide_help(),
                        // Everything else routes through the keymap — the single
                        // source of truth that also drives the help overlay and
                        // footer (see `crate::keymap`). The match below is
                        // exhaustive, so a new `Action` can't be added without a
                        // handler here *and* a help entry there.
                        _ => {
                            let Some(action) = keymap::resolve(&k) else { continue };
                            use keymap::Action;
                            match action {
                                Action::NextService => state.select_next_service(),
                                Action::PrevService => state.select_prev_service(),
                                Action::NextStack => state.select_next_instance(),
                                Action::PrevStack => state.select_prev_instance(),
                                Action::ToggleSidebar => state.toggle_sidebar(),
                                Action::PageUp => state.log_page_up(LOG_PAGE),
                                Action::PageDown => state.log_page_down(LOG_PAGE),
                                Action::HalfPageUp => state.log_scroll_up(LOG_PAGE / 2),
                                Action::HalfPageDown => state.log_scroll_down(LOG_PAGE / 2),
                                Action::LineDown => state.log_scroll_down(1),
                                Action::LineUp => state.log_scroll_up(1),
                                Action::ScrollTop => state.log_scroll_top(),
                                Action::ScrollBottom => state.log_scroll_bottom(),
                                Action::CopyVisibleLogs => {
                                    // Scope the immutable borrow (the lines
                                    // reference state's log buffers) so `notify`
                                    // can take &mut.
                                    let n = {
                                        let lines = state.visible_log_lines();
                                        copy_to_clipboard(&lines);
                                        lines.len()
                                    };
                                    state.notify_transient("copy", copied_lines_msg("visible", n));
                                }
                                Action::CopyAllLogs => {
                                    let n = {
                                        let lines = state.all_log_lines();
                                        copy_to_clipboard(&lines);
                                        lines.len()
                                    };
                                    state.notify_transient("copy", copied_lines_msg("all", n));
                                }
                                Action::CopyDebugPrompt => {
                                    let prompt = build_debug_prompt(state);
                                    copy_to_clipboard(&[&prompt]);
                                    state.notify_transient("copy", "debug prompt copied to clipboard");
                                }
                                Action::CopyMode => {
                                    state.enter_copy_mode();
                                    disable_mouse(terminal.backend_mut())?;
                                }
                                Action::ZoomLogs => state.toggle_zoom(),
                                Action::StartService => {
                                    if let Some((id, name)) = state.selected_instance_and_service() {
                                        registry
                                            .send_to(&id, ClientMessage::Start { service: name, skip_deps: false })
                                            .await;
                                    }
                                }
                                Action::StopService => {
                                    if let Some((id, name)) = state.selected_instance_and_service() {
                                        registry.send_to(&id, ClientMessage::Stop { service: name }).await;
                                    }
                                }
                                Action::RestartService => {
                                    if let Some((id, name)) = state.selected_instance_and_service() {
                                        registry.send_to(&id, ClientMessage::Restart { service: name }).await;
                                    }
                                }
                                Action::OpenUrl => {
                                    // Open the focused service's URL in the
                                    // browser, with a toast so it's never a
                                    // silent no-op. Only web (`http(s)://`)
                                    // services are openable; a database's bare
                                    // `host:port` is copied instead.
                                    let sel = state
                                        .selected_service()
                                        .map(|s| (s.name.clone(), s.url.clone(), s.port));
                                    match sel {
                                        None => {}
                                        Some((name, None, _)) => {
                                            state.notify("open", format!("{name} has no URL"));
                                        }
                                        // On a remote TUI the browser would open
                                        // on the headless host, not the laptop —
                                        // point the user at `c` (copy) instead,
                                        // which rides OSC 52 back.
                                        Some((_, Some(_), _)) if is_remote_tui() => {
                                            state.notify("open", "remote stack — press c to copy the URL");
                                        }
                                        Some((name, Some(tmpl), port)) => {
                                            match resolve_service_url(&tmpl, port, "localhost") {
                                                None => state.notify(
                                                    "open",
                                                    format!("{name} has no port yet"),
                                                ),
                                                Some(url)
                                                    if url.starts_with("http://")
                                                        || url.starts_with("https://") =>
                                                {
                                                    match devme_config::browser::open_url(&url) {
                                                        Ok(()) => state.notify("open", url),
                                                        Err(e) => state.notify(
                                                            "open",
                                                            format!("couldn't open: {e}"),
                                                        ),
                                                    }
                                                }
                                                // No scheme → not browser-openable
                                                // (a db `host:port`, or a web
                                                // service with no `url`/http
                                                // health to tell us so). Copy the
                                                // address and point at the fix.
                                                Some(url) => {
                                                    copy_to_clipboard(&[&url]);
                                                    state.notify(
                                                        "open",
                                                        format!(
                                                            "{name}: no open URL — copied {url} (set url= to open)"
                                                        ),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                Action::CopyUrl => {
                                    // Copy the focused service's URL to the
                                    // clipboard. Uses the service-URL host (the
                                    // remote-injected DEVME_URL_HOST, e.g. a
                                    // Tailscale name, else localhost) so a copy
                                    // from a remote TUI is reachable from the
                                    // laptop. OSC 52 carries it back over SSH.
                                    let sel = state
                                        .selected_service()
                                        .map(|s| (s.name.clone(), s.url.clone(), s.port));
                                    match sel {
                                        None => {}
                                        Some((name, None, _)) => {
                                            state.notify_transient("copy", format!("{name} has no URL"));
                                        }
                                        Some((name, Some(tmpl), port)) => {
                                            match resolve_service_url(&tmpl, port, &service_url_host()) {
                                                None => state.notify(
                                                    "copy",
                                                    format!("{name} has no port yet"),
                                                ),
                                                Some(url) => {
                                                    copy_to_clipboard(&[&url]);
                                                    state.notify_transient("copy", format!("copied {url}"));
                                                }
                                            }
                                        }
                                    }
                                }
                                Action::Settings => state.open_settings(),
                                Action::Notifications => state.toggle_notifications(),
                                // Re-read global.toml (`R`, vs lowercase `r` =
                                // restart service) so an external `devme config
                                // set` applies live.
                                Action::ReloadConfig => state.reload_config(),
                                // Detach: leave every daemon running in the
                                // background (use `devme up -d` to start that way
                                // deliberately).
                                Action::Detach => return Ok(()),
                                Action::Quit => {
                                    // With `tui.confirm_quit`, ask first — but
                                    // only when quitting would actually stop
                                    // services (not detach).
                                    if state.confirm_quit_enabled() && !no_shutdown {
                                        state.open_quit_confirm();
                                    } else {
                                        if !no_shutdown
                                            && let Ok(cwd) = std::env::current_dir()
                                        {
                                            // Stop this stack + the shared
                                            // services (sibling-safe), exactly
                                            // like `devme down`.
                                            crate::worktree::shutdown_current_and_shared(&cwd).await;
                                        }
                                        return Ok(());
                                    }
                                }
                                Action::ToggleHelp => state.toggle_help(),
                            }
                        }
                    }
                }
                Some(Event::Mouse(me)) => match me.kind {
                    MouseEventKind::ScrollUp => state.log_scroll_up(MOUSE_SCROLL_LINES),
                    MouseEventKind::ScrollDown => state.log_scroll_down(MOUSE_SCROLL_LINES),
                    // In the notifications modal, a left-click on a row copies
                    // that notification (and moves the cursor to it).
                    MouseEventKind::Down(MouseButton::Left) if state.notifications_visible() => {
                        if let Some(text) = state.notif_copy_at(me.column, me.row) {
                            copy_to_clipboard(&[&text]);
                            state.notify_transient("copy", "notification copied");
                        }
                    }
                    // Left-click: drive the scrollbar if the press lands on it,
                    // otherwise select the clicked sidebar row or service tab.
                    // Clicks under a modal are ignored (the chrome behind it
                    // is still recorded in the hit-map, but shouldn't react).
                    MouseEventKind::Down(MouseButton::Left) if !state.any_modal_open() => {
                        if state.sidebar_divider_at(me.column, me.row) {
                            state.begin_sidebar_drag();
                            state.sidebar_drag_to(me.column);
                        } else if state.scrollbar_at(me.column, me.row) {
                            state.begin_scrollbar_drag();
                            state.scrollbar_drag_to(me.row);
                        } else {
                            state.click_at(me.column, me.row);
                        }
                    }
                    // Keep steering the divider through the drag even if the
                    // pointer slides off the one-column divider.
                    MouseEventKind::Drag(MouseButton::Left) if state.sidebar_dragging() => {
                        state.sidebar_drag_to(me.column);
                    }
                    // Keep steering the scrollbar through the drag even if the
                    // pointer slides off the one-column track.
                    MouseEventKind::Drag(MouseButton::Left) if state.scrollbar_dragging() => {
                        state.scrollbar_drag_to(me.row);
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        state.end_sidebar_drag();
                        state.end_scrollbar_drag();
                    }
                    _ => {}
                }
                Some(_) => {}
                None => return Ok(()),
            },
            tagged = registry.recv() => match tagged {
                Some(t) => {
                    state.apply_from(&t.instance_id, t.message);
                    // A deliberate `devme down`/quit elsewhere drains every
                    // daemon (each sends Goodbye). Rather than exit, park in a
                    // stopped state and keep watching the socket dir: the TUI
                    // is a durable dashboard, so a later `devme up` reattaches
                    // and repopulates it in place. The TUI's own quit returns
                    // directly and never reaches here, so reaching this with
                    // everything drained is always an *external* shutdown. A
                    // crash leaves the row intact, so this never fires merely
                    // because a service died.
                    if state.all_daemons_shut_down() {
                        if !state.stopped() {
                            let repo = std::env::current_dir().ok().and_then(|d| {
                                d.file_name().map(|n| n.to_string_lossy().into_owned())
                            });
                            state.enter_stopped(repo);
                            // Re-arm "send Start once" so the next `up` (or the
                            // stopped screen's `u`) actually starts services on
                            // the reattached daemons.
                            started.clear();
                        }
                    } else if state.stopped() {
                        // A daemon attached again — back to the live dashboard.
                        state.clear_stopped();
                    }
                    // A crash-on-bind queued a probe — identify the holder
                    // off-thread (docker/lsof) and raise the modal.
                    if let Some((id, service, port)) = state.take_pending_port_conflict() {
                        let holder = tokio::task::spawn_blocking(move || {
                            devme_supervisor::port_preflight::identify_holder(port)
                        })
                        .await
                        .unwrap_or(devme_supervisor::port_preflight::Holder::Unknown);
                        state.open_port_conflict(id, service, port, holder);
                    }
                }
                None => return Ok(()),
            },
            wt = wt_rx.recv() => match wt {
                Some(WorktreeEvent::Discovered { id, label, cwd }) => {
                    state.add_placeholder_instance(id, label, cwd);
                }
                None => {}
            },
            _ = anim.tick() => {
                state.tick();
            }
            _ = git_refresh.tick() => {
                let pairs = state.instance_id_cwd_pairs();
                let tx = git_tx.clone();
                tokio::spawn(async move {
                    for (id, cwd) in pairs {
                        // Always report the branch (even when there's no
                        // upstream, so a checkout still re-labels the row);
                        // ahead/behind rides along when an upstream exists.
                        let branch = crate::worktree::git_branch(&cwd).await;
                        let ahead_behind = crate::worktree::git_ahead_behind(&cwd).await;
                        let _ = tx.send((id, branch, ahead_behind));
                    }
                });
            }
            git = git_rx.recv() => {
                if let Some((id, branch, ahead_behind)) = git {
                    state.apply_git_refresh(&id, branch, ahead_behind);
                }
            }
        }
    }
}

/// Apply a settings-overlay edit (`dir` = +1 next / -1 prev) and write it
/// back to `global.toml`, surfacing a toast if the write fails. A change can
/// either set a value or remove the key (the `(auto)` choice).
fn persist_setting(state: &mut TuiState, dir: i32) {
    use crate::state::SettingWrite;
    let Some(write) = state.settings_change(dir) else {
        return;
    };
    let result = match &write {
        SettingWrite::Set { key, value } => devme_config::GlobalConfig::persist(key, value),
        SettingWrite::Unset { key } => devme_config::GlobalConfig::unset_persisted(key),
    };
    if let Err(err) = result {
        state.push_config_warning(format!("couldn't save {}: {err}", write.key()));
    }
}

/// Carry out a port-conflict remediation off the UI thread. Returns a short
/// success label (for the toast) or an error string. Reuses the same
/// container/process helpers as the pre-launch picker.
fn run_port_remediation(action: &crate::state::PortConflictAction) -> Result<String, String> {
    use crate::state::PortConflictAction as A;
    use devme_config::docker;
    use devme_supervisor::port_preflight::kill_pid;
    match action {
        A::StopContainer(name) => docker::stop_container(name).map(|_| format!("stopped {name}")),
        A::ComposeDown(project) => {
            docker::compose_down(project).map(|_| format!("composed down {project}"))
        }
        A::KillProcess(pids) => {
            let errs: Vec<String> = pids.iter().filter_map(|p| kill_pid(*p).err()).collect();
            if errs.is_empty() {
                Ok("killed process".to_string())
            } else {
                Err(errs.join("; "))
            }
        }
        A::Skip => Ok("skipped".to_string()),
    }
}

/// Host to build service URLs from. When the TUI runs on a remote host
/// (attached via `devme remote`), `DEVME_URL_HOST` is injected with the
/// browser-reachable name (e.g. a Tailscale MagicDNS name) so a copied URL
/// works from the laptop; locally it's unset and we fall back to localhost.
fn service_url_host() -> String {
    std::env::var("DEVME_URL_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

/// Fill a service URL template's `{host}`/`{port}` placeholders. Returns
/// `None` when the template needs a port the service hasn't resolved yet.
/// A verbatim URL (no placeholders) passes through unchanged.
fn resolve_service_url(template: &str, port: Option<u16>, host: &str) -> Option<String> {
    if template.contains("{port}") && port.is_none() {
        return None;
    }
    let mut url = template.replace("{host}", host);
    if let Some(p) = port {
        url = url.replace("{port}", &p.to_string());
    }
    Some(url)
}

/// True when this TUI is the remote stack's TUI, attached over SSH (devme
/// injects `DEVME_URL_HOST`). Opening a browser here would land on the
/// headless host, so `o` redirects the user to `c` (copy) instead.
fn is_remote_tui() -> bool {
    std::env::var_os("DEVME_URL_HOST").is_some_and(|v| !v.is_empty())
}

/// Toast body for a log-copy action, pluralised. `scope` is "visible"/"all".
fn copied_lines_msg(scope: &str, n: usize) -> String {
    format!("copied {n} {scope} log line{}", if n == 1 { "" } else { "s" })
}

/// Copy `lines` to the clipboard. Tries the OS clipboard first (works even
/// when the terminal blocks OSC 52 clipboard writes — a security default in
/// some emulators), and falls back to an OSC 52 escape, which is what carries
/// the copy to the user's *local* machine over SSH or tmux. Over SSH/WSL the
/// native attempt is skipped: it would land on the remote clipboard, not the
/// user's, so OSC 52 is the only thing that reaches them.
fn copy_to_clipboard(lines: &[&str]) {
    if lines.is_empty() {
        return;
    }
    let text = lines.join("\n");
    if !prefer_osc52() && copy_native(&text) {
        return;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let _ = std::io::Write::write_all(
        &mut std::io::stdout(),
        format!("\x1b]52;c;{encoded}\x07").as_bytes(),
    );
}

/// Whether to skip the native clipboard and go straight to OSC 52 — true in
/// SSH and WSL sessions, where the native clipboard isn't the user's.
fn prefer_osc52() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_TTY").is_some()
        || is_wsl()
}

fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::env::var_os("WSL_INTEROP").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| {
                let l = s.to_ascii_lowercase();
                l.contains("microsoft") || l.contains("wsl")
            })
            .unwrap_or(false)
}

/// Pipe `text` into the platform clipboard tool, returning true on success.
/// Tries each candidate in order until one is installed and exits cleanly.
fn copy_native(text: &str) -> bool {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        // Wayland first, then the X11 helpers — whichever the box has.
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };
    candidates.iter().any(|(cmd, args)| pipe_to(cmd, args, text))
}

fn pipe_to(cmd: &str, args: &[&str], text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(text.as_bytes()).is_err()
    {
        return false;
        // Otherwise the borrow ends here, closing the pipe → EOF, so the
        // clipboard tool flushes and exits.
    }
    matches!(child.wait(), Ok(status) if status.success())
}

fn build_debug_prompt(state: &TuiState) -> String {
    let cwd = state.current_instance_cwd();
    let label = if state.shared_selected() { "shared" } else { state.instance_label() };
    let owned = state.services();
    // On a stack, fold in the repo-scoped shared services too — a broken
    // shared dependency (proxy, db) is often the real cause, and on a
    // placeholder worktree they're the only services there are.
    let shared_extra: Vec<&ServiceSnapshot> = if state.shared_selected() {
        Vec::new()
    } else {
        state
            .shared_services()
            .iter()
            .filter(|s| !owned.iter().any(|o| o.name == s.name))
            .collect()
    };
    let entries: Vec<(&ServiceSnapshot, bool)> = owned
        .iter()
        .map(|s| (s, false))
        .chain(shared_extra.iter().map(|s| (*s, true)))
        .collect();

    let mut prompt = format!("My devme dev environment ({label}).\n\nWorking directory: {cwd}\n\n");

    if owned.is_empty() {
        let has_toml = std::path::Path::new(cwd).join("devme.toml").exists();
        if !has_toml {
            prompt.push_str("No devme.toml found in this directory.\n\n");
        } else {
            prompt.push_str("devme.toml exists but no services are running (daemon may not have started yet).\n\n");
        }
    }

    if !entries.is_empty() {
        let tag = |shared: bool| if shared { " (shared)" } else { "" };
        prompt.push_str("## Service states\n\n");
        for (svc, shared) in &entries {
            prompt.push_str(&format!("- **{}**{}: {:?}", svc.name, tag(*shared), svc.state));
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

        for (svc, shared) in &entries {
            let logs = state.service_logs(&svc.name);
            if logs.is_empty() {
                continue;
            }
            let tail: Vec<&str> = if logs.len() > 30 {
                logs.iter().skip(logs.len() - 30).map(|s| s.as_str()).collect()
            } else {
                logs.iter().map(|s| s.as_str()).collect()
            };
            prompt.push_str(&format!(
                "## {}{} logs (last {})\n\n```\n",
                svc.name,
                tag(*shared),
                tail.len()
            ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TuiState;
    use devme_core::{InstanceInfo, ServerMessage, ServiceState};

    fn running(name: &str) -> ServiceSnapshot {
        ServiceSnapshot {
            name: name.into(),
            state: ServiceState::Running { degraded: false, started_without: vec![] },
            pid: None,
            port: None,
            url: None,
            restart_count: 0,
        }
    }

    #[test]
    fn debug_prompt_includes_shared_services_on_a_placeholder_stack() {
        let mut state = TuiState::default();
        state.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "shared::repo".into(),
                label: "shared".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![running("proxy"), running("postgres")],
            steps: vec![],
        });
        state.add_placeholder_instance("inst", "feature/x", "/tmp/a");

        let prompt = build_debug_prompt(&state);
        // The owned side is empty, but the shared deps are folded in (flagged).
        assert!(prompt.contains("No devme.toml"), "{prompt}");
        assert!(prompt.contains("**proxy** (shared)"), "shared service missing:\n{prompt}");
        assert!(prompt.contains("**postgres** (shared)"), "shared service missing:\n{prompt}");
    }
}
