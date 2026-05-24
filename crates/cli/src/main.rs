//! `devme` — user-facing CLI binary. Argument parsing and shared
//! formatters live in [`devme_cli`]; this binary dispatches.

use base64::Engine;
use clap::Parser;
use devme_cli::{Cli, Command, format_status_json, format_status_text};
use devme_core::{ClientMessage, ServerMessage, ServiceState};

fn main() {
    let cli = Cli::parse();
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
        Some(Command::Down) => down().await,
        Some(Command::Up { services }) => up(services).await,
        Some(Command::Start { service }) => start(service).await,
        Some(Command::Stop { service }) => stop(service).await,
        Some(Command::Restart { service }) => restart(service).await,
        Some(Command::Logs { service, follow }) => logs(service, follow).await,
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("devme: {e}");
            1
        }
    }
}

async fn down() -> anyhow::Result<()> {
    let sock = socket_path();
    let mut client = match devme_client::Client::connect(&sock).await {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let reply = client.request(ClientMessage::Shutdown).await?;
    match reply {
        ServerMessage::Goodbye { .. } => Ok(()),
        other => Err(anyhow::anyhow!(
            "daemon replied with unexpected message: {other:?}"
        )),
    }
}

async fn status(as_json: bool) -> anyhow::Result<()> {
    let sock = socket_path();
    ensure_daemon(&sock).await?;
    let mut client = devme_client::Client::connect(&sock).await?;
    let reply = client
        .request(ClientMessage::Subscribe { services: vec![] })
        .await?;

    match reply {
        ServerMessage::Subscribed { services, steps } => {
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

async fn up(_services: Vec<String>) -> anyhow::Result<()> {
    // `services` is ignored for v1; the daemon advances the whole graph and
    // the executor decides what's eligible. Per-service Up filtering would
    // need a new executor entry point.
    //
    // Foreground semantics: stream every service's log lines with a name
    // prefix, prefixed in distinct colours, until the user hits Ctrl-C.
    // Ctrl-C tears the daemon down rather than detaching.
    let sock = socket_path();
    ensure_daemon(&sock).await?;
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

    client
        .send(ClientMessage::Start {
            service: String::new(),
            skip_deps: false,
        })
        .await?;

    eprintln!(
        "devme: streaming logs ({} service{}). Press Ctrl-C to stop everything.",
        snapshot.len(),
        if snapshot.len() == 1 { "" } else { "s" }
    );

    let interrupt = tokio::signal::ctrl_c();
    let mut pinned_interrupt = std::pin::pin!(interrupt);

    loop {
        tokio::select! {
            _ = &mut pinned_interrupt => {
                eprintln!("\ndevme: shutting down…");
                let _ = client.send(ClientMessage::Shutdown).await;
                // Drain the Goodbye + any final messages so the daemon
                // exits cleanly before we return.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    drain_until_goodbye(&mut client),
                )
                .await;
                return Ok(());
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
                            eprintln!("[{service}] {label}");
                        }
                    }
                    ServerMessage::Notice { level, message } => {
                        eprintln!("[devme {level:?}] {message}");
                    }
                    ServerMessage::Goodbye { .. } => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}

async fn drain_until_goodbye(client: &mut devme_client::Client) {
    while let Ok(Some(msg)) = client.next_event().await {
        if matches!(msg, ServerMessage::Goodbye { .. }) {
            return;
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
/// lines are visually distinct in `up`'s combined stream.
fn print_prefixed(service: &str, text: &str) {
    let colors: &[&str] = &[
        "\x1b[36m", "\x1b[33m", "\x1b[35m", "\x1b[32m", "\x1b[34m", "\x1b[91m", "\x1b[96m",
        "\x1b[93m",
    ];
    let reset = "\x1b[0m";
    let dim = "\x1b[2m";
    let mut h: u32 = 5381;
    for b in service.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u32);
    }
    let color = colors[(h as usize) % colors.len()];
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

async fn logs(service: String, follow: bool) -> anyhow::Result<()> {
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
        ServerMessage::Subscribed { services, steps } => {
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

    // Drain buffered lines that arrived alongside the snapshot. We can't tell
    // from the wire alone when replay ends, so we read with a short idle
    // timeout; the first miss is "replay done".
    let mut printed_any = false;
    let drain_idle = std::time::Duration::from_millis(80);
    loop {
        match tokio::time::timeout(drain_idle, client.next_event()).await {
            Ok(Ok(Some(ServerMessage::LogChunk { service: s, bytes, .. })))
                if s == service =>
            {
                if let Ok(decoded) =
                    base64::engine::general_purpose::STANDARD.decode(bytes.as_bytes())
                    && let Ok(text) = String::from_utf8(decoded)
                {
                    println!("{text}");
                    printed_any = true;
                }
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) | Ok(Err(_)) => return Ok(()),
            Err(_) => break, // idle — replay finished
        }
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
                        println!("{text}");
                    }
                }
                Some(ServerMessage::Goodbye { .. }) | None => return Ok(()),
                _ => {}
            }
        }
    }
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
/// binary and wait up to ~5s for it to bind.
async fn ensure_daemon(sock: &std::path::Path) -> anyhow::Result<()> {
    if devme_client::Client::connect(sock).await.is_ok() {
        return Ok(());
    }
    let supervisor = find_supervisor_binary()?;
    let cwd = std::env::current_dir()?;
    std::process::Command::new(&supervisor)
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawning {}: {e}", supervisor.display()))?;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if devme_client::Client::connect(sock).await.is_ok() {
            return Ok(());
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
