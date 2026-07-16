//! Declarative one-shot task and scarce-resource configuration.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A one-shot command exposed through `devme run <name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    /// Semantic purpose used by interactive discovery surfaces.
    #[serde(default)]
    pub kind: TaskKind,
    /// Whether the task belongs on the interactive Home screen.
    #[serde(default)]
    pub visibility: TaskVisibility,
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
    /// Root-relative or absolute paths produced by this task. Devme reports
    /// them but does not upload, retain, or otherwise manage their lifecycle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    /// Hard execution deadline in seconds. Zero means no deadline.
    #[serde(default)]
    pub timeout: u64,
    /// Overall seconds to wait for required services to become ready.
    #[serde(default = "default_readiness_timeout")]
    pub readiness_timeout: u64,
}

/// Controls only human Home-screen discovery. Internal tasks remain public to
/// the CLI, dependency graph, and agent-facing task catalog.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskVisibility {
    #[default]
    Home,
    Internal,
}

/// Small, stable vocabulary for grouping tasks without changing execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskKind {
    Launch,
    Check,
    #[default]
    Utility,
}

impl TaskKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Launch => "Run",
            Self::Check => "Check",
            Self::Utility => "Utilities",
        }
    }
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
