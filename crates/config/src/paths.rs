//! Canonical filesystem locations used by both the daemon and the client.
//!
//! The socket path is derived from the worktree's canonicalized cwd so two
//! `devme` invocations in the same directory always agree on where to look,
//! and two worktrees of the same repo get separate sockets.

use std::path::{Path, PathBuf};

/// Unix socket path for the supervisor of `cwd`. Hashes `cwd` to a short
/// identifier so the resulting path stays a reasonable length even if the
/// repo is deeply nested.
pub fn supervisor_socket(cwd: &Path) -> std::io::Result<PathBuf> {
    Ok(runtime_dir_inner()?.join(format!("{}.sock", instance_id(cwd))))
}

/// Stable per-worktree identifier. Same input → same hex every time within
/// a single build.
pub fn instance_id(cwd: &Path) -> String {
    hash_path(&std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()))
}

/// Stable per-*repo* identifier. Every worktree of the same git repo
/// resolves to the same id, so a repo-scoped supervisor binds to one
/// socket regardless of which worktree spawned it.
///
/// Resolution: `git rev-parse --git-common-dir` from `cwd`, canonicalized
/// and hashed. If `cwd` is not inside a git repo, falls back to
/// [`instance_id`] — equivalent to "treat this directory as its own repo",
/// which is the right behavior for non-git devme setups.
pub fn repo_id(cwd: &Path) -> String {
    match git_common_dir(cwd) {
        Some(dir) => hash_path(&dir),
        None => instance_id(cwd),
    }
}

fn hash_path(p: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Returns the canonical path to the *shared* `.git` directory for the
/// repo containing `cwd`, or None if `cwd` is not inside a git repo. For a
/// regular worktree this is `<repo>/.git`; for a linked worktree
/// (`git worktree add`) it's still the main repo's `.git`, which is the
/// point — both worktrees agree on the same path.
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
    // `--git-common-dir` may be relative to cwd; resolve against it.
    let p = PathBuf::from(trimmed);
    let abs = if p.is_absolute() { p } else { cwd.join(p) };
    std::fs::canonicalize(&abs).ok()
}

/// Unix socket path for the shared-services supervisor of the repo
/// containing `cwd`. See ADR-0007.
pub fn shared_socket(cwd: &Path) -> std::io::Result<PathBuf> {
    let dir = shared_dir(cwd)?;
    Ok(dir.join("shared.sock"))
}

/// `~/.local/share/devme/repos/<repo-id>/`, created if missing. Per-repo
/// state (the shared socket, lock files) lives here.
pub fn shared_dir(cwd: &Path) -> std::io::Result<PathBuf> {
    let dir = runtime_dir_inner()?
        .join("repos")
        .join(repo_id(cwd));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Shared slot-allocator registry path. One file per host coordinates
/// port-slot assignments across every devme daemon on the machine.
pub fn slot_registry() -> std::io::Result<PathBuf> {
    Ok(runtime_dir_inner()?.join("slots.json"))
}

/// Directory where every devme daemon on this host binds its socket. The
/// TUI watches this directory to discover sibling stacks (other worktrees
/// with a running supervisor).
pub fn runtime_dir() -> std::io::Result<PathBuf> {
    runtime_dir_inner()
}

/// `~/.local/share/devme/` or platform equivalent, created if missing.
fn runtime_dir_inner() -> std::io::Result<PathBuf> {
    let dir = if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(d).join("devme")
    } else if let Some(d) = std::env::var_os("TMPDIR") {
        PathBuf::from(d).join("devme")
    } else {
        PathBuf::from("/tmp/devme")
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn instance_id_is_stable_for_the_same_path() {
        let dir = TempDir::new().unwrap();
        let a = instance_id(dir.path());
        let b = instance_id(dir.path());
        assert_eq!(a, b);
    }

    #[test]
    fn different_paths_get_different_ids() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        assert_ne!(instance_id(a.path()), instance_id(b.path()));
    }

    #[test]
    fn repo_id_falls_back_to_instance_id_outside_git() {
        // A non-git tempdir has no rev-parse; repo_id should match
        // instance_id so a non-git devme setup still gets a stable hash.
        let dir = TempDir::new().unwrap();
        let r = repo_id(dir.path());
        let i = instance_id(dir.path());
        assert_eq!(r, i);
    }

    #[test]
    fn repo_id_is_same_for_subdirectories_of_the_same_git_repo() {
        // Two subdirectories of the same git repo must hash to the same
        // repo_id — this is the property linked worktrees rely on.
        let dir = TempDir::new().unwrap();
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .arg("init")
            .arg("-q")
            .status();
        if !matches!(ok, Ok(s) if s.success()) {
            // No git available — skip.
            return;
        }
        let sub_a = dir.path().join("a");
        let sub_b = dir.path().join("b/c");
        std::fs::create_dir_all(&sub_a).unwrap();
        std::fs::create_dir_all(&sub_b).unwrap();
        let id_a = repo_id(&sub_a);
        let id_b = repo_id(&sub_b);
        let id_root = repo_id(dir.path());
        assert_eq!(id_a, id_root);
        assert_eq!(id_b, id_root);
    }

    #[test]
    fn shared_socket_path_ends_in_shared_dot_sock() {
        let dir = TempDir::new().unwrap();
        let p = shared_socket(dir.path()).unwrap();
        assert!(p.ends_with("shared.sock"), "got: {}", p.display());
    }

    #[test]
    fn socket_path_ends_in_dot_sock() {
        let dir = TempDir::new().unwrap();
        let p = supervisor_socket(dir.path()).unwrap();
        assert!(
            p.to_string_lossy().ends_with(".sock"),
            "got: {}",
            p.display()
        );
    }
}
