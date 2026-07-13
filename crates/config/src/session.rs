//! Resource-bound session composition.

use serde::{Deserialize, Serialize};

/// A narrow composition over existing services, resources, and an optional
/// one-shot task. It deliberately does not introduce another dependency graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    /// Services whose ordinary dependency closure must become ready.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// Generic scarce resources held for the complete session lifetime.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<String>,
    /// Optional task invoked after the required services become ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// Seconds to keep the session alive after its final client disconnects.
    #[serde(default = "default_linger")]
    pub linger: u64,
}

fn default_linger() -> u64 {
    30
}
