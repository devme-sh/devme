//! `devme` — user-facing CLI binary. Argument parsing and shared
//! formatters live in [`devme_cli`]; this binary dispatches.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine;
use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use devme_cli::{
    Cli, Command, ConfigAction, SkillAction, WorktreeAction, format_status_all,
    format_status_json, format_status_text,
};
use devme_config::Stack;
use devme_core::{ClientMessage, ServerMessage, ServiceState};

/// True when output should be ANSI-color-free. Set once in `main` from
/// the combination of `--no-color`, `$NO_COLOR`, and stdout-is-a-tty.
static NO_COLOR: AtomicBool = AtomicBool::new(false);
/// True when informational stderr output should be suppressed (`-q`).
/// Errors print regardless.
static QUIET: AtomicBool = AtomicBool::new(false);

fn no_color() -> bool {
    NO_COLOR.load(Ordering::Relaxed)
}

/// Print to stderr unless `--quiet` was passed. Errors should go through
/// `eprintln!` directly so they always surface.
macro_rules! info {
    ($($arg:tt)*) => {
        if !crate::QUIET.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!($($arg)*);
        }
    };
}

fn main() {
    let cli = Cli::parse();
    // Resolve the no-color decision once: CLI flag wins, then `NO_COLOR`
    // env per https://no-color.org, finally a non-TTY stdout (piped to
    // `less`, `grep`, etc.). `QUIET` is a straight pass-through from the
    // CLI flag.
    let no_color = cli.no_color
        || std::env::var_os("NO_COLOR").is_some()
        || !std::io::stdout().is_terminal();
    NO_COLOR.store(no_color, Ordering::Relaxed);
    QUIET.store(cli.quiet, Ordering::Relaxed);

    let is_tui = cli.command.is_none();
    let mut builder = if is_tui {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    let runtime = match builder.enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("devme: tokio init failed: {e}");
            std::process::exit(1);
        }
    };
    std::process::exit(runtime.block_on(run(cli)));
}

async fn run(cli: Cli) -> i32 {
    let result = match cli.command {
        None => return match launch_tui().await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("devme: {e}");
                1
            }
        },
        Some(Command::Status { all }) => {
            if all {
                status_all(cli.json).await
            } else {
                status(cli.json).await
            }
        }
        Some(Command::Down { timeout }) => down(timeout).await,
        Some(Command::Up { services, detach, wait, timeout }) => {
            up(services, detach, wait, timeout).await
        }
        Some(Command::Start { service }) => start(service).await,
        Some(Command::Stop { service }) => stop(service).await,
        Some(Command::Restart { service }) => restart(service).await,
        Some(Command::Url { service, open }) => url(service, open).await,
        Some(Command::Logs { service, follow, tail }) => logs(service, follow, tail).await,
        Some(Command::Completions { shell }) => {
            print_completions(shell);
            Ok(())
        }
        Some(Command::Doctor { tail }) => doctor(tail).await,
        Some(Command::Config { action }) => config_cmd(action),
        Some(Command::Worktree { action }) => worktree_cmd(action, cli.json).await,
        Some(Command::Skill { action }) => skill_cmd(action, cli.json),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("devme: {e}");
            1
        }
    }
}

