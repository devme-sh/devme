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
}
