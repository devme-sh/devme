//! Parsing and validation of `devme.toml` (repo config) and
//! `~/.config/devme/global.toml` (user global config).
//!
//! See `CONTEXT.md` at the repo root and ADR-0001.

pub mod docker;
mod env_var;
mod error;
pub mod global;
mod graph;
mod interpolate;
pub mod paths;
mod provision;
mod service;
mod stack;
mod step;
mod validate;

pub use env_var::EnvVar;
pub use error::ConfigError;
pub use global::GlobalConfig;
pub use graph::{DepStatus, Graph, GraphError, NodeKind, SatisfactionOutcome};
pub use interpolate::{InterpContext, InterpError, interpolate};
pub use provision::Provision;
pub use service::Service;
pub use stack::{SCHEMA_VERSION, Stack, StackMeta};
pub use step::Step;
pub use validate::validate;