async fn down(timeout_secs: u64) -> anyhow::Result<()> {
    let sock = socket_path();
    let mut client = match devme_client::Client::connect(&sock).await {
        Ok(c) => c,
        Err(_) => {
            println!("devme: no daemon running");
            return Ok(());
        }
    };

    // Snapshot first so we know what we're tearing down. The daemon emits
    // StatusUpdate { state: Stopped } per service as it kills each one;
    // we render those as checkmarks docker-compose-style.
    client
        .send(ClientMessage::Subscribe { services: vec![] })
        .await?;
    let services = match client.next_event().await? {
        Some(ServerMessage::Subscribed { services, .. }) => services,
        Some(other) => {
            return Err(anyhow::anyhow!("unexpected initial reply: {other:?}"));
        }
        None => return Err(anyhow::anyhow!("daemon closed before snapshot")),
    };

    // Services that are actually live — Stopped/Failed/CrashLoop are already
    // off the board, no need to checkmark them.
    use devme_core::ServiceState as S;
    let live: Vec<String> = services
        .iter()
        .filter(|s| {
            matches!(
                s.state,
                S::Starting
                    | S::Running { .. }
                    | S::Restarting { .. }
                    | S::WaitingOnDependency { .. }
            )
        })
        .map(|s| s.name.clone())
        .collect();

    let total = live.len();
    println!("[+] Stopping {total}/{total}");

    client.send(ClientMessage::Shutdown).await?;

    let started = std::time::Instant::now();
    let mut stopped: std::collections::HashSet<String> = std::collections::HashSet::new();
    let deadline = started + std::time::Duration::from_secs(timeout_secs);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            eprintln!(
                "devme: timeout after {timeout_secs}s — some services may still be running"
            );
            return Ok(());
        }
        match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(Some(ServerMessage::StatusUpdate {
                service,
                state: S::Stopped,
                ..
            }))) if live.contains(&service) && stopped.insert(service.clone()) => {
                let elapsed = started.elapsed().as_secs_f32();
                println!(" ✔ Service {service:<20}  Stopped   {elapsed:>5.1}s");
            }
            Ok(Ok(Some(ServerMessage::Goodbye { .. }))) | Ok(Ok(None)) => break,
            Ok(Ok(Some(_))) => {} // other frames during teardown
            Ok(Err(_)) => break,
            Err(_) => {
                eprintln!(
                    "devme: timeout after {timeout_secs}s — some services may still be running"
                );
                return Ok(());
            }
        }
    }
    // Any service that never reported Stopped (already-failed, etc.) still
    // gets a line so the count matches what we promised in the header.
    for name in &live {
        if !stopped.contains(name) {
            let elapsed = started.elapsed().as_secs_f32();
            println!(" ✔ Service {name:<20}  Stopped   {elapsed:>5.1}s");
        }
    }

    // Shared (`scope = "repo"`) services — postgres, proxy — live in the
    // shared supervisor, which other worktrees may be using. Tear it down too
    // only when no other worktree still has a running daemon, so `devme down`
    // is a complete stop in the common single-worktree case but doesn't yank
    // a shared Postgres out from under a sibling worktree.
    if let Ok(cwd) = std::env::current_dir() {
        let reports = devme_tui::worktree::gather_worktree_reports(&cwd).await;
        let others_live = reports
            .iter()
            .any(|r| !r.is_cwd && r.services.is_some());
        if !others_live
            && let Ok(shared_sock) = devme_config::paths::shared_socket(&cwd)
            && let Ok(mut shared) = devme_client::Client::connect(&shared_sock).await
        {
            let _ = shared.send(ClientMessage::Shutdown).await;
            println!(" ✔ Shared services            Stopped");
        }
    }
    Ok(())
}

