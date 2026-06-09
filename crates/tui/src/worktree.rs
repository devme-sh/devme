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

use devme_client::Client;
use devme_config::{InterpContext, Stack, interpolate};
use devme_core::{ClientMessage, ServerMessage, ServiceSnapshot};
use devme_slot_allocator::SlotAllocator;
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

/// The worktree's current branch (`git rev-parse --abbrev-ref HEAD`), or
/// `None` on a detached HEAD, a non-repo dir, or git failure. The async
/// sibling of the sync [`git_branch_name`] above: the TUI's periodic refresh
/// calls this off the render thread so checking out a different branch
/// re-labels the sidebar row in place.
pub async fn git_branch(cwd: &str) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
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

/// Commits the worktree's branch is ahead/behind its upstream, as
/// `(ahead, behind)`. `None` when there's no upstream, the dir isn't a git
/// repo, or git fails. Run off the render thread (it shells out to git).
pub async fn git_ahead_behind(cwd: &str) -> Option<(usize, usize)> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-list", "--left-right", "--count", "@{upstream}...HEAD"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // `--left-right` over `@{u}...HEAD` prints "<behind>\t<ahead>".
    let text = String::from_utf8(out.stdout).ok()?;
    let mut parts = text.split_whitespace();
    let behind: usize = parts.next()?.parse().ok()?;
    let ahead: usize = parts.next()?.parse().ok()?;
    Some((ahead, behind))
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

/// Run the `[stack] on_create` script once per worktree, if one is defined.
/// A `.devme-initialized` marker prevents re-running it.
///
/// Most stacks need no `on_create` at all: per-worktree setup (deps, DB
/// clone, migrations) is better expressed as `[step]` check/provision, which
/// is idempotent and reality-based (re-runs if you delete the artifact) — a
/// marker only records that a command *ran*, not that its result still
/// exists. So when there's no `on_create`, we write no marker: nothing to
/// gate, and no stray `.devme-initialized` litters the worktree.
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
        // No on_create → nothing to run once, so don't drop a marker.
        None => return,
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

