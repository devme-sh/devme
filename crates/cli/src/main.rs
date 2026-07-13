//! `devme` — user-facing CLI binary. Argument parsing and shared
//! formatters live in [`devme_cli`]; this binary dispatches.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine;
use clap::{CommandFactory, Parser, error::ErrorKind};
use clap_complete::{Shell, generate};
use devme_cli::{
    Cli, Command, ConfigAction, RemoteAction, SkillAction, WorktreeAction, format_status_all,
    format_status_json, format_status_text,
};
use devme_config::Stack;
use devme_core::{ClientMessage, ServerMessage, ServiceState};

/// True when `--yes` was passed: promote `prompt` provisions to `auto` so
/// preflight fixes run without asking. Set once in `main`.
static ASSUME_YES: AtomicBool = AtomicBool::new(false);
const READINESS_ATTEMPT_TAIL: usize = 32;

/// Should the *stdout* surface (tables, data) use color? Quiet/color
/// resolution lives in [`devme_ui`]; this is the bool the table formatters
/// thread through.
fn no_color() -> bool {
    !devme_ui::out_style().color
}

fn assume_yes() -> bool {
    ASSUME_YES.load(Ordering::Relaxed)
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            error.exit()
        }
        Err(error) => {
            let args = std::env::args().skip(1).collect::<Vec<_>>();
            let json = args.iter().any(|arg| arg == "--json")
                || args.windows(2).any(|pair| pair == ["--output", "json"]);
            let toon = args.windows(2).any(|pair| pair == ["--output", "toon"])
                || args.first().is_some_and(|arg| arg == "agent");
            let message = error.to_string();
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "schema_version": 1,
                        "error": {
                            "code": "invalid_arguments",
                            "message": message.trim(),
                            "help": "Run `devme --help` or the subcommand with `--help`.",
                        }
                    })
                );
            } else if toon {
                print!(
                    "error:\n  code: invalid_arguments\n  message: {}\n  help: {}",
                    toon_cli_string(message.trim()),
                    toon_cli_string("Run `devme --help` or the subcommand with `--help`.")
                );
            } else {
                let _ = error.print();
            }
            std::process::exit(2);
        }
    };
    // Resolve quiet + per-stream color once (ADR-0017): the flag wins, then
    // `NO_COLOR`/`FORCE_COLOR`, then each stream's own tty-ness.
    devme_ui::init(cli.quiet, cli.no_color);
    ASSUME_YES.store(cli.yes, Ordering::Relaxed);

    let is_tui = cli.command.is_none();
    let mut builder = if is_tui {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    let runtime = match builder.enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            devme_ui::error(format!("tokio init failed: {e}"));
            std::process::exit(1);
        }
    };
    std::process::exit(runtime.block_on(run(cli)));
}

async fn run(cli: Cli) -> i32 {
    // Transparent remote proxy: while this project has a live remote sync,
    // daemon-facing commands (status, logs, up, …) run on the remote host so
    // they behave exactly as local but read the VPS. `--local` opts out. See
    // DEV-5.
    if !cli.local
        && let Some(code) = devme_cli::remote::maybe_proxy(&cli.command)
    {
        return code;
    }

    // The global interactivity flags, packaged once for the remote paths.
    let remote_flags = devme_cli::remote::RunFlags {
        no_input: cli.no_input,
        yes: cli.yes,
        quiet: cli.quiet,
    };

    let result = match cli.command {
        None => return launch_default(cli.local, remote_flags).await,
        Some(Command::Run { task, output, args }) => {
            let output = if cli.json {
                devme_cli::OutputFormat::Json
            } else {
                output
            };
            let cwd = match std::env::current_dir() {
                Ok(value) => value,
                Err(e) => return emit_command_error(output, &e.into()),
            };
            let stack = match devme_cli::task::load(&cwd) {
                Ok(value) => value,
                Err(e) => return emit_command_error(output, &e),
            };
            let services = match devme_cli::task::services_for(&stack, &task) {
                Ok(value) => value,
                Err(e) => return emit_command_error(output, &e),
            };
            let readiness_timeout =
                devme_cli::task::readiness_timeout_for(&stack, &task).unwrap_or(60);
            if !services.is_empty()
                && let Err(e) = ensure_task_services(&stack, &services, readiness_timeout).await
            {
                return emit_command_error(output, &e);
            }
            return match devme_cli::task::execute(&stack, &cwd, &task, &args, output).await {
                Ok(result) => result.exit_code,
                Err(e) => emit_command_error(output, &e),
            };
        }
        Some(Command::Tasks { action, output }) => {
            let output = if cli.json {
                devme_cli::OutputFormat::Json
            } else {
                output
            };
            return match std::env::current_dir()
                .map_err(anyhow::Error::from)
                .and_then(|cwd| devme_cli::task::load(&cwd))
            {
                Ok(stack) => match devme_cli::task::show(&stack, action, output) {
                    Ok(()) => 0,
                    Err(e) => emit_command_error(output, &e),
                },
                Err(e) => emit_command_error(output, &e),
            };
        }
        Some(Command::Status { all }) => {
            if all {
                status_all(cli.json).await
            } else {
                status(cli.json).await
            }
        }
        Some(Command::Down { timeout, all }) => down(timeout, all).await,
        Some(Command::Up {
            services,
            detach,
            wait,
            timeout,
        }) => up(services, detach, wait, timeout).await,
        Some(Command::Start { service }) => start(service).await,
        Some(Command::Stop { service }) => stop(service).await,
        Some(Command::Restart { service }) => restart(service).await,
        Some(Command::Url { service, open }) => url(service, open).await,
        Some(Command::Logs {
            service,
            follow,
            tail,
            since,
            json,
        }) => logs(service, follow, tail, since, json).await,
        Some(Command::Completions { shell }) => {
            print_completions(shell);
            Ok(())
        }
        Some(Command::Doctor { name, tail }) => doctor(name, tail).await,
        Some(Command::Config { action }) => config_cmd(action, cli.json),
        Some(Command::Worktree { action }) => worktree_cmd(action, cli.json).await,
        Some(Command::Remote { action }) => remote_cmd(action, cli.json, remote_flags),
        Some(Command::Skill { action }) => skill_cmd(action, cli.json),
        Some(Command::Agent { action }) => {
            let output = if cli.json {
                devme_cli::OutputFormat::Json
            } else {
                devme_cli::OutputFormat::Toon
            };
            return match agent_cmd(action, cli.json).await {
                Ok(()) => 0,
                Err(e) => emit_command_error(output, &e),
            };
        }
        Some(Command::Setup { write }) => setup_cmd(write),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            devme_ui::error(e);
            1
        }
    }
}

fn emit_command_error(format: devme_cli::OutputFormat, error: &anyhow::Error) -> i32 {
    let message = error.to_string();
    let (code, exit_code) = if error
        .downcast_ref::<devme_cli::task::UnknownTask>()
        .is_some()
    {
        ("not_found", 3)
    } else {
        ("operation_failed", 1)
    };
    match format {
        devme_cli::OutputFormat::Human => devme_ui::error(&message),
        devme_cli::OutputFormat::Json => devme_ui::json(&serde_json::json!({
            "schema_version": 1,
            "error": {
                "code": code,
                "message": message,
                "help": "Inspect `devme doctor` or correct the named task/configuration.",
            }
        })),
        devme_cli::OutputFormat::Toon => print!(
            "error:\n  code: {code}\n  message: {}\n  help: {}",
            toon_cli_string(&message),
            toon_cli_string("Inspect `devme doctor` or correct the named task/configuration.")
        ),
    }
    exit_code
}

fn toon_cli_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}

fn setup_cmd(write: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = devme_cli::setup::detect(&cwd)?;
    if write {
        let path = cwd.join("devme.toml");
        if path.exists() {
            anyhow::bail!("{} already exists; refusing to overwrite", path.display());
        }
        std::fs::write(&path, &config)?;
        devme_ui::success(format!("wrote {}", path.display()));
    } else {
        print!("{config}");
    }
    Ok(())
}

async fn agent_cmd(action: devme_cli::AgentAction, json: bool) -> anyhow::Result<()> {
    use devme_cli::AgentAction;
    let cwd = std::env::current_dir()?;
    match action {
        AgentAction::Setup { target } => {
            emit_agent_integrations(devme_cli::agent::setup(&cwd, target)?, json)
        }
        AgentAction::Status { target } => {
            emit_agent_integrations(devme_cli::agent::status(&cwd, target)?, json)
        }
        AgentAction::Remove { target } => {
            emit_agent_integrations(devme_cli::agent::remove(&cwd, target)?, json)
        }
        AgentAction::Context => agent_context(&cwd).await,
    }
}

