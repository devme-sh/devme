//! devme CLI — clap-derive parser and command dispatch.
//!
//! Conventions per ADR-0008 (clig.dev + agent-native):
//! `--json` everywhere, semantic exit codes, no spinner without a tty.

use clap::{Parser, Subcommand};
use clap_complete::Shell;
use devme_core::{ServiceSnapshot, StepSnapshot};

pub mod remote;
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

    /// Run suggested provision fixes without asking — promotes every
    /// `trust = "prompt"` step to `auto` for this invocation. `trust =
    /// "manual"` steps are still never run. Intended for CI and
    /// "I know what I want" mode (ADR-0002).
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    /// Force a command to run against the *local* daemon even when this
    /// project has a live remote sync. By default, daemon-facing commands
    /// (`status`, `logs`, `up`, …) transparently proxy to the remote host
    /// while a sync is active; `--local` is the escape hatch.
    #[arg(long, global = true)]
    pub local: bool,
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
    /// Shut down this worktree's supervisor (and the repo-shared services,
    /// if no sibling worktree is still using them). Use `--all` to stop
    /// every worktree's stack in the repo.
    Down {
        /// Seconds to wait for graceful service stops before SIGKILL.
        /// Matches `docker compose down -t`.
        #[arg(long, short = 't', default_value_t = 10)]
        timeout: u64,
        /// Stop every worktree of the repo, not just the current one — then
        /// the repo-shared services. The repo-wide counterpart to the
        /// current-worktree default, mirroring `devme status --all`.
        #[arg(long)]
        all: bool,
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
    /// Tail logs for a service (or all services interleaved by time).
    Logs {
        /// Service to tail. Omit to interleave every service's logs by time.
        /// (A step name is rejected with a pointer to `devme doctor` — steps
        /// have check/provision *output*, not a runtime stream.)
        service: Option<String>,
        #[arg(long, short)]
        follow: bool,
        /// Show only the last N lines of buffered output before following.
        /// 0 means "all". Default 200 — a `docker compose logs` of a
        /// long-running service is a wall of text.
        #[arg(long, default_value_t = 200)]
        tail: usize,
        /// Only show lines newer than this: a duration (`30s`, `5m`, `2h`,
        /// `1d`) or an epoch-ms timestamp. Reaches into disk history, so it
        /// survives ring eviction and daemon restarts.
        #[arg(long)]
        since: Option<String>,
        /// Emit one JSON object per line — `{ts, service, stream, text}`, ANSI
        /// stripped — for piping to `jq`. Composes with `--follow`.
        #[arg(long)]
        json: bool,
    },
    /// Print a shell completion script. Pipe into your shell's completion
    /// directory: `devme completions fish > ~/.config/fish/completions/devme.fish`.
    Completions {
        /// Target shell.
        shell: Shell,
    },
    /// Diagnostic snapshot: service states + recent error (stderr) lines, plus
    /// step check results. Designed for agents — outputs structured JSON with
    /// everything needed to diagnose failures without multiple round-trips.
    ///
    /// `devme doctor <name>` zooms into one node: a step's check/provision
    /// output (this is where step output lives — `logs` is services-only), or
    /// a service's state + recent stderr.
    Doctor {
        /// Zoom into one step or service by name.
        name: Option<String>,
        /// Maximum log lines per service (default 50).
        #[arg(long, default_value_t = 50)]
        tail: usize,
    },
    /// View or change devme global settings, or lint this project's config.
    ///
    /// `devme config` — list all settings.
    /// `devme config get <key>` — print the value of a setting.
    /// `devme config set <key> <value>` — set a value.
    /// `devme config unset <key>` — remove a value.
    /// `devme config check` — validate and lint this project's `devme.toml`.
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
    /// Live-sync this project to a remote dev host and attach to its
    /// (remote-primary) dev environment.
    ///
    /// The remote runs the stack, supervisor, and `devme tui`; your laptop
    /// edits files (synced via Mutagen) and views the TUI. Work survives
    /// closing the lid — reopen and it reconciles. Configure the host once:
    /// `devme config set remote.host <ssh-target>`.
    ///
    /// `devme remote` — ensure the live-sync, then attach.
    /// `devme remote doctor` — preflight the local + remote setup.
    /// `devme remote status` — conflict-aware sync state.
    /// `devme remote conflicts` — list halted-sync conflicts + how to resolve.
    /// `devme remote sync` — reconcile without attaching.
    /// `devme remote flush` — force an immediate reconcile (e.g. on wake).
    /// `devme remote stop` — terminate the live-sync.
    Remote {
        #[command(subcommand)]
        action: Option<RemoteAction>,
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
    /// Create a worktree for a branch (creating the branch if needed), then
    /// run the `[stack] on_create` hook in it — so an agent gets a
    /// ready-to-use worktree whose per-worktree setup has already run. By
    /// default the path is a sibling of the main worktree named
    /// `<repo>-<branch-leaf>`; pass one to override.
    Add {
        /// Branch to check out (or create) in the new worktree.
        branch: String,
        /// Optional destination path (default: `<repo>-<branch-leaf>` next to
        /// the main worktree).
        path: Option<String>,
    },
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
pub enum RemoteAction {
    /// Preflight the local tooling, host reachability, and remote
    /// `git`/`devme` — with a fixable hint per failure.
    Doctor,
    /// Show the live-sync's conflict-aware state for this project.
    Status {
        /// Refresh a single compact status line until Ctrl-C — for a laptop-
        /// side split pane next to an attached session.
        #[arg(long, short = 'w')]
        watch: bool,
    },
    /// List unresolved sync conflicts (two-way-safe halts on conflict): the
    /// paths involved, the full alpha/beta detail, and how to resolve them.
    Conflicts,
    /// Reconcile the live-sync now without attaching.
    Sync,
    /// Force an immediate reconcile (e.g. right after the laptop wakes).
    Flush,
    /// Terminate the live-sync. The remote files stay; the live link stops.
    Stop,
    /// Reconcile every devme-managed sync now. Run by the wake-hook so changes
    /// the remote made while the laptop slept come down immediately.
    Wake,
    /// Install (or `--uninstall`) the OS wake hook that runs `devme remote
    /// wake` on resume — macOS sleepwatcher's `~/.wakeup`.
    WakeHook {
        /// Remove the hook instead of installing it.
        #[arg(long)]
        uninstall: bool,
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
    /// Validate and lint this project's `devme.toml`: report parse/validation
    /// errors (fatal) and advisory warnings (e.g. a web service that won't
    /// open because it has no `url`). Exits non-zero if any errors are found.
    Check,
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
        S::WaitingOnDependency { blocked_by } => format!("waiting on {blocked_by}"),
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

/// ANSI escape codes for the status renderer. Emitted only when `color` is
/// true (a TTY with color enabled); see [`format_status_text`].
mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BLUE: &str = "\x1b[34m";
    pub const CYAN: &str = "\x1b[36m";
    pub const BRIGHT_RED: &str = "\x1b[91m";
}

/// Wrap `s` in an ANSI `code`…reset pair, or return it untouched when color
/// is disabled. Padding must be applied to `s` *before* calling this so the
/// invisible escape bytes don't throw off column alignment.
fn paint(color: bool, code: &str, s: &str) -> String {
    if color {
        format!("{code}{s}{}", ansi::RESET)
    } else {
        s.to_string()
    }
}

/// Status glyph + color for a service state. Glyphs are all single terminal
/// cells so they don't disturb alignment.
fn service_glyph(state: &devme_core::ServiceState) -> (&'static str, &'static str) {
    use devme_core::ServiceState as S;
    match state {
        S::Running { degraded: false, .. } => ("●", ansi::GREEN),
        S::Running { degraded: true, .. } => ("◐", ansi::YELLOW),
        S::Starting => ("◐", ansi::CYAN),
        S::WaitingOnDependency { .. } => ("◌", ansi::CYAN),
        S::Restarting { .. } => ("↻", ansi::YELLOW),
        S::CrashLoop { .. } => ("✗", ansi::BRIGHT_RED),
        S::Failed { .. } => ("✗", ansi::RED),
        S::Stopped => ("○", ansi::DIM),
        S::External { healthy: true } => ("◆", ansi::GREEN),
        S::External { healthy: false } => ("◆", ansi::RED),
    }
}

/// Status glyph + color for a step state.
fn step_glyph(state: &devme_core::StepState) -> (&'static str, &'static str) {
    use devme_core::StepState as S;
    match state {
        S::Passed => ("✔", ansi::GREEN),
        S::Failed | S::ProvisionFailed => ("✗", ansi::RED),
        S::Unknown => ("·", ansi::DIM),
        S::SkippedThisRun => ("–", ansi::DIM),
        S::Overridden => ("✔", ansi::YELLOW),
    }
}

/// Closing one-liner: an actionable warning when anything is unhealthy,
/// otherwise a quiet running/passing tally.
fn status_summary_line(
    services: &[ServiceSnapshot],
    steps: &[StepSnapshot],
    color: bool,
) -> String {
    use devme_core::{ServiceState as S, StepState};

    let problems: Vec<&ServiceSnapshot> = services
        .iter()
        .filter(|s| {
            matches!(
                s.state,
                S::Failed { .. }
                    | S::CrashLoop { .. }
                    | S::Running { degraded: true, .. }
                    | S::External { healthy: false }
            )
        })
        .collect();

    let step_problems: Vec<&StepSnapshot> = steps
        .iter()
        .filter(|s| matches!(s.state, StepState::Failed | StepState::ProvisionFailed))
        .collect();

    if !problems.is_empty() || !step_problems.is_empty() {
        let list = step_problems
            .iter()
            .map(|s| format!("{} {}", s.name, step_state_label(&s.state)))
            .chain(problems.iter().map(|s| format!("{} {}", s.name, service_state_label(&s.state))))
            .collect::<Vec<_>>()
            .join(", ");
        // Name a concrete next command: logs for a broken service, up to
        // (re-)provision when only steps are failing.
        let hint = match problems.first() {
            Some(svc) => format!("run `devme logs {}`", svc.name),
            None => "run `devme up` to provision".to_string(),
        };
        let body = format!("  ⚠ {list} — {hint}");
        return paint(color, ansi::YELLOW, &body);
    }

    let running = services
        .iter()
        .filter(|s| {
            matches!(
                s.state,
                S::Running { degraded: false, .. } | S::External { healthy: true }
            )
        })
        .count();
    let total = services.len();

    let msg = if total == 0 {
        let passed = steps.iter().filter(|s| matches!(s.state, StepState::Passed)).count();
        format!("  {passed}/{} steps passed", steps.len())
    } else if running == total {
        "  ✔ all services running".to_string()
    } else if running == 0 {
        // Nothing running and nothing broken: name the obvious next move.
        format!("  0/{total} services running — start with `devme up -d`")
    } else {
        format!("  {running}/{total} services running")
    };
    paint(color, ansi::DIM, &msg)
}

/// Format a status snapshot for human consumption — grouped under `STEPS`
/// and `SERVICES` headers, declaration order preserved within each, matching
/// the TUI sidebar. Each row leads with a colored status glyph; services show
/// a clickable `http://host:PORT` URL plus pid and restart count so an agent
/// (or human) sees the worktree's live state at a glance. `descriptions`
/// (node name → `description` from devme.toml) annotates rows that would
/// otherwise be bare, so a step name like `gcloud_adc` explains itself;
/// steps the daemon hasn't evaluated get a "runs on `devme up`" note instead
/// of an unexplained "pending". A closing line surfaces anything unhealthy
/// with the command to dig in. `color` gates all ANSI; callers pass `false`
/// for pipes / `NO_COLOR` / `--no-color`.
pub fn format_status_text(
    services: &[ServiceSnapshot],
    steps: &[StepSnapshot],
    descriptions: &std::collections::HashMap<String, String>,
    color: bool,
) -> String {
    use devme_core::ServiceState;

    if services.is_empty() && steps.is_empty() {
        return "  No services or steps declared in devme.toml.\n".to_string();
    }

    let host = crate::remote::advertise_host();

    // One name column wide enough for steps and services together, so the two
    // groups stay vertically aligned.
    let name_w = steps
        .iter()
        .map(|s| s.name.len())
        .chain(services.iter().map(|s| s.name.len()))
        .max()
        .unwrap_or(0);
    let label_w = steps
        .iter()
        .map(|s| step_state_label(&s.state).len())
        .chain(services.iter().map(|s| service_state_label(&s.state).len()))
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    out.push('\n');

    if !steps.is_empty() {
        out.push_str(&paint(color, ansi::DIM, "  STEPS"));
        out.push('\n');
        for s in steps {
            let (glyph, gcolor) = step_glyph(&s.state);
            let label = step_state_label(&s.state);
            // An unevaluated step isn't broken — it just runs with the stack.
            // Say so instead of leaving "pending" unexplained. Evaluated
            // steps carry their devme.toml description.
            let note = if matches!(s.state, devme_core::StepState::Unknown) {
                Some("runs on `devme up`".to_string())
            } else {
                descriptions.get(&s.name).cloned()
            };
            let line = format!(
                "    {} {}  {}  {}",
                paint(color, gcolor, glyph),
                paint(color, ansi::BOLD, &format!("{:<name_w$}", s.name)),
                paint(color, gcolor, &format!("{label:<label_w$}")),
                paint(color, ansi::DIM, note.as_deref().unwrap_or_default()),
            );
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }

    if !services.is_empty() {
        if !steps.is_empty() {
            out.push('\n');
        }
        out.push_str(&paint(color, ansi::DIM, "  SERVICES"));
        out.push('\n');
        for s in services {
            let (glyph, gcolor) = service_glyph(&s.state);
            let label = service_state_label(&s.state);

            let mut detail = String::new();
            // Prefer the configured URL template (mirrors the TUI's copy/open
            // resolution); fall back to a plain http URL from the port.
            let url = match &s.url {
                Some(t) if !(t.contains("{port}") && s.port.is_none()) => {
                    let mut u = t.replace("{host}", &host);
                    if let Some(p) = s.port {
                        u = u.replace("{port}", &p.to_string());
                    }
                    Some(u)
                }
                Some(_) => None,
                None => s.port.map(|p| format!("http://{host}:{p}")),
            };
            if let Some(u) = url {
                detail.push_str(&paint(color, ansi::BLUE, &u));
            }
            if let Some(pid) = s.pid
                && matches!(s.state, ServiceState::Running { .. })
            {
                if !detail.is_empty() {
                    detail.push_str("  ");
                }
                detail.push_str(&paint(color, ansi::DIM, &format!("pid {pid}")));
            }
            if s.restart_count > 0 {
                if !detail.is_empty() {
                    detail.push_str("  ");
                }
                detail.push_str(&paint(color, ansi::DIM, &format!("↻{}", s.restart_count)));
            }
            // A row with no live detail (typically stopped) still benefits
            // from saying what the service *is*.
            if detail.is_empty()
                && let Some(desc) = descriptions.get(&s.name)
            {
                detail.push_str(&paint(color, ansi::DIM, desc));
            }

            let line = format!(
                "    {} {}  {}  {}",
                paint(color, gcolor, glyph),
                paint(color, ansi::BOLD, &format!("{:<name_w$}", s.name)),
                paint(color, gcolor, &format!("{label:<label_w$}")),
                detail,
            );
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }

    out.push('\n');
    out.push_str(&status_summary_line(services, steps, color));
    out.push('\n');
    out
}

/// Format the cross-worktree status (`devme status --all`) as an aligned
/// matrix: one row per worktree, one column per service. Each cell is the
/// service's state glyph fused to its resolved port (`●8080` = running on
/// 8080, `◌` = still waiting on a dependency, `·` = not present), so a
/// glance across a row shows both *where* and *whether* everything runs —
/// matching the per-worktree `status` look. A leading `*` (bold) marks the
/// current worktree; a worktree with no running daemon shows
/// `(not running)`. `color` gates ANSI exactly like [`format_status_text`].
pub fn format_status_all(reports: &[devme_tui::worktree::WorktreeReport], color: bool) -> String {
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

    // Pad by chars, not bytes — the state glyphs are multi-byte UTF-8 but
    // render as a single terminal cell, so byte-based `{:<w$}` would
    // misalign every column after a glyph.
    let pad = |s: &str, w: usize| -> String {
        let mut out = s.to_string();
        out.extend(std::iter::repeat_n(
            ' ',
            w.saturating_sub(s.chars().count()),
        ));
        out
    };

    let label_of = |r: &WorktreeReport| -> String {
        let mark = if r.is_cwd { "* " } else { "  " };
        format!("{mark}{}", r.label)
    };
    // Cell text + color: glyph fused to the resolved port. A service with no
    // live port (stopped, or still waiting) is just its glyph; a service the
    // worktree doesn't declare is a dim `·`.
    let cell = |r: &WorktreeReport, name: &str| -> (String, &'static str) {
        let Some(svcs) = &r.services else {
            return ("·".into(), ansi::DIM);
        };
        match svcs.iter().find(|s| s.name == name) {
            None => ("·".into(), ansi::DIM),
            Some(s) => {
                let (glyph, gcolor) = service_glyph(&s.state);
                let txt = match s.port {
                    Some(p) => format!("{glyph}{p}"),
                    None => glyph.to_string(),
                };
                (txt, gcolor)
            }
        }
    };

    let label_w = reports
        .iter()
        .map(|r| label_of(r).chars().count())
        .chain(std::iter::once("WORKTREE".len()))
        .max()
        .unwrap();
    let slot_w = "SLOT".len();
    let col_w: Vec<usize> = svc_names
        .iter()
        .map(|name| {
            reports
                .iter()
                .map(|r| cell(r, name).0.chars().count())
                .max()
                .unwrap_or(0)
                .max(name.chars().count())
        })
        .collect();

    let mut out = String::new();
    out.push('\n');

    // Header.
    let mut header = format!("  {}  {}", pad("WORKTREE", label_w), pad("SLOT", slot_w));
    for (name, w) in svc_names.iter().zip(&col_w) {
        header.push_str("  ");
        header.push_str(&pad(name, *w));
    }
    out.push_str(&paint(color, ansi::DIM, header.trim_end()));
    out.push('\n');

    // One row per worktree.
    for r in reports {
        let slot = r.slot.map(|s| s.to_string()).unwrap_or_else(|| "-".into());
        let label = pad(&label_of(r), label_w);
        let label = if r.is_cwd {
            paint(color, ansi::BOLD, &label)
        } else {
            label
        };
        let mut row = format!("  {label}  {}", pad(&slot, slot_w));
        if r.services.is_none() {
            row.push_str("  ");
            row.push_str(&paint(color, ansi::DIM, "(not running)"));
        } else {
            for (name, w) in svc_names.iter().zip(&col_w) {
                let (txt, gcolor) = cell(r, name);
                row.push_str("  ");
                row.push_str(&paint(color, gcolor, &pad(&txt, *w)));
            }
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }

    // Legend — the glyphs are the whole point of the matrix, so say what
    // they mean.
    if reports.iter().any(|r| r.services.is_some()) {
        out.push('\n');
        out.push_str(&paint(
            color,
            ansi::DIM,
            "  ● running  ◐ starting  ◌ waiting  ○ stopped  ✗ failed",
        ));
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
                service: Some("api".into()),
                follow: true,
                tail: 200,
                since: None,
                json: false,
            })
        );
    }

    #[test]
    fn logs_no_service_means_all() {
        let cli = Cli::parse_from(["devme", "logs"]);
        assert_eq!(
            cli.command,
            Some(Command::Logs {
                service: None,
                follow: false,
                tail: 200,
                since: None,
                json: false,
            })
        );
    }

    #[test]
    fn logs_since_and_json_parse() {
        let cli = Cli::parse_from(["devme", "logs", "web", "--since", "5m", "--json"]);
        assert_eq!(
            cli.command,
            Some(Command::Logs {
                service: Some("web".into()),
                follow: false,
                tail: 200,
                since: Some("5m".into()),
                json: true,
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
            url: None,
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

        let out = format_status_text(&services, &steps, &Default::default(), false);
        // Steps group precedes the services group, declaration order preserved.
        let tools = out.find("tools").unwrap();
        let backend = out.find("backend").unwrap();
        let db = out.find("db").unwrap();
        assert!(out.find("STEPS").unwrap() < out.find("SERVICES").unwrap());
        assert!(tools < backend && backend < db, "order wrong: {out}");
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
    fn empty_snapshot_prints_a_friendly_note() {
        let out = format_status_text(&[], &[], &Default::default(), false);
        assert!(out.contains("No services or steps"), "got: {out}");
        // Nothing declared means nothing to color — no stray escape bytes.
        assert!(!out.contains('\x1b'), "leaked ANSI: {out:?}");
    }

    #[test]
    fn status_text_shows_port_and_clean_state() {
        let mut s = svc(
            "backend",
            ServiceState::Running { degraded: false, started_without: vec![] },
        );
        s.port = Some(8090);
        let out = format_status_text(&[s], &[step("tools", StepState::Passed)], &Default::default(), false);
        assert!(out.contains("running"), "got: {out}");
        assert!(out.contains(":8090"), "port missing: {out}");
        // The port is rendered as a clickable URL, not a bare `:PORT`.
        assert!(out.contains("http://"), "url missing: {out}");
        // No raw Debug noise.
        assert!(!out.contains("degraded:"), "leaked Debug: {out}");
        assert!(out.contains("tools") && out.contains("passed"));
    }

    #[test]
    fn status_footer_warns_and_points_at_logs_when_unhealthy() {
        let services = vec![
            svc("api", ServiceState::Failed { exit_code: Some(1) }),
            svc("db", ServiceState::Running { degraded: false, started_without: vec![] }),
        ];
        let out = format_status_text(&services, &[], &Default::default(), false);
        let footer = out.lines().last().unwrap();
        assert!(footer.contains("⚠"), "no warning glyph: {out}");
        assert!(footer.contains("api failed(1)"), "missing failed service: {out}");
        assert!(footer.contains("devme logs api"), "hint should name the service: {out}");
    }

    #[test]
    fn status_footer_warns_on_failed_steps_with_provision_hint() {
        let steps = vec![step("rust", StepState::Failed)];
        let out = format_status_text(&[], &steps, &Default::default(), false);
        let footer = out.lines().last().unwrap();
        assert!(footer.contains("rust failed"), "missing failed step: {out}");
        assert!(footer.contains("devme up"), "missing provision hint: {out}");
    }

    #[test]
    fn status_footer_tallies_when_all_healthy() {
        let services = vec![
            svc("api", ServiceState::Running { degraded: false, started_without: vec![] }),
            svc("db", ServiceState::Running { degraded: false, started_without: vec![] }),
        ];
        let out = format_status_text(&services, &[], &Default::default(), false);
        assert!(out.lines().last().unwrap().contains("all services running"), "got: {out}");
    }

    #[test]
    fn status_annotates_with_descriptions_and_up_note() {
        let steps = vec![
            step("gcloud_adc", StepState::Passed),
            step("migrate", StepState::Unknown),
        ];
        let services = vec![svc("db", ServiceState::Stopped)];
        let descriptions: std::collections::HashMap<String, String> = [
            ("gcloud_adc".to_string(), "gcloud app-default creds".to_string()),
            ("db".to_string(), "Postgres via Docker".to_string()),
        ]
        .into();
        let out = format_status_text(&services, &steps, &descriptions, false);
        assert!(out.contains("gcloud app-default creds"), "step desc missing: {out}");
        assert!(out.contains("runs on `devme up`"), "up note missing: {out}");
        assert!(out.contains("Postgres via Docker"), "stopped svc desc missing: {out}");
    }

    #[test]
    fn status_footer_suggests_up_when_nothing_runs() {
        let services = vec![svc("db", ServiceState::Stopped)];
        let out = format_status_text(&services, &[], &Default::default(), false);
        assert!(
            out.lines().last().unwrap().contains("devme up -d"),
            "missing up hint: {out}"
        );
    }

    #[test]
    fn status_resolves_url_template_over_default_http() {
        let mut s = svc(
            "db",
            ServiceState::Running { degraded: false, started_without: vec![] },
        );
        s.port = Some(5432);
        s.url = Some("postgres://{host}:{port}/dev".into());
        let out = format_status_text(&[s], &[], &Default::default(), false);
        assert!(out.contains("postgres://localhost:5432/dev"), "got: {out}");
        assert!(!out.contains("http://"), "template should win: {out}");
    }

    #[test]
    fn status_color_wraps_glyphs_in_ansi_and_no_color_stays_plain() {
        let services = vec![svc("db", ServiceState::Stopped)];
        let colored = format_status_text(&services, &[], &Default::default(), true);
        assert!(colored.contains('\x1b'), "expected ANSI when color=true");
        let plain = format_status_text(&services, &[], &Default::default(), false);
        assert!(!plain.contains('\x1b'), "expected no ANSI when color=false");
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
        let out = format_status_all(&reports, false);
        assert!(out.contains("WORKTREE") && out.contains("SLOT") && out.contains("backend"));
        assert!(
            out.contains("●8080") && out.contains("●8090"),
            "glyph+port cells missing: {out}"
        );
        assert!(out.contains("* feat/foo"), "cwd marker missing: {out}");
        assert!(out.contains("(not running)"), "stopped worktree row missing: {out}");
        assert!(out.contains("● running"), "legend missing: {out}");
        // color=false must leave no escape bytes behind.
        assert!(!out.contains('\x1b'), "leaked ANSI: {out:?}");

        let colored = format_status_all(&reports, true);
        assert!(colored.contains('\x1b'), "expected ANSI when color=true");
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
                service: Some("api".into()),
                follow: true,
                tail: 200,
                since: None,
                json: false,
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
    fn config_check_parses() {
        let cli = Cli::parse_from(["devme", "config", "check"]);
        assert_eq!(cli.command, Some(Command::Config { action: Some(ConfigAction::Check) }));
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
    fn remote_bare_parses_to_no_action() {
        let cli = Cli::parse_from(["devme", "remote"]);
        assert_eq!(cli.command, Some(Command::Remote { action: None }));
    }

    #[test]
    fn remote_subcommands_parse() {
        for (arg, expected) in [
            ("doctor", RemoteAction::Doctor),
            ("status", RemoteAction::Status { watch: false }),
            ("conflicts", RemoteAction::Conflicts),
            ("sync", RemoteAction::Sync),
            ("flush", RemoteAction::Flush),
            ("stop", RemoteAction::Stop),
            ("wake", RemoteAction::Wake),
        ] {
            let cli = Cli::parse_from(["devme", "remote", arg]);
            assert_eq!(cli.command, Some(Command::Remote { action: Some(expected) }));
        }
    }

    #[test]
    fn remote_status_watch_flag_parses() {
        let cli = Cli::parse_from(["devme", "remote", "status", "--watch"]);
        assert_eq!(
            cli.command,
            Some(Command::Remote { action: Some(RemoteAction::Status { watch: true }) })
        );
        // Short form too.
        let cli = Cli::parse_from(["devme", "remote", "status", "-w"]);
        assert_eq!(
            cli.command,
            Some(Command::Remote { action: Some(RemoteAction::Status { watch: true }) })
        );
    }

    #[test]
    fn remote_wake_hook_parses_install_and_uninstall() {
        let cli = Cli::parse_from(["devme", "remote", "wake-hook"]);
        assert_eq!(
            cli.command,
            Some(Command::Remote { action: Some(RemoteAction::WakeHook { uninstall: false }) })
        );
        let cli = Cli::parse_from(["devme", "remote", "wake-hook", "--uninstall"]);
        assert_eq!(
            cli.command,
            Some(Command::Remote { action: Some(RemoteAction::WakeHook { uninstall: true }) })
        );
    }

    #[test]
    fn local_flag_is_global() {
        let cli = Cli::parse_from(["devme", "--local", "status"]);
        assert!(cli.local);
        assert_eq!(cli.command, Some(Command::Status { all: false }));
    }

    #[test]
    fn worktree_add_parses_with_optional_path() {
        let cli = Cli::parse_from(["devme", "worktree", "add", "feat/x"]);
        assert_eq!(
            cli.command,
            Some(Command::Worktree {
                action: WorktreeAction::Add { branch: "feat/x".into(), path: None }
            })
        );
        let cli = Cli::parse_from(["devme", "worktree", "add", "feat/x", "../wt-x"]);
        assert_eq!(
            cli.command,
            Some(Command::Worktree {
                action: WorktreeAction::Add {
                    branch: "feat/x".into(),
                    path: Some("../wt-x".into())
                }
            })
        );
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
