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
        None => {
            eprintln!("devme: TUI mode not yet implemented");
            1
        }
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

fn socket_path() -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    devme_config::paths::supervisor_socket(&cwd)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/devme.sock"))
}
