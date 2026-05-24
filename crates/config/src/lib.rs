//! Parsing and validation of `devstack.toml` (repo config) and
//! `~/.config/devstack/global.toml` (user global config).
//!
//! See `CONTEXT.md` at the repo root and ADR-0001.

mod error;
mod interpolate;
mod provision;
mod service;
mod stack;
mod step;
mod validate;

pub use error::ConfigError;
pub use interpolate::{InterpContext, InterpError, interpolate};
pub use provision::Provision;
pub use service::Service;
pub use stack::{SCHEMA_VERSION, Stack, StackMeta};
pub use step::Step;
pub use validate::validate;
