//! The embedded devme AI agent skill, plus the pure logic for installing,
//! classifying, and refreshing it.
//!
//! The `SKILL.md` that teaches coding agents to drive devme is **embedded in
//! the binary at build time** (`include_str!`), so the skill an agent reads
//! always matches the devme it's talking to — no skill-vs-binary drift. The
//! `devme-sh/skills` repo (installable via `npx skills add devme-sh/skills`)
//! is a CI-generated mirror of this same file.
//!
//! This module lives in `devme-config` (not `cli`) so both the CLI and the
//! TUI can share it without a dependency cycle. It does filesystem work and
//! reads/writes [`GlobalConfig`], but prints nothing — presentation lives in
//! the callers.
//!
//! Install scopes mirror the Claude Code skills contract:
//! - project: `./.claude/skills/devme/SKILL.md`
//! - global:  `~/.claude/skills/devme/SKILL.md`
//!
//! Every file we write is recorded in [`GlobalConfig`] (path → version +
//! content hash) so we can later tell three states apart: up-to-date,
//! outdated (devme wrote it, a newer binary now ships a newer skill), and
//! modified/foreign (the user edited it, or another tool installed it — we
//! never clobber those without `force`).

use std::path::{Path, PathBuf};

use crate::{GlobalConfig, SkillInstall};

/// The canonical skill, embedded at compile time. Source of truth lives at
/// `crates/config/skill/SKILL.md`; the published `devme-sh/skills` repo
/// mirrors it.
pub const SKILL_MD: &str = include_str!("../skill/SKILL.md");

/// The skill's identity for display/state is the binary version — the skill
/// ships with the binary, so "which devme is this skill for" is the honest
/// question. Staleness itself is decided by content hash, not this string, so
/// a forgotten version bump never hides a real content change.
pub fn embedded_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// FNV-1a (64-bit) of the embedded skill. Tiny, dependency-free, and
/// deterministic across platforms and toolchains — all we need to detect
/// "did this file change since devme wrote it".
pub fn embedded_hash() -> String {
    fnv1a(SKILL_MD)
}

fn fnv1a(s: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The `…/.claude/skills/devme` directory for the requested scope.
pub fn skill_dir(global: bool) -> Option<PathBuf> {
    let base = if global {
        home_dir()?
    } else {
        std::env::current_dir().ok()?
    };
    Some(base.join(".claude").join("skills").join("devme"))
}

/// `…/.claude/skills/devme/SKILL.md` for the requested scope.
pub fn skill_file(global: bool) -> Option<PathBuf> {
    Some(skill_dir(global)?.join("SKILL.md"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStatus {
    /// Nothing on disk at this path.
    Missing,
    /// On disk and byte-identical to the embedded skill.
    UpToDate,
    /// devme wrote it (hash matches our record) but a newer skill now ships.
    Outdated,
    /// devme wrote it once, but the file has been edited since.
    Modified,
    /// Present, not byte-identical, and not something devme recorded writing
    /// (e.g. installed via `npx skills`). Off-limits without `force`.
    Foreign,
}

impl InstallStatus {
    pub fn label(self) -> &'static str {
        match self {
            InstallStatus::Missing => "missing",
            InstallStatus::UpToDate => "up-to-date",
            InstallStatus::Outdated => "outdated",
            InstallStatus::Modified => "modified",
            InstallStatus::Foreign => "foreign",
        }
    }
}

/// Classify what's on disk at `path` given what (if anything) devme recorded
/// writing there.
pub fn status_at(path: &Path, recorded: Option<&SkillInstall>) -> InstallStatus {
    let on_disk = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return InstallStatus::Missing,
    };
    if on_disk == SKILL_MD {
        return InstallStatus::UpToDate;
    }
    let on_disk_hash = fnv1a(&on_disk);
    match recorded {
        Some(r) if r.hash == on_disk_hash => InstallStatus::Outdated,
        Some(_) => InstallStatus::Modified,
        None => InstallStatus::Foreign,
    }
}

/// What `install` did, so callers can phrase their own output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Already byte-identical; nothing written.
    AlreadyCurrent,
    /// Freshly written (no prior devme install here).
    Installed,
    /// Replaced an older devme-written skill.
    Updated,
}

/// Write the embedded skill into the requested scope, recording it in config.
///
/// Refuses (without `force`) to overwrite a hand-edited or foreign file —
/// those return an `Err` describing which. Returns the resolved path and what
/// happened. The caller is responsible for any user-facing output.
pub fn install(global: bool, force: bool) -> std::io::Result<(PathBuf, InstallOutcome)> {
    let dir = skill_dir(global).ok_or_else(|| {
        std::io::Error::other("could not resolve a skills directory (is $HOME set?)")
    })?;
    let file = dir.join("SKILL.md");
    let key = file.to_string_lossy().to_string();

    let mut cfg = GlobalConfig::load();
    let recorded = cfg.skill_installs().get(&key).cloned();
    let status = status_at(&file, recorded.as_ref());

    if status == InstallStatus::UpToDate && !force {
        return Ok((file, InstallOutcome::AlreadyCurrent));
    }
    if matches!(status, InstallStatus::Modified | InstallStatus::Foreign) && !force {
        let why = if status == InstallStatus::Modified {
            "hand-edited"
        } else {
            "installed by another tool"
        };
        return Err(std::io::Error::other(format!(
            "{} looks {why} — refusing to overwrite. Re-run with --force to replace it.",
            file.display()
        )));
    }

    std::fs::create_dir_all(&dir)?;
    std::fs::write(&file, SKILL_MD)?;
    cfg.record_skill_install(&key, &embedded_version(), &embedded_hash());
    let _ = cfg.save();

    let outcome = if status == InstallStatus::Outdated {
        InstallOutcome::Updated
    } else {
        InstallOutcome::Installed
    };
    Ok((file, outcome))
}

/// What `uninstall` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// Nothing was there and nothing was recorded.
    Absent,
    /// A devme-managed install was removed.
    Removed,
}

