//! Explicit project-scoped integrations for coding-agent session startup.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::AgentTarget;

const MARKER: &str = "DEVME_AGENT_CONTEXT";

pub fn setup(root: &Path, target: AgentTarget) -> Result<Vec<(String, &'static str)>> {
    operate(root, target, Operation::Setup)
}

pub fn remove(root: &Path, target: AgentTarget) -> Result<Vec<(String, &'static str)>> {
    operate(root, target, Operation::Remove)
}

pub fn status(root: &Path, target: AgentTarget) -> Result<Vec<(String, &'static str)>> {
    operate(root, target, Operation::Status)
}

#[derive(Clone, Copy)]
enum Operation {
    Setup,
    Remove,
    Status,
}

fn operate(
    root: &Path,
    target: AgentTarget,
    operation: Operation,
) -> Result<Vec<(String, &'static str)>> {
    let targets = match target {
        AgentTarget::All => vec![
            AgentTarget::Claude,
            AgentTarget::Codex,
            AgentTarget::Opencode,
        ],
        value => vec![value],
    };
    targets
        .into_iter()
        .map(|target| {
            let name = format!("{target:?}").to_ascii_lowercase();
            let state = match (target, operation) {
                (AgentTarget::Claude, Operation::Setup) => {
                    update_json_hook(&root.join(".claude/settings.json"), true)?
                }
                (AgentTarget::Claude, Operation::Remove) => {
                    update_json_hook(&root.join(".claude/settings.json"), false)?
                }
                (AgentTarget::Claude, Operation::Status) => {
                    hook_status(&root.join(".claude/settings.json"))?
                }
                (AgentTarget::Codex, Operation::Setup) => {
                    update_codex_feature(root, true)?;
                    update_json_hook(&root.join(".codex/hooks.json"), true)?
                }
                (AgentTarget::Codex, Operation::Remove) => {
                    update_codex_feature(root, false)?;
                    update_json_hook(&root.join(".codex/hooks.json"), false)?
                }
                (AgentTarget::Codex, Operation::Status) => {
                    if hook_status(&root.join(".codex/hooks.json"))? == "installed"
                        && codex_feature_enabled(root)
                    {
                        "installed"
                    } else {
                        "absent"
                    }
                }
                (AgentTarget::Opencode, Operation::Setup) => update_opencode(root, true)?,
                (AgentTarget::Opencode, Operation::Remove) => update_opencode(root, false)?,
                (AgentTarget::Opencode, Operation::Status) => {
                    if opencode_path(root).exists() {
                        "installed"
                    } else {
                        "absent"
                    }
                }
                (AgentTarget::All, _) => unreachable!(),
            };
            Ok((name, state))
        })
        .collect()
}

fn executable() -> String {
    let current = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("devme"));
    let path_match = std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths).find_map(|dir| {
                let candidate = dir.join("devme");
                (candidate.exists()
                    && std::fs::canonicalize(candidate).ok()
                        == std::fs::canonicalize(&current).ok())
                .then_some(())
            })
        })
        .is_some();
    if path_match {
        "devme".into()
    } else {
        current.display().to_string()
    }
}

fn command() -> String {
    format!("{MARKER}=1 {} agent context", shell_quote(&executable()))
}

