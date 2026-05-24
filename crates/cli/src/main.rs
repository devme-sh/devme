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
    let sock = socket_path();
    ensure_daemon(&sock).await?;
    let mut client = devme_client::Client::connect(&sock).await?;
    client
        .send(ClientMessage::Subscribe { services: vec![] })
        .await?;
    let snapshot = match client.next_event().await? {
        Some(ServerMessage::Subscribed { services, .. }) => services,
        Some(other) => {
            return Err(anyhow::anyhow!(
                "unexpected initial reply: {other:?}"
            ));
        }
        None => return Err(anyhow::anyhow!("daemon closed before snapshot")),
    };
    let expected: std::collections::HashSet<String> =
        snapshot.iter().map(|s| s.name.clone()).collect();
    if expected.is_empty() {
        println!("devme: no services declared");
        return Ok(());
    }

    client
        .send(ClientMessage::Start {
            service: String::new(),
            skip_deps: false,
        })
        .await?;

    let mut running: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut failed: std::collections::HashMap<String, Option<i32>> =
        std::collections::HashMap::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);

    while running.len() + failed.len() < expected.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(anyhow::anyhow!(
                "timed out waiting for services to settle"
            ));
        }
        let next = match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(Some(m))) => m,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "timed out waiting for services to settle"
                ));
            }
        };
        match next {
            ServerMessage::StatusUpdate { service, state, .. } if expected.contains(&service) => {
                match state {
                    ServiceState::Running { .. } => {
                        running.insert(service.clone());
                        failed.remove(&service);
                        println!("  up: {service}");
                    }
                    ServiceState::Failed { exit_code } => {
                        failed.insert(service.clone(), exit_code);
                        running.remove(&service);
                        println!("  fail: {service} (exit {exit_code:?})");
                    }
                    _ => {}
                }
            }
            ServerMessage::Goodbye { .. } => break,
            _ => {}
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        let names: Vec<_> = failed.keys().cloned().collect();
        Err(anyhow::anyhow!(
            "service(s) failed during up: {}",
            names.join(", ")
        ))
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
    client
        .send(ClientMessage::Subscribe {
            services: vec![service.clone()],
        })
        .await?;
    // Discard the snapshot — log replay (if any) arrives as LogChunk after it.
    let _ = client.next_event().await?;
    loop {
        let next = match client.next_event().await? {
            Some(m) => m,
            None => break,
        };
        match next {
            ServerMessage::LogChunk {
                service: s, bytes, ..
            } if s == service => {
                if let Ok(decoded) =
                    base64::engine::general_purpose::STANDARD.decode(bytes.as_bytes())
                    && let Ok(text) = String::from_utf8(decoded)
                {
                    println!("{text}");
                }
            }
            ServerMessage::Goodbye { .. } => break,
            _ => {}
        }
        if !follow {
            // Without --follow, we'd want to drain "what's already there" and
            // exit. The buffer replay arrives synchronously after Subscribed;
            // we can't tell where it ends without a sentinel. For v1, treat
            // "no --follow" as "exit after a brief idle period".
            match tokio::time::timeout(
                std::time::Duration::from_millis(50),
                client.next_event(),
            )
            .await
            {
                Ok(Ok(Some(m))) => {
                    if let ServerMessage::LogChunk {
                        service: s, bytes, ..
                    } = m
                        && s == service
                        && let Ok(decoded) =
                            base64::engine::general_purpose::STANDARD.decode(bytes.as_bytes())
                        && let Ok(text) = String::from_utf8(decoded)
                    {
                        println!("{text}");
                    }
                }
                _ => return Ok(()),
            }
        }
    }
    Ok(())
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
    if let Ok(self_exe) = std::env::current_exe()
        && let Some(parent) = self_exe.parent()
    {
        let candidate = parent.join(name);
        if candidate.exists() {
            return Ok(candidate);
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
