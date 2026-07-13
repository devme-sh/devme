//! Parsing and validation of `devme.toml` (repo config) and
//! `~/.config/devme/global.toml` (user global config).
//!
//! See `CONTEXT.md` at the repo root and ADR-0001.

pub mod browser;
pub mod docker;
mod env_var;
mod error;
pub mod global;
mod graph;
mod interpolate;
pub mod paths;
mod provision;
mod redaction;
pub mod remote;
mod service;
mod session;
pub mod skill;
mod stack;
mod step;
mod surgical;
mod task;
mod validate;
mod workspace;

pub use env_var::EnvVar;
pub use error::ConfigError;
pub use global::{GlobalConfig, SkillConfig, SkillInstall};
pub use graph::{DepStatus, Graph, GraphError, NodeKind, SatisfactionOutcome};
pub use interpolate::{InterpContext, InterpError, interpolate};
pub use provision::Provision;
pub use redaction::{Redactor, is_sensitive_key, persistence_redaction_patterns};
pub use remote::RemoteConfig;
pub use service::{Readiness, Service};
pub use session::Session;
pub use stack::{LogPolicy, SCHEMA_VERSION, Stack, StackMeta, Workspace};
pub use step::Step;
pub use task::{Resource, ResourceScope, Task};
pub use validate::{Lint, lint, validate};
pub use workspace::{Focus, Origin, ResolvedWorkspace, WorkspaceError};
