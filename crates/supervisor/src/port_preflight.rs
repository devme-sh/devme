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
use devme_core::{PortSpec, Scope};
use devme_config::{Service, Stack};

// Shared Clack-style constants (kept in sync with `preflight.rs`).
const S_BAR: &str = "│";
const S_BAR_END: &str = "└";
const S_STEP_ACTIVE: &str = "◆";
const S_STEP_SUBMIT: &str = "◇";
const C_RESET: &str = "\x1b[0m";
const C_DIM: &str = "\x1b[2m";
const C_BOLD: &str = "\x1b[1m";
const C_CYAN: &str = "\x1b[36m";
const C_GREEN: &str = "\x1b[32m";
const C_YELLOW: &str = "\x1b[33m";
const C_RED: &str = "\x1b[31m";

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
fn is_port_free(port: u16) -> bool {
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

/// What's holding a port, and how we can free it.
enum Holder {
    /// A Docker container, with its Compose project (if any).
    Container { name: String, project: Option<String> },
    /// One or more host processes (pid, name).
    Process(Vec<(u32, Option<String>)>),
    /// Taken, but we couldn't attribute it to a container or process.
    Unknown,
}

fn identify_holder(port: u16) -> Holder {
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

fn kill_pid(pid: u32) -> Result<(), String> {
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
) -> std::io::Result<()> {
    // Collect knowable ports, deduped (first service to claim a port wins
    // the label). Declaration order is preserved via the IndexMap.
    let mut seen = std::collections::HashSet::new();
    let mut conflicts: Vec<Conflict> = Vec::new();
    for (name, svc) in &stack.service {
        let Some(port) = checkable_port(svc) else { continue };
        if !seen.insert(port) {
            continue;
        }
        if is_port_free(port) {
            continue;
        }
        conflicts.push(Conflict {
            service: name.clone(),
            port,
            holder: identify_holder(port),
        });
    }

    if conflicts.is_empty() {
        return Ok(());
    }

    writeln!(output)?;
    writeln!(
        output,
        "  {C_CYAN}{S_STEP_ACTIVE}{C_RESET}  {C_BOLD}Port conflicts{C_RESET}"
    )?;
    writeln!(output, "  {C_DIM}{S_BAR}{C_RESET}")?;

    let mut unresolved = 0usize;
    for conflict in &conflicts {
        let Conflict { service, port, holder } = conflict;

        let who = match holder {
            Holder::Container { name, project } => match project {
                Some(p) => format!("container {C_BOLD}{name}{C_RESET}{C_DIM} (compose: {p})"),
                None => format!("container {C_BOLD}{name}{C_RESET}"),
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
                format!("{C_BOLD}{label}{C_RESET}")
            }
            Holder::Unknown => format!("{C_DIM}an unknown process{C_RESET}"),
        };

        writeln!(
            output,
            "  {C_DIM}{S_BAR}{C_RESET}  {C_YELLOW}▲{C_RESET} {C_BOLD}{service}{C_RESET} wants \
             port {C_BOLD}{port}{C_RESET}{C_DIM} — held by {C_RESET}{who}{C_RESET}"
        )?;

        if !interactive {
            unresolved += 1;
            continue;
        }

        // Build the prompt offer based on what we can do about it.
        let freed = match holder {
            Holder::Container { name, project } => {
                let prompt = match project {
                    Some(_) => "[s] stop container, [d] compose down, [n] skip",
                    None => "[s] stop container, [n] skip",
                };
                write!(
                    output,
                    "  {C_DIM}{S_BAR}{C_RESET}    {C_DIM}Free it? {prompt} ›{C_RESET} "
                )?;
                output.flush()?;
                let choice = read_choice(input)?;
                match choice.as_str() {
                    "s" => act(output, "docker stop", docker::stop_container(name)),
                    "d" => match project {
                        Some(p) => act(output, "docker compose down", docker::compose_down(p)),
                        None => skip(output)?,
                    },
                    _ => skip(output)?,
                }
            }
            Holder::Process(pids) => {
                write!(
                    output,
                    "  {C_DIM}{S_BAR}{C_RESET}    {C_DIM}Free it? [k] kill, [n] skip ›{C_RESET} "
                )?;
                output.flush()?;
                let choice = read_choice(input)?;
                match choice.as_str() {
                    "k" => {
                        let mut ok = true;
                        for (pid, _) in pids {
                            if let Err(e) = kill_pid(*pid) {
                                ok = false;
                                writeln!(
                                    output,
                                    "  {C_DIM}{S_BAR}{C_RESET}    {C_RED}▲ {e}{C_RESET}"
                                )?;
                            }
                        }
                        if ok {
                            act_ok(output, "Killed")?;
                            true
                        } else {
                            false
                        }
                    }
                    _ => skip(output)?,
                }
            }
            Holder::Unknown => {
                writeln!(
                    output,
                    "  {C_DIM}{S_BAR}{C_RESET}    {C_DIM}can't attribute this port — free it manually{C_RESET}"
                )?;
                false
            }
        };

        // Re-probe: confirm the action actually freed the port.
        if freed {
            if is_port_free(*port) {
                writeln!(
                    output,
                    "  {C_DIM}{S_BAR}{C_RESET}    {C_GREEN}{S_STEP_SUBMIT} Port {port} is free{C_RESET}"
                )?;
            } else {
                unresolved += 1;
                writeln!(
                    output,
                    "  {C_DIM}{S_BAR}{C_RESET}    {C_YELLOW}▲ Port {port} still in use{C_RESET}"
                )?;
            }
        } else {
            unresolved += 1;
        }
    }

    writeln!(output, "  {C_DIM}{S_BAR}{C_RESET}")?;
    if unresolved > 0 {
        writeln!(
            output,
            "  {S_BAR_END}  {C_YELLOW}{unresolved} port conflict{} unresolved — services may fail to start{C_RESET}",
            if unresolved == 1 { "" } else { "s" }
        )?;
    } else {
        writeln!(
            output,
            "  {S_BAR_END}  {C_GREEN}All conflicting ports freed{C_RESET}"
        )?;
    }
    writeln!(output)?;

    Ok(())
}

/// Read a single trimmed, lowercased choice token. Empty line (EOF or bare
/// Enter) reads as "n" — the safe default is to leave things alone.
fn read_choice<R: BufRead>(input: &mut R) -> std::io::Result<String> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok("n".to_string());
    }
    let t = line.trim().to_lowercase();
    Ok(if t.is_empty() { "n".to_string() } else { t })
}

