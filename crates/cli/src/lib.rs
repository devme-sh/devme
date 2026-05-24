//! devstack CLI — clap-derive parser and command dispatch.
//!
//! Conventions per ADR-0008 (clig.dev + agent-native):
//! `--json` everywhere, semantic exit codes, no spinner without a tty.

use clap::{Parser, Subcommand};
use devstack_core::{ServiceSnapshot, StepSnapshot};

#[derive(Debug, Parser, PartialEq, Eq)]
#[command(name = "devstack", version, about = "Multi-service dev environment supervisor")]
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
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Start the supervisor (or attach to a running one) and bring services up.
    Up {
        /// Restrict to a subset of services. Empty = all.
        services: Vec<String>,
    },
    /// Shut down this instance's supervisor.
    Down,
    /// Print a snapshot of current service status.
    Status,
    /// Restart a service.
    Restart { service: String },
    /// Tail logs for a service.
    Logs {
        service: String,
        #[arg(long, short)]
        follow: bool,
    },
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
        let cli = Cli::parse_from(["devstack", "status"]);
        assert_eq!(cli.command, Some(Command::Status));
        assert!(!cli.json);
    }

    #[test]
    fn no_subcommand_defaults_to_none() {
        // Bare `devstack` enters the TUI mode — represented by None here.
        let cli = Cli::parse_from(["devstack"]);
        assert_eq!(cli.command, None);
    }

    #[test]
    fn up_collects_service_list() {
        let cli = Cli::parse_from(["devstack", "up", "backend", "db"]);
        assert_eq!(
            cli.command,
            Some(Command::Up {
                services: vec!["backend".into(), "db".into()]
            })
        );
    }

    #[test]
    fn up_without_services_is_empty_list() {
        let cli = Cli::parse_from(["devstack", "up"]);
        assert_eq!(cli.command, Some(Command::Up { services: vec![] }));
    }

    #[test]
    fn json_flag_is_global() {
        let cli = Cli::parse_from(["devstack", "--json", "status"]);
        assert!(cli.json);
        assert_eq!(cli.command, Some(Command::Status));
    }

    #[test]
    fn restart_requires_a_service_name() {
        let cli = Cli::parse_from(["devstack", "restart", "backend"]);
        assert_eq!(
            cli.command,
            Some(Command::Restart {
                service: "backend".into()
            })
        );
    }

    #[test]
    fn restart_without_service_is_an_error() {
        let result = Cli::try_parse_from(["devstack", "restart"]);
        assert!(result.is_err());
    }

    #[test]
    fn logs_follow_flag_parses() {
        let cli = Cli::parse_from(["devstack", "logs", "api", "--follow"]);
        assert_eq!(
            cli.command,
            Some(Command::Logs {
                service: "api".into(),
                follow: true
            })
        );
    }

    use devstack_core::{ServiceState, StepState};

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
    fn logs_short_follow_flag_parses() {
        let cli = Cli::parse_from(["devstack", "logs", "api", "-f"]);
        assert_eq!(
            cli.command,
            Some(Command::Logs {
                service: "api".into(),
                follow: true
            })
        );
    }
}
