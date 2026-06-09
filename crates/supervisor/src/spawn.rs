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
use std::time::Duration;

use devme_core::ClientMessage;

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

    // A *responsive* daemon is already serving — attach to it.
    //
    // A bare `connect` is not enough here. When the shared daemon shuts
    // down it runs its teardown (`docker compose down`, which can take
    // several seconds) inside the serve loop and only unlinks the socket
    // *after* that returns. During that window its accept loop is parked
    // but the socket is still bound, so `connect` succeeds against a
    // daemon that will never reply and is about to exit. A quick TUI
    // quit→reopen would then "attach" to this corpse and never spawn a
    // fresh daemon — leaving the repo services (proxy/postgres) dead and
    // every dependent instance service waiting forever. Completing a
    // request/response round-trip distinguishes a live daemon (replies
    // immediately) from a dying one (times out). See ADR-0007.
    if daemon_responsive(&sock).await {
        return Ok(false);
    }

    // Not responsive. If the socket is still bound, a daemon is mid-shutdown:
    // spawning now would race its teardown — its `remove_file` on exit would
    // unlink our fresh socket. Wait for it to fully exit first, serialising
    // old-teardown-before-new-startup.
    if devme_client::Client::connect(&sock).await.is_ok() {
        wait_for_daemon_exit(&sock).await;
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

/// Probe whether a daemon on `sock` is alive *and responsive*, without
/// registering as a log subscriber. We send `RecheckHealth` — the shared
/// daemon answers it with a `Notice` rather than treating the connection
/// as a subscriber, so a probe never arms the daemon's idle-shutdown
/// timer (which would otherwise tear down a freshly-started detached
/// stack). Any reply within the timeout means alive; a refused connection
/// or a timeout (daemon mid-shutdown, accept loop parked) means not.
async fn daemon_responsive(sock: &Path) -> bool {
    let probe = async {
        let mut client = devme_client::Client::connect(sock).await.ok()?;
        client.request(ClientMessage::RecheckHealth).await.ok()
    };
    matches!(
        tokio::time::timeout(Duration::from_millis(1500), probe).await,
        Ok(Some(_))
    )
}

/// Poll until no listener answers on `sock` — i.e. a daemon that was
/// mid-shutdown has fully exited and unlinked its socket — so a fresh
/// daemon can bind cleanly. Bounded at ~10s because the shared daemon's
/// `docker compose down` teardown runs before it releases the socket.
async fn wait_for_daemon_exit(sock: &Path) {
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if devme_client::Client::connect(sock).await.is_err() {
            return;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use devme_core::{Envelope, ServerMessage};
    use devme_ipc::FrameCodec;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::{UnixListener, UnixStream};
    use tokio_util::codec::Framed;

    // Minimal stand-in for a *responsive* daemon: accepts one connection
    // and replies to every framed message with a Notice. Mirrors how the
    // shared daemon answers `RecheckHealth` via its catch-all arm.
    async fn responsive_server(listener: UnixListener) {
        if let Ok((stream, _)) = listener.accept().await {
            let mut framed = Framed::new(stream, FrameCodec);
            while let Some(Ok(_)) = framed.next().await {
                let reply = ServerMessage::Notice {
                    level: devme_core::NoticeLevel::Warn,
                    message: "ok".into(),
                };
                let bytes = serde_json::to_vec(&Envelope::new(reply)).unwrap();
                if framed.send(bytes.as_slice()).await.is_err() {
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn responsive_when_daemon_replies() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let _server = tokio::spawn(responsive_server(listener));

        assert!(daemon_responsive(&sock).await);
    }

    #[tokio::test]
    async fn not_responsive_when_nothing_listens() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("missing.sock");
        // No listener bound — connect is refused.
        assert!(!daemon_responsive(&sock).await);
    }

    #[tokio::test]
    async fn not_responsive_when_bound_but_not_accepting() {
        // The core race: a daemon mid-shutdown keeps its socket bound (so
        // `connect` succeeds) but its accept loop is parked, so no reply
        // ever comes. A bare connect check would wrongly call this "alive".
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dying.sock");
        let _listener = UnixListener::bind(&sock).unwrap(); // bound, never accepts

        // A plain connect succeeds against the parked listener...
        assert!(UnixStream::connect(&sock).await.is_ok());
        // ...but the round-trip probe correctly reports not-responsive.
        assert!(!daemon_responsive(&sock).await);
    }

    #[tokio::test]
    async fn wait_for_exit_returns_once_listener_drops() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("exiting.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Drop the listener and unlink shortly after we start waiting,
        // emulating a daemon finishing teardown and releasing its socket.
        let sock_clone = sock.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            drop(listener);
            let _ = std::fs::remove_file(&sock_clone);
        });

        // Should return well within the ~10s bound once connect starts failing.
        tokio::time::timeout(Duration::from_secs(3), wait_for_daemon_exit(&sock))
            .await
            .expect("wait_for_daemon_exit should return after the listener drops");
    }
}
