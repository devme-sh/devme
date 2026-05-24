//! Service (long-running graph node) config schema.

use std::collections::BTreeMap;

use devme_core::{Dependency, HealthCheck, PortSpec, RestartPolicy, Scope};
use serde::{Deserialize, Serialize};

/// One Service in the Stack graph. Stays alive after starting.
///
/// In TOML:
///
/// ```toml
/// [service.backend]
/// cmd = "uv run manage.py runserver 0.0.0.0:{port}"
/// port = { base = 8080, slot_offset = 10 }
/// scope = "instance"
/// depends_on = ["db", "proxy?"]
/// restart = "on-failure"
/// env = { DATABASE_URL = "postgres://localhost/dev" }
/// health = { http = "http://localhost:{port}/health" }
/// ```
///
/// See ADR-0001, ADR-0005, and the `Service` entry in `CONTEXT.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// Shell command to spawn. `{port}` is interpolated from the resolved
    /// [`PortSpec`] if one is declared.
    pub cmd: String,

    /// Working directory for the spawned process. Defaults to the worktree
    /// root (the directory containing `devme.toml`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    /// Environment variables to add to the process's environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,

    /// Port allocation template. Required when `cmd` references `{port}`.
    /// Optional when the service binds a port via env or has no port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<PortSpec>,

    /// Lifetime scope. Defaults to [`Scope::Instance`].
    #[serde(default)]
    pub scope: Scope,

    /// Names of other Steps or Services this Service must wait for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<Dependency>,

    /// When the supervisor should restart this service after exit.
    /// Defaults to [`RestartPolicy::OnFailure`].
    #[serde(default)]
    pub restart: RestartPolicy,

    /// If true, devme never manages the lifecycle — only health-checks.
    /// `health` becomes required when this is set.
    #[serde(default)]
    pub external: bool,

    /// How to determine whether the service is healthy. Required when
    /// `external = true`; advisory otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthCheck>,

    /// Path to a log file to tail. Only meaningful when `external = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_tail: Option<String>,

    /// Human-readable description shown in the TUI tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(toml_src: &str) -> Service {
        #[derive(Deserialize)]
        struct Wrap { service: indexmap::IndexMap<String, Service> }

        let w: Wrap = toml::from_str(toml_src).unwrap();
        w.service.into_iter().next().expect("at least one service").1
    }

    #[test]
    fn minimal_service_just_cmd() {
        let s = parse_one(r#"
[service.backend]
cmd = "uv run manage.py runserver"
"#);
        assert_eq!(s.cmd, "uv run manage.py runserver");
        assert!(s.cwd.is_none());
        assert!(s.env.is_empty());
        assert!(s.port.is_none());
        assert_eq!(s.scope, Scope::Instance);
        assert!(s.depends_on.is_empty());
        assert_eq!(s.restart, RestartPolicy::OnFailure);
        assert!(!s.external);
        assert!(s.health.is_none());
    }

    #[test]
    fn full_service_with_all_fields() {
        let s = parse_one(r#"
[service.backend]
cmd = "uv run manage.py runserver 0.0.0.0:{port}"
cwd = "backend"
env = { DATABASE_URL = "postgres://localhost/dev", DEBUG = "1" }
port = { base = 8080, slot_offset = 10 }
scope = "instance"
depends_on = ["db", "proxy?"]
restart = "on-failure"
health = { http = "http://localhost:{port}/health" }
description = "Django dev server"
"#);
        assert_eq!(s.cmd, "uv run manage.py runserver 0.0.0.0:{port}");
        assert_eq!(s.cwd.as_deref(), Some("backend"));
        assert_eq!(s.env.get("DATABASE_URL").unwrap(), "postgres://localhost/dev");
        assert_eq!(s.env.get("DEBUG").unwrap(), "1");
        assert_eq!(s.port, Some(PortSpec::SlotOffset { base: 8080, slot_offset: 10 }));
        assert_eq!(s.scope, Scope::Instance);
        assert_eq!(s.depends_on, vec![
            Dependency::required("db"),
            Dependency::optional("proxy"),
        ]);
        assert_eq!(s.restart, RestartPolicy::OnFailure);
        assert!(matches!(s.health, Some(HealthCheck::Http { .. })));
    }

    #[test]
    fn repo_scoped_service() {
        let s = parse_one(r#"
[service.proxy]
cmd = "cloud-sql-proxy --port {port} my-project:eu-west-1:db"
scope = "repo"
port = { fixed = 15432 }
restart = "always"
"#);
        assert_eq!(s.scope, Scope::Repo);
        assert_eq!(s.port, Some(PortSpec::Fixed { fixed: 15432 }));
        assert_eq!(s.restart, RestartPolicy::Always);
    }

    #[test]
    fn external_service() {
        let s = parse_one(r#"
[service.postgres]
cmd = ""
external = true
health = { tcp = "localhost:5432" }
log_tail = "/usr/local/var/log/postgresql.log"
"#);
        assert!(s.external);
        assert!(matches!(s.health, Some(HealthCheck::Tcp { .. })));
        assert_eq!(s.log_tail.as_deref(), Some("/usr/local/var/log/postgresql.log"));
    }

    #[test]
    fn rejects_unknown_field() {
        let result: Result<Service, _> = toml::from_str(r#"
cmd = "true"
unexpected = 1
"#);
        assert!(result.is_err());
    }
}
