//! `devcloud` — user-facing remote project context adapter.

use clap::Parser;
use devme_cli::devcloud::{DevcloudCli, run_command};
use devme_config::GlobalConfig;

fn main() {
    let cli = DevcloudCli::parse();
    let cfg = GlobalConfig::load();
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("devcloud: cannot read current directory: {e}");
            std::process::exit(1);
        }
    };

    match run_command(cli.command, &cwd, &cfg.devcloud, cli.json) {
        Ok(output) => {
            print!("{output}");
        }
        Err(e) => {
            eprintln!("devcloud: {e}");
            std::process::exit(1);
        }
    }
}
