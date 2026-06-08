//! Top-level Stack config: the full result of parsing `devme.toml`.

use devme_core::{RestartPolicy, Trust};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::env_var::EnvVar;
use crate::service::Service;
use crate::step::Step;

/// Wire protocol version for `devme.toml`. Bumped on every breaking change.
pub const SCHEMA_VERSION: u32 = 1;

/// The parsed (not yet validated) shape of a `devme.toml` file.
///
/// Validation lives in [`crate::validate`] and produces a [`ValidatedStack`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stack {
    /// Schema version of this config file. Required.
    pub schema_version: u32,

    /// Optional project-level metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<StackMeta>,

    /// Declared environment variables keyed by name (e.g. `DATABASE_URL`).
    /// See ADR-0014. Resolved before step checks on every `devme` run.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, EnvVar>,

    /// Setup nodes keyed by name.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub step: IndexMap<String, Step>,

    /// Long-running nodes keyed by name.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub service: IndexMap<String, Service>,
}

impl Stack {
    /// Parse a `devme.toml` from a string. Does not validate.
    pub fn parse(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }
}

/// Optional `[stack]` metadata table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackMeta {
    /// Project name shown in the TUI sidebar and `devme status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Default `trust` for any Step that doesn't specify its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_trust: Option<Trust>,
    /// Default `restart` policy for any Service that doesn't specify its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_restart: Option<RestartPolicy>,
    /// Shell command to run once when a new worktree is created
    /// (`git worktree add`). Typical use: install deps, copy config.
    /// A `.devme-initialized` marker file prevents re-running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_create: Option<String>,

    /// Env file that declarative `[env.*]` resolution reads and writes
    /// (ADR-0014). Relative to the worktree root. Defaults to `.env.local`
    /// when unset; set to `.env` for projects whose framework loads that
    /// file directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_file: Option<String>,

    /// Shell command to run once when a worktree is being removed. The
    /// symmetric counterpart of [`on_create`](Self::on_create) — intended
    /// for tearing down per-worktree resources (e.g. dropping a cloned
    /// database).
    ///
    /// NOTE: parsed and surfaced via [`crate::Stack`], but devme does not
    /// yet invoke it automatically. devme has no always-on per-repo daemon
    /// that observes `git worktree remove`, and by the time a worktree
    /// directory disappears its `devme.toml` (and the slot/cwd context the
    /// command needs) is already gone. See `crates/tui/src/worktree.rs`
    /// (`run_on_destroy`) for the tested execution path and the wiring
    /// that's deferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_destroy: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config_with_only_schema_version() {
        let s = Stack::parse("schema_version = 1\n").unwrap();
        assert_eq!(s.schema_version, 1);
        assert!(s.stack.is_none());
        assert!(s.step.is_empty());
        assert!(s.service.is_empty());
    }

    #[test]
    fn parse_full_config() {
        let s = Stack::parse(r#"
schema_version = 1

[stack]
name = "kpi-dashboard"
description = "Internal web portal"
default_trust = "prompt"
default_restart = "on-failure"

[step.gcloud_installed]
check = "command -v gcloud"
provision = "brew install --cask google-cloud-sdk"

[step.gcloud_auth]
check = "gcloud auth application-default print-access-token >/dev/null"
provision = "gcloud auth application-default login"
depends_on = ["gcloud_installed"]

[service.db]
cmd = "docker run --rm -p {port}:5432 postgres:16"
port = { base = 5432, slot_offset = 10 }
health = { tcp = "localhost:{port}" }

[service.backend]
cmd = "uv run manage.py runserver 0.0.0.0:{port}"
port = { base = 8080, slot_offset = 10 }
depends_on = ["gcloud_auth", "db"]
health = { http = "http://localhost:{port}/health" }
"#).unwrap();

        assert_eq!(s.schema_version, 1);
        let meta = s.stack.as_ref().unwrap();
        assert_eq!(meta.name.as_deref(), Some("kpi-dashboard"));
        assert_eq!(meta.default_trust, Some(Trust::Prompt));
        assert_eq!(meta.default_restart, Some(RestartPolicy::OnFailure));

        assert_eq!(s.step.len(), 2);
        assert!(s.step.contains_key("gcloud_installed"));
        assert!(s.step.contains_key("gcloud_auth"));

        assert_eq!(s.service.len(), 2);
        assert!(s.service.contains_key("db"));
        assert!(s.service.contains_key("backend"));
    }

    #[test]
    fn declaration_order_preserved_via_indexmap() {
        // IndexMap preserves insertion order — important for the TUI tab
        // ordering and Supervisor-tab listing to match the user's config.
        let s = Stack::parse(r#"
schema_version = 1

[service.frontend]
cmd = "bun run dev"

[service.backend]
cmd = "uv run manage.py runserver"

[service.db]
cmd = "docker run postgres"
"#).unwrap();

        let names: Vec<&str> = s.service.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["frontend", "backend", "db"]);
    }

    #[test]
    fn missing_schema_version_is_a_parse_error() {
        let result = Stack::parse(r#"
[service.backend]
cmd = "true"
"#);
        assert!(result.is_err(), "expected missing schema_version to fail");
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        let result = Stack::parse(r#"
schema_version = 1
typo_field = "oops"
"#);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_unknown_stack_meta_key() {
        let result = Stack::parse(r#"
schema_version = 1

[stack]
name = "x"
unsupported_meta = true
"#);
        assert!(result.is_err());
    }

    #[test]
    fn empty_step_and_service_tables_omitted_from_serialization() {
        let s = Stack {
            schema_version: 1,
            stack: None,
            env: IndexMap::new(),
            step: IndexMap::new(),
            service: IndexMap::new(),
        };
        let toml_str = toml::to_string(&s).unwrap();
        assert!(!toml_str.contains("[step"), "got: {toml_str}");
        assert!(!toml_str.contains("[service"), "got: {toml_str}");
    }

    #[test]
    fn schema_version_constant_is_one() {
        // Guards against accidental edits — bumping requires a checklist.
        assert_eq!(SCHEMA_VERSION, 1);
    }

    #[test]
    fn parse_stack_meta_env_file_and_on_destroy() {
        let s = Stack::parse(r#"
schema_version = 1

[stack]
name = "kpi"
env_file = ".env"
on_create = "uv sync"
on_destroy = "dropdb optoscale_slot{slot}"
"#).unwrap();
        let meta = s.stack.as_ref().unwrap();
        assert_eq!(meta.env_file.as_deref(), Some(".env"));
        assert_eq!(meta.on_create.as_deref(), Some("uv sync"));
        assert_eq!(meta.on_destroy.as_deref(), Some("dropdb optoscale_slot{slot}"));
    }

    #[test]
    fn env_file_defaults_to_none_when_unset() {
        let s = Stack::parse(r#"
schema_version = 1

[stack]
name = "x"
"#).unwrap();
        assert!(s.stack.as_ref().unwrap().env_file.is_none());
    }

    #[test]
    fn parse_config_with_env_vars() {
        let s = Stack::parse(r#"
schema_version = 1

[env.DATABASE_URL]
required = true
default = "postgres://localhost/dev"
help = "Connection string for the dev database"

[env.SECRET_KEY]
generate = "openssl rand -hex 32"

[env.REGION]
choices = ["us-east-1", "eu-west-1"]
default = "eu-west-1"

[service.web]
cmd = "npm run dev"
"#).unwrap();

        assert_eq!(s.env.len(), 3);
        assert!(s.env["DATABASE_URL"].required);
        assert_eq!(s.env["SECRET_KEY"].generate.as_deref(), Some("openssl rand -hex 32"));
        assert_eq!(s.env["REGION"].choices, vec!["us-east-1", "eu-west-1"]);
    }

    #[test]
    fn env_table_omitted_from_serialization_when_empty() {
        let s = Stack {
            schema_version: 1,
            stack: None,
            env: IndexMap::new(),
            step: IndexMap::new(),
            service: IndexMap::new(),
        };
        let toml_str = toml::to_string(&s).unwrap();
        assert!(!toml_str.contains("[env"), "got: {toml_str}");
    }
}
