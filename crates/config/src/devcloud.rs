//! User-global `devcloud` settings.
//!
//! devcloud is the remote project context adapter. These settings describe
//! where that adapter connects and where Git-derived project paths live on
//! the remote host; devme stack supervision does not read them.

use serde::{Deserialize, Serialize};

const DEFAULT_HOST: &str = "vps";
const DEFAULT_ROOT: &str = "~/development/projects";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevcloudConfig {
    /// SSH target for the remote development host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Remote source root. Canonical project paths are derived beneath this
    /// root as `<root>/<provider-host>/<owner-or-group>/<repo>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

impl DevcloudConfig {
    pub fn is_empty(&self) -> bool {
        self.host.is_none() && self.root.is_none()
    }

    pub fn host_or_default(&self) -> &str {
        self.host.as_deref().unwrap_or(DEFAULT_HOST)
    }

    pub fn root_or_default(&self) -> &str {
        self.root.as_deref().unwrap_or(DEFAULT_ROOT)
    }
}
