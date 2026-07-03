//! Pre-start port-conflict detection.
//!
//! Before the daemon spawns anything, probe the host ports devme is about
//! to bind. When a port is already taken, identify the holder — a Docker
//! container (offer `docker stop`, or `docker compose down` for a Compose
//! project) or a plain host process (offer to kill it) — and let the user
//! free it in place. Renders Clack-style, mirroring `preflight.rs`.
//!
//! Scope: only ports we can know client-side — `Fixed` ports and
//! repo-scoped slot-offset ports (which always sit at slot 0). Instance
//! slot-offset ports are allocated daemon-side and aren't knowable here,
//! so they're skipped (see ADR-0007).

use std::io::{BufRead, Write};
use std::net::TcpListener;

use devme_config::docker;
use devme_config::{Service, Stack};
use devme_core::{PortSpec, Scope};
use devme_ui::{Item, Section, Style, glyph};

/// The concrete host port a service will bind, when it's knowable before
/// the daemon allocates a slot.
///
/// - `Fixed` → always that port.
/// - repo-scoped `SlotOffset` → resolves at slot 0 (repo services are
///   singletons; see [`Scope::Repo`]).
/// - instance-scoped `SlotOffset` → `None`; the slot is chosen daemon-side
///   so the port isn't knowable here.
fn checkable_port(svc: &Service) -> Option<u16> {
    match svc.port? {
        PortSpec::Fixed { fixed } => Some(fixed),
        PortSpec::SlotOffset { .. } => match svc.scope {
            Scope::Repo => Some(svc.port?.resolve(0)),
            Scope::Instance => None,
        },
    }
}

/// True if `port` can be bound right now (i.e. nothing is listening on it).
pub fn is_port_free(port: u16) -> bool {
    TcpListener::bind(("0.0.0.0", port)).is_ok()
}

/// PIDs of host processes listening on `port` (via `lsof`).
fn pids_on_port(port: u16) -> Vec<u32> {
    let out = std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .collect(),
        _ => Vec::new(),
    }
}

/// Short command name for a PID (via `ps -o comm=`).
fn process_name(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // `comm` is often a full path; show just the basename.
    let short = name.rsplit('/').next().unwrap_or(&name).to_string();
    if short.is_empty() { None } else { Some(short) }
}

/// What's holding a port, and how we can free it. Public so the TUI's
/// reactive port-conflict modal can reuse the same detection + remediation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holder {
    /// A Docker container, with its Compose project (if any).
    Container {
        name: String,
        project: Option<String>,
    },
    /// One or more host processes (pid, name).
    Process(Vec<(u32, Option<String>)>),
    /// Taken, but we couldn't attribute it to a container or process.
    Unknown,
}

/// Attribute `port` to whatever is currently holding it — a Docker container
/// (preferred, with its Compose project) or one or more host processes.
pub fn identify_holder(port: u16) -> Holder {
    if let Some(name) = docker::container_publishing_port(port) {
        let project = docker::container_compose_project(&name);
        return Holder::Container { name, project };
    }
    let pids = pids_on_port(port);
    if !pids.is_empty() {
        return Holder::Process(pids.into_iter().map(|p| (p, process_name(p))).collect());
    }
    Holder::Unknown
}

