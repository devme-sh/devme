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
    /// Host that *this* machine advertises in `devme url` output when it's the
    /// one running the stack (e.g. the VPS). Lets an agent in a herdr pane on
    /// the host hand back a laptop-reachable link instead of `localhost`. A
    /// hostname, or the literal `"auto"` to read the machine's own Tailscale
    /// MagicDNS name. Deliberately distinct from `url_host` (which the *laptop*
    /// uses to rewrite a proxied URL): a laptop never sets `advertise_host`, so
    /// plain local `devme url` is never silently rewritten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertise_host: Option<String>,
    /// When attaching to the remote (bare `devme` / `devme remote`), first
    /// ensure the stack is up on the host (`devme up -d`) so the dev server is
    /// already running under the supervisor before you land in herdr/ssh/tui.
    /// Default true. The supervisor — not the attach session — owns the
    /// stack's lifetime, so it survives detach and the laptop sleeping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up_on_attach: Option<bool>,
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
            && self.advertise_host.is_none()
            && self.up_on_attach.is_none()
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

    /// Whether attaching should first ensure the remote stack is up. Default
    /// true — landing in herdr/ssh with a dead stack is the wrong default.
    pub fn up_on_attach_or_default(&self) -> bool {
        self.up_on_attach.unwrap_or(true)
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
pub fn expand_attach(
    attach: &str,
    host: &str,
    remote_path: &str,
    name: &str,
    url_host: &str,
) -> String {
    // `env VAR=val cmd` (not `VAR=val cmd`) so it works under fish too, which
    // has no inline assignment syntax. `DEVME_URL_HOST` lets the remote TUI's
    // copy-URL keybind hand back a laptop-reachable (Tailscale) URL.
    let template = match attach {
        "tui" => "ssh -t {host} 'cd {remote_path} && exec env DEVME_URL_HOST={url_host} devme tui'",
        "ssh" => "ssh -t {host} 'cd {remote_path} && exec $SHELL'",
        "tmux" => {
            "ssh -t {host} 'tmux new -A -s {name} -c {remote_path} env DEVME_URL_HOST={url_host} devme tui'"
        }
        "herdr" => "herdr --remote {host} --session {name}",
        raw => raw,
    };
    template
        .replace("{host}", host)
        .replace("{remote_path}", remote_path)
        .replace("{name}", name)
        .replace("{url_host}", url_host)
}

// --- herdr preset preparation ------------------------------------------------

/// POSIX single-quote a shell argument. Simple tokens (service names, flags,
/// numbers) pass through unquoted. The `'\''` escape is also valid in fish,
/// so this is safe whatever login shell the remote uses.
pub fn shell_quote(s: &str) -> String {
    // `~` is allowed unquoted so a remote path like `~/development/foo` keeps
    // its tilde expansion (single-quoting would make the remote shell `cd`
    // into a literal `~` directory).
    let simple = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "_./:=-~".contains(c));
    if simple {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Shell command that lists the named herdr session's workspaces (JSON on
/// stdout). Doubles as the cheapest "is the session server running?" probe —
/// herdr exits non-zero when the session's socket is absent.
pub fn herdr_list_cmd(session: &str) -> String {
    format!("env HERDR_SESSION={} herdr workspace list", shell_quote(session))
}

/// Shell command that starts the named herdr session server headless on the
/// remote, detached from the ssh connection that launches it, with the
/// project directory as its process cwd.
pub fn herdr_server_start_cmd(session: &str, remote_path: &str) -> String {
    format!(
        "cd {} && env HERDR_SESSION={} sh -c 'nohup herdr server >/dev/null 2>&1 &'",
        shell_quote(remote_path),
        shell_quote(session)
    )
}

/// Shell command that creates a workspace rooted at the project directory in
/// the named herdr session, so the first attach opens in the project instead
/// of the login dir.
pub fn herdr_workspace_create_cmd(session: &str, remote_path: &str, label: &str) -> String {
    format!(
        "env HERDR_SESSION={} herdr workspace create --cwd {} --label {}",
        shell_quote(session),
        shell_quote(remote_path),
        shell_quote(label)
    )
}

/// Number of workspaces in a `herdr workspace list` response, or `None` when
/// the output isn't the JSON shape we expect (herdr missing/old, error text).
/// `None` and `Some(0)` are deliberately distinct: only a *confirmed* empty
/// session gets a workspace seeded.
pub fn herdr_workspace_count(output: &str) -> Option<u64> {
    let line = output.lines().find(|l| l.trim_start().starts_with('{'))?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    Some(v.get("result")?.get("workspaces")?.as_array()?.len() as u64)
}

/// Sentinel `advertise_host` value meaning "autodetect this machine's own
/// Tailscale MagicDNS name." Keeps devme network-agnostic: Tailscale is an
/// opt-in autodetect, never a hard dependency.
pub const ADVERTISE_AUTO: &str = "auto";

/// Pick the host `devme url` should advertise on the machine running the
/// stack, from already-resolved inputs (pure, so it's unit-tested). Priority:
///
/// 1. `env` — `$DEVME_URL_HOST`, exported by the laptop's attach templates so
///    a URL copied inside the remote TUI is laptop-reachable.
/// 2. `configured` — `remote.advertise_host`; the literal `"auto"` defers to
///    `tailscale` (which the caller looks up only in that case).
///
/// `None` means "no host to advertise — fall back to `localhost`". Whitespace
/// and empty strings are treated as absent so a blank config never wins.
pub fn pick_advertise_host(
    env: Option<&str>,
    configured: Option<&str>,
    tailscale: Option<&str>,
) -> Option<String> {
    fn clean(s: &str) -> Option<String> {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    }
    if let Some(e) = env.and_then(clean) {
        return Some(e);
    }
    match configured.map(str::trim) {
        Some(ADVERTISE_AUTO) => tailscale.and_then(clean),
        Some(other) => clean(other),
        None => None,
    }
}

/// Rewrite a local URL's host to `url_host` so a `http://localhost:<port>`
/// from the remote daemon becomes reachable from the laptop (e.g. over
/// Tailscale). Only the loopback authority is swapped; the port and path are
/// preserved.
pub fn rewrite_url_host(url: &str, url_host: &str) -> String {
    url.replace("//localhost:", &format!("//{url_host}:"))
        .replace("//127.0.0.1:", &format!("//{url_host}:"))
}

// --- live-sync health (laptop-side watcher) ---------------------------------

/// Coarse health of a live sync, derived from Mutagen's status string + its
/// conflict count — only the distinctions worth *telling the user* about,
/// collapsing Mutagen's many internal states. Drives the background watcher
/// that runs alongside an attached `devme remote` session and the
/// `devme remote status --watch` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncHealth {
    /// Scanning / watching / staging / applying / idle — nothing wrong.
    Healthy,
    /// One or more conflicts → two-way-safe halts. The actionable state: your
    /// edits stop flowing until it's resolved.
    Conflict,
    /// Session gone, halted, errored, or an endpoint disconnected.
    Down,
}

/// Classify a sync observation into a [`SyncHealth`]. `exists` is whether the
/// session is still present at all; `status` is Mutagen's status string (None
/// when the session is gone or the status couldn't be read); `conflicts` is
/// its conflict count. Conflicts dominate — a halted-on-conflict sync is the
/// #1 failure mode and the whole reason this watcher exists.
pub fn classify_sync(exists: bool, status: Option<&str>, conflicts: u64) -> SyncHealth {
    if !exists {
        return SyncHealth::Down;
    }
    if conflicts > 0 {
        return SyncHealth::Conflict;
    }
    match status {
        // Session is present but its status couldn't be read (an unexpected
        // Mutagen version) — assume healthy rather than cry wolf; a real
        // conflict still shows up via the count above.
        None => SyncHealth::Healthy,
        Some(s) => {
            let low = s.to_ascii_lowercase();
            if low.contains("halt")
                || low.contains("error")
                || low.contains("disconnect")
                || low.contains("problem")
            {
                SyncHealth::Down
            } else {
                SyncHealth::Healthy
            }
        }
    }
}

/// What the watcher should announce when health moves `from` → `to` (edge-
/// triggered). `None` means stay quiet: no change, or a change not worth a
/// notification (notably a *healthy* first observation — we don't pop a banner
/// just because the sync started fine). Passing `from: None` with a problem
/// `to` yields the problem message, so it doubles as the "remind once" text.
pub fn sync_transition_message(
    from: Option<SyncHealth>,
    to: SyncHealth,
    conflicts: u64,
) -> Option<String> {
    if from == Some(to) {
        return None;
    }
    match to {
        SyncHealth::Conflict => Some(format!(
            "⚠ {conflicts} sync conflict(s) — two-way-safe sync HALTED; \
             your edits aren't flowing. Resolve: devme remote conflicts"
        )),
        SyncHealth::Down => {
            Some("⚠ live-sync is down (halted / disconnected). Check: devme remote status".into())
        }
        // Only celebrate recovery if we were previously *in* a problem — not on
        // a healthy cold start (from == None).
        SyncHealth::Healthy => from.map(|_| "✓ live-sync healthy again".to_string()),
    }
}

/// A compact one-line status for `devme remote status --watch` and the
/// post-detach summary. `status` is Mutagen's raw status string, folded in
/// when healthy for a little extra context ("✓ synced · Watching for changes").
pub fn sync_status_line(health: SyncHealth, conflicts: u64, status: Option<&str>) -> String {
    match health {
        SyncHealth::Conflict => {
            format!("⚠ {conflicts} conflict(s) — HALTED · resolve: devme remote conflicts")
        }
        SyncHealth::Down => match status {
            Some(s) if !s.is_empty() => format!("⚠ sync down · {s}"),
            _ => "⚠ sync down (halted / disconnected)".to_string(),
        },
        SyncHealth::Healthy => match status {
            Some(s) if !s.is_empty() => format!("✓ synced · {s}"),
            _ => "✓ synced".to_string(),
        },
    }
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
        // up_on_attach defaults on — landing in herdr with a dead stack is wrong.
        assert!(cfg.up_on_attach_or_default());
    }

    #[test]
    fn advertise_host_priority_env_then_config_then_auto() {
        // env (DEVME_URL_HOST from the attach template) wins outright.
        assert_eq!(
            pick_advertise_host(Some("env.host"), Some("cfg.host"), Some("ts.host")).as_deref(),
            Some("env.host")
        );
        // No env → explicit config is used verbatim.
        assert_eq!(
            pick_advertise_host(None, Some("cfg.host"), Some("ts.host")).as_deref(),
            Some("cfg.host")
        );
        // "auto" defers to the autodetected Tailscale name.
        assert_eq!(
            pick_advertise_host(None, Some("auto"), Some("vps.goose-viper.ts.net")).as_deref(),
            Some("vps.goose-viper.ts.net")
        );
        // "auto" but Tailscale unavailable → None (caller uses localhost).
        assert_eq!(pick_advertise_host(None, Some("auto"), None), None);
        // Nothing configured → None, so a plain laptop `devme url` is untouched.
        assert_eq!(pick_advertise_host(None, None, None), None);
        // Blanks are treated as absent.
        assert_eq!(
            pick_advertise_host(Some("  "), Some("  "), Some("ts")),
            None
        );
    }

    #[test]
    fn up_on_attach_round_trips() {
        let cfg = RemoteConfig {
            up_on_attach: Some(false),
            ..Default::default()
        };
        assert!(!cfg.up_on_attach_or_default());
        assert!(!cfg.is_empty());
    }

    #[test]
    fn explicit_ignore_replaces_defaults() {
        let cfg = RemoteConfig {
            ignore: vec!["foo".into()],
            ..Default::default()
        };
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
        let cmd = expand_attach("tui", "vps", "~/development/api-abc", "devme-api", "vps");
        assert!(cmd.contains("ssh -t vps"));
        assert!(cmd.contains("cd ~/development/api-abc"));
        assert!(cmd.contains("exec env DEVME_URL_HOST=vps devme tui"));
    }

    #[test]
    fn attach_presets_cover_persistence_options() {
        assert!(expand_attach("ssh", "vps", "/p", "n", "vps").contains("exec $SHELL"));
        assert!(expand_attach("tmux", "vps", "/p", "n", "vps").contains("tmux new -A -s n"));
        assert!(
            expand_attach("herdr", "vps", "/p", "sess", "vps")
                .contains("herdr --remote vps --session sess")
        );
    }

    #[test]
    fn attach_raw_template_substitutes_placeholders() {
        let cmd = expand_attach(
            "mosh {host} -- tmux a -t {name}",
            "box",
            "/p",
            "proj",
            "box",
        );
        assert_eq!(cmd, "mosh box -- tmux a -t proj");
    }

    #[test]
    fn shell_quote_passes_simple_tokens_and_quotes_the_rest() {
        assert_eq!(shell_quote("api"), "api");
        assert_eq!(shell_quote("--tail"), "--tail");
        assert_eq!(shell_quote("200"), "200");
        assert_eq!(shell_quote("svc-1.2/x:y=z"), "svc-1.2/x:y=z");
        // Tilde paths pass through so remote `cd ~/…` still expands.
        assert_eq!(shell_quote("~/development/api-abc"), "~/development/api-abc");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn herdr_commands_target_the_named_session() {
        assert_eq!(
            herdr_list_cmd("devme-api-abc"),
            "env HERDR_SESSION=devme-api-abc herdr workspace list"
        );
        let start = herdr_server_start_cmd("devme-api-abc", "~/development/api-abc");
        assert!(start.starts_with("cd ~/development/api-abc && "), "got {start}");
        assert!(start.contains("env HERDR_SESSION=devme-api-abc"));
        assert!(start.contains("nohup herdr server"));
        let create = herdr_workspace_create_cmd("s", "~/dev/x", "My App");
        assert!(create.contains("--cwd ~/dev/x"), "got {create}");
        assert!(create.contains("--label 'My App'"), "got {create}");
    }

    #[test]
    fn herdr_workspace_count_parses_cli_json_only() {
        let one = r#"{"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[{"workspace_id":"w1"}]}}"#;
        assert_eq!(herdr_workspace_count(one), Some(1));
        let empty = r#"{"id":"x","result":{"type":"workspace_list","workspaces":[]}}"#;
        assert_eq!(herdr_workspace_count(empty), Some(0));
        // Error text / no JSON → None, which the caller treats as "don't touch".
        assert_eq!(herdr_workspace_count("Error: Os { code: 2, kind: NotFound }"), None);
        assert_eq!(herdr_workspace_count(""), None);
        assert_eq!(herdr_workspace_count(r#"{"result":{}}"#), None);
    }

    #[test]
    fn url_host_strips_login_user_and_honors_override() {
        let cfg = RemoteConfig::default();
        assert_eq!(cfg.url_host_for("vps"), "vps");
        assert_eq!(cfg.url_host_for("dev@10.0.0.1"), "10.0.0.1");
        let cfg = RemoteConfig {
            url_host: Some("vps.tailnet.ts.net".into()),
            ..Default::default()
        };
        assert_eq!(cfg.url_host_for("dev@10.0.0.1"), "vps.tailnet.ts.net");
    }

    #[test]
    fn rewrite_url_host_swaps_only_loopback_authority() {
        assert_eq!(
            rewrite_url_host("http://localhost:8090", "vps"),
            "http://vps:8090"
        );
        assert_eq!(
            rewrite_url_host("http://127.0.0.1:5432/db", "vps"),
            "http://vps:5432/db"
        );
        // A non-loopback host is left alone.
        assert_eq!(
            rewrite_url_host("http://example.com:80", "vps"),
            "http://example.com:80"
        );
    }

    #[test]
    fn default_flag_round_trips() {
        let cfg = RemoteConfig {
            default: Some(true),
            ..Default::default()
        };
        assert!(cfg.is_default());
        assert!(!cfg.is_empty());
        assert!(!RemoteConfig::default().is_default());
    }

    #[test]
    fn classify_sync_prioritises_conflicts_then_down_then_healthy() {
        use SyncHealth::*;
        // A terminated/absent session is Down regardless of the rest.
        assert_eq!(classify_sync(false, None, 0), Down);
        // Conflicts dominate even a "Watching" status.
        assert_eq!(
            classify_sync(true, Some("Watching for changes"), 3),
            Conflict
        );
        // Problem words in the status → Down.
        assert_eq!(classify_sync(true, Some("Halted on root emptied"), 0), Down);
        assert_eq!(classify_sync(true, Some("Connection error"), 0), Down);
        // Normal working states are Healthy.
        assert_eq!(
            classify_sync(true, Some("Watching for changes"), 0),
            Healthy
        );
        assert_eq!(
            classify_sync(true, Some("Staging files on beta"), 0),
            Healthy
        );
        // Present but unreadable status → assume Healthy (don't cry wolf).
        assert_eq!(classify_sync(true, None, 0), Healthy);
    }

    #[test]
    fn sync_transition_is_edge_triggered() {
        use SyncHealth::*;
        // No change → silent.
        assert_eq!(sync_transition_message(Some(Healthy), Healthy, 0), None);
        // Healthy cold start → silent (no banner just for starting fine).
        assert_eq!(sync_transition_message(None, Healthy, 0), None);
        // Entering a conflict announces, with the count.
        let m = sync_transition_message(Some(Healthy), Conflict, 2).unwrap();
        assert!(m.contains("2 sync conflict") && m.contains("HALTED"), "{m}");
        // From a cold start straight into a problem still announces (reused as
        // the "remind once" text).
        assert!(sync_transition_message(None, Conflict, 1).is_some());
        assert!(sync_transition_message(None, Down, 0).is_some());
        // Recovery is celebrated only when we were previously in a problem.
        assert!(sync_transition_message(Some(Conflict), Healthy, 0).is_some());
        assert_eq!(sync_transition_message(None, Healthy, 0), None);
    }

    #[test]
    fn sync_status_line_is_glanceable() {
        use SyncHealth::*;
        assert!(sync_status_line(Conflict, 3, None).contains("3 conflict"));
        assert!(sync_status_line(Down, 0, Some("Halted")).contains("Halted"));
        assert!(sync_status_line(Healthy, 0, Some("Watching for changes")).starts_with("✓ synced"));
        assert_eq!(sync_status_line(Healthy, 0, None), "✓ synced");
    }
}
