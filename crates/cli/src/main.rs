//! `devme` — user-facing CLI binary. Argument parsing and shared
//! formatters live in [`devme_cli`]; this binary dispatches.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine;
use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use devme_cli::{Cli, Command, format_status_json, format_status_text};
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

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
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
        Some(Command::Status) => status(cli.json).await,
        Some(Command::Down { timeout }) => down(timeout).await,
        Some(Command::Up { services, detach, wait, timeout }) => {
            up(services, detach, wait, timeout).await
        }
        Some(Command::Start { service }) => start(service).await,
        Some(Command::Stop { service }) => stop(service).await,
        Some(Command::Restart { service }) => restart(service).await,
        Some(Command::Logs { service, follow, tail }) => logs(service, follow, tail).await,
        Some(Command::Completions { shell }) => {
            print_completions(shell);
            Ok(())
        }
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

fn print_completions(shell: Shell) {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "devme", &mut std::io::stdout());
}

/// Launch the TUI by exec'ing devme-tui — keeps the CLI binary small and
/// avoids pulling crossterm/ratatui into it. Daemon is auto-spawned first.
async fn launch_tui() -> anyhow::Result<i32> {
    let sock = socket_path();
    ensure_daemon(&sock).await?;

    let tui_path = find_sibling_binary("devme-tui")?;
    let status = std::process::Command::new(&tui_path).status()?;
    Ok(status.code().unwrap_or(0))
}

fn find_sibling_binary(name: &str) -> anyhow::Result<std::path::PathBuf> {
    if let Ok(self_exe) = std::env::current_exe() {
        // Resolve symlinks — when devme is installed via `ln -s` into
        // ~/.local/bin or similar, current_exe() returns the symlink path
        // on macOS, and the sibling binaries live next to the *target*.
        let resolved = std::fs::canonicalize(&self_exe).unwrap_or(self_exe);
        if let Some(parent) = resolved.parent() {
            let candidate = parent.join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    Ok(std::path::PathBuf::from(name))
}

/// Make sure a daemon is listening on `sock`. If not, fork the supervisor
/// binary and wait up to ~5s for it to bind. Returns `true` if a new
/// daemon was spawned, `false` if one was already running — callers use
/// this to switch between "started N services" and "attaching to N
/// services" banners (docker compose `up` re-entrancy).
async fn ensure_daemon(sock: &std::path::Path) -> anyhow::Result<bool> {
    if devme_client::Client::connect(sock).await.is_ok() {
        return Ok(false);
    }
    // Fail-fast checks that produce a useful error message *before* we
    // burn 5 seconds waiting for a daemon that's never going to bind.
    let cwd = std::env::current_dir()?;
    if !cwd.join("devme.toml").exists() {
        return Err(anyhow::anyhow!(
            "no devme.toml in {} (run from a directory containing one)",
            cwd.display()
        ));
    }

    let supervisor = find_supervisor_binary()?;
    // Capture stderr so that if the daemon dies during startup we can show
    // the user *why* instead of the generic "didn't come up in 5s" timeout.
    let mut child = std::process::Command::new(&supervisor)
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawning {}: {e}", supervisor.display()))?;

    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if devme_client::Client::connect(sock).await.is_ok() {
            // Daemon is up. Detach stderr so it doesn't fill the pipe and
            // backpressure the daemon's writes; from this point on the
            // daemon's own logs go to its log file, not our process.
            drop(child.stderr.take());
            return Ok(true);
        }
        // Check whether the supervisor exited (config error, panic, etc.)
        // before its grace period to surface its stderr immediately.
        if let Ok(Some(_status)) = child.try_wait() {
            let mut stderr = String::new();
            if let Some(mut handle) = child.stderr.take() {
                use std::io::Read;
                let _ = handle.read_to_string(&mut stderr);
            }
            let stderr = stderr.trim();
            return Err(anyhow::anyhow!(
                "supervisor exited during startup\n{}",
                if stderr.is_empty() { "(no stderr)" } else { stderr }
            ));
        }
    }
    Err(anyhow::anyhow!(
        "daemon didn't come up at {} within 5s",
        sock.display()
    ))
}

fn find_supervisor_binary() -> anyhow::Result<std::path::PathBuf> {
    find_sibling_binary("devme-supervisor")
}

fn socket_path() -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    devme_config::paths::supervisor_socket(&cwd)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/devme.sock"))
}
