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
    use std::hash::{Hash, Hasher};

    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut h);
    format!("{:016x}", h.finish())
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