/// Remove a devme-managed install. Refuses to touch a foreign file (present
/// but never recorded by devme).
pub fn uninstall(global: bool) -> std::io::Result<(PathBuf, RemoveOutcome)> {
    let dir = skill_dir(global).ok_or_else(|| {
        std::io::Error::other("could not resolve a skills directory (is $HOME set?)")
    })?;
    let file = dir.join("SKILL.md");
    let key = file.to_string_lossy().to_string();

    let mut cfg = GlobalConfig::load();
    let recorded = cfg.skill_installs().get(&key).cloned();

    if recorded.is_none() && file.exists() {
        return Err(std::io::Error::other(format!(
            "{} was not installed by devme — remove it yourself (e.g. `npx skills remove devme`).",
            file.display()
        )));
    }
    if !file.exists() && recorded.is_none() {
        return Ok((file, RemoveOutcome::Absent));
    }

    if file.exists() {
        std::fs::remove_file(&file)?;
    }
    // Only removes the directory if it's now empty (it's ours: `…/skills/devme`).
    let _ = std::fs::remove_dir(&dir);
    cfg.forget_skill_install(&key);
    let _ = cfg.save();

    Ok((file, RemoveOutcome::Removed))
}

/// A devme-managed install that is outdated and unmodified — safe to refresh.
#[derive(Debug, Clone)]
pub struct StaleInstall {
    pub path: String,
    pub from: String,
    pub to: String,
}

/// Every recorded install that is `Outdated` (devme-managed, unmodified, but
/// a newer skill now ships). These are the only paths the nudge/auto-update
/// will ever touch.
pub fn stale_installs(cfg: &GlobalConfig) -> Vec<StaleInstall> {
    let to = embedded_version();
    cfg.skill_installs()
        .iter()
        .filter(|(path, rec)| status_at(Path::new(path), Some(rec)) == InstallStatus::Outdated)
        .map(|(path, rec)| StaleInstall {
            path: path.clone(),
            from: rec.version.clone(),
            to: to.clone(),
        })
        .collect()
}

/// Regenerate every stale, devme-managed, unmodified install in place.
/// Returns the paths actually rewritten. Saves config if anything changed.
pub fn auto_update(cfg: &mut GlobalConfig) -> Vec<String> {
    let version = embedded_version();
    let hash = embedded_hash();
    let mut updated = Vec::new();
    for stale in stale_installs(cfg) {
        if std::fs::write(&stale.path, SKILL_MD).is_ok() {
            cfg.record_skill_install(&stale.path, &version, &hash);
            updated.push(stale.path);
        }
    }
    if !updated.is_empty() {
        let _ = cfg.save();
    }
    updated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn fnv1a_is_deterministic_and_differs() {
        assert_eq!(fnv1a("abc"), fnv1a("abc"));
        assert_ne!(fnv1a("abc"), fnv1a("abd"));
    }

    #[test]
    fn status_distinguishes_the_four_cases() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("SKILL.md");

        assert_eq!(status_at(&file, None), InstallStatus::Missing);

        fs::write(&file, SKILL_MD).unwrap();
        assert_eq!(status_at(&file, None), InstallStatus::UpToDate);

        let old_body = "old skill body";
        fs::write(&file, old_body).unwrap();
        let rec = SkillInstall { version: "0.0.1".into(), hash: fnv1a(old_body) };
        assert_eq!(status_at(&file, Some(&rec)), InstallStatus::Outdated);

        fs::write(&file, "user edited this").unwrap();
        assert_eq!(status_at(&file, Some(&rec)), InstallStatus::Modified);

        assert_eq!(status_at(&file, None), InstallStatus::Foreign);
    }

    #[test]
    fn embedded_skill_has_expected_frontmatter() {
        assert!(SKILL_MD.starts_with("---\nname: devme\n"));
        assert!(SKILL_MD.contains("### CLI reference"));
    }
}