/// Cross-worktree status (`--all`): every worktree of the repo with its slot
/// and each service's resolved port. Connects to each worktree's daemon
/// read-only — never spawns one — so a stopped worktree just shows as such.
async fn status_all(as_json: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let reports = devme_tui::worktree::gather_worktree_reports(&cwd).await;
    if as_json {
        let worktrees: Vec<serde_json::Value> = reports
            .iter()
            .map(|r| {
                serde_json::json!({
                    "label": r.label,
                    "path": r.path.display().to_string(),
                    "is_cwd": r.is_cwd,
                    "slot": r.slot,
                    "running": r.services.is_some(),
                    "services": r.services,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "worktrees": worktrees }));
    } else {
        print!("{}", format_status_all(&reports));
    }
    Ok(())
}

/// Print (and optionally open) a service's `http://localhost:<port>` URL,
/// resolved from the current worktree's running daemon.
async fn url(service: String, open: bool) -> anyhow::Result<()> {
    let sock = socket_path();
    ensure_daemon(&sock).await?;
    let mut client = devme_client::Client::connect(&sock).await?;
    let reply = client
        .request(ClientMessage::Subscribe { services: vec![] })
        .await?;
    let services = match reply {
        ServerMessage::Subscribed { services, .. } => services,
        other => return Err(anyhow::anyhow!("daemon replied unexpectedly: {other:?}")),
    };
    let svc = services
        .iter()
        .find(|s| s.name == service)
        .ok_or_else(|| anyhow::anyhow!("no service named {service:?} in devme.toml"))?;
    let port = svc
        .port
        .ok_or_else(|| anyhow::anyhow!("service {service:?} has no port to build a URL from"))?;
    let url = format!("http://localhost:{port}");
    println!("{url}");
    if !open {
        return Ok(());
    }
    if let Err(e) = devme_config::browser::open_url(&url) {
        eprintln!("devme: couldn't open browser: {e}");
    }
    Ok(())
}

async fn status(as_json: bool) -> anyhow::Result<()> {
    let sock = socket_path();
    ensure_daemon(&sock).await?;
    let mut client = devme_client::Client::connect(&sock).await?;
    let reply = client
        .request(ClientMessage::Subscribe { services: vec![] })
        .await?;

    match reply {
        ServerMessage::Subscribed { services, steps, .. } => {
            if as_json {
                println!("{}", format_status_json(&services, &steps));
            } else {
                print!("{}", format_status_text(&services, &steps));
            }
            Ok(())
        }
        other => Err(anyhow::anyhow!(
            "daemon replied with unexpected message: {other:?}"
        )),
    }
}

async fn up(
    _services: Vec<String>,
    detach: bool,
    wait: bool,
    timeout: u64,
) -> anyhow::Result<()> {
    // `services` is ignored for v1; the daemon advances the whole graph and
    // the executor decides what's eligible. Per-service Up filtering would
    // need a new executor entry point.
    //
    // Foreground semantics (default): stream every service's log lines with a
    // name prefix in distinct colours until Ctrl-C, which tears the daemon
    // down rather than detaching.
    //
    // Detached (`-d`): kick the graph and exit, leaving the daemon running.
    let sock = socket_path();
    let fresh_daemon = ensure_daemon(&sock).await?;
    let mut client = devme_client::Client::connect(&sock).await?;
    client
        .send(ClientMessage::Subscribe { services: vec![] })
        .await?;
    let snapshot = match client.next_event().await? {
        Some(ServerMessage::Subscribed { services, .. }) => services,
        Some(other) => {
            return Err(anyhow::anyhow!("unexpected initial reply: {other:?}"));
        }
        None => return Err(anyhow::anyhow!("daemon closed before snapshot")),
    };
    if snapshot.is_empty() {
        println!("devme: no services declared");
        return Ok(());
    }

    // Start is idempotent; safe to send even when re-entering — already-
    // Running services stay Running, services explicitly Stopped this
    // session stay Stopped.
    client
        .send(ClientMessage::Start {
            service: String::new(),
            skip_deps: false,
        })
        .await?;

    if detach {
        if wait {
            await_all_running(&mut client, &snapshot, timeout).await?;
        }
        let n = snapshot.len();
        let verb = if fresh_daemon { "started" } else { "attached to" };
        println!(
            "devme: {verb} {n} service{}; daemon running in background.\n\
             devme logs <service>   tail one service\n\
             devme status           snapshot\n\
             devme down             stop everything",
            if n == 1 { "" } else { "s" }
        );
        maybe_skill_update();
        maybe_show_skills_hint();
        return Ok(());
    }

    let names: Vec<&str> = snapshot.iter().map(|s| s.name.as_str()).collect();
    if fresh_daemon {
        info!(
            "[+] Running {n}/{n}\nAttaching to {names}",
            n = snapshot.len(),
            names = names.join(", ")
        );
    } else {
        // Re-entrancy: daemon already alive. Skip the boot header — those
        // services have been up for a while. Just announce the attach.
        info!("Attaching to {} (already running)", names.join(", "));
    }
    info!("(Ctrl-C: graceful stop · twice: force quit)");

    // Two-stage signal handling matches `docker compose up`:
    //   1st SIGINT  → "Gracefully stopping… (press Ctrl+C again to force)",
    //                 send Shutdown, keep draining so the user sees the
    //                 services actually stop;
    //   2nd SIGINT  → SIGKILL ourselves, exit 130 (POSIX "killed by signal").
    // SIGTERM (external, systemd, supervisord) takes the graceful path with
    // a different message — no "press again" hint to spam unattended logs.
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut stopping = false;

    loop {
        tokio::select! {
            _ = sigint.recv() => {
                if stopping {
                    info!("\ndevme: force-quitting.");
                    std::process::exit(130);
                }
                stopping = true;
                info!("\ndevme: gracefully stopping… (press Ctrl+C again to force)");
                let _ = client.send(ClientMessage::Shutdown).await;
            }
            _ = sigterm.recv() => {
                if !stopping {
                    stopping = true;
                    info!("\ndevme: SIGTERM received, gracefully stopping…");
                    let _ = client.send(ClientMessage::Shutdown).await;
                }
            }
            msg = client.next_event() => {
                let m = match msg? {
                    Some(m) => m,
                    None => return Ok(()),
                };
                match m {
                    ServerMessage::LogChunk { service, bytes, .. } => {
                        if let Ok(decoded) =
                            base64::engine::general_purpose::STANDARD.decode(bytes.as_bytes())
                            && let Ok(text) = String::from_utf8(decoded)
                        {
                            print_prefixed(&service, &text);
                        }
                    }
                    ServerMessage::StatusUpdate { service, state, .. } => {
                        if let Some(label) = transition_label(&state) {
                            info!("[{service}] {label}");
                        }
                    }
                    ServerMessage::Notice { level, message } => {
                        info!("[devme {level:?}] {message}");
                    }
                    ServerMessage::Goodbye { .. } => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}

/// Block on StatusUpdate stream until every service in `snapshot` is in a
/// terminal post-boot state (Running, Failed, or CrashLoop). Used by
/// `up -d --wait` so CI/scripts can know whether the stack is actually
/// up before proceeding. Returns Err on timeout.
async fn await_all_running(
    client: &mut devme_client::Client,
    snapshot: &[devme_core::ServiceSnapshot],
    timeout_secs: u64,
) -> anyhow::Result<()> {
    use std::collections::HashMap;
    let mut states: HashMap<String, ServiceState> = snapshot
        .iter()
        .map(|s| (s.name.clone(), s.state.clone()))
        .collect();
    let is_settled = |s: &ServiceState| {
        matches!(
            s,
            ServiceState::Running { .. }
                | ServiceState::Failed { .. }
                | ServiceState::CrashLoop { .. }
                | ServiceState::External { .. }
        )
    };
    if states.values().all(is_settled) {
        return Ok(());
    }
    let deadline = if timeout_secs == 0 {
        None
    } else {
        Some(std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs))
    };
    loop {
        let timeout = match deadline {
            Some(d) => d.saturating_duration_since(std::time::Instant::now()),
            None => std::time::Duration::from_secs(3600),
        };
        if deadline.is_some() && timeout.is_zero() {
            return Err(anyhow::anyhow!(
                "--wait timed out before all services settled"
            ));
        }
        match tokio::time::timeout(timeout, client.next_event()).await {
            Ok(Ok(Some(ServerMessage::StatusUpdate { service, state, .. }))) => {
                states.insert(service, state);
                if states.values().all(is_settled) {
                    return Ok(());
                }
            }
            Ok(Ok(Some(_))) => {} // ignore non-status frames
            Ok(Ok(None)) | Ok(Err(_)) => {
                return Err(anyhow::anyhow!("daemon disconnected while waiting"));
            }
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "--wait timed out before all services settled"
                ));
            }
        }
    }
}

fn transition_label(state: &ServiceState) -> Option<&'static str> {
    use ServiceState as S;
    Some(match state {
        S::Starting => "starting",
        S::Running { .. } => "running",
        S::Stopped => "stopped",
        S::Failed { .. } => "failed",
        S::CrashLoop { .. } => "crash-loop",
        _ => return None,
    })
}