fn emit_agent_integrations(rows: Vec<(String, &'static str)>, json: bool) -> anyhow::Result<()> {
    if json {
        let integrations = rows
            .into_iter()
            .map(|(target, status)| serde_json::json!({"target": target, "status": status}))
            .collect::<Vec<_>>();
        devme_ui::json(&serde_json::json!({
            "schema_version": 1,
            "integrations": integrations,
        }));
        return Ok(());
    }
    let mut out = format!("integrations[{}]{{target,status}}:", rows.len());
    for (target, status) in rows {
        out.push_str(&format!("\n  {target},{status}"));
    }
    print!("{out}");
    Ok(())
}

async fn agent_context(cwd: &std::path::Path) -> anyhow::Result<()> {
    let stack = devme_cli::task::load(cwd)?;
    let bin = std::env::current_exe()?
        .display()
        .to_string()
        .replace(&std::env::var("HOME").unwrap_or_default(), "~");
    let history = devme_cli::task::read_history(cwd, None, None)?;
    let failed = history
        .iter()
        .rev()
        .filter(|run| run.exit_code != 0)
        .take(3)
        .collect::<Vec<_>>();
    let mut services = Vec::new();
    if let Ok(sock) = devme_config::paths::supervisor_socket(cwd)
        && let Ok(mut client) = devme_client::Client::connect(&sock).await
        && let Ok(ServerMessage::Subscribed {
            services: snapshot, ..
        }) = client
            .request(ClientMessage::Subscribe { services: vec![] })
            .await
    {
        services = snapshot;
    }
    let active = services
        .iter()
        .filter(|service| service.state.is_up())
        .count();
    let mut out = format!(
        "bin: {}\ndescription: Orchestrate this directory's setup, services, tasks, resources, and diagnostics\nstate:\n  services: {active}/{} ready\n  tasks: {} declared\n  recent_failures: {}",
        toon_cli_string(&bin),
        services.len(),
        stack.task.len(),
        failed.len()
    );
    if !failed.is_empty() {
        out.push_str(&format!(
            "\nfailures[{}]{{task,exit_code,finished_at}}:",
            failed.len()
        ));
        for run in failed {
            out.push_str(&format!(
                "\n  {},{},{}",
                toon_cli_string(&run.task),
                run.exit_code,
                run.finished_at
            ));
        }
    }
    let commands: Vec<_> = devme_config::skill::AGENT_GUIDANCE
        .lines()
        .filter_map(|line| line.strip_prefix("- "))
        .collect();
    out.push_str(&format!("\nhelp[{}]:", commands.len()));
    for command in commands {
        out.push_str(&format!("\n  - {command}"));
    }
    print!("{out}");
    Ok(())
}

async fn down(timeout_secs: u64, all: bool) -> anyhow::Result<()> {
    if all {
        return down_all(timeout_secs).await;
    }

    let sock = socket_path();
    if !teardown_daemon(&sock, timeout_secs, None).await? {
        devme_ui::info("no daemon running");
        return Ok(());
    }

    // Shared (`scope = "repo"`) services — postgres, proxy — live in the
    // shared supervisor, which other worktrees may be using. Tear it down too
    // only when no other worktree still has a running daemon, so `devme down`
    // is a complete stop in the common single-worktree case but doesn't yank
    // a shared Postgres out from under a sibling worktree. (`down --all` stops
    // it unconditionally — by then nothing is left to use it.)
    if let Ok(cwd) = std::env::current_dir()
        && devme_tui::worktree::shutdown_shared_if_last(&cwd).await
    {
        devme_ui::success("shared services stopped");
    }
    Ok(())
}

/// `devme down --all`: stop every worktree's stack in the repo, then the
/// repo-shared services. The repo-wide counterpart to the current-worktree
/// default, scoped like `devme status --all` — and the CLI twin of the TUI's
/// quit, which (because it autospawns every worktree) also stops them all.
async fn down_all(timeout_secs: u64) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let reports = devme_tui::worktree::gather_worktree_reports(&cwd).await;

    let mut any = false;
    for r in &reports {
        // Only worktrees with a live daemon have anything to stop.
        if r.services.is_none() {
            continue;
        }
        let Ok(sock) = devme_config::paths::supervisor_socket(&r.path) else {
            continue;
        };
        if teardown_daemon(&sock, timeout_secs, Some(&r.label)).await? {
            any = true;
        }
    }

    // Every instance daemon is down now, so the shared services are free to
    // stop unconditionally (no sibling can still be relying on them).
    if let Ok(shared_sock) = devme_config::paths::shared_socket(&cwd)
        && let Ok(mut shared) = devme_client::Client::connect(&shared_sock).await
    {
        let _ = shared.send(ClientMessage::Shutdown).await;
        devme_ui::success("shared services stopped");
        any = true;
    }

    if !any {
        devme_ui::info("no daemons running");
    }
    Ok(())
}

/// Stop one instance daemon at `sock`, rendering docker-compose-style
/// checkmarks as each service stops. `label`, when set, prefixes the section
/// header so `--all` shows which worktree is being torn down. Returns `false`
/// when no daemon was listening (nothing to stop).
async fn teardown_daemon(
    sock: &std::path::Path,
    timeout_secs: u64,
    label: Option<&str>,
) -> anyhow::Result<bool> {
    let mut client = match devme_client::Client::connect(sock).await {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };

    // Snapshot first so we know what we're tearing down. The daemon emits
    // StatusUpdate { state: Stopped } per service as it kills each one;
    // we render those as checkmarks docker-compose-style.
    client
        .send(ClientMessage::Subscribe { services: vec![] })
        .await?;
    let services = match client.next_event().await? {
        Some(ServerMessage::Subscribed { services, .. }) => services,
        Some(other) => {
            return Err(anyhow::anyhow!("unexpected initial reply: {other:?}"));
        }
        None => return Err(anyhow::anyhow!("daemon closed before snapshot")),
    };

    // Services that are actually live — Stopped/Failed/CrashLoop are already
    // off the board, no need to checkmark them.
    use devme_core::ServiceState as S;
    let live: Vec<String> = services
        .iter()
        .filter(|s| {
            matches!(
                s.state,
                S::Starting
                    | S::Running { .. }
                    | S::Restarting { .. }
                    | S::WaitingOnDependency { .. }
            )
        })
        .map(|s| s.name.clone())
        .collect();

    // Stopping is narration, not data — it renders to stderr as the house
    // tree style (ADR-0017), one item ticking in as each service stops.
    // Under `-q` the tree goes to a sink: a clean stop is silent, while the
    // timeout summary still surfaces through the Warn end-cap below.
    let total = live.len();
    let mut out: Box<dyn std::io::Write> = if devme_ui::quiet() {
        Box::new(std::io::sink())
    } else {
        Box::new(std::io::stderr())
    };
    let title = match label {
        Some(l) => format!("Stopping {l}"),
        None => "Stopping stack".to_string(),
    };
    let mut sec = devme_ui::Section::begin(&mut out, devme_ui::err_style(), &title)?;

    client.send(ClientMessage::Shutdown).await?;

    let started = std::time::Instant::now();
    let mut stopped: std::collections::HashSet<String> = std::collections::HashSet::new();
    let deadline = started + std::time::Duration::from_secs(timeout_secs);
    let timeout_summary =
        format!("timeout after {timeout_secs}s — some services may still be running");
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            sec.end(devme_ui::Item::Warn, &timeout_summary)?;
            // The section is sunk under `-q`; a timeout is a warning and
            // must surface regardless.
            if devme_ui::quiet() {
                devme_ui::warn(&timeout_summary);
            }
            return Ok(true);
        }
        match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(Some(ServerMessage::StatusUpdate {
                service,
                state: S::Stopped,
                ..
            }))) if live.contains(&service) && stopped.insert(service.clone()) => {
                let elapsed = started.elapsed().as_secs_f32();
                sec.item(
                    devme_ui::Item::Ok,
                    &format!("{service:<20}"),
                    Some(&format!("stopped  {elapsed:>5.1}s")),
                )?;
            }
            Ok(Ok(Some(ServerMessage::Goodbye { .. }))) | Ok(Ok(None)) => break,
            Ok(Ok(Some(_))) => {} // other frames during teardown
            Ok(Err(_)) => break,
            Err(_) => {
                sec.end(devme_ui::Item::Warn, &timeout_summary)?;
                if devme_ui::quiet() {
                    devme_ui::warn(&timeout_summary);
                }
                return Ok(true);
            }
        }
    }
    // Any service that never reported Stopped (already-failed, etc.) still
    // gets a line so the count matches what we promised in the header.
    for name in &live {
        if !stopped.contains(name) {
            let elapsed = started.elapsed().as_secs_f32();
            sec.item(
                devme_ui::Item::Ok,
                &format!("{name:<20}"),
                Some(&format!("stopped  {elapsed:>5.1}s")),
            )?;
        }
    }
    sec.end(
        devme_ui::Item::Ok,
        &format!(
            "{total} service{} stopped",
            if total == 1 { "" } else { "s" }
        ),
    )?;
    Ok(true)
}

/// Cross-worktree status (`--all`): every worktree of the repo with its slot
/// and each service's resolved port. Connects to each worktree's daemon
/// read-only — never spawns one — so a stopped worktree just shows as such.
async fn status_all(as_json: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let mut reports = devme_tui::worktree::gather_worktree_reports(&cwd).await;
    // Repo-scoped services are owned by the shared supervisor, not the
    // per-worktree daemons — overlay its snapshot so the shared column shows
    // a real state + port instead of whatever each instance last probed.
    if let Some(stack) = std::fs::read_to_string(cwd.join("devme.toml"))
        .ok()
        .and_then(|t| Stack::parse(&t).ok())
    {
        for r in reports.iter_mut() {
            if let Some(services) = &mut r.services {
                overlay_shared_services(services, &stack, &cwd).await;
            }
        }
    }
    for r in reports.iter_mut() {
        if let Some(services) = &mut r.services {
            devme_cli::resolve_service_urls(services, &devme_cli::remote::advertise_host());
        }
    }
    if as_json {
        let worktrees: Vec<serde_json::Value> = reports
            .iter()
            .map(|r| {
                serde_json::json!({
                    "label": r.label,
                    "path": r.path.display().to_string(),
                    "is_cwd": r.is_cwd,
                    "slot": r.slot,
                    "running": r.services.is_some(),
                    "services": r.services,
                })
            })
            .collect();
        devme_ui::json(&serde_json::json!({ "worktrees": worktrees }));
    } else {
        print!("{}", format_status_all(&reports, !no_color()));
    }
    Ok(())
}

