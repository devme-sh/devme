//! Docker daemon detection and lifecycle.
//!
//! Detects which Docker-compatible daemons are installed on the host,
//! checks whether Docker is currently reachable, and starts the
//! user's preferred daemon.

use crate::Stack;

/// A Docker-compatible daemon detected on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedDaemon {
    /// Identifier stored in config (e.g. "orbstack").
    pub id: String,
    /// Human-friendly name (e.g. "OrbStack").
    pub label: String,
}

/// Known daemons and how to detect/start them.
struct DaemonDef {
    id: &'static str,
    label: &'static str,
    detect: fn() -> bool,
    start_cmd: &'static [&'static str],
}

const DAEMONS: &[DaemonDef] = &[
    DaemonDef {
        id: "orbstack",
        label: "OrbStack",
        detect: || {
            std::path::Path::new("/Applications/OrbStack.app").exists()
                || which("orbstack")
        },
        start_cmd: &["open", "-a", "OrbStack"],
    },
    DaemonDef {
        id: "docker-desktop",
        label: "Docker Desktop",
        detect: || std::path::Path::new("/Applications/Docker.app").exists(),
        start_cmd: &["open", "-a", "Docker"],
    },
    DaemonDef {
        id: "colima",
        label: "Colima",
        detect: || which("colima"),
        start_cmd: &["colima", "start"],
    },
    DaemonDef {
        id: "rancher-desktop",
        label: "Rancher Desktop",
        detect: || std::path::Path::new("/Applications/Rancher Desktop.app").exists(),
        start_cmd: &["open", "-a", "Rancher Desktop"],
    },
];

fn which(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// List every Docker daemon installed on this machine.
pub fn detect_installed() -> Vec<DetectedDaemon> {
    DAEMONS
        .iter()
        .filter(|d| (d.detect)())
        .map(|d| DetectedDaemon {
            id: d.id.to_string(),
            label: d.label.to_string(),
        })
        .collect()
}

/// True if `docker info` succeeds (i.e. a Docker daemon is reachable).
pub fn is_docker_running() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Start the daemon identified by `id`. Blocks until `docker info` succeeds
/// or the timeout (30s) is reached.
pub fn start_daemon(id: &str) -> Result<(), String> {
    let def = DAEMONS
        .iter()
        .find(|d| d.id == id)
        .ok_or_else(|| format!("unknown docker daemon: {id}"))?;

    let args = def.start_cmd;
    let status = std::process::Command::new(args[0])
        .args(&args[1..])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("failed to start {}: {e}", def.label))?;

    if !status.success() {
        return Err(format!("{} exited with {}", def.label, status));
    }

    for _ in 0..60 {
        if is_docker_running() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(format!("{} started but Docker didn't become ready within 30s", def.label))
}

/// True if any service in the stack uses docker in its command.
pub fn stack_needs_docker(stack: &Stack) -> bool {
    stack.service.values().any(|svc| cmd_needs_docker(&svc.cmd))
}

fn cmd_needs_docker(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    trimmed.starts_with("docker ")
        || trimmed.starts_with("docker-compose ")
        || trimmed.contains("docker compose")
        || trimmed.contains("docker run")
        || trimmed.contains("docker exec")
        || trimmed.contains("docker build")
}

/// Return the start command for a daemon id, for use as a step provision.
pub fn start_command_for(id: &str) -> Option<String> {
    DAEMONS
        .iter()
        .find(|d| d.id == id)
        .map(|d| d.start_cmd.join(" "))
}

// ── Port-conflict helpers (see `supervisor::port_preflight`) ────────────────

/// Name of the running container publishing `host_port`, if any.
///
/// `docker ps --filter publish=<port>` matches containers that publish that
/// host port. Returns `None` when Docker isn't reachable or nothing matches —
/// the caller then falls back to host-process detection.
pub fn container_publishing_port(host_port: u16) -> Option<String> {
    let out = std::process::Command::new("docker")
        .args(["ps", "--filter", &format!("publish={host_port}"), "--format", "{{.Names}}"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// The Compose project a container belongs to (its
/// `com.docker.compose.project` label), if it was started by Compose.
pub fn container_compose_project(container: &str) -> Option<String> {
    let out = std::process::Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{ index .Config.Labels \"com.docker.compose.project\" }}",
            container,
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let proj = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // `docker inspect` prints `<no value>` for a missing label.
    if proj.is_empty() || proj == "<no value>" {
        None
    } else {
        Some(proj)
    }
}

/// `docker stop <container>` — graceful stop of a single container. Keeps the
/// container (restartable with `docker start`); just frees its ports.
pub fn stop_container(container: &str) -> Result<(), String> {
    run_docker(["stop", container], &format!("docker stop {container}"))
}

/// `docker compose -p <project> down` — stop and remove a whole Compose
/// project (the bigger hammer; volumes survive). Works by project name without
/// the compose file present, since Compose locates containers by label.
pub fn compose_down(project: &str) -> Result<(), String> {
    run_docker(["compose", "-p", project, "down"], &format!("docker compose -p {project} down"))
}

fn run_docker<const N: usize>(args: [&str; N], label: &str) -> Result<(), String> {
    let status = std::process::Command::new("docker")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("{label}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{label} exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_needs_docker_detects_docker_commands() {
        assert!(cmd_needs_docker("docker compose up"));
        assert!(cmd_needs_docker("docker run -d nginx"));
        assert!(cmd_needs_docker("docker-compose up -d"));
        assert!(cmd_needs_docker("docker build ."));
        assert!(cmd_needs_docker("docker exec -it web bash"));
    }

    #[test]
    fn cmd_needs_docker_ignores_non_docker() {
        assert!(!cmd_needs_docker("npm run dev"));
        assert!(!cmd_needs_docker("cargo run"));
        assert!(!cmd_needs_docker("python manage.py runserver"));
    }

    #[test]
    fn detect_installed_returns_vec() {
        let installed = detect_installed();
        // Can't assert specific daemons since this depends on the machine,
        // but the function should not panic.
        assert!(installed.len() <= DAEMONS.len());
    }
}