fn hook_entry() -> Value {
    json!({ "hooks": [{ "type": "command", "command": command() }] })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn update_json_hook(path: &Path, install: bool) -> Result<&'static str> {
    let mut root: Value = match std::fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("invalid {}", path.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e.into()),
    };
    let original = serde_json::to_string(&root)?;
    if !install {
        let Some(sessions) = root
            .get_mut("hooks")
            .and_then(Value::as_object_mut)
            .and_then(|hooks| hooks.get_mut("SessionStart"))
            .and_then(Value::as_array_mut)
        else {
            return Ok("absent");
        };
        let was_installed = sessions
            .iter()
            .any(|entry| entry.to_string().contains(MARKER));
        sessions.retain(|entry| !entry.to_string().contains(MARKER));
        if was_installed {
            std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&root)?))?;
            return Ok("removed");
        }
        return Ok("absent");
    }
    let was_installed;
    {
        let hooks = root
            .as_object_mut()
            .context("agent settings root must be an object")?
            .entry("hooks")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .context("hooks must be an object")?;
        let sessions = hooks
            .entry("SessionStart")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("hooks.SessionStart must be an array")?;
        was_installed = sessions
            .iter()
            .any(|entry| entry.to_string().contains(MARKER));
        sessions.retain(|entry| !entry.to_string().contains(MARKER));
        sessions.push(hook_entry());
    }
    let changed = original != serde_json::to_string(&root)?;
    if changed || !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&root)?))?;
    }
    Ok(if was_installed && !changed {
        "unchanged"
    } else {
        "installed"
    })
}

fn hook_status(path: &Path) -> Result<&'static str> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("absent"),
        Err(e) => return Err(e.into()),
    };
    Ok(if text.contains(MARKER) {
        "installed"
    } else {
        "absent"
    })
}

fn opencode_path(root: &Path) -> PathBuf {
    root.join(".opencode/plugins/devme.js")
}

fn codex_feature_enabled(root: &Path) -> bool {
    std::fs::read_to_string(root.join(".codex/config.toml"))
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|value| value.get("features")?.get("hooks")?.as_bool())
        .unwrap_or(false)
}

fn update_codex_feature(root: &Path, install: bool) -> Result<()> {
    let path = root.join(".codex/config.toml");
    let marker = root.join(".codex/.devme-hooks-enabled");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut value = if text.trim().is_empty() {
        toml::Value::Table(Default::default())
    } else {
        toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?
    };
    if install && !codex_feature_enabled(root) {
        value
            .as_table_mut()
            .context("Codex config root must be a table")?
            .entry("features")
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .context("features must be a table")?
            .insert("hooks".into(), toml::Value::Boolean(true));
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, toml::to_string_pretty(&value)?)?;
        std::fs::write(marker, MARKER)?;
    } else if !install && marker.exists() {
        if let Some(features) = value
            .get_mut("features")
            .and_then(toml::Value::as_table_mut)
        {
            features.remove("hooks");
        }
        std::fs::write(&path, toml::to_string_pretty(&value)?)?;
        std::fs::remove_file(marker)?;
    }
    Ok(())
}

fn update_opencode(root: &Path, install: bool) -> Result<&'static str> {
    let path = opencode_path(root);
    if !install {
        return if path.exists() {
            std::fs::remove_file(path)?;
            Ok("removed")
        } else {
            Ok("absent")
        };
    }
    let content = format!(
        "// {MARKER}\nimport {{ execFileSync }} from 'node:child_process'\nexport const DevmePlugin = async () => ({{\n  'experimental.chat.system.transform': async (_input, output) => {{\n    output.system.push(execFileSync('{}', ['agent', 'context'], {{ encoding: 'utf8' }}))\n  }}\n}})\n",
        executable().replace('\\', "\\\\").replace('\'', "\\'")
    );
    if std::fs::read_to_string(&path).ok().as_deref() == Some(&content) {
        return Ok("unchanged");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok("installed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn setup_is_idempotent_and_remove_preserves_other_settings() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".claude/settings.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"theme":"dark"}"#).unwrap();
        setup(dir.path(), AgentTarget::Claude).unwrap();
        setup(dir.path(), AgentTarget::Claude).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches(MARKER).count(), 1);
        remove(dir.path(), AgentTarget::Claude).unwrap();
        assert!(std::fs::read_to_string(path).unwrap().contains("dark"));
    }

    #[test]
    fn removing_an_absent_integration_creates_nothing() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            remove(dir.path(), AgentTarget::Claude).unwrap(),
            vec![("claude".into(), "absent")]
        );
        assert!(!dir.path().join(".claude").exists());
    }
}
