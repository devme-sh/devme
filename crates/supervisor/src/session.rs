//! Session lifetime adapter for the shared Resource lease module.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use devme_config::Resource;
use devme_resource_lease::{LeaseOwner, ResourceLeases};
use indexmap::IndexMap;

#[derive(Debug)]
pub struct SessionLeases(ResourceLeases);

impl SessionLeases {
    pub fn try_acquire(
        resources: &IndexMap<String, Resource>,
        root: &Path,
        session: &str,
        names: &[String],
    ) -> Result<Option<Self>> {
        Ok(
            ResourceLeases::try_acquire(
                resources,
                root,
                LeaseOwner::session(session, root),
                names,
            )?
            .map(Self),
        )
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        self.0.env()
    }

    pub fn record_child(&self, pid: u32) -> Result<()> {
        self.0.record_child(pid)
    }
}