/// Run an already-interpolated `[stack] on_destroy` command — the low-level
/// execution primitive, symmetric to [`run_on_create_if_needed`]. Intended
/// for dropping per-worktree resources (e.g. a cloned database).
///
/// `cmd` must already be interpolated by the caller: by the time a worktree
/// is removed its directory — and the slot/`{branch}` context the command
/// needs — is gone, so resolution happens *before* removal. `run_dir` is
/// where the command executes; the worktree path no longer exists, so
/// callers pass a still-present directory (the repo's main worktree).
///
/// [`remove_worktree`] is the orchestrator that resolves the command, tears
/// the worktree down, and calls this. The `devme worktree rm` subcommand is
/// the user-facing entry point; a bare `git worktree remove` bypasses devme
/// and runs no hook.
pub fn run_on_destroy(cmd: &str, run_dir: &Path) -> std::io::Result<std::process::ExitStatus> {
    tracing::info!(run_dir = %run_dir.display(), "running on_destroy script");
    std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(run_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
}

/// What [`remove_worktree`] did — for the CLI to report (human or `--json`).
#[derive(Debug, Clone)]
pub struct RemovalReport {
    /// Canonical path of the removed worktree.
    pub path: PathBuf,
    /// Branch the worktree was on, if any.
    pub branch: Option<String>,
    /// Slot it held at removal time (from the allocator registry), if any.
    pub slot: Option<u8>,
    /// Whether an instance supervisor was found and shut down.
    pub instance_stopped: bool,
    /// `None` if the stack declared no `on_destroy`; `Some(true)` if the hook
    /// ran and exited 0; `Some(false)` if it ran and failed.
    pub on_destroy_ran: Option<bool>,
}

/// One worktree as reported by `git worktree list --porcelain`.
#[derive(Debug, Clone)]
struct WorktreeMeta {
    path: PathBuf,
    branch: Option<String>,
    /// True for the repo's main worktree (the first porcelain entry).
    is_main: bool,
}

/// Tear down and remove a worktree, firing its `[stack] on_destroy` hook —
/// the deterministic counterpart to [`run_on_create_if_needed`].
///
/// Because the command and its `{slot}`/`{branch}` context vanish with the
/// worktree, the order matters: resolve everything *first* (while the
/// directory and slot claim still exist), then stop the instance, then
/// `git worktree remove`, and only run `on_destroy` once removal succeeds —
/// so a failed/aborted removal never drops a database out from under a
/// worktree that's still on disk.
///
/// `cwd` anchors the `git worktree list` lookup. `target` matches by
/// absolute/relative path, directory name, or branch name.
pub async fn remove_worktree(
    cwd: &Path,
    target: &str,
    force: bool,
) -> anyhow::Result<RemovalReport> {
    let worktrees = list_worktrees_detailed(cwd);
    let resolved = resolve_target(&worktrees, target)?;
    if resolved.is_main {
        anyhow::bail!(
            "refusing to remove the main worktree ({})",
            resolved.path.display()
        );
    }
    let path = resolved.path;
    let branch = resolved.branch;

    // 1. Resolve on_destroy + slot WHILE the worktree (devme.toml + slot
    //    claim) still exists. Interpolation failure here aborts before
    //    anything is torn down.
    let on_destroy = read_on_destroy(&path);
    let slot = slot_for(&path);
    let resolved_cmd = match &on_destroy {
        Some(cmd) => Some(interpolate_on_destroy(cmd, &path, slot, branch.as_deref())?),
        None => None,
    };

    // 2. Stop this worktree's instance stack (best-effort). Shared (repo)
    //    services keep running — other worktrees may depend on them, and the
    //    hook (e.g. `dropdb`) usually targets one of them.
    let instance_stopped = stop_instance(&path).await;

    // 3. Remove the worktree. Pick a run dir for the hook that survives the
    //    removal (the main worktree), before the dir is gone.
    let run_dir = main_root(&worktrees).unwrap_or_else(|| cwd.to_path_buf());
    git_worktree_remove(cwd, &path, force)?;

    // Release the slot claim now the worktree is gone (best-effort; the
    // daemon usually released it on shutdown, and stale claims get swept).
    if let Ok(registry) = devme_config::paths::slot_registry() {
        let _ = SlotAllocator::open(&registry).release(&devme_config::paths::instance_id(&path));
    }

    // 4. Removal succeeded — fire the hook.
    let on_destroy_ran = resolved_cmd.map(|cmd| {
        run_on_destroy(&cmd, &run_dir)
            .map(|s| s.success())
            .unwrap_or(false)
    });

    Ok(RemovalReport {
        path,
        branch,
        slot,
        instance_stopped,
        on_destroy_ran,
    })
}

/// Report from `devme worktree add`.
#[derive(Debug, Clone)]
pub struct AddReport {
    /// Canonical path of the new worktree.
    pub path: PathBuf,
    /// Branch checked out (or created) in it.
    pub branch: String,
    /// True when the branch didn't exist and was created (`-b`).
    pub created_branch: bool,
    /// Whether an `[stack] on_create` hook was present (and thus run).
    pub on_create_ran: bool,
}

/// Create a git worktree for `branch` — creating the branch if it doesn't
/// exist — then fire the repo's `[stack] on_create` hook in it. The
/// deterministic counterpart to [`remove_worktree`]: an agent calls this to
/// get a worktree whose per-worktree setup (db clone, deps) has already run.
///
/// `dest` overrides the default path, which is a sibling of the main
/// worktree named `<main-basename>-<branch-leaf>` (e.g. main `…/devme` +
/// branch `feat/x` → `…/devme-x`).
pub fn add_worktree(cwd: &Path, branch: &str, dest: Option<&str>) -> anyhow::Result<AddReport> {
    let worktrees = list_worktrees_detailed(cwd);
    let main = main_root(&worktrees).unwrap_or_else(|| cwd.to_path_buf());

    let path = match dest {
        Some(d) => {
            let p = PathBuf::from(d);
            if p.is_absolute() { p } else { cwd.join(p) }
        }
        None => {
            let leaf = branch.rsplit('/').next().unwrap_or(branch);
            let base = main.file_name().and_then(|n| n.to_str()).unwrap_or("worktree");
            main.parent().unwrap_or(&main).join(format!("{base}-{leaf}"))
        }
    };

    if path.exists() {
        anyhow::bail!("destination already exists: {}", path.display());
    }

    let created_branch = !branch_exists(cwd, branch);

    // `git worktree add -b <new> <path>` creates the branch; `git worktree
    // add <path> <existing>` checks one out.
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(cwd).args(["worktree", "add"]);
    if created_branch {
        cmd.arg("-b").arg(branch).arg(&path);
    } else {
        cmd.arg(&path).arg(branch);
    }
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("running git worktree add: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = if stderr.trim().is_empty() { "(no stderr)" } else { stderr.trim() };
        anyhow::bail!("git worktree add failed: {detail}");
    }

    let canon = std::fs::canonicalize(&path).unwrap_or(path);
    // Fire on_create deterministically (symmetry with rm's on_destroy).
    let on_create_ran = stack_declares_on_create(&canon);
    run_on_create_if_needed(&canon);

    Ok(AddReport { path: canon, branch: branch.to_string(), created_branch, on_create_ran })
}

/// Does a local branch named `branch` already exist?
fn branch_exists(cwd: &Path, branch: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether the worktree's `devme.toml` declares an `[stack] on_create` hook.
fn stack_declares_on_create(path: &Path) -> bool {
    std::fs::read_to_string(path.join("devme.toml"))
        .ok()
        .and_then(|s| devme_config::Stack::parse(&s).ok())
        .and_then(|stack| stack.stack)
        .is_some_and(|meta| meta.on_create.is_some())
}

/// One worktree's status for the cross-worktree `devme status --all` view.
#[derive(Debug, Clone)]
pub struct WorktreeReport {
    /// Display label — branch name, else directory basename.
    pub label: String,
    /// Canonical worktree path.
    pub path: PathBuf,
    /// True for the worktree the command was run from.
    pub is_cwd: bool,
    /// Slot it currently holds (from the allocator registry), if any.
    pub slot: Option<u8>,
    /// Service snapshot from its daemon, or `None` if no daemon is running.
    pub services: Option<Vec<ServiceSnapshot>>,
}

/// Gather a status report for every worktree of the repo containing `cwd`.
/// Connects to each worktree's instance daemon (without spawning one) to read
/// its live services + resolved ports; worktrees with no running daemon come
/// back with `services: None`.
pub async fn gather_worktree_reports(cwd: &Path) -> Vec<WorktreeReport> {
    let canon_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let worktrees = list_worktrees_detailed(cwd);
    let mut reports = Vec::with_capacity(worktrees.len());
    for wt in worktrees {
        let label = wt.branch.clone().unwrap_or_else(|| {
            wt.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("worktree")
                .to_string()
        });
        let is_cwd = wt.path == canon_cwd;
        let slot = slot_for(&wt.path);
        let services = snapshot_instance(&wt.path).await;
        reports.push(WorktreeReport {
            label,
            path: wt.path,
            is_cwd,
            slot,
            services,
        });
    }
    reports
}

/// Shut down the repo-shared supervisor for `cwd`, but only when no *other*
/// worktree of the repo still has a live daemon — so a sibling's shared
/// Postgres isn't yanked out from under it. Returns `true` if a `Shutdown`
/// was sent. Best-effort; never errors.
///
/// Shared by `devme down` and the TUI's `q` so both have identical
/// sibling-safe teardown semantics.
pub async fn shutdown_shared_if_last(cwd: &Path) -> bool {
    let reports = gather_worktree_reports(cwd).await;
    let others_live = reports.iter().any(|r| !r.is_cwd && r.services.is_some());
    if others_live {
        return false;
    }
    if let Ok(shared_sock) = devme_config::paths::shared_socket(cwd)
        && let Ok(mut shared) = Client::connect(&shared_sock).await
    {
        let _ = shared.send(ClientMessage::Shutdown).await;
        return true;
    }
    false
}

/// Graceful "quit everything" used by the TUI's `q`: stop *every* worktree's
/// stack in the repo, then the repo-shared services. The TUI autospawns a
/// daemon for every worktree (see [`AutoSpawner::bind`]), so quitting it stops
/// every one it started rather than orphaning the siblings — the repo-wide
/// twin of `devme down --all`. Each `Shutdown` goes over a fresh awaited
/// connection (not the discovery channel, whose writer task drops on process
/// exit), so delivery is guaranteed; we don't drain the replies, keeping `q`
/// snappy (the daemons stop their services on their own). Best-effort:
/// worktrees with no running daemon are skipped.
pub async fn shutdown_all(cwd: &Path) {
    for wt in list_worktrees(cwd) {
        if let Ok(sock) = devme_config::paths::supervisor_socket(&wt.path)
            && let Ok(mut client) = Client::connect(&sock).await
        {
            let _ = client.send(ClientMessage::Shutdown).await;
        }
    }
    // Every instance daemon has been told to stop, so the shared services are
    // free to stop unconditionally (no sibling can still be relying on them).
    if let Ok(shared_sock) = devme_config::paths::shared_socket(cwd)
        && let Ok(mut shared) = Client::connect(&shared_sock).await
    {
        let _ = shared.send(ClientMessage::Shutdown).await;
    }
}

/// Re-spawn the repo-shared daemon and an instance daemon for every worktree
/// that has a `devme.toml` — the same pass [`AutoSpawner::bind`] runs at
/// startup. Used by the TUI's stopped-state `u` key to bring the stack back
/// after a `devme down` without leaving the dashboard: once the daemons bind,
/// the discovery [`Registry`](crate::discovery::Registry) reattaches and the
/// event loop sends each its `Start`, exactly like a fresh launch. Best-effort
/// — per-worktree failures are logged, never surfaced (the TUI owns the
/// terminal).
pub async fn start_all(cwd: &Path) {
    if let Err(e) = ensure_shared_daemon(cwd).await {
        tracing::debug!(error = %e, "shared supervisor not started (may have no repo-scoped services)");
    }
    for wt in list_worktrees(cwd) {
        ensure_for(&wt.path).await;
    }
}

/// One-shot snapshot of a worktree's instance daemon. `None` if no daemon is
/// listening (connect fails) — we never spawn one here.
async fn snapshot_instance(path: &Path) -> Option<Vec<ServiceSnapshot>> {
    let sock = devme_config::paths::supervisor_socket(path).ok()?;
    let mut client = Client::connect(&sock).await.ok()?;
    match client
        .request(ClientMessage::Subscribe { services: vec![] })
        .await
        .ok()?
    {
        ServerMessage::Subscribed { services, .. } => Some(services),
        _ => None,
    }
}

/// Parse `git worktree list --porcelain` into one entry per worktree. The
/// first entry is the main worktree. Empty if `cwd` isn't a git repo.
fn list_worktrees_detailed(cwd: &Path) -> Vec<WorktreeMeta> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["worktree", "list", "--porcelain"])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }

    let mut metas: Vec<WorktreeMeta> = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_branch: Option<String> = None;
    let push = |path: Option<PathBuf>, branch: Option<String>, metas: &mut Vec<WorktreeMeta>| {
        if let Some(p) = path {
            let canon = std::fs::canonicalize(&p).unwrap_or(p);
            let is_main = metas.is_empty();
            metas.push(WorktreeMeta { path: canon, branch, is_main });
        }
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            // A new `worktree` line starts a block — flush the previous one.
            push(cur_path.take(), cur_branch.take(), &mut metas);
            cur_path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            cur_branch = Some(b.trim_start_matches("refs/heads/").to_string());
        }
    }
    push(cur_path.take(), cur_branch.take(), &mut metas);
    metas
}

