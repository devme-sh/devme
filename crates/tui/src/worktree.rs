//! Worktree autodiscovery — the supply side of the multi-stack TUI.
//!
//! [`discovery::Registry`](crate::discovery::Registry) attaches Clients to
//! whatever daemons are bound. But a fresh `git worktree add` doesn't
//! bind anything by itself, so without this module the new worktree
//! stays invisible until the user runs `devme` inside it.
//!
//! This module:
//! 1. Enumerates every worktree of the current repo (`git worktree
//!    list --porcelain`).
//! 2. Emits a [`WorktreeEvent::Discovered`] for each one so the TUI can
//!    add a placeholder row to its sidebar — even worktrees with no
//!    `devme.toml` show up, marked as "no config".
//! 3. For any worktree that does have a `devme.toml`, ensures a
//!    `devme-supervisor` is running in that cwd. The resulting socket
//!    bind is what `Registry`'s watcher picks up; we never feed Clients
//!    in directly.
//! 4. Watches `<git-common-dir>/worktrees/` for additions and each
//!    worktree's root for `devme.toml` appearing — both trigger a
//!    re-scan so a worktree's status flips from placeholder to running
//!    the moment its config lands.

use std::path::{Path, PathBuf};
use std::time::Duration;

use devme_supervisor::spawn::{ensure_daemon, ensure_shared_daemon};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// One worktree of the current repo, as reported by git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Canonical path to the worktree root.
    pub path: PathBuf,
}

/// Events from [`AutoSpawner`] to the TUI's main loop.
#[derive(Debug, Clone)]
pub enum WorktreeEvent {
    /// A worktree of the current repo exists. The TUI uses this to add a
    /// placeholder sidebar row. Fired once per worktree on initial scan
    /// and again whenever a new worktree is added at runtime; duplicates
    /// are filtered by the TUI via the stable `id`.
    Discovered {
        /// Stable per-worktree id (`paths::instance_id(path)`). Matches
        /// the id the future supervisor will advertise, so the placeholder
        /// row is upserted in place when the daemon binds.
        id: String,
        /// Human-friendly label — the worktree basename.
        label: String,
        /// Canonical worktree path.
        cwd: String,
    },
}

/// Live state for worktree autodiscovery. Holding it keeps the watchers
/// alive.
pub struct AutoSpawner {
    _watchers: Vec<RecommendedWatcher>,
}

impl AutoSpawner {
    /// Enumerate worktrees of the repo containing `cwd`, emit a
    /// `Discovered` event per worktree (the TUI uses these to populate
    /// sidebar placeholders), and ensure a supervisor is running for
    /// every worktree that has a `devme.toml`. Then watch:
    ///
    /// - `<git-common-dir>/worktrees/` for new worktrees;
    /// - each worktree's root for `devme.toml` appearing.
    pub async fn bind(
        cwd: &Path,
        events: mpsc::UnboundedSender<WorktreeEvent>,
    ) -> anyhow::Result<Self> {
        let common = git_common_dir(cwd);
        let mut watchers: Vec<RecommendedWatcher> = Vec::new();

        // Initial pass: emit events and try to spawn for each worktree.
        let initial = list_worktrees(cwd);
        for wt in &initial {
            emit_discovered(&events, &wt.path);
            // Add a watch on the worktree root so a future devme.toml
            // creation triggers a re-scan.
            if let Some(w) = watch_worktree_root(&wt.path, events.clone()) {
                watchers.push(w);
            }
        }
        // Sort: home worktree (cwd) first so it claims slot 0, then
        // remaining worktrees sorted by path for deterministic port assignment.
        let canon_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let mut initial_paths: Vec<PathBuf> = initial.iter().map(|w| w.path.clone()).collect();
        initial_paths.sort_by(|a, b| {
            let a_home = a == &canon_cwd;
            let b_home = b == &canon_cwd;
            match (a_home, b_home) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.cmp(b),
            }
        });

        // Sweep stale slot claims so the first worktree gets slot 0.
        if let Ok(registry_path) = devme_config::paths::slot_registry() {
            let allocator = devme_slot_allocator::SlotAllocator::open(&registry_path);
            let _ = allocator.list(); // triggers sweep_stale
        }

        let cwd_for_shared = cwd.to_path_buf();
        tokio::spawn(async move {
            if let Err(e) = ensure_shared_daemon(&cwd_for_shared).await {
                tracing::debug!(error = %e, "shared supervisor not started (may have no repo-scoped services)");
            }
            for p in initial_paths {
                ensure_for(&p).await;
            }
        });

        // If we're not inside a git repo, there are no sibling worktrees
        // to watch for; just hold the per-root watcher(s) and return.
        let Some(common) = common else {
            return Ok(Self { _watchers: watchers });
        };