/// Print (and optionally open) a service's `http://localhost:<port>` URL,
/// resolved from the current worktree's running daemon.
async fn url(service: String, open: bool) -> anyhow::Result<()> {
    let sock = socket_path();
    ensure_daemon(&sock).await?;
    let mut client = devme_client::Client::connect(&sock).await?;
    let reply = client
        .request(ClientMessage::Subscribe { services: vec![] })
        .await?;
    let services = match reply {
        ServerMessage::Subscribed { services, .. } => services,
        other => return Err(anyhow::anyhow!("daemon replied unexpectedly: {other:?}")),
    };
    let svc = services
        .iter()
        .find(|s| s.name == service)
        .ok_or_else(|| anyhow::anyhow!("no service named {service:?} in devme.toml"))?;
    let port = svc
        .port
        .ok_or_else(|| anyhow::anyhow!("service {service:?} has no port to build a URL from"))?;
    // Advertise a reachable host: on the VPS (with `remote.advertise_host` or
    // `$DEVME_URL_HOST` set) this hands back a laptop-reachable link instead of
    // `localhost`, so an agent in a herdr pane can print a clickable URL. On a
    // plain laptop it stays `localhost`. See remote::advertise_host.
    let host = devme_cli::remote::advertise_host();
    let url = format!("http://{host}:{port}");
    println!("{url}");
    if !open {
        return Ok(());
    }
    if let Err(e) = devme_config::browser::open_url(&url) {
        devme_ui::warn(format!("couldn't open browser: {e}"));
    }
    Ok(())
}

async fn status(as_json: bool) -> anyhow::Result<()> {
    let sock = socket_path();
    let cwd = std::env::current_dir()?;
    // Read-only query: like `logs`, skip the provisioning preflight so
    // `status` doesn't re-render the "Check dependencies" tree on every call.
    ensure_daemon_inner(&sock, &cwd).await?;
    let mut client = devme_client::Client::connect(&sock).await?;
    let reply = client
        .request(ClientMessage::Subscribe { services: vec![] })
        .await?;

    // One stack parse serves both the step-check overlay and the
    // description annotations.
    let stack = std::fs::read_to_string(cwd.join("devme.toml"))
        .ok()
        .and_then(|t| Stack::parse(&t).ok());

    match reply {
        ServerMessage::Subscribed {
            mut services,
            mut steps,
            ..
        } => {
            if let Some(stack) = &stack {
                overlay_step_checks(&mut steps, stack, &cwd);
                overlay_shared_services(&mut services, stack, &cwd).await;
            }
            // Hand out ready-to-use URLs — agents reading `--json` shouldn't
            // have to resolve `{host}`/`{port}` templates themselves.
            devme_cli::resolve_service_urls(&mut services, &devme_cli::remote::advertise_host());
            if as_json {
                println!("{}", format_status_json(&services, &steps));
            } else {
                let descriptions = stack.as_ref().map(node_descriptions).unwrap_or_default();
                print!(
                    "{}",
                    format_status_text(&services, &steps, &descriptions, !no_color())
                );
            }
            Ok(())
        }
        other => Err(anyhow::anyhow!(
            "daemon replied with unexpected message: {other:?}"
        )),
    }
}

/// Overlay repo-scoped (`scope = "repo"`) services with the shared
/// supervisor's own snapshot. The instance daemon only *health-probes*
/// these (it doesn't own the process), so a freshly-spawned daemon reports
/// them `Stopped` before its first probe lands — `status` right after a
/// Ctrl-C showed the proxy as stopped while it was still running. The
/// shared daemon is the owner; its state is the truth. Connect-only: if no
/// shared daemon is listening the services really are down, and we must
/// not spawn one just to ask. Uses a one-shot `LogQuery` (not `Subscribe`)
/// so the probe never registers as a subscriber — a Subscribe's disconnect
/// would arm the shared daemon's idle teardown.
async fn overlay_shared_services(
    services: &mut [devme_core::ServiceSnapshot],
    stack: &Stack,
    cwd: &std::path::Path,
) {
    let repo_scoped: Vec<&str> = stack
        .service
        .iter()
        .filter(|(_, s)| s.scope == devme_core::Scope::Repo)
        .map(|(name, _)| name.as_str())
        .collect();
    if repo_scoped.is_empty() {
        return;
    }
    let Ok(shared_sock) = devme_config::paths::shared_socket(cwd) else {
        return;
    };
    let Ok(mut shared) = devme_client::Client::connect(&shared_sock).await else {
        return;
    };
    let Ok(ServerMessage::Subscribed {
        services: shared_snap,
        ..
    }) = shared
        .request(ClientMessage::LogQuery {
            services: vec![],
            since: None,
            tail: Some(0),
            follow: false,
        })
        .await
    else {
        return;
    };
    for s in services
        .iter_mut()
        .filter(|s| repo_scoped.contains(&s.name.as_str()))
    {
        if let Some(owned) = shared_snap.iter().find(|o| o.name == s.name) {
            s.state = owned.state.clone();
            s.pid = owned.pid;
            if owned.port.is_some() {
                s.port = owned.port;
            }
        }
    }
}

/// Node name → `description` from devme.toml, for the status table's dim
/// annotation column. Steps and services share the map; the config layer
/// already rejects a step and service with the same name.
fn node_descriptions(stack: &Stack) -> std::collections::HashMap<String, String> {
    let steps = stack
        .step
        .iter()
        .filter_map(|(name, s)| Some((name.clone(), s.description.clone()?)));
    let services = stack
        .service
        .iter()
        .filter_map(|(name, s)| Some((name.clone(), s.description.clone()?)));
    steps.chain(services).collect()
}

/// Fill in step states the daemon hasn't evaluated yet. The daemon only
/// runs step checks when the graph advances (`up`), so a daemon freshly
/// spawned by `status` reports every step as `Unknown` ("pending") even
/// when its check passes right now. Re-run the service-independent checks
/// quietly and overlay the results; states the daemon *has* established
/// win, and service-dependent steps stay `Unknown` (only meaningful with
/// their services up).
fn overlay_step_checks(
    steps: &mut [devme_core::StepSnapshot],
    stack: &Stack,
    cwd: &std::path::Path,
) {
    use devme_core::StepState;
    if !steps.iter().any(|s| s.state == StepState::Unknown) {
        return;
    }
    let results = devme_supervisor::preflight::quiet_check_results(stack, cwd);
    for s in steps.iter_mut().filter(|s| s.state == StepState::Unknown) {
        if let Some((_, passed)) = results.iter().find(|(name, _)| name == &s.name) {
            s.state = if *passed {
                StepState::Passed
            } else {
                StepState::Failed
            };
        }
    }
}

async fn up(_services: Vec<String>, detach: bool, wait: bool, timeout: u64) -> anyhow::Result<()> {
    // `services` is ignored for v1; the daemon advances the whole graph and
    // the executor decides what's eligible. Per-service Up filtering would
    // need a new executor entry point.
    //
    // Foreground semantics (default): stream every service's log lines with a
    // name prefix in distinct colours until Ctrl-C, which tears the daemon
    // down rather than detaching.
    //
    // Detached (`-d`): kick the graph and exit, leaving the daemon running.
    let sock = socket_path();
    // Repo-scoped services (scope = "repo") are owned by the shared
    // supervisor, not this instance daemon — which now treats them as
    // external and only health-checks them. So `up` must make sure the
    // shared daemon is running, or nothing ever spawns proxy/postgres and
    // their dependents wait forever. Non-fatal: a stack with no repo-scoped
    // services simply has no shared daemon to start. The TUI does the same
    // (see tui::worktree).
    if let Ok(cwd) = std::env::current_dir()
        && let Err(e) = ensure_shared_daemon(&cwd).await
    {
        devme_ui::warn(format!("shared supervisor not started: {e}"));
    }
    let fresh_daemon = ensure_daemon(&sock).await?;
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
        devme_ui::info("no services declared");
        return Ok(());
    }

    // Start is idempotent; safe to send even when re-entering — already-
    // Running services stay Running, services explicitly Stopped this
    // session stay Stopped.
    client
        .send(ClientMessage::Start {
            service: String::new(),
            skip_deps: false,
        })
        .await?;

    if detach {
        if wait {
            await_all_running(&mut client, &snapshot, timeout).await?;
        }
        let n = snapshot.len();
        let plural = if n == 1 { "" } else { "s" };
        if fresh_daemon {
            devme_ui::success(format!(
                "started {n} service{plural}; daemon running in background"
            ));
            devme_ui::hint("devme logs <service> — tail one service");
            devme_ui::hint("devme status — snapshot");
            devme_ui::hint("devme down — stop everything");
        } else {
            // Re-entry — the stack was already up; one line, no hint block
            // (it printed when the daemon booted, and `devme remote` re-runs
            // `up -d` on every attach).
            devme_ui::success(format!("{n} service{plural} up; daemon already running"));
        }
        maybe_skill_update();
        maybe_show_skills_hint();
        return Ok(());
    }

    let names: Vec<&str> = snapshot.iter().map(|s| s.name.as_str()).collect();
    if fresh_daemon {
        devme_ui::info(format!(
            "running {n}/{n} — attaching to {names}",
            n = snapshot.len(),
            names = names.join(", ")
        ));
    } else {
        // Re-entrancy: daemon already alive. Skip the boot header — those
        // services have been up for a while. Just announce the attach.
        devme_ui::info(format!(
            "attaching to {} (already running)",
            names.join(", ")
        ));
    }
    devme_ui::hint("Ctrl-C: graceful stop · twice: force quit");

    // Two-stage signal handling matches `docker compose up`:
    //   1st SIGINT  → "Gracefully stopping… (press Ctrl+C again to force)",
    //                 send Shutdown, keep draining so the user sees the
    //                 services actually stop;
    //   2nd SIGINT  → SIGKILL ourselves, exit 130 (POSIX "killed by signal").
    // SIGTERM (external, systemd, supervisord) takes the graceful path with
    // a different message — no "press again" hint to spam unattended logs.
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut stopping = false;

    loop {
        tokio::select! {
            _ = sigint.recv() => {
                if stopping {
                    devme_ui::note("");
                    devme_ui::info("force-quitting");
                    std::process::exit(130);
                }
                stopping = true;
                devme_ui::note("");
                devme_ui::info("gracefully stopping… (press Ctrl+C again to force)");
                let _ = client.send(ClientMessage::Shutdown).await;
            }
            _ = sigterm.recv() => {
                if !stopping {
                    stopping = true;
                    devme_ui::note("");
                    devme_ui::info("SIGTERM received, gracefully stopping…");
                    let _ = client.send(ClientMessage::Shutdown).await;
                }
            }
            msg = client.next_event() => {
                let m = match msg? {
                    Some(m) => m,
                    None => break,
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
                            devme_ui::note(format!("[{service}] {label}"));
                        }
                    }
                    ServerMessage::Notice { level, message } => {
                        devme_ui::note(format!("[devme {level:?}] {message}"));
                    }
                    ServerMessage::Goodbye { .. } => break,
                    _ => {}
                }
            }
        }
    }

    // Ctrl-C on a foreground `up` is a full stop, like `devme down`: also
    // stop the repo-shared services unless a sibling worktree still has a
    // live daemon. Gated on `stopping` so a daemon crash (not a requested
    // stop) leaves the shared supervisor alone. Without this the shared
    // supervisor lingers after ^C while `status` reads "everything stopped".
    if stopping
        && let Ok(cwd) = std::env::current_dir()
        && devme_tui::worktree::shutdown_shared_if_last(&cwd).await
    {
        devme_ui::success("shared services stopped");
    }
    Ok(())
}

