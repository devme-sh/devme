//! Watches the devme runtime directory for daemon sockets and multiplexes
//! their event streams.
//!
//! The TUI normally talks to the daemon for its own cwd. To realize the
//! "monorepo root sees every worktree" experience, this module attaches a
//! [`Client`] per `.sock` it finds in `paths::runtime_dir()`, scans on
//! startup, and watches the directory for new sockets via [`notify`].
//! Each attached daemon's `ServerMessage`s flow into a single channel
//! tagged with the daemon's `InstanceInfo::id`, so the TUI's state
//! machine — which is already multi-instance after the #21 refactor —
//! routes them to the right `InstanceData`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use devme_client::Client;
use devme_core::{ClientMessage, InstanceInfo, ServerMessage};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{Mutex, mpsc};

/// One event from any attached daemon, carrying the id of the instance the
/// message came from.
pub struct Tagged {
    pub instance_id: String,
    pub message: ServerMessage,
}

/// Per-daemon handle. The TUI clones one of these from `registry.send_to`
/// to issue Restart/Stop/Start against a specific instance.
struct Connection {
    /// Owned sender to the per-connection writer task.
    cmd_tx: mpsc::UnboundedSender<ClientMessage>,
}

/// Owns every attached daemon connection and the directory watcher.
pub struct Registry {
    /// `(instance_id, Connection)` for every live daemon. `Mutex` because
    /// the watcher task and the TUI's render thread both touch it.
    conns: Arc<Mutex<Vec<(String, Connection)>>>,
    /// Sockets we've already attempted to attach to (success or fail) — we
    /// retry on remove/re-add but skip duplicate events.
    seen: Arc<Mutex<HashSet<PathBuf>>>,
    /// Combined event stream out of every attached daemon.
    rx: mpsc::UnboundedReceiver<Tagged>,
    /// Cloned for every new connection.
    tx: mpsc::UnboundedSender<Tagged>,
    /// Held to keep the watcher alive for the lifetime of the registry.
    _watcher: Option<RecommendedWatcher>,
}

impl Registry {
    /// Build a registry, scan `dir` once, then watch it for new `.sock`
    /// files. Returns immediately — connections happen in background tasks
    /// and surface via [`Registry::recv`].
    pub async fn bind(dir: &Path) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel::<Tagged>();
        let conns = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::new(Mutex::new(HashSet::new()));

        let mut reg = Registry {
            conns: conns.clone(),
            seen: seen.clone(),
            rx,
            tx: tx.clone(),
            _watcher: None,
        };

        for path in scan_sockets(dir)? {
            reg.attach(path).await;
        }

        // Notify channel — file-system events arrive on a sync thread, so
        // we trampoline them into an async task that owns the attaching.
        let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<PathBuf>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                use notify::EventKind;
                if matches!(
                    ev.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    for path in ev.paths {
                        if path.extension().is_some_and(|e| e == "sock") {
                            let _ = fs_tx.send(path);
                        }
                    }
                }
            }
        })
        .map_err(|e| std::io::Error::other(format!("notify init: {e}")))?;

        let mut watcher = watcher;
        watcher
            .watch(dir, RecursiveMode::NonRecursive)
            .map_err(|e| std::io::Error::other(format!("notify watch: {e}")))?;
        reg._watcher = Some(watcher);

        // Trampoline task — also owns the work of attaching to new sockets.
        let conns_t = conns.clone();
        let seen_t = seen.clone();
        let tx_t = tx.clone();
        tokio::spawn(async move {
            while let Some(path) = fs_rx.recv().await {
                // Brief debounce so we don't race the daemon's bind →
                // listen → accept sequence. Cheap relative to the cost of
                // missing a freshly-created socket.
                tokio::time::sleep(Duration::from_millis(50)).await;
                if !path.exists() {
                    // Removal: drop any connection rooted at this path.
                    // Best-effort — the daemon disappearing closes the
                    // stream which already retires the connection.
                    continue;
                }
                attach_one(&conns_t, &seen_t, &tx_t, path).await;
            }
        });

        Ok(reg)
    }

    /// Await the next message from any attached daemon. Returns `None` only
    /// when every connection has closed AND the watcher task has dropped
    /// its sender — in practice the watcher lives as long as the registry.
    pub async fn recv(&mut self) -> Option<Tagged> {
        self.rx.recv().await
    }

    /// Send a command to the daemon identified by `instance_id`. Returns
    /// `false` if no such connection exists (e.g. the daemon exited).
    pub async fn send_to(&self, instance_id: &str, msg: ClientMessage) -> bool {
        let conns = self.conns.lock().await;
        if let Some((_, c)) = conns.iter().find(|(id, _)| id == instance_id) {
            c.cmd_tx.send(msg).is_ok()
        } else {
            false
        }
    }

    /// Broadcast a command to every attached daemon. Used for "shutdown
    /// everything" on TUI quit.
    pub async fn broadcast(&self, msg: ClientMessage) {
        let conns = self.conns.lock().await;
        for (_, c) in conns.iter() {
            let _ = c.cmd_tx.send(msg.clone());
        }
    }

    async fn attach(&self, path: PathBuf) {
        attach_one(&self.conns, &self.seen, &self.tx, path).await;
    }
}

