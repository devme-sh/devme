//! `[remote]` configuration and the pure path/template logic behind
//! `devme remote` — bootstrap + live-sync a project to a remote dev host.
//!
//! See DEV-5. The remote is the **primary** environment: the supervisor,
//! stack, and (optionally) agents run there and keep working while the
//! laptop sleeps. devme owns the Mutagen *sync session* (not the daemon —
//! the OS keeps that alive) and an opaque, herdr-agnostic `attach` command.
//!
//! Everything here is pure and unit-tested; the imperative orchestration
//! (shelling out to `mutagen`/`ssh`) lives in the `devme` binary.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Heavy/generated dirs never worth syncing. Used when `[remote] ignore`
/// is unset; an explicit list replaces these wholesale.
pub const DEFAULT_IGNORES: &[&str] = &[
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "dist",
    "build",
    ".next",
    "target",
    ".turbo",
    "*.log",
    ".DS_Store",
];

/// Mutagen sync modes devme exposes. `two-way-safe` is the default: because
/// the remote is primary and keeps working while the laptop sleeps, a
/// fixed-winner mode would silently clobber overnight remote work. Safe mode
/// halts on conflict and surfaces it via `devme remote status`.
pub const SYNC_MODES: &[&str] = &["two-way-safe", "two-way-resolved"];

/// Attach presets shipped with devme. Anything else is treated as a raw
/// command template with `{host}` / `{remote_path}` / `{name}` placeholders.
pub const ATTACH_PRESETS: &[&str] = &["tui", "ssh", "tmux", "herdr"];

const DEFAULT_ROOT: &str = "~/development";
const DEFAULT_SYNC_MODE: &str = "two-way-safe";
const DEFAULT_ATTACH: &str = "tui";

/// User-global `[remote]` settings. Lives in `~/.config/devme/config.toml`
/// alongside the other [`crate::GlobalConfig`] sections; a project's
/// `devme.toml` may override the `ignore` list narrowly (handled by the
/// caller, not here).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Opaque SSH target — a Tailscale MagicDNS name, an `~/.ssh/config`
    /// alias, or `user@host`. devme never special-cases Tailscale; it's the
    /// network layer, SSH is the transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Remote parent directory; the project dir is derived under it. Default
    /// `~/development`. Both `ssh` and `mutagen` expand a leading `~`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Mutagen sync mode — see [`SYNC_MODES`]. Default `two-way-safe`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_mode: Option<String>,
    /// Attach command: a preset name (see [`ATTACH_PRESETS`]) or a raw
    /// template. Default `tui` → the remote stack's `devme tui`, full-screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<String>,
    /// Mutagen ignore patterns. Empty → [`DEFAULT_IGNORES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    /// Hostname to build service URLs from when a sync is live (e.g. a
    /// Tailscale MagicDNS name reachable from the laptop browser). Defaults
    /// to `host` with any `user@` stripped — so `devme url` over a live
    /// remote resolves to `http://<url_host>:<port>` instead of localhost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_host: Option<String>,
    /// When true, bare `devme` behaves as `devme remote` — the project is
    /// remote-first, so opening it attaches to the remote stack's TUI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
}

impl RemoteConfig {
    pub fn is_empty(&self) -> bool {
        self.host.is_none()
            && self.root.is_none()
            && self.sync_mode.is_none()
            && self.attach.is_none()
            && self.ignore.is_empty()
            && self.url_host.is_none()
            && self.default.is_none()
    }

    /// Whether bare `devme` should default to `devme remote` for this user.
    pub fn is_default(&self) -> bool {
        self.default.unwrap_or(false)
    }

    /// The host to build browser URLs from: explicit `url_host`, else `host`
    /// with any `user@` prefix stripped (an SSH login user isn't part of an
    /// HTTP authority).
    pub fn url_host_for(&self, host: &str) -> String {
        self.url_host
            .clone()
            .unwrap_or_else(|| host.rsplit('@').next().unwrap_or(host).to_string())
    }

    pub fn root_or_default(&self) -> &str {
        self.root.as_deref().unwrap_or(DEFAULT_ROOT)
    }

    pub fn sync_mode_or_default(&self) -> &str {
        self.sync_mode.as_deref().unwrap_or(DEFAULT_SYNC_MODE)
    }