        let worktrees_dir = common.join("worktrees");
        let _ = std::fs::create_dir_all(&worktrees_dir);

        let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<()>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                use notify::EventKind;
                if matches!(ev.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                    let _ = fs_tx.send(());
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("notify init: {e}"))?;
        let mut watcher = watcher;
        watcher
            .watch(&worktrees_dir, RecursiveMode::Recursive)
            .map_err(|e| anyhow::anyhow!("watch {}: {e}", worktrees_dir.display()))?;
        watchers.push(watcher);

        // Re-scan on each event. Debounce so a single `git worktree add`
        // (which writes several files) doesn't fan out into many scans.
        let cwd_w = cwd.to_path_buf();
        let events_w = events.clone();
        tokio::spawn(async move {
            // Each fresh worktree needs its own root watcher so that
            // `devme.toml` later appearing fires a re-scan. Kept alive
            // by being moved into the task.
            let mut per_worktree_watchers: Vec<RecommendedWatcher> = Vec::new();
            let mut known_paths: Vec<PathBuf> = Vec::new();
            while fs_rx.recv().await.is_some() {
                while tokio::time::timeout(Duration::from_millis(200), fs_rx.recv())
                    .await
                    .is_ok()
                {}
                for wt in list_worktrees(&cwd_w) {
                    if !known_paths.contains(&wt.path) {
                        known_paths.push(wt.path.clone());
                        emit_discovered(&events_w, &wt.path);
                        if let Some(w) = watch_worktree_root(&wt.path, events_w.clone()) {
                            per_worktree_watchers.push(w);
                        }
                    }
                    ensure_for(&wt.path).await;
                }
            }
        });

        Ok(Self { _watchers: watchers })
    }
}

/// Send a Discovered event for a worktree. Cheap to call repeatedly —
/// the TUI dedupes by id.
fn emit_discovered(tx: &mpsc::UnboundedSender<WorktreeEvent>, path: &Path) {
    let id = devme_config::paths::instance_id(path);
    let label = git_branch_name(path).unwrap_or_else(|| {
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("worktree")
            .to_string()
    });
    let cwd = path.display().to_string();
    let _ = tx.send(WorktreeEvent::Discovered { id, label, cwd });
}

fn git_branch_name(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8(out.stdout).ok()?;
    let trimmed = branch.trim();
    if trimmed.is_empty() || trimmed == "HEAD" {
        return None;
    }
    Some(trimmed.to_string())
}

/// Watch a single worktree's root non-recursively. When `devme.toml`
/// appears, re-run `ensure_for(path)` — that's the moment a
/// previously-empty worktree becomes runnable.
///
/// The notify callback fires on a non-tokio thread, so we capture the
/// current runtime handle and use it to spawn the async work; calling
/// `tokio::spawn` directly would panic with "no current runtime".
fn watch_worktree_root(
    path: &Path,
    events_tx: mpsc::UnboundedSender<WorktreeEvent>,
) -> Option<RecommendedWatcher> {
    let watch_path = path.to_path_buf();
    let path_for_handler = watch_path.clone();
    let handle = tokio::runtime::Handle::current();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            use notify::EventKind;
            if !matches!(
                ev.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            // Only react to events involving devme.toml — keeps us out of
            // a hot loop when a chatty source file in the worktree is
            // being edited.
            let mentions_devme_toml = ev
                .paths
                .iter()
                .any(|p| p.file_name().is_some_and(|n| n == "devme.toml"));
            if !mentions_devme_toml {
                return;
            }
            let _ = events_tx.send(WorktreeEvent::Discovered {
                id: devme_config::paths::instance_id(&path_for_handler),
                label: path_for_handler
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("worktree")
                    .to_string(),
                cwd: path_for_handler.display().to_string(),
            });
            let path_for_spawn = path_for_handler.clone();
            handle.spawn(async move {
                ensure_for(&path_for_spawn).await;
            });
        }
    })
    .ok()?;
    let mut watcher = watcher;
    watcher.watch(&watch_path, RecursiveMode::NonRecursive).ok()?;
    Some(watcher)
}

/// Spawn a supervisor for `path` if it has a `devme.toml` and no daemon
/// is bound there yet. Errors are logged via `tracing` because the TUI
/// owns the terminal and we don't want to crash it for a broken
/// worktree.
async fn ensure_for(path: &Path) {
    if !path.join("devme.toml").exists() {
        return;
    }
    run_on_create_if_needed(path);
    ensure_docker_for(path);
    let Ok(sock) = devme_config::paths::supervisor_socket(path) else {
        return;
    };
    if let Err(e) = ensure_daemon(&sock, path).await {
        tracing::warn!(
            worktree = %path.display(),
            error = %e,
            "auto-spawn supervisor failed; worktree will not appear until manually started"
        );
    }
}