async fn ensure_task_services(
    stack: &Stack,
    services: &[String],
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let closure = required_service_closure(stack, services);
    if closure.iter().any(|name| {
        stack
            .service
            .get(name)
            .is_some_and(|service| service.scope == devme_core::Scope::Repo)
    }) {
        ensure_shared_daemon(&cwd).await?;
    }
    let sock = socket_path();
    ensure_daemon(&sock).await?;
    let mut client = devme_client::Client::connect(&sock).await?;
    let initial = client
        .request(ClientMessage::Subscribe {
            services: closure.clone(),
        })
        .await?;
    let mut states: std::collections::HashMap<String, ServiceState> = match initial {
        ServerMessage::Subscribed {
            services: snapshot, ..
        } => snapshot
            .into_iter()
            .filter(|item| closure.contains(&item.name))
            .map(|item| (item.name, item.state))
            .collect(),
        other => anyhow::bail!("unexpected supervisor reply: {other:?}"),
    };
    client
        .send(ClientMessage::StartTargets {
            services: services.to_vec(),
        })
        .await?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut attempts: std::collections::HashMap<String, Vec<(u32, String)>> =
        std::collections::HashMap::new();
    let mut omitted_attempts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    loop {
        if services
            .iter()
            .all(|name| states.get(name).is_some_and(ServiceState::is_up))
        {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let timeout_error = || {
            let detail = closure
                .iter()
                .filter_map(|name| {
                    let service = stack.service.get(name)?;
                    let readiness = service.readiness.clone().unwrap_or_default();
                    let evidence = attempts
                        .get(name)
                        .map(|items| {
                            let tail = items
                                .iter()
                                .map(|(attempt, error)| format!("attempt {attempt}: {error}"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            match omitted_attempts.get(name).copied().unwrap_or_default() {
                                0 => tail,
                                omitted => format!("{omitted} earlier attempts omitted, {tail}"),
                            }
                        })
                        .unwrap_or_else(|| "no probe attempt reported".to_string());
                    Some(format!(
                        "{name}: {evidence} (interval_ms={}, timeout_ms={}, retries={}, deadline_seconds={timeout_secs})",
                        readiness.interval_ms, readiness.timeout_ms, readiness.retries
                    ))
                })
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::anyhow!(
                "required services were not ready after {timeout_secs}s{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            )
        };
        if remaining.is_zero() {
            return Err(timeout_error());
        }
        let event = match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(event) => event?,
            Err(_) => return Err(timeout_error()),
        };
        match event {
            Some(ServerMessage::StatusUpdate { service, state, .. }) => {
                states.insert(service, state);
            }
            Some(ServerMessage::Readiness {
                service,
                attempt,
                last_error: Some(error),
                ..
            }) => {
                let service_attempts = attempts.entry(service.clone()).or_default();
                if service_attempts.len() == READINESS_ATTEMPT_TAIL {
                    service_attempts.remove(0);
                    *omitted_attempts.entry(service).or_default() += 1;
                }
                service_attempts.push((attempt, error));
            }
            Some(ServerMessage::Error { message, .. }) => anyhow::bail!("{message}"),
            Some(_) => {}
            None => anyhow::bail!("supervisor stopped while waiting for readiness"),
        }
    }
}

fn required_service_closure(stack: &Stack, targets: &[String]) -> Vec<String> {
    fn visit(
        graph: &devme_config::Graph,
        stack: &Stack,
        name: &str,
        seen: &mut std::collections::HashSet<String>,
        services: &mut Vec<String>,
    ) {
        if !seen.insert(name.to_string()) {
            return;
        }
        if stack.service.contains_key(name) {
            services.push(name.to_string());
        }
        for dependency in graph
            .dependencies(name)
            .iter()
            .filter(|dependency| dependency.required)
        {
            visit(graph, stack, &dependency.name, seen, services);
        }
    }

    let graph = devme_config::Graph::from_stack(stack);
    let mut seen = std::collections::HashSet::new();
    let mut services = Vec::new();
    for target in targets {
        visit(&graph, stack, target, &mut seen, &mut services);
    }
    services.sort();
    services
}

/// Block on StatusUpdate stream until every service in `snapshot` is in a
/// terminal post-boot state (Running, Failed, or CrashLoop). Used by
/// `up -d --wait` so CI/scripts can know whether the stack is actually
/// up before proceeding. Returns Err on timeout.
async fn await_all_running(
    client: &mut devme_client::Client,
    snapshot: &[devme_core::ServiceSnapshot],
    timeout_secs: u64,
) -> anyhow::Result<()> {
    use std::collections::HashMap;
    let mut states: HashMap<String, ServiceState> = snapshot
        .iter()
        .map(|s| (s.name.clone(), s.state.clone()))
        .collect();
    let is_settled = |s: &ServiceState| {
        matches!(
            s,
            ServiceState::Running { .. }
                | ServiceState::Failed { .. }
                | ServiceState::CrashLoop { .. }
                | ServiceState::External { .. }
        )
    };
    if states.values().all(is_settled) {
        return Ok(());
    }
    let deadline = if timeout_secs == 0 {
        None
    } else {
        Some(std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs))
    };
    loop {
        let timeout = match deadline {
            Some(d) => d.saturating_duration_since(std::time::Instant::now()),
            None => std::time::Duration::from_secs(3600),
        };
        if deadline.is_some() && timeout.is_zero() {
            return Err(anyhow::anyhow!(
                "--wait timed out before all services settled"
            ));
        }
        match tokio::time::timeout(timeout, client.next_event()).await {
            Ok(Ok(Some(ServerMessage::StatusUpdate { service, state, .. }))) => {
                states.insert(service, state);
                if states.values().all(is_settled) {
                    return Ok(());
                }
            }
            Ok(Ok(Some(_))) => {} // ignore non-status frames
            Ok(Ok(None)) | Ok(Err(_)) => {
                return Err(anyhow::anyhow!("daemon disconnected while waiting"));
            }
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "--wait timed out before all services settled"
                ));
            }
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
/// lines are visually distinct in `up`'s combined stream. Strips colors
/// when [`no_color`] is true (piped output, `--no-color`, `NO_COLOR=1`).
fn print_prefixed(service: &str, text: &str) {
    let (color, reset, dim) = if no_color() {
        ("", "", "")
    } else {
        let colors: &[&str] = &[
            "\x1b[36m", "\x1b[33m", "\x1b[35m", "\x1b[32m", "\x1b[34m", "\x1b[91m", "\x1b[96m",
            "\x1b[93m",
        ];
        let mut h: u32 = 5381;
        for b in service.bytes() {
            h = h.wrapping_mul(33).wrapping_add(b as u32);
        }
        (colors[(h as usize) % colors.len()], "\x1b[0m", "\x1b[2m")
    };
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

async fn logs(
    service: Option<String>,
    follow: bool,
    tail: usize,
    since: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let since_ms = match since.as_deref() {
        Some(s) => Some(parse_since(s)?),
        None => None,
    };
    // `--tail 0` means "everything".
    let tail_opt = if tail == 0 { None } else { Some(tail) };

    let cwd = std::env::current_dir()?;
    let stack = std::fs::read_to_string(cwd.join("devme.toml"))
        .ok()
        .and_then(|text| Stack::parse(&text).ok());
    if service.as_ref().is_some_and(|name| {
        stack
            .as_ref()
            .is_some_and(|stack| stack.task.contains_key(name))
    }) {
        if follow {
            anyhow::bail!(
                "task history is finite; omit --follow and rerun the task separately with `devme run {}`",
                service.as_deref().unwrap()
            );
        }
        let names = service
            .clone()
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let mut lines = task_history_lines(&cwd, Some(&names), since_ms)?;
        if tail > 0 && lines.len() > tail {
            lines.drain(0..lines.len() - tail);
        }
        for line in lines {
            emit_plain_log(&line.source, line.ts, line.stream, &line.text, json);
        }
        return Ok(());
    }

    // Routing: a *named* repo-scoped service is owned by the shared supervisor
    // (the instance daemon only health-checks it and holds no log buffer), so
    // route there. The instance daemon owns everything else and the
    // all-services view. (A repo service still needs to be named explicitly to
    // appear; the all-services view shows the instance daemon's services.)
    let is_repo_scoped = match &service {
        Some(name) => std::fs::read_to_string(cwd.join("devme.toml"))
            .ok()
            .and_then(|t| Stack::parse(&t).ok())
            .and_then(|s| {
                s.service
                    .get(name)
                    .map(|svc| svc.scope == devme_core::Scope::Repo)
            })
            .unwrap_or(false),
        None => false,
    };

    let sock = if is_repo_scoped {
        let shared = devme_config::paths::shared_socket(&cwd)?;
        let _ = ensure_shared_daemon(&cwd).await;
        shared
    } else {
        let s = socket_path();
        // Read-only query: connect (spawning the daemon only if absent) but
        // skip the provisioning preflight — otherwise every `logs` call
        // re-renders the "Check dependencies" tree to stderr, which reads as
        // "logs is dumping the dependency check".
        ensure_daemon_inner(&s, &cwd).await?;
        s
    };
    let mut client = devme_client::Client::connect(&sock).await?;

    let services_arg = match &service {
        Some(name) => vec![name.clone()],
        None => vec![],
    };
    let snap = client
        .request(ClientMessage::LogQuery {
            services: services_arg,
            since: since_ms,
            tail: if service.is_none() { None } else { tail_opt },
            follow,
        })
        .await?;

    // Validate a named target against the snapshot. A step is redirected to
    // `doctor` — steps have check/provision *output*, not a runtime stream, and
    // dumping it here is the "logs build shows the dependency tree" bug. An
    // unknown name errors instead of silently waiting for logs that never come.
    let mut service_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let ServerMessage::Subscribed {
        services, steps, ..
    } = &snap
    {
        service_names = services.iter().map(|s| s.name.clone()).collect();
        if let Some(name) = &service {
            let is_service = service_names.contains(name);
            let is_step = steps.iter().any(|s| &s.name == name);
            if !is_service && is_step {
                return Err(anyhow::anyhow!(
                    "{name:?} is a step, not a service — its check/provision output lives in \
                     `devme doctor {name}`, not the log stream"
                ));
            }
            if !is_service && !is_step {
                return Err(anyhow::anyhow!(
                    "no service or step named {name:?} in devme.toml"
                ));
            }
        }
    }

    // Single-service mode filters to that service. All-services mode accepts
    // only *service* chunks — the daemon also broadcasts step check/provision
    // lines (for the TUI), and those must never leak into the logs channel
    // (they live in `doctor`), e.g. when a step re-runs mid-`--follow`.
    let want = |s: &str| match service.as_deref() {
        Some(n) => n == s,
        None => service_names.contains(s),
    };

    // Drain the disk-backed replay up to the daemon's LogEnd marker. The
    // generous timeout is only a safety net against a daemon that dies
    // mid-replay; the marker is what normally terminates the loop.
    let drain_max = std::time::Duration::from_secs(10);
    let mut replay = Vec::new();
    loop {
        match tokio::time::timeout(drain_max, client.next_event()).await {
            Ok(Ok(Some(ServerMessage::LogChunk {
                service: s,
                bytes,
                ts,
                stream,
            }))) if want(&s) => {
                if let Some(text) = decode_log(&bytes) {
                    for line in text.lines().filter(|line| !line.is_empty()) {
                        replay.push(CorrelatedLine {
                            source: s.clone(),
                            ts,
                            stream,
                            text: strip_ansi(line.trim_end_matches('\r')),
                        });
                    }
                }
            }
            Ok(Ok(Some(ServerMessage::LogEnd {}))) => break, // replay finished
            Ok(Ok(Some(ServerMessage::Notice { message, .. }))) => {
                // Truncation marker etc. — to stderr so it never pollutes the
                // (possibly JSON) stdout stream.
                devme_ui::info(message);
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => return Ok(()),
        }
    }

    if service.is_none() {
        replay.extend(task_history_lines(&cwd, None, since_ms)?);
        replay.sort_by_key(|line| line.ts);
        if tail > 0 && replay.len() > tail {
            replay.drain(0..replay.len() - tail);
        }
    }
    let printed_any = !replay.is_empty();
    for line in replay {
        emit_plain_log(&line.source, line.ts, line.stream, &line.text, json);
    }

    if !follow {
        if !printed_any {
            let what = service.as_deref().unwrap_or("any service");
            devme_ui::info(format!("no logs for {what} yet (try --follow to wait)"));
        }
        return Ok(());
    }

    // --follow: keep streaming new lines indefinitely. Ctrl-C exits cleanly.
    if !printed_any {
        let what = service.as_deref().unwrap_or("all services");
        devme_ui::info(format!("tailing {what} (Ctrl-C to stop)"));
    }
    let interrupt = tokio::signal::ctrl_c();
    let mut pinned_interrupt = std::pin::pin!(interrupt);
    loop {
        tokio::select! {
            _ = &mut pinned_interrupt => return Ok(()),
            msg = client.next_event() => match msg? {
                Some(ServerMessage::LogChunk { service: s, bytes, ts, stream }) if want(&s) => {
                    emit_log(&s, ts, stream, &bytes, json);
                }
                Some(ServerMessage::Goodbye { .. }) | None => return Ok(()),
                _ => {}
            }
        }
    }
}

/// Decode and print one log chunk — either prefixed text or an NDJSON record
/// (`{ts, service, stream, text}`, ANSI stripped) for piping to `jq`.
fn emit_log(service: &str, ts: u64, stream: devme_core::LogStream, bytes: &str, json: bool) {
    let Some(text) = decode_log(bytes) else {
        return;
    };
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        emit_plain_log(service, ts, stream, line, json);
    }
}

#[derive(Clone)]
struct CorrelatedLine {
    source: String,
    ts: u64,
    stream: devme_core::LogStream,
    text: String,
}

fn decode_log(bytes: &str) -> Option<String> {
    base64::engine::general_purpose::STANDARD
        .decode(bytes.as_bytes())
        .ok()
        .map(|decoded| String::from_utf8_lossy(&decoded).into_owned())
}

fn emit_plain_log(source: &str, ts: u64, stream: devme_core::LogStream, text: &str, json: bool) {
    if json {
        let source_kind = if source.starts_with("task:") {
            "task"
        } else {
            "service"
        };
        println!(
            "{}",
            serde_json::json!({
                "ts": ts,
                // Keep the established key for JSON consumers. Task records use
                // the unambiguous `task:<name>` namespace.
                "service": source,
                "source_kind": source_kind,
                "stream": stream,
                "text": strip_ansi(text),
            })
        );
    } else {
        print_prefixed(source, text);
    }
}

fn task_history_lines(
    cwd: &std::path::Path,
    names: Option<&std::collections::HashSet<String>>,
    since: Option<u64>,
) -> anyhow::Result<Vec<CorrelatedLine>> {
    let mut lines = Vec::new();
    for result in devme_cli::task::read_history(cwd, names, since)? {
        for text in result.stdout.lines() {
            lines.push(CorrelatedLine {
                source: format!("task:{}", result.task),
                ts: result.finished_at,
                stream: devme_core::LogStream::Stdout,
                text: text.into(),
            });
        }
        for text in result.stderr.lines() {
            lines.push(CorrelatedLine {
                source: format!("task:{}", result.task),
                ts: result.finished_at,
                stream: devme_core::LogStream::Stderr,
                text: text.into(),
            });
        }
    }
    Ok(lines)
}

/// Parse a `--since` value into an epoch-ms floor. Accepts a duration relative
/// to now (`30s`, `5m`, `2h`, `1d`) or a bare epoch-ms timestamp.
fn parse_since(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty --since");
    }
    // Bare digits → absolute epoch-ms timestamp.
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return s
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("invalid --since timestamp {s:?}"));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid --since {s:?}; use 30s, 5m, 2h, 1d or epoch-ms"))?;
    let ms = match unit {
        "s" => n * 1_000,
        "m" => n * 60_000,
        "h" => n * 3_600_000,
        "d" => n * 86_400_000,
        _ => anyhow::bail!("invalid --since unit in {s:?}; use s, m, h or d"),
    };
    Ok(now.saturating_sub(ms))
}

/// Strip ANSI escape sequences (color/cursor) — noise for an agent reading
/// `--json`. Drops `ESC [ … <final-byte>` and lone `ESC x` pairs.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&c2) = chars.peek() {
                        chars.next();
                        if c2.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// One captured log line for `doctor`: (ts, stream, text).
type DoctorLine = (u64, devme_core::LogStream, String);

async fn doctor(name: Option<String>, tail: usize) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let stack = devme_cli::task::load(&cwd).ok();
    if let Some(task_name) = name.as_ref().filter(|name| {
        stack
            .as_ref()
            .is_some_and(|stack| stack.task.contains_key(*name))
    }) {
        let names = [task_name.clone()].into_iter().collect();
        let history = devme_cli::task::read_history(&cwd, Some(&names), None)?;
        let latest = history.last();
        devme_ui::json(&serde_json::json!({
            "name": task_name, "kind": "task", "runs": history.len(),
            "latest": latest, "status": latest.map(|run| run.status.as_str()).unwrap_or("never_run")
        }));
        return Ok(());
    }
    if let (Some(node), Some(stack)) = (&name, &stack)
        && !stack.service.contains_key(node)
        && !stack.step.contains_key(node)
    {
        anyhow::bail!("no task, service, or step named {node:?} in devme.toml");
    }
    let task_history = devme_cli::task::read_history(&cwd, None, None)?;
    let task_digest = latest_task_digest(&task_history);
    let sock = socket_path();
    let mut client = match devme_client::Client::connect(&sock).await {
        Ok(c) => c,
        Err(_) => {
            let report = serde_json::json!({
                "status": "no_daemon",
                "message": "no devme daemon running — start one with `devme up -d`",
                "services": [],
                "steps": [],
                "tasks": task_digest,
            });
            devme_ui::json(&report);
            return Ok(());
        }
    };

    // One LogQuery serves both modes. It replays the *disk* history tier, so
    // the diagnosis survives ring eviction and daemon restarts — a service
    // that crashed an hour ago still has its dying stderr here. Explicit
    // names also reach step output (which `logs` deliberately refuses).
    client
        .send(ClientMessage::LogQuery {
            services: name.clone().map(|n| vec![n]).unwrap_or_default(),
            since: None,
            tail: None,
            follow: false,
        })
        .await?;
    let (services, steps) = match client.next_event().await? {
        Some(ServerMessage::Subscribed {
            services, steps, ..
        }) => (services, steps),
        _ => return Err(anyhow::anyhow!("unexpected reply from daemon")),
    };

    // Validate the zoom target before draining, so a typo fails fast.
    if let Some(n) = &name
        && !services.iter().any(|s| &s.name == n)
        && !steps.iter().any(|s| &s.name == n)
    {
        return Err(anyhow::anyhow!(
            "no service or step named {n:?} in devme.toml"
        ));
    }

    // The empty-names query covers services only; step output must be asked
    // for by name. For the no-arg digest, issue a second query naming the
    // steps so failed checks come back with their output.
    if name.is_none() && !steps.is_empty() {
        client
            .send(ClientMessage::LogQuery {
                services: steps.iter().map(|s| s.name.clone()).collect(),
                since: None,
                tail: None,
                follow: false,
            })
            .await?;
    }

    let mut log_ends_expected = if name.is_none() && !steps.is_empty() {
        2
    } else {
        1
    };
    let drain_max = std::time::Duration::from_secs(10);
    let mut all_logs: std::collections::HashMap<String, Vec<DoctorLine>> =
        std::collections::HashMap::new();
    loop {
        match tokio::time::timeout(drain_max, client.next_event()).await {
            Ok(Ok(Some(ServerMessage::LogChunk {
                service,
                bytes,
                ts,
                stream,
            }))) => {
                if let Ok(decoded) =
                    base64::engine::general_purpose::STANDARD.decode(bytes.as_bytes())
                    && let Ok(text) = String::from_utf8(decoded)
                {
                    let buf = all_logs.entry(service).or_default();
                    for line in text.split('\n') {
                        let line = line.trim_end_matches('\r');
                        if !line.is_empty() {
                            buf.push((ts, stream, strip_ansi(line)));
                        }
                    }
                }
            }
            Ok(Ok(Some(ServerMessage::LogEnd {}))) => {
                log_ends_expected -= 1;
                if log_ends_expected == 0 {
                    break;
                }
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
        }
    }

    // The last N lines of `lines`, formatted as `[stream] text` so an agent
    // can tell a traceback (stderr) from routine chatter (stdout) without a
    // second query. `tail == 0` means everything.
    let fmt_tail = |lines: &[DoctorLine], tail: usize| -> Vec<String> {
        let skip = if tail == 0 {
            0
        } else {
            lines.len().saturating_sub(tail)
        };
        lines[skip..]
            .iter()
            .map(|(_, stream, text)| match stream {
                devme_core::LogStream::Stderr => format!("[stderr] {text}"),
                devme_core::LogStream::Stdout => text.clone(),
            })
            .collect()
    };

    // Zoom mode: everything devme knows about one node, inline.
    if let Some(n) = name {
        let lines = all_logs.remove(&n).unwrap_or_default();
        let report = if let Some(s) = steps.iter().find(|s| s.name == n) {
            serde_json::json!({
                "name": n,
                "kind": "step",
                "state": format!("{:?}", s.state),
                // A step's full check/provision output — this is the only
                // place it surfaces (it is not a runtime log stream).
                "output": fmt_tail(&lines, tail),
            })
        } else {
            let s = services
                .iter()
                .find(|s| s.name == n)
                .expect("validated above");
            let errors: Vec<DoctorLine> = lines
                .iter()
                .filter(|(_, st, _)| st.is_stderr())
                .cloned()
                .collect();
            serde_json::json!({
                "name": n,
                "kind": "service",
                "state": format!("{:?}", s.state),
                "pid": s.pid,
                "port": s.port,
                "restart_count": s.restart_count,
                "readiness": s.readiness,
                "recent_errors": fmt_tail(&errors, tail),
                "recent_logs": fmt_tail(&lines, tail),
            })
        };
        devme_ui::json(&report);
        return Ok(());
    }

    // Digest mode: states for everything, but log lines anchored on *errors* —
    // per-service stderr, plus step output only when the step actually failed.
    // Healthy chatter costs the reader tokens without aiding diagnosis; it
    // stays one `devme logs` away.
    let has_failures = task_history.iter().any(|run| run.exit_code != 0)
        || services.iter().any(|s| {
            matches!(
                s.state,
                ServiceState::Failed { .. } | ServiceState::CrashLoop { .. }
            )
        })
        || steps.iter().any(|s| {
            matches!(
                s.state,
                devme_core::StepState::Failed | devme_core::StepState::ProvisionFailed
            )
        });

    let svc_json: Vec<serde_json::Value> = services
        .iter()
        .map(|s| {
            let lines = all_logs.get(&s.name).map(|v| v.as_slice()).unwrap_or(&[]);
            let errors: Vec<DoctorLine> = lines
                .iter()
                .filter(|(_, st, _)| st.is_stderr())
                .cloned()
                .collect();
            serde_json::json!({
                "name": s.name,
                "state": format!("{:?}", s.state),
                "pid": s.pid,
                "port": s.port,
                "restart_count": s.restart_count,
                "readiness": s.readiness,
                "recent_errors": fmt_tail(&errors, tail),
            })
        })
        .collect();

    let step_json: Vec<serde_json::Value> = steps
        .iter()
        .map(|s| {
            let failed = matches!(
                s.state,
                devme_core::StepState::Failed | devme_core::StepState::ProvisionFailed
            );
            let mut node = serde_json::json!({
                "name": s.name,
                "state": format!("{:?}", s.state),
            });
            if failed {
                let lines = all_logs.get(&s.name).map(|v| v.as_slice()).unwrap_or(&[]);
                node["output"] = serde_json::json!(fmt_tail(lines, tail));
            }
            node
        })
        .collect();

    let report = serde_json::json!({
        "status": if has_failures { "unhealthy" } else { "healthy" },
        "services": svc_json,
        "steps": step_json,
        "tasks": task_digest,
        "hint": "zoom with `devme doctor <name>`; stream services and tasks with `devme logs`",
    });

    devme_ui::json(&report);
    Ok(())
}

fn latest_task_digest(history: &[devme_cli::task::TaskResult]) -> Vec<serde_json::Value> {
    let mut latest = std::collections::BTreeMap::new();
    for run in history {
        latest.insert(run.task.clone(), run);
    }
    latest
        .into_iter()
        .map(|(name, run)| {
            serde_json::json!({
                "name": name, "status": run.status, "exit_code": run.exit_code,
                "duration_ms": run.duration_ms, "finished_at": run.finished_at,
                "recent_error": run.stderr.lines().last(),
            })
        })
        .collect()
}

fn config_cmd(action: Option<ConfigAction>, json: bool) -> anyhow::Result<()> {
    use devme_config::GlobalConfig;

    match action {
        Some(ConfigAction::Check) => config_check(json),
        None => {
            let (cfg, warning) = GlobalConfig::load_checked();
            if let Some(w) = warning {
                devme_ui::warn(w);
            }
            for (key, desc) in GlobalConfig::keys() {
                let value = cfg.get(key).unwrap_or_else(|| "(unset)".into());
                println!("{key:<24} {value:<20} # {desc}");
            }
            Ok(())
        }
        Some(ConfigAction::Get { key }) => {
            let (cfg, warning) = GlobalConfig::load_checked();
            if let Some(w) = warning {
                devme_ui::warn(w);
            }
            match cfg.get(&key) {
                Some(v) => println!("{v}"),
                None => println!("(unset)"),
            }
            Ok(())
        }
        // Surgical writes preserve any comments/formatting in the file.
        Some(ConfigAction::Set { key, value }) => {
            GlobalConfig::persist(&key, &value).map_err(|e| anyhow::anyhow!("{e}"))?;
            devme_ui::info(format!("{key} = {value}"));
            Ok(())
        }
        Some(ConfigAction::Unset { key }) => {
            GlobalConfig::unset_persisted(&key).map_err(|e| anyhow::anyhow!("{e}"))?;
            devme_ui::info(format!("unset {key}"));
            Ok(())
        }
    }
}

/// `devme config check` — static analysis of this project's `devme.toml`:
/// parse, then [`validate`] (fatal errors: cycles, unknown deps, …) and
/// [`lint`] (advisories: a web service with no openable `url`, a literal
/// `{port}`, …). Built for agents: clean JSON with `--json`, and a non-zero
/// exit whenever there are errors so a script can gate on it.
fn config_check(json: bool) -> anyhow::Result<()> {
    use devme_config::{Stack, lint, validate};

    let path = std::env::current_dir()?.join("devme.toml");
    let toml_str = std::fs::read_to_string(&path)
        .map_err(|_| anyhow::anyhow!("no devme.toml in {}", path.display()))?;

    // A parse failure is terminal — nothing to validate/lint against.
    let stack = match Stack::parse(&toml_str) {
        Ok(s) => s,
        Err(e) => {
            if json {
                let v = serde_json::json!({
                    "ok": false,
                    "parse_error": e.to_string(),
                    "errors": [],
                    "warnings": [],
                });
                devme_ui::json(&v);
            } else {
                println!("✗ devme.toml failed to parse:\n  {e}");
            }
            return Err(anyhow::anyhow!("config has a parse error"));
        }
    };

    let errors: Vec<String> = match validate(&stack) {
        Ok(()) => Vec::new(),
        Err(errs) => errs.iter().map(|e| e.to_string()).collect(),
    };
    let warnings = lint(&stack);

    if json {
        let v = serde_json::json!({
            "ok": errors.is_empty(),
            "errors": errors,
            "warnings": warnings
                .iter()
                .map(|l| serde_json::json!({
                    "target": l.target,
                    "message": l.message,
                    "hint": l.hint,
                }))
                .collect::<Vec<_>>(),
        });
        devme_ui::json(&v);
    } else {
        for e in &errors {
            println!("✗ {e}");
        }
        for l in &warnings {
            println!("⚠ {}", l.message);
            println!("  fix: {}", l.hint);
        }
        if errors.is_empty() && warnings.is_empty() {
            println!("✔ devme.toml looks good — no errors or warnings");
        } else {
            println!("\n{} error(s), {} warning(s)", errors.len(), warnings.len());
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("{} config error(s)", errors.len()))
    }
}

/// `devme worktree …` — worktree lifecycle coordinated with devme.
async fn worktree_cmd(action: WorktreeAction, json: bool) -> anyhow::Result<()> {
    match action {
        WorktreeAction::Add { branch, path } => {
            let cwd = std::env::current_dir()?;
            let report = devme_tui::worktree::add_worktree(&cwd, &branch, path.as_deref())?;
            if json {
                let value = serde_json::json!({
                    "path": report.path.display().to_string(),
                    "branch": report.branch,
                    "created_branch": report.created_branch,
                });
                devme_ui::json(&value);
            } else {
                devme_ui::success(format!("created worktree {}", report.path.display()));
                devme_ui::info(format!(
                    "branch: {}{}",
                    report.branch,
                    if report.created_branch { " (new)" } else { "" }
                ));
                devme_ui::hint(format!("cd {} && devme up -d", report.path.display()));
            }
            Ok(())
        }
        WorktreeAction::Rm { target, force } => {
            let cwd = std::env::current_dir()?;
            let report = devme_tui::worktree::remove_worktree(&cwd, &target, force).await?;
            if json {
                let value = serde_json::json!({
                    "path": report.path.display().to_string(),
                    "branch": report.branch,
                    "slot": report.slot,
                    "instance_stopped": report.instance_stopped,
                    "already_gone": report.already_gone,
                });
                devme_ui::json(&value);
            } else if report.already_gone {
                // Idempotent: nothing was on disk to remove. Say so plainly
                // rather than claiming a removal that didn't happen.
                devme_ui::info(format!(
                    "worktree {} was already gone — pruned stale state",
                    report.path.display()
                ));
            } else {
                devme_ui::success(format!("removed worktree {}", report.path.display()));
                if let Some(b) = &report.branch {
                    devme_ui::info(format!(
                        "branch: {b} (kept — `git branch -d {b}` to delete)"
                    ));
                }
                if let Some(s) = report.slot {
                    devme_ui::info(format!("slot {s} released"));
                }
                if report.instance_stopped {
                    devme_ui::info("stopped instance stack");
                }
            }
            Ok(())
        }
    }
}

/// `devme remote …` — live-sync + attach to a remote dev host. Shells out
/// to `mutagen`/`ssh`, so it's synchronous (no devme daemon involved).
fn remote_cmd(
    action: Option<RemoteAction>,
    json: bool,
    flags: devme_cli::remote::RunFlags,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    match action {
        None => devme_cli::remote::run(&cwd, flags),
        Some(RemoteAction::Doctor) => devme_cli::remote::doctor(&cwd, json),
        Some(RemoteAction::Status { watch }) => devme_cli::remote::status(&cwd, json, watch),
        Some(RemoteAction::Conflicts) => devme_cli::remote::conflicts(&cwd, json),
        Some(RemoteAction::Sync) => devme_cli::remote::sync(&cwd),
        Some(RemoteAction::Flush) => devme_cli::remote::flush(&cwd),
        Some(RemoteAction::Stop) => devme_cli::remote::stop(&cwd),
        Some(RemoteAction::Wake) => devme_cli::remote::wake(),
        Some(RemoteAction::Toggle) => devme_cli::remote::toggle(),
        Some(RemoteAction::WakeHook { uninstall }) => devme_cli::remote::wake_hook(uninstall),
    }
}

/// `devme skill …` — manage the embedded AI agent skill. Pure filesystem
/// work, so it's synchronous (no daemon involved).
fn skill_cmd(action: SkillAction, json: bool) -> anyhow::Result<()> {
    match action {
        SkillAction::Install { global, force } => devme_cli::skill::install(global, force, json),
        SkillAction::Uninstall { global } => devme_cli::skill::uninstall(global, json),
        SkillAction::Status => devme_cli::skill::status(json),
    }
}

fn print_completions(shell: Shell) {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "devme", &mut std::io::stdout());
}

