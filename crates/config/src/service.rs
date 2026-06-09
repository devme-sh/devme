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

    /// Optional teardown command run when the service is stopped (on
    /// `devme down` or a single-service stop), before the process is
    /// signalled. For services whose lifecycle outlives the supervised
    /// process — e.g. a `docker compose up` whose container is owned by
    /// `dockerd` — set this to the real teardown (`docker compose down`),
    /// since SIGTERM/SIGKILL of the `up` client alone leaves the container
    /// running. Interpolated with `{slot}`, `{port}`, `{worktree}`,
    /// `{branch}`, and `{port.<service>}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<String>,

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

    /// Explicit browser-open / copy URL *template*, overriding the
    /// health-check heuristic in [`Self::url_template`]. Set this for a web
    /// service that has no `http` health check (a Vite/Next dev server, say) so
    /// the TUI's `o` (open) treats it as openable instead of a bare `host:port`.
    /// `{host}` and `{port}` are interpolated like everywhere else; include the
    /// scheme (`http://{host}:{port}`, or a path like `http://{host}:{port}/app`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl Service {
    /// A copy/open URL *template* for this service, or `None` when there's
    /// nothing to point at (no port). `{host}` and `{port}` are placeholders
    /// the client fills with the reachable host (localhost, or a remote name)
    /// and the service's resolved port — kept as placeholders because the host
    /// is a client-side concern (remote TUIs rewrite it) and the port isn't
    /// known until spawn.
    ///
    /// An `http` health check means the service speaks HTTP, so it gets a
    /// browser-openable `http(s)://{host}:{port}`. Everything else (databases,
    /// other tcp services) gets a bare `{host}:{port}` — exactly what a DB
    /// client (`psql`, TablePlus, …) wants pasted in. We deliberately do *not*
    /// synthesize a full connection string: the credentials and database name
    /// aren't in the port, and scanning `env` for a URL-shaped value is unsafe
    /// (a frontend's `VITE_API_BASE_URL` points at the *backend*, not itself).
    pub fn url_template(&self) -> Option<String> {
        // An explicit `url` wins: it's the one signal that disambiguates a web
        // server from a database when neither declares an `http` health check
        // (they're otherwise identical — both just a `port`). Honoured even
        // without a port, so a fixed-URL external service still resolves.
        if let Some(url) = &self.url {
            return Some(url.clone());
        }
        self.port?;
        Some(match &self.health {
            Some(HealthCheck::Http { http }) => {
                let scheme = if http.starts_with("https://") {
                    "https"
                } else {
                    "http"
                };
                format!("{scheme}://{{host}}:{{port}}")
            }
            _ => "{host}:{port}".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(toml_src: &str) -> Service {
        #[derive(Deserialize)]
        struct Wrap {
            service: indexmap::IndexMap<String, Service>,
        }

        let w: Wrap = toml::from_str(toml_src).unwrap();
        w.service
            .into_iter()
            .next()
            .expect("at least one service")
            .1
    }

    #[test]
    fn minimal_service_just_cmd() {
        let s = parse_one(
            r#"
[service.backend]
cmd = "uv run manage.py runserver"
"#,
        );
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
        let s = parse_one(
            r#"
[service.backend]
cmd = "uv run manage.py runserver 0.0.0.0:{port}"
cwd = "backend"
stop = "docker compose down"
env = { DATABASE_URL = "postgres://localhost/dev", DEBUG = "1" }
port = { base = 8080, slot_offset = 10 }
scope = "instance"
depends_on = ["db", "proxy?"]
restart = "on-failure"
health = { http = "http://localhost:{port}/health" }
description = "Django dev server"
"#,
        );
        assert_eq!(s.cmd, "uv run manage.py runserver 0.0.0.0:{port}");
        assert_eq!(s.cwd.as_deref(), Some("backend"));
        assert_eq!(s.stop.as_deref(), Some("docker compose down"));
        assert_eq!(
            s.env.get("DATABASE_URL").unwrap(),
            "postgres://localhost/dev"
        );
        assert_eq!(s.env.get("DEBUG").unwrap(), "1");
        assert_eq!(
            s.port,
            Some(PortSpec::SlotOffset {
                base: 8080,
                slot_offset: 10
            })
        );
        assert_eq!(s.scope, Scope::Instance);
        assert_eq!(
            s.depends_on,
            vec![Dependency::required("db"), Dependency::optional("proxy"),]
        );
        assert_eq!(s.restart, RestartPolicy::OnFailure);
        assert!(matches!(s.health, Some(HealthCheck::Http { .. })));
    }

    #[test]
    fn url_template_picks_scheme_from_health() {
        // http health → browser-openable http URL.
        let web = parse_one(
            r#"
[service.backend]
cmd = "runserver 0.0.0.0:{port}"
port = { base = 8080, slot_offset = 10 }
health = { http = "http://localhost:{port}/health" }
"#,
        );
        assert_eq!(web.url_template().as_deref(), Some("http://{host}:{port}"));

        // https health → https.
        let secure = parse_one(
            r#"
[service.api]
cmd = "serve"
port = { fixed = 8443 }
health = { http = "https://localhost:{port}/" }
"#,
        );
        assert_eq!(
            secure.url_template().as_deref(),
            Some("https://{host}:{port}")
        );

        // tcp/shell health (a database) → bare host:port for DB clients.
        let db = parse_one(
            r#"
[service.postgres]
cmd = "docker compose up db"
port = { fixed = 5433 }
health = { tcp = "localhost:{port}" }
"#,
        );
        assert_eq!(db.url_template().as_deref(), Some("{host}:{port}"));

        // No port → nothing to point at.
        let portless = parse_one(
            r#"
[service.worker]
cmd = "run-worker"
"#,
        );
        assert_eq!(portless.url_template(), None);
    }

    #[test]
    fn explicit_url_overrides_health_heuristic() {
        // A web dev server with no http health check would otherwise be
        // misread as a bare `host:port` (a database) and refuse to open.
        // An explicit `url` makes it openable.
        let frontend = parse_one(
            r#"
[service.frontend]
cmd = "npm run dev"
port = { base = 5173, slot_offset = 10 }
url = "http://{host}:{port}"
"#,
        );
        assert_eq!(
            frontend.url_template().as_deref(),
            Some("http://{host}:{port}")
        );

        // `url` wins even over an http health check pointing elsewhere — the
        // user said exactly where to open.
        let proxied = parse_one(
            r#"
[service.web]
cmd = "serve"
port = { fixed = 8080 }
health = { http = "http://localhost:{port}/healthz" }
url = "http://{host}:{port}/app"
"#,
        );
        assert_eq!(
            proxied.url_template().as_deref(),
            Some("http://{host}:{port}/app")
        );
    }

    #[test]
    fn repo_scoped_service() {
        let s = parse_one(
            r#"
[service.proxy]
cmd = "cloud-sql-proxy --port {port} my-project:eu-west-1:db"
scope = "repo"
port = { fixed = 15432 }
restart = "always"
"#,
        );
        assert_eq!(s.scope, Scope::Repo);
        assert_eq!(s.port, Some(PortSpec::Fixed { fixed: 15432 }));
        assert_eq!(s.restart, RestartPolicy::Always);
    }

    #[test]
    fn external_service() {
        let s = parse_one(
            r#"
[service.postgres]
cmd = ""
external = true
health = { tcp = "localhost:5432" }
log_tail = "/usr/local/var/log/postgresql.log"
"#,
        );
        assert!(s.external);
        assert!(matches!(s.health, Some(HealthCheck::Tcp { .. })));
        assert_eq!(
            s.log_tail.as_deref(),
            Some("/usr/local/var/log/postgresql.log")
        );
    }

    #[test]
    fn rejects_unknown_field() {
        let result: Result<Service, _> = toml::from_str(
            r#"
cmd = "true"
unexpected = 1
"#,
        );
        assert!(result.is_err());
    }
}
