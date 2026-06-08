//! devme CLI — clap-derive parser and command dispatch.
//!
//! Conventions per ADR-0008 (clig.dev + agent-native):
//! `--json` everywhere, semantic exit codes, no spinner without a tty.

use clap::{Parser, Subcommand};
use clap_complete::Shell;
use devme_core::{ServiceSnapshot, StepSnapshot};

pub mod skill;

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
    Status {
        /// Show every worktree of the repo — its slot and each service's
        /// resolved port — not just the current one. The CLI form of the
        /// TUI sidebar; handy for an agent coordinating across worktrees.
        #[arg(long)]
        all: bool,
    },
    /// Restart a service.
    Restart { service: String },
    /// Stop a single service (keep the daemon running).
    Stop { service: String },
    /// Start a single service.
    Start { service: String },
    /// Print a service's local URL (`http://localhost:<port>`) for the
    /// current worktree. Resolves the port from the running daemon, so it
    /// reflects this worktree's slot.
    Url {
        /// Service name.
        service: String,
        /// Also open the URL in the default browser.
        #[arg(long, short = 'o')]
        open: bool,
    },
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
    /// Install or update the devme AI agent skill — a `SKILL.md` that teaches
    /// coding agents (Claude Code et al.) how to drive devme.
    ///
    /// The skill is embedded in this binary, so `devme skill install` always
    /// writes the version matching the devme you're running — no drift.
    ///
    /// `devme skill install` — into this project's `.claude/skills/devme/`.
    /// `devme skill install --global` — into `~/.claude/skills/devme/`.
    /// `devme skill status` — show where it's installed and if it's current.
    /// `devme skill uninstall` — remove a devme-managed install.
    Skill {
        #[command(subcommand)]
        action: SkillAction,
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
pub enum SkillAction {
    /// Write the embedded `SKILL.md` into a Claude Code skills directory.
    Install {
        /// Install into `~/.claude/skills/devme/` instead of this project's
        /// `.claude/skills/devme/`.
        #[arg(long, short = 'g')]
        global: bool,
        /// Overwrite even a hand-edited install or one placed by another tool.
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Remove a devme-managed skill install (refuses to touch a foreign one).
    Uninstall {
        /// Target the global `~/.claude/skills/devme/` install.
        #[arg(long, short = 'g')]
        global: bool,
    },
    /// Show where the skill is installed and whether each copy is current.
    Status,
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

/// Compact, human-friendly label for a service state — the noisy `Debug`
/// form (`Running { degraded: false, started_without: [] }`) is useless in a
/// status table.
pub fn service_state_label(state: &devme_core::ServiceState) -> String {
    use devme_core::ServiceState as S;
    match state {
        S::Stopped => "stopped".into(),
        S::Starting => "starting".into(),
        S::Running { degraded: false, .. } => "running".into(),
        S::Running { degraded: true, .. } => "degraded".into(),
        S::WaitingOnDependency { .. } => "waiting".into(),
        S::Restarting { attempt } => format!("restarting({attempt})"),
        S::CrashLoop { .. } => "crash-loop".into(),
        S::Failed { exit_code: Some(c) } => format!("failed({c})"),
        S::Failed { exit_code: None } => "failed".into(),
        S::External { healthy: true } => "external".into(),
        S::External { healthy: false } => "external(down)".into(),
    }
}

/// Compact label for a step state.
pub fn step_state_label(state: &devme_core::StepState) -> &'static str {
    use devme_core::StepState as S;
    match state {
        S::Unknown => "pending",
        S::Passed => "passed",
        S::Failed => "failed",
        S::SkippedThisRun => "skipped",
        S::Overridden => "overridden",
        S::ProvisionFailed => "provision-failed",
    }
}

/// Format a status snapshot for human consumption — one row per node,
/// declaration order preserved. Steps come first, then services, matching
/// the TUI sidebar. Services show their resolved `:PORT` so an agent sees
/// the worktree's ports at a glance.
pub fn format_status_text(services: &[ServiceSnapshot], steps: &[StepSnapshot]) -> String {
    let mut out = String::new();
    for s in steps {
        out.push_str(&format!("step    {:<20} {}\n", s.name, step_state_label(&s.state)));
    }
    for s in services {
        let port = s.port.map(|p| format!(":{p}")).unwrap_or_default();
        let line = format!(
            "service {:<20} {:<14} {}",
            s.name,
            service_state_label(&s.state),
            port
        );
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Format the cross-worktree status (`devme status --all`) as an aligned
/// table: one row per worktree, one column per service (its resolved port),
/// plus the slot. A leading `*` marks the current worktree; a worktree with
/// no running daemon shows `(not running)`.
pub fn format_status_all(reports: &[devme_tui::worktree::WorktreeReport]) -> String {
    use devme_tui::worktree::WorktreeReport;
    if reports.is_empty() {
        return "no worktrees found\n".to_string();
    }

    // Column order = union of service names in first-seen order.
    let mut svc_names: Vec<String> = Vec::new();
    for r in reports {
        if let Some(svcs) = &r.services {
            for s in svcs {
                if !svc_names.iter().any(|n| n == &s.name) {
                    svc_names.push(s.name.clone());
                }
            }
        }
    }

    let label_of = |r: &WorktreeReport| -> String {
        let mark = if r.is_cwd { "* " } else { "  " };
        format!("{mark}{}", r.label)
    };
    let cell = |r: &WorktreeReport, name: &str| -> String {
        match &r.services {
            None => "-".to_string(),
            Some(svcs) => svcs
                .iter()
                .find(|s| s.name == name)
                .and_then(|s| s.port)
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string()),
        }
    };

    let label_w = reports
        .iter()
        .map(|r| label_of(r).len())
        .chain(std::iter::once("WORKTREE".len()))
        .max()
        .unwrap();
    let slot_w = "SLOT".len();
    let col_w: Vec<usize> = svc_names
        .iter()
        .map(|name| {
            reports
                .iter()
                .map(|r| cell(r, name).len())
                .max()
                .unwrap_or(0)
                .max(name.len())
        })
        .collect();

    let mut rows: Vec<String> = Vec::new();
    // Header.
    let mut header = format!("{:<label_w$}  {:<slot_w$}", "WORKTREE", "SLOT");
    for (name, w) in svc_names.iter().zip(&col_w) {
        header.push_str(&format!("  {name:<w$}", w = *w));
    }
    rows.push(header);
    // One row per worktree.
    for r in reports {
        let slot = r.slot.map(|s| s.to_string()).unwrap_or_else(|| "-".into());
        let mut row = format!("{:<label_w$}  {:<slot_w$}", label_of(r), slot);
        if r.services.is_none() {
            row.push_str("  (not running)");
        } else {
            for (name, w) in svc_names.iter().zip(&col_w) {
                row.push_str(&format!("  {:<w$}", cell(r, name), w = *w));
            }
        }
        rows.push(row);
    }

    let mut out = String::new();
    for row in rows {
        out.push_str(row.trim_end());
        out.push('\n');
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
        assert_eq!(cli.command, Some(Command::Status { all: false }));
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
        assert_eq!(cli.command, Some(Command::Status { all: false }));
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
    fn status_text_shows_port_and_clean_state() {
        let mut s = svc(
            "backend",
            ServiceState::Running { degraded: false, started_without: vec![] },
        );
        s.port = Some(8090);
        let out = format_status_text(&[s], &[step("tools", StepState::Passed)]);
        assert!(out.contains("running"), "got: {out}");
        assert!(out.contains(":8090"), "port missing: {out}");
        // No raw Debug noise.
        assert!(!out.contains("degraded:"), "leaked Debug: {out}");
        assert!(out.contains("tools") && out.contains("passed"));
    }

    #[test]
    fn url_parses_with_service_and_open_flag() {
        let cli = Cli::parse_from(["devme", "url", "backend"]);
        assert_eq!(
            cli.command,
            Some(Command::Url { service: "backend".into(), open: false })
        );
        let cli = Cli::parse_from(["devme", "url", "-o", "backend"]);
        assert_eq!(
            cli.command,
            Some(Command::Url { service: "backend".into(), open: true })
        );
    }

    #[test]
    fn status_all_flag_parses() {
        let cli = Cli::parse_from(["devme", "status", "--all"]);
        assert_eq!(cli.command, Some(Command::Status { all: true }));
    }

    #[test]
    fn format_status_all_builds_matrix_with_ports_and_cwd_marker() {
        use devme_tui::worktree::WorktreeReport;
        let mut backend = svc(
            "backend",
            ServiceState::Running { degraded: false, started_without: vec![] },
        );
        backend.port = Some(8090);
        let reports = vec![
            WorktreeReport {
                label: "main".into(),
                path: "/repo".into(),
                is_cwd: false,
                slot: Some(0),
                services: Some(vec![{
                    let mut b = backend.clone();
                    b.port = Some(8080);
                    b
                }]),
            },
            WorktreeReport {
                label: "feat/foo".into(),
                path: "/repo-foo".into(),
                is_cwd: true,
                slot: Some(1),
                services: Some(vec![backend]),
            },
            WorktreeReport {
                label: "stale".into(),
                path: "/repo-stale".into(),
                is_cwd: false,
                slot: None,
                services: None,
            },
        ];
        let out = format_status_all(&reports);
        assert!(out.contains("WORKTREE") && out.contains("SLOT") && out.contains("backend"));
        assert!(out.contains("8080") && out.contains("8090"), "ports missing: {out}");
        assert!(out.contains("* feat/foo"), "cwd marker missing: {out}");
        assert!(out.contains("(not running)"), "stopped worktree row missing: {out}");
    }

    #[test]
    fn service_state_labels_are_compact() {
        use devme_core::ServiceState as S;
        assert_eq!(service_state_label(&S::Stopped), "stopped");
        assert_eq!(
            service_state_label(&S::Running { degraded: true, started_without: vec![] }),
            "degraded"
        );
        assert_eq!(service_state_label(&S::Failed { exit_code: Some(2) }), "failed(2)");
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