    pub fn attach_or_default(&self) -> &str {
        self.attach.as_deref().unwrap_or(DEFAULT_ATTACH)
    }

    /// The effective ignore list — the configured one, or [`DEFAULT_IGNORES`].
    pub fn ignores(&self) -> Vec<String> {
        if self.ignore.is_empty() {
            DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect()
        } else {
            self.ignore.clone()
        }
    }
}

/// Reject a sync mode devme doesn't support before it reaches Mutagen.
pub fn validate_sync_mode(value: &str) -> Result<(), String> {
    if SYNC_MODES.contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "remote.sync_mode expects one of {}, got: {value}",
            SYNC_MODES.join("/")
        ))
    }
}

/// Lowercase, collapse non-alphanumerics to single hyphens, trim. Shared by
/// the remote directory name and the Mutagen session name so both are
/// stable, readable, and filesystem/Mutagen-safe.
fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Collision-free remote directory name for a repo's main worktree:
/// `<slug>-<repo8>`, where `repo8` is the first 8 hex of the stable
/// [`crate::paths::repo_id`]. Every worktree of a repo maps to the same
/// name (model 1a, shared `.git`); two unrelated repos sharing a basename
/// stay distinct.
pub fn remote_dir_name(local_root: &Path) -> String {
    let base = local_root
        .file_name()
        .and_then(|n| n.to_str())
        .map(slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "project".to_string());
    let id = crate::paths::repo_id(local_root);
    format!("{base}-{}", &id[..8.min(id.len())])
}

/// Remote project path: `<root>/<remote_dir_name>`. `root` may contain a
/// leading `~`; the remote shell / Mutagen expand it.
pub fn remote_path(root: &str, local_root: &Path) -> String {
    let root = root.trim_end_matches('/');
    format!("{root}/{}", remote_dir_name(local_root))
}

/// Mutagen session name for a repo: `devme-<dir-name>`. The `devme-` prefix
/// guarantees a leading letter (Mutagen requires DNS-label-like names).
pub fn sync_session_name(local_root: &Path) -> String {
    format!("devme-{}", remote_dir_name(local_root))
}

/// Expand an `attach` setting into a shell command line. A known preset
/// expands to its template; anything else is treated as a raw template and
/// has its `{host}` / `{remote_path}` / `{name}` placeholders substituted.
///
/// Presets:
/// - `tui` — `devme tui` is the whole session, full-screen on the remote.
/// - `ssh` — a bare login shell in the project dir (zero-dep, no persistence).
/// - `tmux` — `devme tui` inside a persistent tmux session, re-attachable.
/// - `herdr` — attach a herdr remote session (the herdr setup is the user's).
pub fn expand_attach(attach: &str, host: &str, remote_path: &str, name: &str) -> String {
    let template = match attach {
        "tui" => "ssh -t {host} 'cd {remote_path} && exec devme tui'",
        "ssh" => "ssh -t {host} 'cd {remote_path} && exec $SHELL'",
        "tmux" => "ssh -t {host} 'tmux new -A -s {name} -c {remote_path} devme tui'",
        "herdr" => "herdr --remote {host} --session {name}",
        raw => raw,
    };
    template
        .replace("{host}", host)
        .replace("{remote_path}", remote_path)
        .replace("{name}", name)
}

