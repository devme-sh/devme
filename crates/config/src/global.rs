//! User-global configuration stored at `~/.config/devme/config.toml`.
//!
//! Separate from per-project `devme.toml` — this holds machine-level
//! preferences like which Docker daemon to use.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default, skip_serializing_if = "DockerConfig::is_empty")]
    pub docker: DockerConfig,
    #[serde(default, skip_serializing_if = "HintsConfig::is_empty")]
    pub hints: HintsConfig,
    #[serde(default, skip_serializing_if = "SkillConfig::is_empty")]
    pub skill: SkillConfig,
    #[serde(default, skip_serializing_if = "TuiConfig::is_empty")]
    pub tui: TuiConfig,
    #[serde(default, skip_serializing_if = "crate::RemoteConfig::is_empty")]
    pub remote: crate::RemoteConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiConfig {
    /// Colour theme for the TUI: "mocha" (dark, default), "latte" (light),
    /// or "auto" (match the terminal's background).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Ask for confirmation before quitting (which stops every service).
    /// Off by default; `q` shuts down immediately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_quit: Option<bool>,
    /// Show transient corner notifications when a service crashes or recovers.
    /// On by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toasts: Option<bool>,
}

impl TuiConfig {
    fn is_empty(&self) -> bool {
        self.theme.is_none() && self.confirm_quit.is_none() && self.toasts.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerConfig {
    /// Which Docker daemon to start when Docker isn't running.
    /// Values: "orbstack", "docker-desktop", "colima", "rancher-desktop".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<String>,
}

impl DockerConfig {
    fn is_empty(&self) -> bool {
        self.daemon.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HintsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<String>,
}

impl HintsConfig {
    fn is_empty(&self) -> bool {
        self.skills.is_none()
    }
}

/// State for the embedded AI agent skill (`devme skill install`). Tracks
/// every devme-managed install so we can detect when an upgraded binary
/// ships a newer skill — and never clobber a copy the user hand-edited or
/// installed via another tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillConfig {
    /// When true, a stale-but-unmodified devme-managed install is silently
    /// regenerated on the next interactive run instead of nudging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<bool>,
    /// The embedded skill version we last nudged about, so we nag at most
    /// once per binary version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_nudge_version: Option<String>,
    /// Absolute `SKILL.md` path → what devme last wrote there.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub installs: BTreeMap<String, SkillInstall>,
}

/// A record of one devme-written skill file: the binary version that wrote
/// it and the content hash, so we can tell "outdated" from "user-modified".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInstall {
    pub version: String,
    pub hash: String,
}

impl SkillConfig {
    fn is_empty(&self) -> bool {
        self.auto_update.is_none() && self.last_nudge_version.is_none() && self.installs.is_empty()
    }
}

impl GlobalConfig {
    pub fn load() -> Self {
        Self::load_checked().0
    }

    /// Load the config, surfacing a human-readable warning when the file
    /// exists but doesn't parse. A typo in `global.toml` previously caused
    /// every setting to be silently discarded; now we still fall back to
    /// defaults but hand the caller a diagnostic to show.
    pub fn load_checked() -> (Self, Option<String>) {
        let path = global_config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            // Missing file is the normal first-run case — not a warning.
            Err(_) => return (Self::default(), None),
        };
        match toml::from_str(&text) {
            Ok(cfg) => (cfg, None),
            Err(e) => {
                let first = e.to_string().lines().next().unwrap_or("parse error").to_string();
                (
                    Self::default(),
                    Some(format!(
                        "{} has an error ({first}); using defaults",
                        path.display()
                    )),
                )
            }
        }
    }

    #[cfg(test)]
    fn load_from(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        toml::from_str(&text).ok()
    }

    /// Surgically write `key = value` to `global.toml`, preserving the
    /// user's comments and formatting. Validates the value first (same rules
    /// as [`set`](Self::set)). Used by `devme config set` and the in-TUI
    /// settings overlay so both paths agree and neither clobbers comments.
    pub fn persist(key: &str, value: &str) -> Result<(), String> {
        // Validate against a throwaway config so a bad key/value is rejected
        // before we touch the file.
        Self::default().set(key, value)?;
        let (section, leaf) = key
            .rsplit_once('.')
            .ok_or_else(|| format!("config key must be `section.key`, got: {key}"))?;
        let rendered = render_toml_value(key, value);
        let path = global_config_path();
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let updated = crate::surgical::upsert_section_value(&content, section, leaf, &rendered);
        write_atomic(&path, &updated).map_err(|e| e.to_string())
    }