/// Run a remediation, render the outcome, and report whether it succeeded.
fn act<W: Write>(output: &mut W, label: &str, result: Result<(), String>) -> bool {
    match result {
        Ok(()) => {
            let _ = writeln!(
                output,
                "  {C_DIM}{S_BAR}{C_RESET}    {C_GREEN}{S_STEP_SUBMIT} {label}{C_RESET}"
            );
            true
        }
        Err(e) => {
            let _ = writeln!(
                output,
                "  {C_DIM}{S_BAR}{C_RESET}    {C_RED}▲ {label} failed: {e}{C_RESET}"
            );
            false
        }
    }
}

fn act_ok<W: Write>(output: &mut W, label: &str) -> std::io::Result<()> {
    writeln!(
        output,
        "  {C_DIM}{S_BAR}{C_RESET}    {C_GREEN}{S_STEP_SUBMIT} {label}{C_RESET}"
    )
}

fn skip<W: Write>(output: &mut W) -> std::io::Result<bool> {
    writeln!(
        output,
        "  {C_DIM}{S_BAR}{C_RESET}    {C_DIM}{S_STEP_SUBMIT} Skipped{C_RESET}"
    )?;
    Ok(false)
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
        check_ports(&stack, &mut input, &mut output, false).unwrap();
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
        check_ports(&stack, &mut input, &mut output, false).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Port conflicts"), "got: {text}");
        assert!(text.contains(&port.to_string()));
        assert!(text.contains("unresolved"));
    }
}