/// Hash a service name to a stable terminal-color escape so each service's
/// lines are visually distinct in `up`'s combined stream. Strips colors
/// when [`no_color`] is true (piped output, `--no-color`, `NO_COLOR=1`).
fn print_prefixed(service: &str, text: &str) {
    let (color, reset, dim) = if no_color() {
        ("", "", "")
    } else {
        let colors: &[&str] = &[
            "\x1b[36m", "\x1b[33m", "\x1b[35m", "\x1b[32m", "\x1b[34m", "\x1b[91m", "\x1b[96m",
            "\x1b[93m",
        ];
        let mut h: u32 = 5381;
        for b in service.bytes() {
            h = h.wrapping_mul(33).wrapping_add(b as u32);
        }
        (colors[(h as usize) % colors.len()], "\x1b[0m", "\x1b[2m")
    };
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        println!("{color}{service:>10}{reset} {dim}|{reset} {line}");
    }
}

async fn start(service: String) -> anyhow::Result<()> {
    let sock = socket_path();
    ensure_daemon(&sock).await?;
    let mut client = devme_client::Client::connect(&sock).await?;
    client
        .send(ClientMessage::Start {
            service,
            skip_deps: false,
        })
        .await?;
    Ok(())
}

async fn stop(service: String) -> anyhow::Result<()> {
    let sock = socket_path();
    let mut client = devme_client::Client::connect(&sock).await?;
    client.send(ClientMessage::Stop { service }).await?;
    Ok(())
}

async fn restart(service: String) -> anyhow::Result<()> {
    let sock = socket_path();
    ensure_daemon(&sock).await?;
    let mut client = devme_client::Client::connect(&sock).await?;
    client.send(ClientMessage::Restart { service }).await?;
    Ok(())
}

