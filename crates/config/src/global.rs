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
        let path = global_config_path();
        Self::load_from(&path).unwrap_or_default()
    }

    fn load_from(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        toml::from_str(&text).ok()
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
            _ => Err(format!("unknown config key: {key}")),
        }
    }

    pub fn keys() -> &'static [(&'static str, &'static str)] {
        &[
            ("docker.daemon", "Docker daemon to start (orbstack, docker-desktop, colima, rancher-desktop)"),
            ("hints.skills", "Show AI skill install hint (true/false)"),
            ("skill.auto_update", "Auto-update the embedded AI skill when devme updates (true/false)"),
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
