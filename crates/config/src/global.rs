//! User-global configuration stored at `~/.config/devme/config.toml`.
//!
//! Separate from per-project `devme.toml` — this holds machine-level
//! preferences like which Docker daemon to use.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default, skip_serializing_if = "DockerConfig::is_empty")]
    pub docker: DockerConfig,
    #[serde(default, skip_serializing_if = "HintsConfig::is_empty")]
    pub hints: HintsConfig,
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
            _ => Err(format!("unknown config key: {key}")),
        }
    }

    pub fn keys() -> &'static [(&'static str, &'static str)] {
        &[
            ("docker.daemon", "Docker daemon to start (orbstack, docker-desktop, colima, rancher-desktop)"),
            ("hints.skills", "Show AI skill install hint (true/false)"),
        ]
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