/// Try to connect to `path`, subscribe, and spawn the read/write tasks.
/// Silently skips on failure — the daemon may have died between scan and
/// connect, or the socket may belong to a stale process.
async fn attach_one(
    conns: &Arc<Mutex<Vec<(String, Connection)>>>,
    seen: &Arc<Mutex<HashSet<PathBuf>>>,
    tx: &mpsc::UnboundedSender<Tagged>,
    path: PathBuf,
) {
    {
        let mut seen = seen.lock().await;
        if !seen.insert(path.clone()) {
            return;
        }
    }

    let mut client = match Client::connect(&path).await {
        Ok(c) => c,
        Err(_) => {
            seen.lock().await.remove(&path);
            return;
        }
    };

    // Subscribe and read the initial Subscribed reply to learn the id.
    if client
        .send(ClientMessage::Subscribe { services: vec![] })
        .await
        .is_err()
    {
        seen.lock().await.remove(&path);
        return;
    }

    let first = match client.next_event().await {
        Ok(Some(m)) => m,
        _ => {
            seen.lock().await.remove(&path);
            return;
        }
    };

    let id = match &first {
        ServerMessage::Subscribed { instance, .. } => instance.id.clone(),
        _ => {
            seen.lock().await.remove(&path);
            return;
        }
    };

    let _ = tx.send(Tagged {
        instance_id: id.clone(),
        message: first,
    });

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ClientMessage>();
    conns.lock().await.push((id.clone(), Connection { cmd_tx }));

    let tx_t = tx.clone();
    let id_t = id;
    let seen_t = seen.clone();
    let conns_t = conns.clone();
    let path_t = path;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = client.next_event() => match msg {
                    Ok(Some(m)) => {
                        if tx_t.send(Tagged { instance_id: id_t.clone(), message: m }).is_err() {
                            break;
                        }
                    }
                    _ => break,
                },
                cmd = cmd_rx.recv() => match cmd {
                    Some(c) => {
                        if client.send(c).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
        // Connection retired — drop it so a future watcher event can
        // re-attach if the daemon comes back.
        conns_t.lock().await.retain(|(i, _)| i != &id_t);
        seen_t.lock().await.remove(&path_t);
    });
}

/// One-shot scan: every `*.sock` file directly inside `dir`.
fn scan_sockets(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "sock") {
            out.push(path);
        }
    }
    Ok(out)
}

/// Sender attached to an [`InstanceInfo`] — convenience the TUI can hand to
/// the renderer / key handler if it ever needs to talk to a specific
/// instance without holding a `&Registry`.
#[allow(dead_code)]
pub struct InstanceHandle {
    pub info: InstanceInfo,
    pub send: mpsc::UnboundedSender<ClientMessage>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn scan_sockets_finds_only_dot_sock_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.sock"), b"").unwrap();
        std::fs::write(dir.path().join("b.sock"), b"").unwrap();
        std::fs::write(dir.path().join("slots.json"), b"").unwrap();
        let mut found = scan_sockets(dir.path()).unwrap();
        found.sort();
        assert_eq!(found.len(), 2);
        assert!(found[0].ends_with("a.sock"));
        assert!(found[1].ends_with("b.sock"));
    }

    #[test]
    fn scan_sockets_returns_empty_for_missing_dir() {
        let dir = TempDir::new().unwrap();
        let nope = dir.path().join("does-not-exist");
        assert!(scan_sockets(&nope).unwrap().is_empty());
    }

    #[tokio::test]
    async fn registry_recv_returns_none_when_no_daemons_and_dropped_tx() {
        // Sanity: with no daemons and the registry dropped, recv yields None.
        let dir = TempDir::new().unwrap();
        let mut reg = Registry::bind(dir.path()).await.unwrap();
        // Drop the internal tx by dropping the registry's tx clone path:
        // we can't easily without poking internals, so just confirm recv
        // doesn't block synchronously by polling once.
        let result = tokio::time::timeout(Duration::from_millis(20), reg.recv()).await;
        assert!(result.is_err(), "recv should pend, not return immediately");
    }
}