async fn logs(service: String, follow: bool, tail: usize) -> anyhow::Result<()> {
    let sock = socket_path();
    ensure_daemon(&sock).await?;
    let mut client = devme_client::Client::connect(&sock).await?;

    // Confirm the requested name is actually known to the daemon — otherwise
    // we'd silently sit waiting for logs that can never arrive.
    let snap = client
        .request(ClientMessage::Subscribe {
            services: vec![service.clone()],
        })
        .await?;
    let known = match &snap {
        ServerMessage::Subscribed { services, steps, .. } => {
            services.iter().any(|s| s.name == service)
                || steps.iter().any(|s| s.name == service)
        }
        _ => false,
    };
    if !known {
        return Err(anyhow::anyhow!(
            "no service or step named {service:?} in devme.toml"
        ));
    }

    // Drain buffered lines from the daemon's ring. We can't tell from the
    // wire alone when replay ends, so we read with a short idle timeout
    // and call the first miss "replay done". The replay buffer goes into
    // a Vec rather than straight to stdout so `--tail N` can drop the
    // older lines before printing.
    let drain_idle = std::time::Duration::from_millis(80);
    let mut buffered: Vec<String> = Vec::new();
    loop {
        match tokio::time::timeout(drain_idle, client.next_event()).await {
            Ok(Ok(Some(ServerMessage::LogChunk { service: s, bytes, .. })))
                if s == service =>
            {
                if let Ok(decoded) =
                    base64::engine::general_purpose::STANDARD.decode(bytes.as_bytes())
                    && let Ok(text) = String::from_utf8(decoded)
                {
                    for line in text.split('\n') {
                        let line = line.trim_end_matches('\r');
                        if !line.is_empty() {
                            buffered.push(line.to_string());
                        }
                    }
                }
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) | Ok(Err(_)) => return Ok(()),
            Err(_) => break, // idle — replay finished
        }
    }
    // Apply --tail: 0 = unlimited (docker-compose semantics).
    let printed_any = !buffered.is_empty();
    let skip = if tail == 0 {
        0
    } else {
        buffered.len().saturating_sub(tail)
    };
    for line in buffered.iter().skip(skip) {
        print_prefixed(&service, line);
    }

    if !follow {
        if !printed_any {
            eprintln!("devme: no buffered logs for {service:?} yet (try --follow to wait)");
        }
        return Ok(());
    }

    // --follow: keep streaming new lines indefinitely. Ctrl-C exits cleanly.
    if !printed_any {
        eprintln!("devme: tailing {service:?} (Ctrl-C to stop)");
    }
    let interrupt = tokio::signal::ctrl_c();
    let mut pinned_interrupt = std::pin::pin!(interrupt);
    loop {
        tokio::select! {
            _ = &mut pinned_interrupt => return Ok(()),
            msg = client.next_event() => match msg? {
                Some(ServerMessage::LogChunk { service: s, bytes, .. }) if s == service => {
                    if let Ok(decoded) =
                        base64::engine::general_purpose::STANDARD.decode(bytes.as_bytes())
                        && let Ok(text) = String::from_utf8(decoded)
                    {
                        print_prefixed(&service, &text);
                    }
                }
                Some(ServerMessage::Goodbye { .. }) | None => return Ok(()),
                _ => {}
            }
        }
    }
}

