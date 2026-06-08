//! devme CLI — clap-derive parser and command dispatch.
//!
//! Conventions per ADR-0008 (clig.dev + agent-native):
//! `--json` everywhere, semantic exit codes, no spinner without a tty.

use clap::{Parser, Subcommand};
use clap_complete::Shell;
use devme_core::{ServiceSnapshot, StepSnapshot};

#[derive(Debug, Parser, PartialEq, Eq)]
#[command(name = "devme", version, about = "Multi-service dev environment supervisor")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Emit machine-readable JSON instead of human-friendly output. Honored
    /// by every subcommand that prints data.
    #[arg(long, global = true)]
    pub json: bool,

    /// Disable interactive prompts. Required for non-tty contexts (CI,
    /// agents). Fails closed: any prompt aborts with exit code 2.
    #[arg(long, global = true)]
    pub no_input: bool,

    /// Suppress informational/progress output on stderr. Errors still print.
    /// Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes.
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,

    /// Strip ANSI color codes from all output. Also honored: the `NO_COLOR`
    /// environment variable (see https://no-color.org) and a non-TTY stdout.
    #[arg(long, global = true)]
    pub no_color: bool,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Start the supervisor (or attach to a running one) and bring services up.
    Up {
        /// Restrict to a subset of services. Empty = all.
        services: Vec<String>,
        /// Start services then exit without tailing logs. The daemon keeps
        /// running in the background; use `devme down` to stop it.
        #[arg(long, short = 'd')]
        detach: bool,
        /// With `-d`, block until every service is healthy (or has Started)
        /// before exiting. Pairs with `--timeout` to cap the wait.
        #[arg(long)]
        wait: bool,
        /// Seconds to wait for `--wait`. 0 means "no timeout" (docker
        /// convention). Default 30s, only consulted with `--wait`.
        #[arg(long, default_value_t = 30, requires = "wait")]
        timeout: u64,
    },
    /// Shut down this instance's supervisor.
    Down {
        /// Seconds to wait for graceful service stops before SIGKILL.
        /// Matches `docker compose down -t`.
        #[arg(long, short = 't', default_value_t = 10)]
        timeout: u64,
    },
    /// Print a snapshot of current service status.
    Status,
    /// Restart a service.
    Restart { service: String },
    /// Stop a single service (keep the daemon running).
    Stop { service: String },
    /// Start a single service.
    Start { service: String },
    /// Tail logs for a service.
    Logs {
        service: String,
        #[arg(long, short)]
        follow: bool,
        /// Show only the last N lines of buffered output before following.
        /// 0 means "all" (the daemon's full ring). Default 200 — a `docker
        /// compose logs` of a long-running service is a wall of text.
        #[arg(long, default_value_t = 200)]
        tail: usize,
    },
    /// Print a shell completion script. Pipe into your shell's completion
    /// directory: `devme completions fish > ~/.config/fish/completions/devme.fish`.
    Completions {
        /// Target shell.
        shell: Shell,
    },
    /// Diagnostic snapshot: service states + recent error logs. Designed for
    /// agents — outputs structured JSON with everything needed to diagnose
    /// failures without multiple round-trips.
    Doctor {
        /// Maximum log lines per service (default 50).
        #[arg(long, default_value_t = 50)]
        tail: usize,
    },
    /// View or change devme global settings.
    ///
    /// `devme config` — list all settings.
    /// `devme config get <key>` — print the value of a setting.
    /// `devme config set <key> <value>` — set a value.
    /// `devme config unset <key>` — remove a value.
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Manage git worktrees in coordination with devme.
    ///
    /// `devme worktree rm <target>` — tear down a worktree's stack, run its
    /// `[stack] on_destroy` hook, then `git worktree remove` it.
    Worktree {
        #[command(subcommand)]
        action: WorktreeAction,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum WorktreeAction {
    /// Remove a worktree: stop its instance stack, run the `[stack]
    /// on_destroy` hook (resolved against its slot/branch while the worktree
    /// still exists), then `git worktree remove` it. This is the
    /// deterministic path that makes `on_destroy` fire — a bare `git
    /// worktree remove` bypasses devme and runs no hook.
    Rm {
        /// Which worktree to remove: a path, its directory name, or its
        /// branch name.
        target: String,
        /// Forward `--force` to `git worktree remove` (removes even with
        /// uncommitted changes / untracked files).
        #[arg(long, short = 'f')]
        force: bool,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum ConfigAction {
    /// Print the value of a setting.
    Get { key: String },
    /// Set a value.
    Set { key: String, value: String },
    /// Remove a value (reset to default).
    Unset { key: String },
}

/// Format a status snapshot for human consumption — one row per node,
/// declaration order preserved. Steps come first, then services, matching
/// the TUI sidebar.
pub fn format_status_text(services: &[ServiceSnapshot], steps: &[StepSnapshot]) -> String {
    let mut out = String::new();
    for s in steps {
        out.push_str(&format!("step    {:<24} {:?}\n", s.name, s.state));
    }
    for s in services {
        out.push_str(&format!("service {:<24} {:?}\n", s.name, s.state));
    }
    out
}

/// Format a status snapshot as JSON. Stable shape:
/// `{ "services": [...], "steps": [...] }`.
pub fn format_status_json(
    services: &[ServiceSnapshot],
    steps: &[StepSnapshot],
) -> serde_json::Value {
    serde_json::json!({
        "services": services,
        "steps": steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_subcommand() {
        let cli = Cli::parse_from(["devme", "status"]);
        assert_eq!(cli.command, Some(Command::Status));
        assert!(!cli.json);
    }

    #[test]
    fn no_subcommand_defaults_to_none() {
        // Bare `devme` enters the TUI mode — represented by None here.
        let cli = Cli::parse_from(["devme"]);
        assert_eq!(cli.command, None);
    }

    #[test]
    fn up_collects_service_list() {
        let cli = Cli::parse_from(["devme", "up", "backend", "db"]);
        assert_eq!(
            cli.command,
            Some(Command::Up {
                services: vec!["backend".into(), "db".into()],
                detach: false,
                wait: false,
                timeout: 30,
            })
        );
    }

    #[test]
    fn up_without_services_is_empty_list() {
        let cli = Cli::parse_from(["devme", "up"]);
        assert_eq!(
            cli.command,
            Some(Command::Up { services: vec![], detach: false, wait: false, timeout: 30 })
        );
    }

    #[test]
    fn up_detach_short_flag_parses() {
        let cli = Cli::parse_from(["devme", "up", "-d"]);
        assert_eq!(
            cli.command,
            Some(Command::Up { services: vec![], detach: true, wait: false, timeout: 30 })
        );
    }

    #[test]
    fn up_detach_long_flag_parses() {
        let cli = Cli::parse_from(["devme", "up", "--detach", "backend"]);
        assert_eq!(
            cli.command,
            Some(Command::Up {
                services: vec!["backend".into()],
                detach: true,
                wait: false,
                timeout: 30,
            })
        );
    }

    #[test]
    fn completions_subcommand_parses() {
        let cli = Cli::parse_from(["devme", "completions", "fish"]);
        assert_eq!(
            cli.command,
            Some(Command::Completions { shell: Shell::Fish })
        );
    }

    #[test]
    fn completions_renders_a_non_empty_script() {
        // Direct rendering check — we don't want a regression that produces
        // an empty / mangled script for any of the supported shells.
        let shells = [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell];
        let mut cmd = <Cli as clap::CommandFactory>::command();
        for shell in shells {
            let mut buf: Vec<u8> = Vec::new();
            clap_complete::generate(shell, &mut cmd, "devme", &mut buf);
            let s = String::from_utf8(buf).unwrap();
            assert!(!s.is_empty(), "{shell:?} completion script was empty");
            assert!(s.contains("devme"), "{shell:?} script omits binary name");
        }
    }

    #[test]
    fn json_flag_is_global() {
        let cli = Cli::parse_from(["devme", "--json", "status"]);
        assert!(cli.json);
        assert_eq!(cli.command, Some(Command::Status));
    }

    #[test]
    fn restart_requires_a_service_name() {
        let cli = Cli::parse_from(["devme", "restart", "backend"]);
        assert_eq!(
            cli.command,
            Some(Command::Restart {
                service: "backend".into()
            })
        );
    }

    #[test]
    fn restart_without_service_is_an_error() {
        let result = Cli::try_parse_from(["devme", "restart"]);
        assert!(result.is_err());
    }

    #[test]
    fn logs_follow_flag_parses() {
        let cli = Cli::parse_from(["devme", "logs", "api", "--follow"]);
        assert_eq!(
            cli.command,
            Some(Command::Logs {
                service: "api".into(),
                follow: true,
                tail: 200,
            })
        );
    }

    use devme_core::{ServiceState, StepState};

    fn svc(name: &str, state: ServiceState) -> ServiceSnapshot {
        ServiceSnapshot {
            name: name.into(),
            state,
            pid: None,
            port: None,
            restart_count: 0,
        }
    }

    fn step(name: &str, state: StepState) -> StepSnapshot {
        StepSnapshot {
            name: name.into(),
            state,
        }
    }

    #[test]
    fn format_status_text_lists_steps_first_then_services() {
        let services = vec![
            svc(
                "backend",
                ServiceState::Running {
                    degraded: false,
                    started_without: vec![],
                },
            ),
            svc("db", ServiceState::Stopped),
        ];
        let steps = vec![step("tools", StepState::Passed)];

        let out = format_status_text(&services, &steps);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("step    tools"));
        assert!(lines[1].starts_with("service backend"));
        assert!(lines[2].starts_with("service db"));
    }

    #[test]
    fn format_status_json_round_trips_via_serde() {
        let services = vec![svc("db", ServiceState::Stopped)];
        let steps = vec![step("tools", StepState::Passed)];

        let value = format_status_json(&services, &steps);
        assert_eq!(value["services"][0]["name"], "db");
        assert_eq!(value["steps"][0]["name"], "tools");
    }

    #[test]
    fn empty_snapshot_formats_to_empty_text() {
        assert_eq!(format_status_text(&[], &[]), "");
    }

    #[test]
    fn stop_requires_a_service_name() {
        let cli = Cli::parse_from(["devme", "stop", "backend"]);
        assert_eq!(
            cli.command,
            Some(Command::Stop {
                service: "backend".into()
            })
        );
    }

    #[test]
    fn start_requires_a_service_name() {
        let cli = Cli::parse_from(["devme", "start", "backend"]);
        assert_eq!(
            cli.command,
            Some(Command::Start {
                service: "backend".into()
            })
        );
    }

    #[test]
    fn logs_short_follow_flag_parses() {
        let cli = Cli::parse_from(["devme", "logs", "api", "-f"]);
        assert_eq!(
            cli.command,
            Some(Command::Logs {
                service: "api".into(),
                follow: true,
                tail: 200,
            })
        );
    }

    #[test]
    fn config_list_parses() {
        let cli = Cli::parse_from(["devme", "config"]);
        assert_eq!(cli.command, Some(Command::Config { action: None }));
    }

    #[test]
    fn config_set_parses() {
        let cli = Cli::parse_from(["devme", "config", "set", "docker.daemon", "orbstack"]);
        assert_eq!(
            cli.command,
            Some(Command::Config {
                action: Some(ConfigAction::Set {
                    key: "docker.daemon".into(),
                    value: "orbstack".into(),
                })
            })
        );
    }

    #[test]
    fn worktree_rm_parses_with_target() {
        let cli = Cli::parse_from(["devme", "worktree", "rm", "IWP-86"]);
        assert_eq!(
            cli.command,
            Some(Command::Worktree {
                action: WorktreeAction::Rm {
                    target: "IWP-86".into(),
                    force: false,
                }
            })
        );
    }

    #[test]
    fn worktree_rm_force_flag_parses() {
        let cli = Cli::parse_from(["devme", "worktree", "rm", "-f", "../wt"]);
        assert_eq!(
            cli.command,
            Some(Command::Worktree {
                action: WorktreeAction::Rm {
                    target: "../wt".into(),
                    force: true,
                }
            })
        );
    }

    #[test]
    fn worktree_rm_without_target_is_an_error() {
        assert!(Cli::try_parse_from(["devme", "worktree", "rm"]).is_err());
    }

    #[test]
    fn config_get_parses() {
        let cli = Cli::parse_from(["devme", "config", "get", "docker.daemon"]);
        assert_eq!(
            cli.command,
            Some(Command::Config {
                action: Some(ConfigAction::Get {
                    key: "docker.daemon".into(),
                })
            })
        );
    }
}