/// Match `target` against the worktree set by path, directory name, or
/// branch. Errors on no match or an ambiguous match.
fn resolve_target(worktrees: &[WorktreeMeta], target: &str) -> anyhow::Result<WorktreeMeta> {
    if worktrees.is_empty() {
        anyhow::bail!("no git worktrees found (is this a git repo?)");
    }
    let canon_target = std::fs::canonicalize(target).ok();
    let matches: Vec<&WorktreeMeta> = worktrees
        .iter()
        .filter(|w| {
            if canon_target.as_ref() == Some(&w.path) {
                return true;
            }
            if w.path.file_name().and_then(|n| n.to_str()) == Some(target) {
                return true;
            }
            match &w.branch {
                Some(b) => b == target || b.rsplit('/').next() == Some(target),
                None => false,
            }
        })
        .collect();

    match matches.as_slice() {
        [] => anyhow::bail!(
            "no worktree matches '{target}' (try a path, directory name, or branch)"
        ),
        [one] => Ok((*one).clone()),
        many => {
            let names: Vec<String> = many.iter().map(|w| w.path.display().to_string()).collect();
            anyhow::bail!("'{target}' is ambiguous — matches: {}", names.join(", "))
        }
    }
}

/// The repo's main worktree path, used as a still-present cwd for the hook.
fn main_root(worktrees: &[WorktreeMeta]) -> Option<PathBuf> {
    worktrees.iter().find(|w| w.is_main).map(|w| w.path.clone())
}