/// Rewrite a local URL's host to `url_host` so a `http://localhost:<port>`
/// from the remote daemon becomes reachable from the laptop (e.g. over
/// Tailscale). Only the loopback authority is swapped; the port and path are
/// preserved.
pub fn rewrite_url_host(url: &str, url_host: &str) -> String {
    url.replace("//localhost:", &format!("//{url_host}:"))
        .replace("//127.0.0.1:", &format!("//{url_host}:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn defaults_apply_when_unset() {
        let cfg = RemoteConfig::default();
        assert!(cfg.is_empty());
        assert_eq!(cfg.root_or_default(), "~/development");
        assert_eq!(cfg.sync_mode_or_default(), "two-way-safe");
        assert_eq!(cfg.attach_or_default(), "tui");
        assert_eq!(cfg.ignores(), DEFAULT_IGNORES);
    }

    #[test]
    fn explicit_ignore_replaces_defaults() {
        let cfg = RemoteConfig { ignore: vec!["foo".into()], ..Default::default() };
        assert_eq!(cfg.ignores(), vec!["foo".to_string()]);
        assert!(!cfg.is_empty());
    }

    #[test]
    fn sync_mode_validation() {
        assert!(validate_sync_mode("two-way-safe").is_ok());
        assert!(validate_sync_mode("two-way-resolved").is_ok());
        assert!(validate_sync_mode("one-way-replica").is_err());
        assert!(validate_sync_mode("nonsense").is_err());
    }

    #[test]
    fn slugify_is_filesystem_safe() {
        assert_eq!(slugify("My Project!"), "my-project");
        assert_eq!(slugify("kpi_dashboard"), "kpi-dashboard");
        assert_eq!(slugify("--weird__name--"), "weird-name");
    }

    #[test]
    fn remote_dir_name_is_stable_and_collision_free() {
        // Two non-git tempdirs sharing a basename get distinct names because
        // repo_id falls back to a hash of the (different) path.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let pa = a.path().join("api");
        let pb = b.path().join("api");
        std::fs::create_dir_all(&pa).unwrap();
        std::fs::create_dir_all(&pb).unwrap();
        let na = remote_dir_name(&pa);
        let nb = remote_dir_name(&pb);
        assert!(na.starts_with("api-"), "got {na}");
        assert!(nb.starts_with("api-"), "got {nb}");
        assert_ne!(na, nb, "basename collision not disambiguated");
        // Stable across calls.
        assert_eq!(na, remote_dir_name(&pa));
    }

    #[test]
    fn remote_path_joins_under_root() {
        let p = PathBuf::from("/home/me/dev/api");
        let rp = remote_path("~/development/", &p);
        assert!(rp.starts_with("~/development/api-"), "got {rp}");
        // Trailing slash on root is normalized away (no `//`).
        assert!(!rp.contains("//"));
    }

    #[test]
    fn session_name_is_mutagen_safe() {
        let name = sync_session_name(&PathBuf::from("/x/My App"));
        assert!(name.starts_with("devme-my-app-"));
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn attach_tui_preset_runs_devme_tui_remotely() {
        let cmd = expand_attach("tui", "vps", "~/development/api-abc", "devme-api");
        assert!(cmd.contains("ssh -t vps"));
        assert!(cmd.contains("cd ~/development/api-abc"));
        assert!(cmd.contains("exec devme tui"));
    }

    #[test]
    fn attach_presets_cover_persistence_options() {
        assert!(expand_attach("ssh", "vps", "/p", "n").contains("exec $SHELL"));
        assert!(expand_attach("tmux", "vps", "/p", "n").contains("tmux new -A -s n"));
        assert!(expand_attach("herdr", "vps", "/p", "sess").contains("herdr --remote vps --session sess"));
    }

    #[test]
    fn attach_raw_template_substitutes_placeholders() {
        let cmd = expand_attach("mosh {host} -- tmux a -t {name}", "box", "/p", "proj");
        assert_eq!(cmd, "mosh box -- tmux a -t proj");
    }

    #[test]
    fn url_host_strips_login_user_and_honors_override() {
        let cfg = RemoteConfig::default();
        assert_eq!(cfg.url_host_for("vps"), "vps");
        assert_eq!(cfg.url_host_for("dev@10.0.0.1"), "10.0.0.1");
        let cfg = RemoteConfig { url_host: Some("vps.tailnet.ts.net".into()), ..Default::default() };
        assert_eq!(cfg.url_host_for("dev@10.0.0.1"), "vps.tailnet.ts.net");
    }

    #[test]
    fn rewrite_url_host_swaps_only_loopback_authority() {
        assert_eq!(rewrite_url_host("http://localhost:8090", "vps"), "http://vps:8090");
        assert_eq!(rewrite_url_host("http://127.0.0.1:5432/db", "vps"), "http://vps:5432/db");
        // A non-loopback host is left alone.
        assert_eq!(rewrite_url_host("http://example.com:80", "vps"), "http://example.com:80");
    }

    #[test]
    fn default_flag_round_trips() {
        let cfg = RemoteConfig { default: Some(true), ..Default::default() };
        assert!(cfg.is_default());
        assert!(!cfg.is_empty());
        assert!(!RemoteConfig::default().is_default());
    }
}