/// Bare `devme` entry point. With `remote.default = true` (and a host set),
/// the project is remote-first, so this behaves as `devme remote` — ensure
/// the live-sync, then attach to the remote stack's TUI. Otherwise it opens
/// the local TUI. `--local` forces the local TUI regardless.
async fn launch_default(force_local: bool, flags: devme_cli::remote::RunFlags) -> i32 {
    if !force_local {
        let cfg = devme_config::GlobalConfig::load();
        if cfg.remote.is_default() && cfg.remote.host.is_some() {
            return match std::env::current_dir() {
                Ok(cwd) => match devme_cli::remote::run(&cwd, flags) {
                    Ok(()) => 0,
                    Err(e) => {
                        devme_ui::error(e);
                        1
                    }
                },
                Err(e) => {
                    devme_ui::error(e);
                    1
                }
            };
        }
    }
    match launch_tui().await {
        Ok(code) => code,
        Err(e) => {
            devme_ui::error(e);
            1
        }
    }
}

/// Launch the TUI directly. Runs preflight checks first, then hands off
/// to the TUI event loop which manages all daemon spawning.
async fn launch_tui() -> anyhow::Result<i32> {
    let cwd = std::env::current_dir()?;
    let config_path = cwd.join("devme.toml");
    if let Ok(toml_str) = std::fs::read_to_string(&config_path)
        && let Ok(stack) = Stack::parse(&toml_str)
    {
        // Env resolution only prompts when vars are missing — silent otherwise.
        if !stack.env.is_empty() {
            // Honour `[stack] env_file` (ADR-0014) — compute the target
            // path before moving `stack.env` out below.
            let env_file = devme_supervisor::env_resolve::env_file_path(&stack, &cwd);
            let env_pairs: Vec<(String, devme_config::EnvVar)> = stack.env.into_iter().collect();
            let interactive = std::io::stdin().is_terminal();
            let mut stdin = std::io::BufReader::new(std::io::stdin());
            let mut stderr = std::io::stderr();
            let _ = devme_supervisor::env_resolve::resolve_env_vars(
                &env_pairs,
                &env_file,
                &cwd,
                &mut stdin,
                &mut stderr,
                interactive,
                devme_ui::err_style(),
            );
        }
        // Only show preflight output when something needs provisioning.
        if let Ok(stack) = Stack::parse(&toml_str) {
            if !devme_supervisor::preflight::all_checks_pass(&stack, &cwd) {
                let interactive = std::io::stdin().is_terminal();
                let mut stdin = std::io::BufReader::new(std::io::stdin());
                run_preflight_quiet_aware(&stack, &cwd, &mut stdin, interactive);
            }
            ensure_docker_if_needed(&stack)?;

            // Catch ports already taken by a stray container/process and
            // offer to free them before the daemon tries to bind.
            let interactive = std::io::stdin().is_terminal();
            let mut stdin = std::io::BufReader::new(std::io::stdin());
            let mut stderr = std::io::stderr();
            let _ = devme_supervisor::port_preflight::check_ports(
                &stack,
                &mut stdin,
                &mut stderr,
                interactive,
                devme_ui::err_style(),
            );
        }
    }

    devme_tui::launch(false).await?;
    maybe_show_skills_hint();
    Ok(0)
}

