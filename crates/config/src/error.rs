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
}
