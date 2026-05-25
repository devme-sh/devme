//! Spawning helpers shared between the `devme` CLI and the TUI's
//! worktree autodiscovery.
//!
//! `ensure_daemon` is the same primitive both call: connect to the
//! supervisor socket if one is already there, otherwise fork
//! `devme-supervisor` in `cwd` and wait for it to bind. Lives here
//! (alongside the supervisor it spawns) instead of in `devme-client` so
//! the layering reads cleanly: the binary that creates is also the one
//! that holds the spawn knowledge.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Make sure a supervisor daemon is listening on `sock`. If not, fork
/// `devme-supervisor` with `cwd` as its working directory and wait up
/// to ~5s for it to bind.
///
/// Returns `true` if a new daemon was spawned, `false` if one was
/// already running. Returns an error if `cwd` has no `devme.toml`
/// (no point spawning a supervisor that's going to immediately error
/// out) or if the supervisor exits before binding.
pub async fn ensure_daemon(sock: &Path, cwd: &Path) -> anyhow::Result<bool> {
    if devme_client::Client::connect(sock).await.is_ok() {
        return Ok(false);
    }
    if !cwd.join("devme.toml").exists() {
        return Err(anyhow::anyhow!(
            "no devme.toml in {} (run from a directory containing one)",
            cwd.display()
        ));
    }

    let supervisor = find_sibling_binary("devme-supervisor")?;
    let mut child = std::process::Command::new(&supervisor)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawning {}: {e}", supervisor.display()))?;

    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if devme_client::Client::connect(sock).await.is_ok() {
            // Daemon is up. Detach stderr so it doesn't fill the pipe
            // and backpressure the daemon; from this point its logs go
            // to its own log file, not our process.
            drop(child.stderr.take());
            return Ok(true);
        }
        if let Ok(Some(_)) = child.try_wait() {
            let mut stderr = String::new();
            if let Some(mut handle) = child.stderr.take() {
                let _ = handle.read_to_string(&mut stderr);
            }
            let stderr = stderr.trim();
            return Err(anyhow::anyhow!(
                "supervisor exited during startup\n{}",
                if stderr.is_empty() { "(no stderr)" } else { stderr }
            ));
        }
    }
    Err(anyhow::anyhow!(
        "daemon didn't come up at {} within 5s",
        sock.display()
    ))
}

/// Like [`ensure_daemon`] but for the repo-scoped shared supervisor.
/// Spawns `devme-shared-supervisor` in `cwd` if no daemon is listening
/// on the shared socket yet. The shared daemon manages `scope = "repo"`
/// services for all worktrees of the same git repo.
pub async fn ensure_shared_daemon(cwd: &Path) -> anyhow::Result<bool> {
    let sock = devme_config::paths::shared_socket(cwd)?;
    if devme_client::Client::connect(&sock).await.is_ok() {
        return Ok(false);
    }

    let binary = find_sibling_binary("devme-shared-supervisor")?;
    let mut child = std::process::Command::new(&binary)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawning {}: {e}", binary.display()))?;

    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if devme_client::Client::connect(&sock).await.is_ok() {
            drop(child.stderr.take());
            return Ok(true);
        }
        if let Ok(Some(_)) = child.try_wait() {
            let mut stderr = String::new();
            if let Some(mut handle) = child.stderr.take() {
                let _ = handle.read_to_string(&mut stderr);
            }
            let stderr = stderr.trim();
            return Err(anyhow::anyhow!(
                "shared supervisor exited during startup\n{}",
                if stderr.is_empty() { "(no stderr)" } else { stderr }
            ));
        }
    }
    Err(anyhow::anyhow!(
        "shared daemon didn't come up at {} within 5s",
        sock.display()
    ))
}

/// Resolve a sibling binary next to the calling executable. Handles
/// symlink installs (e.g. `ln -s` into `~/.local/bin`) by canonicalizing
/// `current_exe()` before reading its parent.
pub fn find_sibling_binary(name: &str) -> anyhow::Result<PathBuf> {
    if let Ok(self_exe) = std::env::current_exe() {
        let resolved = std::fs::canonicalize(&self_exe).unwrap_or(self_exe);
        if let Some(parent) = resolved.parent() {
            let candidate = parent.join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    Ok(PathBuf::from(name))
}