use devme_supervisor::spawn::{ensure_daemon as ensure_daemon_inner, ensure_shared_daemon};

/// Make sure a daemon is listening on `sock` for the current cwd. Thin
/// wrapper that pins `cwd` to the process's working directory; see
/// `devme_supervisor::spawn::ensure_daemon` for the underlying logic.
///
/// Before spawning a new daemon, resolves any declared `[env.*]` vars
/// from `devme.toml` — prompting the user for missing values while we
/// still have a terminal attached (ADR-0014).
async fn ensure_daemon(sock: &std::path::Path) -> anyhow::Result<bool> {
    let cwd = std::env::current_dir()?;

    // Re-entrant `up` (daemon already listening): skip the boot preflight —
    // env resolution, dependency checks, Docker, port probes all gated the
    // *boot* that already happened, and re-running them adds seconds and a
    // full "Check dependencies" tree to every re-attach (`devme remote` runs
    // `up -d` on each attach). A daemon that's alive resolved its env and
    // passed its checks when it booted; if dependencies broke since, `devme
    // doctor` (or `down` + `up`) is the re-check path.
    if devme_client::Client::connect(sock).await.is_ok() {
        return Ok(false);
    }

    let config_path = cwd.join("devme.toml");
    if let Ok(toml_str) = std::fs::read_to_string(&config_path)
        && let Ok(stack) = Stack::parse(&toml_str)
    {
        if !stack.env.is_empty() {
            // Honour `[stack] env_file` (ADR-0014) — compute the target
            // path before moving `stack.env` out below.
            let env_file = devme_supervisor::env_resolve::env_file_path(&stack, &cwd);
            let env_pairs: Vec<(String, devme_config::EnvVar)> = stack.env.into_iter().collect();
            let interactive = std::io::stdin().is_terminal();
            let mut stdin = std::io::BufReader::new(std::io::stdin());
            let mut stderr = std::io::stderr();
            if let Err(e) = devme_supervisor::env_resolve::resolve_env_vars(
                &env_pairs,
                &env_file,
                &cwd,
                &mut stdin,
                &mut stderr,
                interactive,
                devme_ui::err_style(),
            ) {
                devme_ui::warn(format!("env resolution failed: {e}"));
            }
        }
        // Re-parse since we moved `stack.env` above
        if let Ok(stack) = Stack::parse(&toml_str) {
            // Preflight: check dependencies that don't need services.
            // Under `-q` the tree renders into a buffer that's dumped
            // only when something failed — quiet suppresses information,
            // not warnings.
            let interactive = std::io::stdin().is_terminal();
            let mut stdin = std::io::BufReader::new(std::io::stdin());
            run_preflight_quiet_aware(&stack, &cwd, &mut stdin, interactive);

            ensure_docker_if_needed(&stack)?;

            // Catch ports already taken by a stray container/process and
            // offer to free them before the daemon tries to bind.
            let interactive = std::io::stdin().is_terminal();
            let mut stdin = std::io::BufReader::new(std::io::stdin());
            let mut stderr = std::io::stderr();
            let _ = devme_supervisor::port_preflight::check_ports(
                &stack,
                &mut stdin,
                &mut stderr,
                interactive,
                devme_ui::err_style(),
            );
        }
    }

    ensure_daemon_inner(sock, &cwd).await
}

