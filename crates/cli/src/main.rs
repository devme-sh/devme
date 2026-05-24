//! `devstack` — user-facing CLI binary. Argument parsing and shared
//! formatters live in [`devstack_cli`]; this binary dispatches.

use clap::Parser;
use devstack_cli::{Cli, Command, format_status_json, format_status_text};
use devstack_core::{ClientMessage, ServerMessage};

fn main() {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("devstack: tokio init failed: {e}");
            std::process::exit(1);
        }
    };
    std::process::exit(runtime.block_on(run(cli)));
}

async fn run(cli: Cli) -> i32 {
    match cli.command {
        None => {
            eprintln!("devstack: TUI mode not yet implemented");
            1
        }
        Some(Command::Status) => match status(cli.json).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("devstack: {e}");
                1
            }
        },
        Some(_) => {
            eprintln!("devstack: subcommand not yet implemented");
            1
        }
    }
}

async fn status(as_json: bool) -> anyhow::Result<()> {
    let sock = socket_path();
    let mut client = devstack_client::Client::connect(&sock).await?;
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
    // V1: single fixed location. Per-instance routing comes with the
    // instance_id work (ADR-0006).
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return std::path::PathBuf::from(dir).join("devstack.sock");
    }
    std::path::PathBuf::from("/tmp/devstack.sock")
}
