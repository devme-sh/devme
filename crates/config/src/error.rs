use thiserror::Error;

/// All ways a `devme.toml` can be invalid after parsing succeeds.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("schema_version {found} is not supported; this devme expects {expected}")]
    UnsupportedSchemaVersion { found: u32, expected: u32 },

    #[error("name '{name}' is declared as both a step and a service; they share a namespace")]
    NameCollision { name: String },

    #[error("'{from}' depends on '{to}', but no step or service with that name is declared")]
    UnknownDependency { from: String, to: String },

    #[error("dependency cycle: {cycle}")]
    Cycle { cycle: String },

    #[error("service '{name}' is declared as external but has no `health` field")]
    ExternalServiceMissingHealth { name: String },

    #[error("task '{task}' references unknown {kind} '{name}'")]
    UnknownTaskReference {
        task: String,
        kind: &'static str,
        name: String,
    },

    #[error("task dependency cycle: {cycle}")]
    TaskCycle { cycle: String },

    #[error("resource '{name}' has capacity 0; capacity must be at least 1")]
    InvalidResourceCapacity { name: String },

    #[error(
        "aggregate task '{task}' cannot declare `{field}`; aggregates contain dependencies only"
    )]
    InvalidAggregateTaskField { task: String, field: &'static str },

    #[error("service '{name}' readiness.{field} must be at least 1")]
    InvalidReadinessValue { name: String, field: &'static str },

    #[error("invalid logs.redact pattern {pattern:?}: {message}")]
    InvalidRedactionPattern { pattern: String, message: String },

    #[error("session '{session}' references unknown {kind} '{name}'")]
    UnknownSessionReference {
        session: String,
        kind: &'static str,
        name: String,
    },

    #[error(
        "session-scoped service '{service}' must belong to exactly one [session] dependency closure; found {owners}"
    )]
    SessionServiceOwnerCount { service: String, owners: usize },

    #[error(
        "non-session service '{service}' cannot depend on session-scoped service '{dependency}'"
    )]
    SessionDependencyFromOrdinary { service: String, dependency: String },

    #[error("session-scoped service '{service}' cannot be external")]
    ExternalSessionService { service: String },

    #[error(
        "session '{session}' resources '{first}' and '{second}' both expose environment variable '{env}'"
    )]
    DuplicateSessionResourceEnv {
        session: String,
        first: String,
        second: String,
        env: String,
    },

    #[error(
        "task '{task}' cannot require session-scoped service '{service}'; open its owning session"
    )]
    TaskUsesSessionService { task: String, service: String },

    #[error(
        "session '{session}' task '{task}' cannot declare resources; declare them on the session"
    )]
    SessionRunTaskResources { session: String, task: String },

    #[error(
        "session '{session}' must include service '{service}' required by run task '{task}' in its `needs` closure"
    )]
    SessionRunMissingService {
        session: String,
        task: String,
        service: String,
    },
}
