//! `devme` — user-facing CLI binary. Argument parsing and shared
//! formatters live in [`devme_cli`]; this binary dispatches.

use clap::Parser;
use devme_cli::{Cli, Command, format_status_json, format_status_text};
use devme_core::{ClientMessage, ServerMessage};

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
    match cli.command {
        None => match launch_tui().await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("devme: {e}");
                1
            }
        },
        Some(Command::Status) => match status(cli.json).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("devme: {e}");
                1
            }
        },
        Some(Command::Down) => match down().await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("devme: {e}");
                1
            }
        },
        Some(_) => {
            eprintln!("devme: subcommand not yet implemented");
            1
        }
    }
}

async fn down() -> anyhow::Result<()> {
    let sock = socket_path();
    let mut client = match devme_client::Client::connect(&sock).await {
        Ok(c) => c,
        Err(_) => {
            // No daemon running — `down` is a no-op success.
            return Ok(());
        }
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