/// Extract the Compose project name from a `docker compose … -p NAME …`
/// service command. Returns `None` when the command isn't a Compose
/// invocation or declares no explicit project. Handles `-p NAME`, `-pNAME`,
/// `--project-name NAME`, and `--project-name=NAME`.
fn compose_project_in_cmd(cmd: &str) -> Option<String> {
    // Guard: only treat `-p` as a project flag inside an actual compose
    // command — other tools (psql, etc.) use `-p` for unrelated things.
    if !cmd.contains("compose") {
        return None;
    }
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    for (i, t) in tokens.iter().enumerate() {
        if let Some(v) = t.strip_prefix("--project-name=") {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        } else if *t == "--project-name" || *t == "-p" {
            if let Some(v) = tokens.get(i + 1).filter(|v| !v.is_empty()) {
                return Some((*v).to_string());
            }
        } else if let Some(v) = t.strip_prefix("-p") {
            // attached short form `-pNAME` (bare `-p` handled above)
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// True when `holder` is a container in the same Compose project that this
/// service's `cmd` manages — i.e. devme's own already-running shared
/// service. `docker compose up` will adopt the running container rather than
/// collide with it, so this is no conflict; the daemon's health check is the
/// real arbiter. The common case: a repo-scoped Postgres left up by a prior
/// session, intentionally persistent across worktrees (see ADR-0007).
fn is_own_compose_service(holder: &Holder, cmd: &str) -> bool {
    match holder {
        Holder::Container {
            project: Some(proj),
            ..
        } => compose_project_in_cmd(cmd).as_deref() == Some(proj.as_str()),
        _ => false,
    }
}

/// `kill <pid>` (SIGTERM). Frees a port held by a plain host process.
pub fn kill_pid(pid: u32) -> Result<(), String> {
    let status = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .map_err(|e| format!("kill {pid}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("kill {pid} exited with {status}"))
    }
}

/// One service with a knowable, currently-occupied port.
struct Conflict {
    service: String,
    port: u16,
    holder: Holder,
}

/// Probe every knowable port in the stack. When some are occupied, render a
/// Clack-style report and — in interactive mode — offer to free each in
/// place (stop the container, tear down its Compose project, or kill the
/// host process), re-probing after each action.
///
/// Non-interactive: reports the conflicts and returns without prompting.
/// Never fails the launch — devme's own allocation/health checks remain the
/// source of truth; this is a courtesy that catches the common case early.
pub fn check_ports<R: BufRead, W: Write>(
    stack: &Stack,
    input: &mut R,
    output: &mut W,
    interactive: bool,
    style: Style,
) -> std::io::Result<()> {
    // Collect knowable ports, deduped (first service to claim a port wins
    // the label). Declaration order is preserved via the IndexMap.
    let mut seen = std::collections::HashSet::new();
    let mut conflicts: Vec<Conflict> = Vec::new();
    for (name, svc) in &stack.service {
        let Some(port) = checkable_port(svc) else {
            continue;
        };
        if !seen.insert(port) {
            continue;
        }
        if is_port_free(port) {
            continue;
        }
        let holder = identify_holder(port);
        // Adopt, don't conflict: when the port is held by devme's own
        // already-running shared Compose service, `docker compose up` reuses
        // it and the daemon's health check governs. Skip it silently rather
        // than prompting the user to tear down their own warm shared stack.
        if is_own_compose_service(&holder, &svc.cmd) {
            continue;
        }
        conflicts.push(Conflict {
            service: name.clone(),
            port,
            holder,
        });
    }

    if conflicts.is_empty() {
        return Ok(());
    }

    let mut sec = Section::begin(output, style, "Port conflicts")?;

    let mut unresolved = 0usize;
    for conflict in &conflicts {
        let Conflict {
            service,
            port,
            holder,
        } = conflict;

        let who = match holder {
            Holder::Container { name, project } => match project {
                Some(p) => format!(
                    "container {}{}",
                    style.bold(name),
                    style.dim(&format!(" (compose: {p})"))
                ),
                None => format!("container {}", style.bold(name)),
            },
            Holder::Process(pids) => {
                let label = pids
                    .iter()
                    .map(|(pid, name)| match name {
                        Some(n) => format!("{n} ({pid})"),
                        None => format!("pid {pid}"),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                style.bold(&label)
            }
            Holder::Unknown => style.dim("an unknown process"),
        };

        sec.line(&format!(
            "{} {} wants port {}{}{who}",
            style.warn(glyph::WARN),
            style.bold(service),
            style.bold(&port.to_string()),
            style.dim(" — held by ")
        ))?;

        if !interactive {
            unresolved += 1;
            continue;
        }

        // A port we can't attribute gets no menu — there's nothing to act on.
        if let Holder::Unknown = holder {
            sec.sub_note("can't attribute this port — free it manually")?;
            unresolved += 1;
            continue;
        }

        // Offer the remediations as a single-select menu — arrow keys / j,k
        // on a TTY, numbered fallback otherwise. The primary action is
        // pre-selected; "Skip" is always last. `Esc`/`Ctrl-C` reads as Skip.
        enum Act<'a> {
            Stop(&'a str),
            Down(&'a str),
            Kill(&'a [(u32, Option<String>)]),
            Skip,
        }

        let mut choices: Vec<String> = Vec::new();
        let mut actions: Vec<Act> = Vec::new();
        match holder {
            Holder::Container { name, project } => {
                choices.push(format!("Stop container {name}"));
                actions.push(Act::Stop(name));
                if let Some(p) = project {
                    choices.push(format!("Compose down {p} (stops the whole project)"));
                    actions.push(Act::Down(p));
                }
            }
            Holder::Process(pids) => {
                let label = pids
                    .iter()
                    .map(|(pid, n)| match n {
                        Some(n) => format!("{n} ({pid})"),
                        None => format!("pid {pid}"),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                choices.push(format!("Kill {label}"));
                actions.push(Act::Kill(pids));
            }
            Holder::Unknown => unreachable!("handled above"),
        }
        choices.push("Skip".to_string());
        actions.push(Act::Skip);

        let picked = crate::prompt::select_one(input, sec.writer(), &choices, 0, style)?
            .unwrap_or(actions.len() - 1); // aborted → Skip (always last)

        let freed = match &actions[picked] {
            Act::Stop(name) => act(&mut sec, "docker stop", docker::stop_container(name)),
            Act::Down(project) => {
                act(&mut sec, "docker compose down", docker::compose_down(project))
            }
            Act::Kill(pids) => {
                let mut ok = true;
                for (pid, _) in pids.iter() {
                    if let Err(e) = kill_pid(*pid) {
                        ok = false;
                        sec.sub(Item::Fail, &e)?;
                    }
                }
                if ok {
                    sec.sub(Item::Ok, "Killed")?;
                    true
                } else {
                    false
                }
            }
            Act::Skip => {
                sec.sub_note("skipped")?;
                false
            }
        };

        // Re-probe: confirm the action actually freed the port.
        if freed {
            if is_port_free(*port) {
                sec.sub(Item::Ok, &format!("Port {port} is free"))?;
            } else {
                unresolved += 1;
                sec.sub(Item::Warn, &format!("Port {port} still in use"))?;
            }
        } else {
            unresolved += 1;
        }
    }

    if unresolved > 0 {
        sec.end(
            Item::Warn,
            &format!(
                "{unresolved} port conflict{} unresolved — services may fail to start",
                if unresolved == 1 { "" } else { "s" }
            ),
        )?;
    } else {
        sec.end(Item::Ok, "All conflicting ports freed")?;
    }

    Ok(())
}

/// Run a remediation, render the outcome, and report whether it succeeded.
fn act<W: Write>(sec: &mut Section<W>, label: &str, result: Result<(), String>) -> bool {
    match result {
        Ok(()) => {
            let _ = sec.sub(Item::Ok, label);
            true
        }
        Err(e) => {
            let _ = sec.sub(Item::Fail, &format!("{label} failed: {e}"));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse_stack(toml: &str) -> Stack {
        Stack::parse(toml).expect("parse")
    }

    #[test]
    fn fixed_port_is_checkable() {
        let stack = parse_stack(
            r#"
schema_version = 1

[service.db]
cmd = "echo db"
port = { fixed = 15432 }
"#,
        );
        let svc = &stack.service["db"];
        assert_eq!(checkable_port(svc), Some(15432));
    }

    #[test]
    fn repo_scoped_slot_offset_resolves_at_slot_zero() {
        let stack = parse_stack(
            r#"
schema_version = 1

[service.proxy]
cmd = "echo proxy"
scope = "repo"
port = { base = 8080, slot_offset = 10 }
"#,
        );
        let svc = &stack.service["proxy"];
        assert_eq!(checkable_port(svc), Some(8080));
    }

    #[test]
    fn instance_scoped_slot_offset_is_not_checkable() {
        let stack = parse_stack(
            r#"
schema_version = 1

[service.backend]
cmd = "echo backend"
port = { base = 8080, slot_offset = 10 }
"#,
        );
        let svc = &stack.service["backend"];
        assert_eq!(checkable_port(svc), None);
    }

    #[test]
    fn no_port_is_not_checkable() {
        let stack = parse_stack(
            r#"
schema_version = 1

[service.worker]
cmd = "echo worker"
"#,
        );
        let svc = &stack.service["worker"];
        assert_eq!(checkable_port(svc), None);
    }

    #[test]
    fn free_port_produces_no_output() {
        // Bind a port, drop the listener to free it, then check that port.
        let port = {
            let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let stack = parse_stack(&format!(
            r#"
schema_version = 1

[service.db]
cmd = "echo db"
port = {{ fixed = {port} }}
"#,
        ));
        let mut input = Cursor::new(b"");
        let mut output = Vec::new();
        check_ports(&stack, &mut input, &mut output, false, Style::PLAIN).unwrap();
        assert!(output.is_empty(), "free port should yield no report");
    }

    #[test]
    fn occupied_port_is_reported_non_interactive() {
        // Hold a port open for the duration of the check.
        let listener = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stack = parse_stack(&format!(
            r#"
schema_version = 1

[service.db]
cmd = "echo db"
port = {{ fixed = {port} }}
"#,
        ));
        let mut input = Cursor::new(b"");
        let mut output = Vec::new();
        check_ports(&stack, &mut input, &mut output, false, Style::PLAIN).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Port conflicts"), "got: {text}");
        assert!(text.contains(&port.to_string()));
        assert!(text.contains("unresolved"));
    }

    #[test]
    fn compose_project_parsed_from_cmd_variants() {
        let p = compose_project_in_cmd;
        assert_eq!(
            p("docker compose -f docker-compose.yml -p kpi-shared up db").as_deref(),
            Some("kpi-shared")
        );
        assert_eq!(
            p("docker compose --project-name kpi-shared up db").as_deref(),
            Some("kpi-shared")
        );
        assert_eq!(
            p("docker compose --project-name=kpi-shared up db").as_deref(),
            Some("kpi-shared")
        );
        assert_eq!(
            p("docker compose -pkpi-shared up db").as_deref(),
            Some("kpi-shared")
        );
        // No explicit project.
        assert_eq!(p("docker compose up db"), None);
        // Not a compose command — `-p` must not be read as a project flag.
        assert_eq!(p("psql -h localhost -p 5433 -U postgres"), None);
    }

    #[test]
    fn own_compose_service_is_adopted_not_conflicted() {
        let holder = Holder::Container {
            name: "kpi-shared-db-1".to_string(),
            project: Some("kpi-shared".to_string()),
        };
        // Same project the service manages → ours, adopt.
        assert!(is_own_compose_service(
            &holder,
            "docker compose -f docker-compose.yml -p kpi-shared up db"
        ));
        // A foreign project with the same port → genuine conflict.
        assert!(!is_own_compose_service(
            &holder,
            "docker compose -p some-other-project up db"
        ));
        // Service doesn't manage Compose at all → not ours.
        assert!(!is_own_compose_service(
            &holder,
            "cloud-sql-proxy --port 15432"
        ));
        // Host-process holders are never adopted.
        assert!(!is_own_compose_service(
            &Holder::Process(vec![(123, Some("postgres".to_string()))]),
            "docker compose -p kpi-shared up db"
        ));
    }

    #[test]
    fn occupied_port_interactive_skip_leaves_it() {
        // Hold the port with this test process so it's detected as a host
        // process. Tests aren't a TTY, so the menu uses the numbered
        // fallback reading from `input` — pick the last option (Skip) so we
        // never kill our own test runner.
        let listener = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stack = parse_stack(&format!(
            r#"
schema_version = 1

[service.db]
cmd = "echo db"
port = {{ fixed = {port} }}
"#,
        ));
        // "2" = Skip for a host-process holder (choices are [Kill, Skip]).
        let mut input = Cursor::new(b"2\n");
        let mut output = Vec::new();
        check_ports(&stack, &mut input, &mut output, true, Style::PLAIN).unwrap();
        let text = String::from_utf8(output).unwrap();
        // The interactive menu fired (either a Kill option, or — if the port
        // couldn't be attributed — the manual-free hint), and nothing was
        // freed, so the conflict stays unresolved.
        assert!(
            text.contains("Kill") || text.contains("can't attribute"),
            "expected an interactive remediation path, got: {text}"
        );
        assert!(text.contains("unresolved"), "got: {text}");
    }
}