    /// Surgically remove `key` from `global.toml`.
    pub fn unset_persisted(key: &str) -> Result<(), String> {
        Self::default().unset(key)?;
        let (section, leaf) = key
            .rsplit_once('.')
            .ok_or_else(|| format!("config key must be `section.key`, got: {key}"))?;
        let path = global_config_path();
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let updated = crate::surgical::remove_section_key(&content, section, leaf);
        write_atomic(&path, &updated).map_err(|e| e.to_string())
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = global_config_path();
        self.save_to(&path)
    }

    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::other(format!("serializing config: {e}")))?;
        std::fs::write(path, text)
    }

    pub fn get(&self, key: &str) -> Option<String> {
        match key {
            "docker.daemon" => self.docker.daemon.clone(),
            "hints.skills" => self.hints.skills.clone(),
            "skill.auto_update" => self.skill.auto_update.map(|b| b.to_string()),
            "tui.theme" => self.tui.theme.clone(),
            "tui.confirm_quit" => self.tui.confirm_quit.map(|b| b.to_string()),
            "tui.toasts" => self.tui.toasts.map(|b| b.to_string()),
            "remote.host" => self.remote.host.clone(),
            "remote.root" => self.remote.root.clone(),
            "remote.sync_mode" => self.remote.sync_mode.clone(),
            "remote.attach" => self.remote.attach.clone(),
            _ => None,
        }
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "docker.daemon" => {
                self.docker.daemon = Some(value.to_string());
                Ok(())
            }
            "hints.skills" => {
                self.hints.skills = Some(value.to_string());
                Ok(())
            }
            "skill.auto_update" => {
                let b = parse_bool(value)
                    .ok_or_else(|| format!("skill.auto_update expects true/false, got: {value}"))?;
                self.skill.auto_update = Some(b);
                Ok(())
            }
            "tui.theme" => match value {
                "mocha" | "latte" | "auto" => {
                    self.tui.theme = Some(value.to_string());
                    Ok(())
                }
                _ => Err(format!("tui.theme expects mocha/latte/auto, got: {value}")),
            },
            "tui.confirm_quit" => {
                let b = parse_bool(value)
                    .ok_or_else(|| format!("tui.confirm_quit expects true/false, got: {value}"))?;
                self.tui.confirm_quit = Some(b);
                Ok(())
            }
            "tui.toasts" => {
                let b = parse_bool(value)
                    .ok_or_else(|| format!("tui.toasts expects true/false, got: {value}"))?;
                self.tui.toasts = Some(b);
                Ok(())
            }
            "remote.host" => {
                self.remote.host = Some(value.to_string());
                Ok(())
            }
            "remote.root" => {
                self.remote.root = Some(value.to_string());
                Ok(())
            }
            "remote.sync_mode" => {
                crate::remote::validate_sync_mode(value)?;
                self.remote.sync_mode = Some(value.to_string());
                Ok(())
            }
            "remote.attach" => {
                self.remote.attach = Some(value.to_string());
                Ok(())
            }
            _ => Err(format!("unknown config key: {key}")),
        }
    }

    pub fn unset(&mut self, key: &str) -> Result<(), String> {
        match key {
            "docker.daemon" => {
                self.docker.daemon = None;
                Ok(())
            }
            "hints.skills" => {
                self.hints.skills = None;
                Ok(())
            }
            "skill.auto_update" => {
                self.skill.auto_update = None;
                Ok(())
            }
            "tui.theme" => {
                self.tui.theme = None;
                Ok(())
            }
            "tui.confirm_quit" => {
                self.tui.confirm_quit = None;
                Ok(())
            }
            "tui.toasts" => {
                self.tui.toasts = None;
                Ok(())
            }
            "remote.host" => {
                self.remote.host = None;
                Ok(())
            }
            "remote.root" => {
                self.remote.root = None;
                Ok(())
            }
            "remote.sync_mode" => {
                self.remote.sync_mode = None;
                Ok(())
            }
            "remote.attach" => {
                self.remote.attach = None;
                Ok(())
            }
            _ => Err(format!("unknown config key: {key}")),
        }
    }

    pub fn keys() -> &'static [(&'static str, &'static str)] {
        &[
            ("docker.daemon", "Docker daemon to start (orbstack, docker-desktop, colima, rancher-desktop)"),
            ("hints.skills", "Show AI skill install hint (true/false)"),
            ("skill.auto_update", "Auto-update the embedded AI skill when devme updates (true/false)"),
            ("tui.theme", "TUI colour theme (mocha/latte/auto)"),
            ("tui.confirm_quit", "Confirm before quitting the TUI (true/false)"),
            ("tui.toasts", "Show service crash/recovery notifications (true/false)"),
            ("remote.host", "Remote dev host: an SSH target (Tailscale MagicDNS name, ~/.ssh/config alias, or user@host)"),
            ("remote.root", "Remote parent dir for synced projects (default ~/development)"),
            ("remote.sync_mode", "Mutagen sync mode (two-way-safe/two-way-resolved)"),
            ("remote.attach", "Attach command after sync: preset (tui/ssh/tmux/herdr) or a raw template"),
        ]
    }

    // --- Embedded-skill state (see `SkillConfig`) ---

    /// Whether stale, devme-managed skill installs should regenerate silently.
    pub fn skill_auto_update(&self) -> bool {
        self.skill.auto_update.unwrap_or(false)
    }

    /// Record that devme wrote `version`/`hash` to `path`.
    pub fn record_skill_install(&mut self, path: &str, version: &str, hash: &str) {
        self.skill.installs.insert(
            path.to_string(),
            SkillInstall { version: version.to_string(), hash: hash.to_string() },
        );
    }

    /// Drop the record for a path devme no longer manages.
    pub fn forget_skill_install(&mut self, path: &str) {
        self.skill.installs.remove(path);
    }

    pub fn skill_installs(&self) -> &BTreeMap<String, SkillInstall> {
        &self.skill.installs
    }

    pub fn skill_last_nudge(&self) -> Option<&str> {
        self.skill.last_nudge_version.as_deref()
    }

    pub fn set_skill_last_nudge(&mut self, version: &str) {
        self.skill.last_nudge_version = Some(version.to_string());
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Render a config value as TOML for surgical writes. Bools become literal
/// `true`/`false`; everything else (theme names, daemon names, the
/// string-typed `hints.skills`) is quoted.
fn render_toml_value(key: &str, value: &str) -> String {
    match key {
        "skill.auto_update" | "tui.confirm_quit" | "tui.toasts" => value.to_string(),
        _ => format!("\"{value}\""),
    }
}

/// Write `content` to `path`, creating the parent dir. Uses a temp file +
/// rename so a crash mid-write can't truncate the user's config.
fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

/// `~/.config/devme/config.toml` or `$XDG_CONFIG_HOME/devme/config.toml`.
pub fn global_config_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("devme").join("config.toml");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("devme").join("config.toml");
    }
    PathBuf::from("/tmp/devme-config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_empty_config() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = GlobalConfig::default();
        cfg.save_to(&path).unwrap();
        let loaded = GlobalConfig::load_from(&path).unwrap();
        assert_eq!(cfg, loaded);
    }

    #[test]
    fn round_trip_with_docker_daemon() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = GlobalConfig::default();
        cfg.docker.daemon = Some("orbstack".into());
        cfg.save_to(&path).unwrap();
        let loaded = GlobalConfig::load_from(&path).unwrap();
        assert_eq!(loaded.docker.daemon.as_deref(), Some("orbstack"));
    }

    #[test]
    fn get_set_unset() {
        let mut cfg = GlobalConfig::default();
        assert_eq!(cfg.get("docker.daemon"), None);
        cfg.set("docker.daemon", "orbstack").unwrap();
        assert_eq!(cfg.get("docker.daemon"), Some("orbstack".into()));
        cfg.unset("docker.daemon").unwrap();
        assert_eq!(cfg.get("docker.daemon"), None);
    }

    #[test]
    fn tui_bool_keys_round_trip_and_render_as_literals() {
        let mut cfg = GlobalConfig::default();
        assert_eq!(cfg.get("tui.toasts"), None);
        cfg.set("tui.confirm_quit", "true").unwrap();
        cfg.set("tui.toasts", "false").unwrap();
        assert_eq!(cfg.get("tui.confirm_quit"), Some("true".into()));
        assert_eq!(cfg.get("tui.toasts"), Some("false".into()));
        assert!(cfg.set("tui.toasts", "maybe").is_err());
        // Bool keys are written as TOML literals, not quoted strings.
        assert_eq!(render_toml_value("tui.confirm_quit", "true"), "true");
        assert_eq!(render_toml_value("tui.toasts", "false"), "false");
        assert_eq!(render_toml_value("tui.theme", "latte"), "\"latte\"");
    }

    #[test]
    fn remote_keys_round_trip_and_validate() {
        let mut cfg = GlobalConfig::default();
        assert_eq!(cfg.get("remote.host"), None);
        cfg.set("remote.host", "vps").unwrap();
        cfg.set("remote.root", "~/dev").unwrap();
        cfg.set("remote.sync_mode", "two-way-safe").unwrap();
        cfg.set("remote.attach", "tui").unwrap();
        assert_eq!(cfg.get("remote.host"), Some("vps".into()));
        assert_eq!(cfg.get("remote.sync_mode"), Some("two-way-safe".into()));
        // Bad sync mode is rejected at set time.
        assert!(cfg.set("remote.sync_mode", "one-way-replica").is_err());
        // Round-trips through the file.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        cfg.save_to(&path).unwrap();
        let loaded = GlobalConfig::load_from(&path).unwrap();
        assert_eq!(loaded.remote.host.as_deref(), Some("vps"));
        assert_eq!(loaded.remote.attach.as_deref(), Some("tui"));
        cfg.unset("remote.host").unwrap();
        assert_eq!(cfg.get("remote.host"), None);
    }

    #[test]
    fn set_rejects_unknown_key() {
        let mut cfg = GlobalConfig::default();
        assert!(cfg.set("unknown.key", "value").is_err());
    }

    #[test]
    fn missing_file_returns_none() {
        let loaded = GlobalConfig::load_from(Path::new("/nonexistent/config.toml"));
        assert!(loaded.is_none());
    }
}
