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
