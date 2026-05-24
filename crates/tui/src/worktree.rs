//! Worktree autodiscovery — the supply side of the multi-stack TUI.
//!
//! [`discovery::Registry`](crate::discovery::Registry) is the *consume*
//! side: it attaches Clients to whatever daemons are bound. But a fresh
//! `git worktree add` doesn't bind anything by itself, so without this
//! module the new worktree stays invisible until the user runs `devme`
//! inside it.
//!
//! This module:
//! 1. Enumerates every worktree of the current repo (`git worktree list
//!    --porcelain`).
//! 2. For each one that has a `devme.toml`, ensures a `devme-supervisor`
//!    is running in that cwd. The resulting socket bind is what
//!    `Registry`'s watcher picks up — we never need to feed Clients in
//!    directly.
//! 3. Watches `<git-common-dir>/worktrees/` for additions and re-runs
//!    step 2 when a new worktree appears.
//!
//! Failure mode: per-worktree errors are logged and skipped. One broken
//! worktree must not stop the others.

use std::path::{Path, PathBuf};
use std::time::Duration;

use devme_supervisor::spawn::ensure_daemon;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// One worktree of the current repo, as reported by git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Canonical path to the worktree root.
    pub path: PathBuf,
}

/// Live state for worktree autodiscovery. Holding it keeps the
/// `git-common-dir/worktrees/` watcher alive.
pub struct AutoSpawner {
    _watcher: Option<RecommendedWatcher>,
}

impl AutoSpawner {
    /// Enumerate worktrees of the repo containing `cwd`, ensure each one
    /// with a `devme.toml` has a running supervisor, then watch for
    /// additions and ensure those too.
    ///
    /// Returns immediately — initial ensure runs happen in a background
    /// task so the TUI can start drawing without blocking on 5s daemon
    /// startup timeouts.
    pub async fn bind(cwd: &Path) -> anyhow::Result<Self> {
        let common = git_common_dir(cwd);

        // Kick off the initial scan in the background.
        let cwd_t = cwd.to_path_buf();
        tokio::spawn(async move {
            for wt in list_worktrees(&cwd_t) {
                ensure_for(&wt.path).await;
            }
        });

        // If we're not inside a git repo, there's nothing to watch. The
        // initial scan above will still have run via `list_worktrees`
        // (which falls back to "just this cwd" for non-git setups).
        let Some(common) = common else {
            return Ok(Self { _watcher: None });
        };

        let worktrees_dir = common.join("worktrees");
        // The worktrees subdir only exists once `git worktree add` is run
        // for the first time — create it so the watcher has something to
        // attach to. Empty is fine; new additions still fire events.
        let _ = std::fs::create_dir_all(&worktrees_dir);

        // Notify -> mpsc trampoline (same pattern as discovery.rs).
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

        // On each filesystem event, re-enumerate and ensure. Debounce so
        // a single `git worktree add` (which touches several files) only
        // triggers one scan.
        let cwd_w = cwd.to_path_buf();
        tokio::spawn(async move {
            while fs_rx.recv().await.is_some() {
                // Drain extra events that fired during the debounce.
                while tokio::time::timeout(Duration::from_millis(200), fs_rx.recv())
                    .await
                    .is_ok()
                {}
                for wt in list_worktrees(&cwd_w) {
                    ensure_for(&wt.path).await;
                }
            }
        });

        Ok(Self { _watcher: Some(watcher) })
    }
}

/// Spawn a supervisor for `path` if it has a `devme.toml` and no daemon
/// is bound there yet. Errors are logged via `eprintln!` because the TUI
/// owns the terminal and we don't want to crash it for a broken
/// worktree.
async fn ensure_for(path: &Path) {
    if !path.join("devme.toml").exists() {
        return;
    }
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
        // /tmp short path to avoid macOS SUN_LEN issues if anyone later
        // adds a socket assertion here.
        let id = std::process::id();
        let root = std::path::PathBuf::from(format!("/tmp/devme-wt-test-{id}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();

        if !run_git(&main, &["init", "-q"]) {
            // No git — skip silently.
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        // git needs an initial commit before `worktree add` works.
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
        // Canonical forms — both should appear.
        let main_canon = std::fs::canonicalize(&main).unwrap();
        let linked_canon = std::fs::canonicalize(&linked).unwrap();
        assert!(paths.contains(&main_canon.as_path()), "missing main: {paths:?}");
        assert!(paths.contains(&linked_canon.as_path()), "missing linked: {paths:?}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