async fn doctor(tail: usize) -> anyhow::Result<()> {
    let sock = socket_path();
    let mut client = match devme_client::Client::connect(&sock).await {
        Ok(c) => c,
        Err(_) => {
            let report = serde_json::json!({
                "status": "no_daemon",
                "message": "no devme daemon running — start one with `devme up -d`",
                "services": [],
                "steps": [],
            });
            println!("{}", serde_json::to_string_pretty(&report)?);
            return Ok(());
        }
    };

    client
        .send(ClientMessage::Subscribe { services: vec![] })
        .await?;
    let (services, steps) = match client.next_event().await? {
        Some(ServerMessage::Subscribed { services, steps, .. }) => (services, steps),
        _ => return Err(anyhow::anyhow!("unexpected reply from daemon")),
    };

    let drain_idle = std::time::Duration::from_millis(80);
    let mut all_logs: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    loop {
        match tokio::time::timeout(drain_idle, client.next_event()).await {
            Ok(Ok(Some(ServerMessage::LogChunk { service, bytes, .. }))) => {
                if let Ok(decoded) =
                    base64::engine::general_purpose::STANDARD.decode(bytes.as_bytes())
                    && let Ok(text) = String::from_utf8(decoded)
                {
                    let buf = all_logs.entry(service).or_default();
                    for line in text.split('\n') {
                        let line = line.trim_end_matches('\r');
                        if !line.is_empty() {
                            buf.push(line.to_string());
                        }
                    }
                }
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) | Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    let has_failures = services.iter().any(|s| {
        matches!(
            s.state,
            ServiceState::Failed { .. } | ServiceState::CrashLoop { .. }
        )
    }) || steps.iter().any(|s| {
        matches!(
            s.state,
            devme_core::StepState::Failed | devme_core::StepState::ProvisionFailed
        )
    });

    let svc_json: Vec<serde_json::Value> = services
        .iter()
        .map(|s| {
            let mut logs = all_logs
                .get(&s.name)
                .cloned()
                .unwrap_or_default();
            let skip = if tail == 0 { 0 } else { logs.len().saturating_sub(tail) };
            logs = logs.into_iter().skip(skip).collect();
            serde_json::json!({
                "name": s.name,
                "state": format!("{:?}", s.state),
                "pid": s.pid,
                "port": s.port,
                "restart_count": s.restart_count,
                "logs": logs,
            })
        })
        .collect();

    let step_json: Vec<serde_json::Value> = steps
        .iter()
        .map(|s| {
            let logs = all_logs
                .get(&s.name)
                .cloned()
                .unwrap_or_default();
            serde_json::json!({
                "name": s.name,
                "state": format!("{:?}", s.state),
                "logs": logs,
            })
        })
        .collect();

    let report = serde_json::json!({
        "status": if has_failures { "unhealthy" } else { "healthy" },
        "services": svc_json,
        "steps": step_json,
    });

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn config_cmd(action: Option<ConfigAction>) -> anyhow::Result<()> {
    use devme_config::GlobalConfig;

    match action {
        None => {
            let (cfg, warning) = GlobalConfig::load_checked();
            if let Some(w) = warning {
                eprintln!("warning: {w}");
            }
            for (key, desc) in GlobalConfig::keys() {
                let value = cfg.get(key).unwrap_or_else(|| "(unset)".into());
                println!("{key:<24} {value:<20} # {desc}");
            }
            Ok(())
        }
        Some(ConfigAction::Get { key }) => {
            let (cfg, warning) = GlobalConfig::load_checked();
            if let Some(w) = warning {
                eprintln!("warning: {w}");
            }
            match cfg.get(&key) {
                Some(v) => println!("{v}"),
                None => println!("(unset)"),
            }
            Ok(())
        }
        // Surgical writes preserve any comments/formatting in the file.
        Some(ConfigAction::Set { key, value }) => {
            GlobalConfig::persist(&key, &value).map_err(|e| anyhow::anyhow!("{e}"))?;
            info!("devme: {key} = {value}");
            Ok(())
        }
        Some(ConfigAction::Unset { key }) => {
            GlobalConfig::unset_persisted(&key).map_err(|e| anyhow::anyhow!("{e}"))?;
            info!("devme: unset {key}");
            Ok(())
        }
    }
}

/// `devme worktree …` — worktree lifecycle coordinated with devme.
async fn worktree_cmd(action: WorktreeAction, json: bool) -> anyhow::Result<()> {
    match action {
        WorktreeAction::Rm { target, force } => {
            let cwd = std::env::current_dir()?;
            let report = devme_tui::worktree::remove_worktree(&cwd, &target, force).await?;
            if json {
                let value = serde_json::json!({
                    "path": report.path.display().to_string(),
                    "branch": report.branch,
                    "slot": report.slot,
                    "instance_stopped": report.instance_stopped,
                    "on_destroy_ran": report.on_destroy_ran,
                });
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("Removed worktree {}", report.path.display());
                if let Some(b) = &report.branch {
                    info!("  branch: {b}");
                }
                if let Some(s) = report.slot {
                    info!("  slot:   {s}");
                }
                if report.instance_stopped {
                    info!("  stopped instance stack");
                }
                match report.on_destroy_ran {
                    Some(true) => info!("  ran on_destroy hook"),
                    Some(false) => {
                        eprintln!("devme: on_destroy hook failed (worktree was still removed)")
                    }
                    None => {}
                }
            }
            Ok(())
        }
    }
}

/// `devme skill …` — manage the embedded AI agent skill. Pure filesystem
/// work, so it's synchronous (no daemon involved).
fn skill_cmd(action: SkillAction, json: bool) -> anyhow::Result<()> {
    match action {
        SkillAction::Install { global, force } => devme_cli::skill::install(global, force, json),
        SkillAction::Uninstall { global } => devme_cli::skill::uninstall(global, json),
        SkillAction::Status => devme_cli::skill::status(json),
    }
}

fn print_completions(shell: Shell) {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "devme", &mut std::io::stdout());
}

/// Launch the TUI directly. Runs preflight checks first, then hands off
/// to the TUI event loop which manages all daemon spawning.
async fn launch_tui() -> anyhow::Result<i32> {
    let cwd = std::env::current_dir()?;
    let config_path = cwd.join("devme.toml");
    if let Ok(toml_str) = std::fs::read_to_string(&config_path)
        && let Ok(stack) = Stack::parse(&toml_str) {
            // Env resolution only prompts when vars are missing — silent otherwise.
            if !stack.env.is_empty() {
                // Honour `[stack] env_file` (ADR-0014) — compute the target
                // path before moving `stack.env` out below.
                let env_file = devme_supervisor::env_resolve::env_file_path(&stack, &cwd);
                let env_pairs: Vec<(String, devme_config::EnvVar)> =
                    stack.env.into_iter().collect();
                let interactive = std::io::stdin().is_terminal();
                let mut stdin = std::io::BufReader::new(std::io::stdin());
                let mut stderr = std::io::stderr();
                let _ = devme_supervisor::env_resolve::resolve_env_vars(
                    &env_pairs, &env_file, &cwd, &mut stdin, &mut stderr, interactive,
                );
            }
            // Only show preflight output when something needs provisioning.
            if let Ok(stack) = Stack::parse(&toml_str) {
                if !devme_supervisor::preflight::all_checks_pass(&stack, &cwd) {
                    let interactive = std::io::stdin().is_terminal();
                    let mut stdin = std::io::BufReader::new(std::io::stdin());
                    let mut stderr = std::io::stderr();
                    let _ = devme_supervisor::preflight::run_preflight(
                        &stack, &cwd, &mut stdin, &mut stderr, interactive,
                    );
                }
                ensure_docker_if_needed(&stack)?;

                // Catch ports already taken by a stray container/process and
                // offer to free them before the daemon tries to bind.
                let interactive = std::io::stdin().is_terminal();
                let mut stdin = std::io::BufReader::new(std::io::stdin());
                let mut stderr = std::io::stderr();
                let _ = devme_supervisor::port_preflight::check_ports(
                    &stack, &mut stdin, &mut stderr, interactive,
                );
            }
        }

    devme_tui::launch(false).await?;
    maybe_show_skills_hint();
    Ok(0)
}

use devme_supervisor::spawn::{
    ensure_daemon as ensure_daemon_inner,
};

/// Make sure a daemon is listening on `sock` for the current cwd. Thin
/// wrapper that pins `cwd` to the process's working directory; see
/// `devme_supervisor::spawn::ensure_daemon` for the underlying logic.
///
/// Before spawning a new daemon, resolves any declared `[env.*]` vars
/// from `devme.toml` — prompting the user for missing values while we
/// still have a terminal attached (ADR-0014).
async fn ensure_daemon(sock: &std::path::Path) -> anyhow::Result<bool> {
    let cwd = std::env::current_dir()?;

    let config_path = cwd.join("devme.toml");
    if let Ok(toml_str) = std::fs::read_to_string(&config_path)
        && let Ok(stack) = Stack::parse(&toml_str) {
            if !stack.env.is_empty() {
                // Honour `[stack] env_file` (ADR-0014) — compute the target
                // path before moving `stack.env` out below.
                let env_file = devme_supervisor::env_resolve::env_file_path(&stack, &cwd);
                let env_pairs: Vec<(String, devme_config::EnvVar)> =
                    stack.env.into_iter().collect();
                let interactive = std::io::stdin().is_terminal();
                let mut stdin = std::io::BufReader::new(std::io::stdin());
                let mut stderr = std::io::stderr();
                if let Err(e) = devme_supervisor::env_resolve::resolve_env_vars(
                    &env_pairs, &env_file, &cwd, &mut stdin, &mut stderr, interactive,
                ) {
                    eprintln!("devme: env resolution failed: {e}");
                }
            }
            // Re-parse since we moved `stack.env` above
            if let Ok(stack) = Stack::parse(&toml_str) {
                // Preflight: check dependencies that don't need services
                let interactive = std::io::stdin().is_terminal();
                let mut stdin = std::io::BufReader::new(std::io::stdin());
                let mut stderr = std::io::stderr();
                let _ = devme_supervisor::preflight::run_preflight(
                    &stack, &cwd, &mut stdin, &mut stderr, interactive,
                );

                ensure_docker_if_needed(&stack)?;

                // Catch ports already taken by a stray container/process and
                // offer to free them before the daemon tries to bind.
                let interactive = std::io::stdin().is_terminal();
                let mut stdin = std::io::BufReader::new(std::io::stdin());
                let mut stderr = std::io::stderr();
                let _ = devme_supervisor::port_preflight::check_ports(
                    &stack, &mut stdin, &mut stderr, interactive,
                );
            }
        }

    ensure_daemon_inner(sock, &cwd).await
}

/// If the stack has services that use Docker and Docker isn't running,
/// start the user's preferred daemon (prompting on first use).
fn ensure_docker_if_needed(stack: &Stack) -> anyhow::Result<()> {
    use devme_config::{GlobalConfig, docker};

    if !docker::stack_needs_docker(stack) {
        return Ok(());
    }
    if docker::is_docker_running() {
        return Ok(());
    }

    let mut cfg = GlobalConfig::load();

    let daemon_id = match &cfg.docker.daemon {
        Some(id) => id.clone(),
        None => {
            let installed = docker::detect_installed();
            if installed.is_empty() {
                return Err(anyhow::anyhow!(
                    "services require Docker but no Docker daemon is installed\n\
                     install OrbStack, Docker Desktop, or Colima"
                ));
            }
            if installed.len() == 1 {
                let id = installed[0].id.clone();
                info!("devme: auto-selected {} (only daemon installed)", installed[0].label);
                cfg.docker.daemon = Some(id.clone());
                let _ = cfg.save();
                id
            } else {
                if !std::io::stdin().is_terminal() {
                    return Err(anyhow::anyhow!(
                        "Docker is not running and no daemon is configured\n\
                         run: devme config set docker.daemon <name>"
                    ));
                }
                eprintln!("Docker is required but not running. Which daemon should devme start?\n");
                for (i, d) in installed.iter().enumerate() {
                    eprintln!("  [{}] {}", i + 1, d.label);
                }
                eprint!("\nChoice [1]: ");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                let trimmed = input.trim();
                let idx = if trimmed.is_empty() {
                    0
                } else {
                    trimmed.parse::<usize>()
                        .map_err(|_| anyhow::anyhow!("invalid choice"))?
                        .checked_sub(1)
                        .ok_or_else(|| anyhow::anyhow!("invalid choice"))?
                };
                let chosen = installed.get(idx)
                    .ok_or_else(|| anyhow::anyhow!("invalid choice"))?;
                info!("devme: saved docker.daemon = {}", chosen.id);
                cfg.docker.daemon = Some(chosen.id.clone());
                let _ = cfg.save();
                chosen.id.clone()
            }
        }
    };

    info!("devme: starting Docker via {daemon_id}…");
    docker::start_daemon(&daemon_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    info!("devme: Docker is ready");
    Ok(())
}

/// Keep a devme-managed skill install in step with this binary. Two modes:
///
/// - `skill.auto_update = true`: silently regenerate any stale, unmodified,
///   devme-written install. No prompt, no nag.
/// - otherwise: on an interactive terminal only, print a one-line nudge —
///   throttled to once per binary version — pointing at `devme skill install`.
///
/// Either way we only ever touch installs devme recorded writing and that the
/// user hasn't edited since; foreign/modified copies are left alone. Agents
/// and pipes (no tty) get nothing but the silent auto-update path.
fn maybe_skill_update() {
    if QUIET.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let mut cfg = devme_config::GlobalConfig::load();
    if cfg.skill_installs().is_empty() {
        return;
    }

    if cfg.skill_auto_update() {
        let updated = devme_config::skill::auto_update(&mut cfg);
        if !updated.is_empty() {
            info!(
                "devme: refreshed AI skill to v{} in {} location(s)",
                devme_config::skill::embedded_version(),
                updated.len()
            );
        }
        return;
    }

    // Nudge only a human at a keyboard — never an agent or a pipe.
    if !std::io::stdin().is_terminal() {
        return;
    }
    if cfg.get("hints.skills").as_deref() == Some("false") {
        return;
    }
    let stale = devme_config::skill::stale_installs(&cfg);
    let Some(first) = stale.first() else {
        return;
    };
    let embedded = devme_config::skill::embedded_version();
    if cfg.skill_last_nudge() == Some(embedded.as_str()) {
        return;
    }
    eprintln!(
        "hint: devme's AI skill is out of date (v{} → v{}). Update: devme skill install",
        first.from, first.to
    );
    cfg.set_skill_last_nudge(&embedded);
    let _ = cfg.save();
}

fn maybe_show_skills_hint() {
    if QUIET.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }

    let cfg = devme_config::GlobalConfig::load();
    if cfg.get("hints.skills") == Some("false".into()) {
        return;
    }
    // Don't nag to install a skill devme already manages — `maybe_skill_update`
    // owns keeping it current.
    if !cfg.skill_installs().is_empty() {
        return;
    }

    let config_dir = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        std::path::PathBuf::from(xdg).join("devme")
    } else if let Some(home) = std::env::var_os("HOME") {
        std::path::PathBuf::from(home).join(".config").join("devme")
    } else {
        return;
    };

    let state_file = config_dir.join("skills-hint-state");
    let (count, last_shown) = match std::fs::read_to_string(&state_file) {
        Ok(contents) => {
            let mut lines = contents.lines();
            let count: u32 = lines.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let ts: u64 = lines.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            (count, ts)
        }
        Err(_) => (0, 0),
    };

    if count >= 4 {
        return;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Backoff: 0s, 3 days, 2 weeks, 6 weeks
    let min_gap_secs: u64 = match count {
        0 => 0,
        1 => 3 * 86400,
        2 => 14 * 86400,
        3 => 42 * 86400,
        _ => return,
    };

    if now.saturating_sub(last_shown) < min_gap_secs {
        return;
    }

    eprintln!(
        "hint: devme has an AI coding skill. Install it: devme skill install \
         (or: npx skills add devme-sh/skills)"
    );
    if count == 0 {
        eprintln!("hint: suppress with: devme config set hints.skills false");
    }

    let _ = std::fs::create_dir_all(&config_dir);
    let _ = std::fs::write(&state_file, format!("{}\n{now}", count + 1));
}

fn socket_path() -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    devme_config::paths::supervisor_socket(&cwd)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/devme.sock"))
}