/// If the stack at `path` needs Docker and Docker isn't running, start the
/// configured daemon. No prompting — the TUI doesn't own a terminal for
/// interactive input; the user sets `docker.daemon` via `devme config set`
/// or by running `devme up` once (which prompts).
fn ensure_docker_for(path: &Path) {
    use devme_config::{GlobalConfig, Stack, docker};

    let Ok(toml_str) = std::fs::read_to_string(path.join("devme.toml")) else {
        return;
    };
    let Ok(stack) = Stack::parse(&toml_str) else {
        return;
    };
    if !docker::stack_needs_docker(&stack) || docker::is_docker_running() {
        return;
    }
    let cfg = GlobalConfig::load();
    let Some(daemon_id) = &cfg.docker.daemon else {
        tracing::warn!("services need Docker but no daemon configured — run: devme config set docker.daemon <name>");
        return;
    };
    tracing::info!(daemon = %daemon_id, "starting Docker");
    if let Err(e) = docker::start_daemon(daemon_id) {
        tracing::warn!(error = %e, "failed to start Docker daemon");
    }
}

/// Run the `[stack] on_create` script if this worktree hasn't been
/// initialized yet. A `.devme-initialized` marker file prevents re-running.
fn run_on_create_if_needed(path: &Path) {
    use devme_config::Stack;

    let marker = path.join(".devme-initialized");
    if marker.exists() {
        return;
    }
    let Ok(toml_str) = std::fs::read_to_string(path.join("devme.toml")) else {
        return;
    };
    let Ok(stack) = Stack::parse(&toml_str) else {
        return;
    };
    let cmd = match stack.stack.as_ref().and_then(|s| s.on_create.as_ref()) {
        Some(c) => c.clone(),
        None => {
            let _ = std::fs::write(&marker, "");
            return;
        }
    };
    tracing::info!(worktree = %path.display(), "running on_create script");
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {
            let _ = std::fs::write(&marker, "");
            tracing::info!(worktree = %path.display(), "on_create completed");
        }
        Ok(s) => {
            tracing::warn!(worktree = %path.display(), exit = ?s.code(), "on_create failed");
        }
        Err(e) => {
            tracing::warn!(worktree = %path.display(), error = %e, "on_create spawn failed");
        }
    }
}

/// `git rev-parse --git-common-dir` for `cwd`, canonicalized. None if
/// `cwd` is not inside a git repo.
fn git_common_dir(cwd: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let p = PathBuf::from(trimmed);
    let abs = if p.is_absolute() { p } else { cwd.join(p) };
    std::fs::canonicalize(&abs).ok()
}

/// Every worktree of the repo containing `cwd`. Returns just `cwd`
/// itself if not inside a git repo, so non-git devme setups still get
/// their daemon ensured.
fn list_worktrees(cwd: &Path) -> Vec<Worktree> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["worktree", "list", "--porcelain"])
        .output();
    let Ok(out) = out else {
        return vec![Worktree { path: cwd.to_path_buf() }];
    };
    if !out.status.success() {
        return vec![Worktree { path: cwd.to_path_buf() }];
    }
    let mut worktrees = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            let path = PathBuf::from(p);
            if let Ok(canon) = std::fs::canonicalize(&path) {
                worktrees.push(Worktree { path: canon });
            } else {
                worktrees.push(Worktree { path });
            }
        }
    }
    if worktrees.is_empty() {
        worktrees.push(Worktree { path: cwd.to_path_buf() });
    }
    worktrees
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn list_worktrees_returns_cwd_for_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let wts = list_worktrees(dir.path());
        assert_eq!(wts.len(), 1);
        assert_eq!(wts[0].path, dir.path());
    }

    #[test]
    fn list_worktrees_finds_main_and_linked_worktrees() {
        let id = std::process::id();
        let root = std::path::PathBuf::from(format!("/tmp/devme-wt-test-{id}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();

        if !run_git(&main, &["init", "-q"]) {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["config", "user.email", "test@example.com"])
            .status();
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["config", "user.name", "test"])
            .status();
        std::fs::write(main.join("a.txt"), b"hi").unwrap();
        run_git(&main, &["add", "a.txt"]);
        run_git(&main, &["commit", "-qm", "init"]);

        let linked = root.join("linked");
        run_git(&main, &["worktree", "add", linked.to_str().unwrap()]);

        let wts = list_worktrees(&main);
        let paths: Vec<&Path> = wts.iter().map(|w| w.path.as_path()).collect();
        let main_canon = std::fs::canonicalize(&main).unwrap();
        let linked_canon = std::fs::canonicalize(&linked).unwrap();
        assert!(paths.contains(&main_canon.as_path()), "missing main: {paths:?}");
        assert!(paths.contains(&linked_canon.as_path()), "missing linked: {paths:?}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