/// Run the dependency preflight with `-q` semantics: normally the tree
/// streams to stderr; under quiet it renders into a buffer that is shown
/// only when a step ended Failed/Manual — so a clean quiet boot is silent
/// but a broken dependency still surfaces (quiet suppresses information,
/// not warnings).
fn run_preflight_quiet_aware<R: std::io::BufRead>(
    stack: &Stack,
    cwd: &std::path::Path,
    stdin: &mut R,
    interactive: bool,
) {
    use devme_supervisor::preflight::{StepResult, run_preflight};
    if !devme_ui::quiet() {
        let mut stderr = std::io::stderr();
        let _ = run_preflight(
            stack,
            cwd,
            stdin,
            &mut stderr,
            interactive,
            assume_yes(),
            devme_ui::err_style(),
        );
        return;
    }
    let mut buf: Vec<u8> = Vec::new();
    // Quiet implies no interactive prompts — a prompt into a buffer would
    // hang invisibly, so provisioning falls back to its non-interactive path.
    let result = run_preflight(
        stack,
        cwd,
        stdin,
        &mut buf,
        false,
        assume_yes(),
        devme_ui::err_style(),
    );
    let failed = match &result {
        Ok(r) => r
            .results
            .iter()
            .any(|(_, s)| matches!(s, StepResult::Failed | StepResult::Manual)),
        Err(_) => true,
    };
    if failed {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), &buf);
    }
}