/// Read `[stack] on_destroy` from a worktree's `devme.toml`, if present.
fn read_on_destroy(path: &Path) -> Option<String> {
    let toml = std::fs::read_to_string(path.join("devme.toml")).ok()?;
    Stack::parse(&toml).ok()?.stack.and_then(|m| m.on_destroy)
}

/// The slot the worktree rooted at `cwd` currently holds, by path string —
/// the TUI's stack-info modal reads it here (off the render path, since it
/// touches the allocator registry file). `None` when the dir holds no claim.
pub fn slot_for_cwd(cwd: &str) -> Option<u8> {
    slot_for(Path::new(cwd))
}

/// The slot a worktree currently holds, from the allocator registry.
fn slot_for(path: &Path) -> Option<u8> {
    let registry = devme_config::paths::slot_registry().ok()?;
    let id = devme_config::paths::instance_id(path);
    SlotAllocator::open(&registry)
        .list()
        .ok()?
        .iter()
        .find(|c| c.instance_id == id)
        .map(|c| c.slot.as_u8())
}

/// Interpolate `{slot}`/`{worktree}`/`{branch}` into the `on_destroy`
/// command. `{slot}` is only present when a claim was found — a command that
/// references it without one fails here (so we never run a half-resolved
/// `dropdb`).
fn interpolate_on_destroy(
    cmd: &str,
    path: &Path,
    slot: Option<u8>,
    branch: Option<&str>,
) -> anyhow::Result<String> {
    let mut ctx = InterpContext::new()
        .set("worktree", path.display().to_string())
        .set("branch", branch.unwrap_or_default());
    if let Some(s) = slot {
        ctx.insert("slot", s.to_string());
    }
    interpolate(cmd, &ctx).map_err(|e| {
        anyhow::anyhow!(
            "cannot resolve on_destroy for {}: {e}\n\
             (hint: {{slot}} needs the worktree's stack to have an active slot \
             claim — start it first, or drop the resource manually)",
            path.display()
        )
    })
}

