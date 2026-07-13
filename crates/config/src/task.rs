//! Declarative one-shot task and scarce-resource configuration.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A one-shot command exposed through `devme run <name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    /// Shell command. Optional for aggregate tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Other one-shot tasks that must succeed first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Setup steps that must be satisfied before execution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<String>,
    /// Long-running services that must be ready before execution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<String>,
    /// Named scarce resources acquired in declaration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<String>,
    /// Hard execution deadline in seconds. Zero means no deadline.
    #[serde(default)]
    pub timeout: u64,
    /// Overall seconds to wait for required services to become ready.
    #[serde(default = "default_readiness_timeout")]
    pub readiness_timeout: u64,
}

fn default_readiness_timeout() -> u64 {
    60
}

/// A generic bounded resource pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    #[serde(default = "one")]
    pub capacity: u32,
    #[serde(default)]
    pub scope: ResourceScope,
    /// Environment variable receiving the allocated zero-based identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceScope {
    Host,
    Repo,
    #[default]
    Worktree,
}