/// If the stack has services that use Docker and Docker isn't running,
/// start the user's preferred daemon (prompting on first use).
fn ensure_docker_if_needed(stack: &Stack) -> anyhow::Result<()> {
    use devme_config::{GlobalConfig, docker};

    if !docker::stack_needs_docker(stack) {
        return Ok(());
    }
    if docker::is_docker_running() {
        return Ok(());
    }

    let mut cfg = GlobalConfig::load();

    let daemon_id = match &cfg.docker.daemon {
        Some(id) => id.clone(),
        None => {
            let installed = docker::detect_installed();
            if installed.is_empty() {
                return Err(anyhow::anyhow!(
                    "services require Docker but no Docker daemon is installed\n\
                     install OrbStack, Docker Desktop, or Colima"
                ));
            }
            if installed.len() == 1 {
                let id = installed[0].id.clone();
                devme_ui::info(format!(
                    "auto-selected {} (only daemon installed)",
                    installed[0].label
                ));
                cfg.docker.daemon = Some(id.clone());
                let _ = cfg.save();
                id
            } else {
                if !std::io::stdin().is_terminal() {
                    return Err(anyhow::anyhow!(
                        "Docker is not running and no daemon is configured\n\
                         run: devme config set docker.daemon <name>"
                    ));
                }
                eprintln!("Docker is required but not running. Which daemon should devme start?\n");
                for (i, d) in installed.iter().enumerate() {
                    eprintln!("  [{}] {}", i + 1, d.label);
                }
                eprint!("\nChoice [1]: ");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                let trimmed = input.trim();
                let idx = if trimmed.is_empty() {
                    0
                } else {
                    trimmed
                        .parse::<usize>()
                        .map_err(|_| anyhow::anyhow!("invalid choice"))?
                        .checked_sub(1)
                        .ok_or_else(|| anyhow::anyhow!("invalid choice"))?
                };
                let chosen = installed
                    .get(idx)
                    .ok_or_else(|| anyhow::anyhow!("invalid choice"))?;
                devme_ui::info(format!("saved docker.daemon = {}", chosen.id));
                cfg.docker.daemon = Some(chosen.id.clone());
                let _ = cfg.save();
                chosen.id.clone()
            }
        }
    };

    devme_ui::info(format!("starting Docker via {daemon_id}…"));
    docker::start_daemon(&daemon_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    devme_ui::success("Docker is ready");
    Ok(())
}

/// Keep a devme-managed skill install in step with this binary. Two modes:
///
/// - `skill.auto_update = true`: silently regenerate any stale, unmodified,
///   devme-written install. No prompt, no nag.
/// - otherwise: on an interactive terminal only, print a one-line nudge —
///   throttled to once per binary version — pointing at `devme skill install`.
///
/// Either way we only ever touch installs devme recorded writing and that the
/// user hasn't edited since; foreign/modified copies are left alone. Agents
/// and pipes (no tty) get nothing but the silent auto-update path.
fn maybe_skill_update() {
    if devme_ui::quiet() {
        return;
    }
    let mut cfg = devme_config::GlobalConfig::load();
    if cfg.skill_installs().is_empty() {
        return;
    }

    if cfg.skill_auto_update() {
        let updated = devme_config::skill::auto_update(&mut cfg);
        if !updated.is_empty() {
            devme_ui::info(format!(
                "refreshed AI skill to v{} in {} location(s)",
                devme_config::skill::embedded_version(),
                updated.len()
            ));
        }
        return;
    }

    // Nudge only a human at a keyboard — never an agent or a pipe.
    if !std::io::stdin().is_terminal() {
        return;
    }
    if cfg.get("hints.skills").as_deref() == Some("false") {
        return;
    }
    let stale = devme_config::skill::stale_installs(&cfg);
    let Some(first) = stale.first() else {
        return;
    };
    let embedded = devme_config::skill::embedded_version();
    if cfg.skill_last_nudge() == Some(embedded.as_str()) {
        return;
    }
    devme_ui::info(format!(
        "AI skill is out of date (v{} → v{})",
        first.from, first.to
    ));
    devme_ui::hint("devme skill install");
    cfg.set_skill_last_nudge(&embedded);
    let _ = cfg.save();
}

fn maybe_show_skills_hint() {
    if devme_ui::quiet() {
        return;
    }

    let cfg = devme_config::GlobalConfig::load();
    if cfg.get("hints.skills") == Some("false".into()) {
        return;
    }
    // Don't nag to install a skill devme already manages — `maybe_skill_update`
    // owns keeping it current.
    if !cfg.skill_installs().is_empty() {
        return;
    }

    let config_dir = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        std::path::PathBuf::from(xdg).join("devme")
    } else if let Some(home) = std::env::var_os("HOME") {
        std::path::PathBuf::from(home).join(".config").join("devme")
    } else {
        return;
    };

    let state_file = config_dir.join("skills-hint-state");
    let (count, last_shown) = match std::fs::read_to_string(&state_file) {
        Ok(contents) => {
            let mut lines = contents.lines();
            let count: u32 = lines.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let ts: u64 = lines.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            (count, ts)
        }
        Err(_) => (0, 0),
    };

    if count >= 4 {
        return;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Backoff: 0s, 3 days, 2 weeks, 6 weeks
    let min_gap_secs: u64 = match count {
        0 => 0,
        1 => 3 * 86400,
        2 => 14 * 86400,
        3 => 42 * 86400,
        _ => return,
    };

    if now.saturating_sub(last_shown) < min_gap_secs {
        return;
    }

    devme_ui::info("devme has an AI coding skill");
    devme_ui::hint("install: devme skill install (or: npx skills add devme-sh/skills)");
    if count == 0 {
        devme_ui::hint("suppress with: devme config set hints.skills false");
    }

    let _ = std::fs::create_dir_all(&config_dir);
    let _ = std::fs::write(&state_file, format!("{}\n{now}", count + 1));
}

fn socket_path() -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    devme_config::paths::supervisor_socket(&cwd)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/devme.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_since_accepts_durations() {
        // 30s before "now" should be ~30_000 ms below now; assert the delta.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let got = parse_since("30s").unwrap();
        let delta = now.saturating_sub(got);
        assert!((29_000..=31_000).contains(&delta), "delta was {delta}");
        // Units scale as expected, measured against a single `now`.
        assert!(parse_since("5m").unwrap() < parse_since("30s").unwrap());
        assert!(parse_since("2h").unwrap() < parse_since("5m").unwrap());
        assert!(parse_since("1d").unwrap() < parse_since("2h").unwrap());
    }

    #[test]
    fn parse_since_accepts_bare_epoch_ms() {
        assert_eq!(parse_since("1730000000000").unwrap(), 1_730_000_000_000);
    }

    #[test]
    fn parse_since_rejects_garbage() {
        assert!(parse_since("soon").is_err());
        assert!(parse_since("5y").is_err());
        assert!(parse_since("").is_err());
    }

    #[test]
    fn strip_ansi_removes_color_keeps_text() {
        assert_eq!(strip_ansi("\x1b[32mok\x1b[0m done"), "ok done");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x1b[2K\x1b[0Gprogress"), "progress");
    }
}