/// Shut down a worktree's instance supervisor (services + daemon).
/// Best-effort: returns false if no daemon was listening.
async fn stop_instance(path: &Path) -> bool {
    let Ok(sock) = devme_config::paths::supervisor_socket(path) else {
        return false;
    };
    let Ok(mut client) = Client::connect(&sock).await else {
        return false;
    };
    if client.send(ClientMessage::Shutdown).await.is_err() {
        return false;
    }
    // Drain until the daemon closes the connection or a grace window elapses.
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while let Ok(Some(_)) = client.next_event().await {}
    })
    .await;
    true
}

/// `git worktree remove [--force] <path>`, surfacing git's stderr on failure.
fn git_worktree_remove(cwd: &Path, path: &Path, force: bool) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(cwd).args(["worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    cmd.arg(path);
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("running git worktree remove: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = if stderr.trim().is_empty() {
            "(no stderr)"
        } else {
            stderr.trim()
        };
        anyhow::bail!("git worktree remove failed: {detail}");
    }
    Ok(())
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
    fn run_on_destroy_executes_interpolated_command() {
        // The execution primitive for the on_destroy hook: it runs an
        // already-interpolated shell command in the given directory. Here
        // we stand in for `dropdb optoscale_slot3` with a marker write.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("destroyed-slot3");
        let cmd = format!("touch {}", marker.to_string_lossy());
        let status = run_on_destroy(&cmd, dir.path()).unwrap();
        assert!(status.success());
        assert!(marker.exists(), "on_destroy command did not run");
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

    /// Build a repo with one linked worktree on `branch`. Returns
    /// `(root, main, linked)`, or `None` if git isn't usable here.
    fn setup_linked_worktree(
        tag: &str,
        branch: &str,
        devme_toml: Option<&str>,
    ) -> Option<(PathBuf, PathBuf, PathBuf)> {
        let id = std::process::id();
        let root = std::env::temp_dir().join(format!("devme-rmwt-{tag}-{id}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;
        let main = root.join("main");
        std::fs::create_dir_all(&main).ok()?;
        if !run_git(&main, &["init", "-q"]) {
            let _ = std::fs::remove_dir_all(&root);
            return None;
        }
        run_git(&main, &["config", "user.email", "t@example.com"]);
        run_git(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join("a.txt"), b"hi").ok()?;
        run_git(&main, &["add", "a.txt"]);
        // Commit devme.toml in the main worktree so the linked worktree
        // inherits it as a *tracked, clean* file — matching reality (the
        // config is checked in) and letting `git worktree remove` succeed
        // without --force.
        if let Some(toml) = devme_toml {
            std::fs::write(main.join("devme.toml"), toml).ok()?;
            run_git(&main, &["add", "devme.toml"]);
        }
        run_git(&main, &["commit", "-qm", "init"]);
        let linked = root.join("linked");
        if !run_git(&main, &["worktree", "add", "-q", "-b", branch, linked.to_str().unwrap()]) {
            let _ = std::fs::remove_dir_all(&root);
            return None;
        }
        Some((root, main, linked))
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn resolve_target_matches_by_name_then_branch() {
        let Some((root, _main, linked)) =
            setup_linked_worktree("resolve", "feature/foo", None)
        else {
            return;
        };
        let wts = list_worktrees_detailed(&linked);
        let linked_canon = std::fs::canonicalize(&linked).unwrap();

        // by directory name
        let by_dir = resolve_target(&wts, "linked").unwrap();
        assert_eq!(by_dir.path, linked_canon);
        assert!(!by_dir.is_main);
        // by full branch name
        assert_eq!(resolve_target(&wts, "feature/foo").unwrap().path, linked_canon);
        // by branch tail
        assert_eq!(resolve_target(&wts, "foo").unwrap().path, linked_canon);
        // no match
        assert!(resolve_target(&wts, "does-not-exist").is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_worktree_runs_on_destroy_then_removes() {
        // on_destroy touches a marker in the run dir (the main worktree).
        let toml = "schema_version = 1\n\n[stack]\non_destroy = \"touch destroyed.marker\"\n";
        let Some((root, main, linked)) =
            setup_linked_worktree("happy", "feature/foo", Some(toml))
        else {
            return;
        };

        let report = block_on(remove_worktree(&main, "linked", false)).unwrap();

        assert!(!linked.exists(), "worktree dir should be removed");
        assert_eq!(report.on_destroy_ran, Some(true));
        assert!(main.join("destroyed.marker").exists(), "on_destroy didn't run in main root");
        assert!(!report.instance_stopped, "no daemon was running");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_worktree_refuses_main_worktree() {
        let Some((root, main, _linked)) =
            setup_linked_worktree("guard", "feature/foo", None)
        else {
            return;
        };
        let err = block_on(remove_worktree(&main, "main", false)).unwrap_err();
        assert!(err.to_string().contains("main worktree"), "got: {err}");
        assert!(main.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_worktree_fails_closed_when_slot_unresolved() {
        // on_destroy needs {slot}, but a fresh temp worktree holds no slot
        // claim → resolution must fail and leave the worktree on disk.
        let toml = "schema_version = 1\n\n[stack]\non_destroy = \"dropdb slot{slot}\"\n";
        let Some((root, main, linked)) =
            setup_linked_worktree("slotfail", "feature/foo", Some(toml))
        else {
            return;
        };

        let err = block_on(remove_worktree(&main, "linked", false)).unwrap_err();
        assert!(err.to_string().contains("on_destroy"), "got: {err}");
        assert!(linked.exists(), "worktree must remain when on_destroy can't resolve");

        let _ = std::fs::remove_dir_all(&root);
    }
}
