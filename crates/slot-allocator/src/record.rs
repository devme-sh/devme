//! On-disk format for the slot registry — a flat list of [`ClaimRecord`].
//!
//! See ADR-0006. Pure data: no I/O, no liveness checks.

use serde::{Deserialize, Serialize};

use devstack_core::Slot;

/// One worktree's claim on a slot. Persisted as a row in `slots.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRecord {
    /// The allocated slot number.
    pub slot: Slot,
    /// Stable identifier for the worktree that claimed this slot.
    ///
    /// In practice a hash of the worktree path; the allocator treats it as
    /// opaque.
    pub instance_id: String,
    /// PID of the process that holds the claim. Used to detect stale claims
    /// when the daemon dies without releasing.
    pub pid: u32,
    /// Wall-clock time the claim was made, in seconds since the UNIX epoch.
    /// Informational — not used for staleness checks.
    pub claimed_at: u64,
}

/// The full set of claims read from `slots.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    #[serde(default, rename = "claim")]
    pub claims: Vec<ClaimRecord>,
}

impl Registry {
    /// Parse a registry from a TOML string. Empty input yields an empty
    /// registry — a missing file is not an error.
    pub fn parse(toml_str: &str) -> Result<Self, toml::de::Error> {
        if toml_str.trim().is_empty() {
            return Ok(Self::default());
        }
        toml::from_str(toml_str)
    }

    pub fn serialize(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    pub fn find_by_instance(&self, instance_id: &str) -> Option<&ClaimRecord> {
        self.claims.iter().find(|c| c.instance_id == instance_id)
    }

    pub fn find_by_slot(&self, slot: Slot) -> Option<&ClaimRecord> {
        self.claims.iter().find(|c| c.slot == slot)
    }

    /// Remove any claim matching `instance_id`, returning the removed record.
    pub fn remove_by_instance(&mut self, instance_id: &str) -> Option<ClaimRecord> {
        let idx = self.claims.iter().position(|c| c.instance_id == instance_id)?;
        Some(self.claims.remove(idx))
    }

    /// Remove the claim at `slot`, returning the removed record.
    pub fn remove_by_slot(&mut self, slot: Slot) -> Option<ClaimRecord> {
        let idx = self.claims.iter().position(|c| c.slot == slot)?;
        Some(self.claims.remove(idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(slot: u8, instance: &str, pid: u32) -> ClaimRecord {
        ClaimRecord {
            slot: Slot::new(slot).unwrap(),
            instance_id: instance.into(),
            pid,
            claimed_at: 1_700_000_000,
        }
    }

    #[test]
    fn empty_string_parses_to_empty_registry() {
        let r = Registry::parse("").unwrap();
        assert!(r.claims.is_empty());
    }

    #[test]
    fn whitespace_only_parses_to_empty_registry() {
        let r = Registry::parse("   \n\n  ").unwrap();
        assert!(r.claims.is_empty());
    }

    #[test]
    fn parse_single_claim() {
        let r = Registry::parse(
            r#"
[[claim]]
slot = 2
instance_id = "abc123"
pid = 12345
claimed_at = 1700000000
"#,
        )
        .unwrap();
        assert_eq!(r.claims.len(), 1);
        assert_eq!(r.claims[0].slot, Slot::new(2).unwrap());
        assert_eq!(r.claims[0].instance_id, "abc123");
    }

    #[test]
    fn serialize_roundtrip() {
        let r = Registry {
            claims: vec![rec(0, "a", 1), rec(3, "b", 2)],
        };
        let s = r.serialize().unwrap();
        let parsed = Registry::parse(&s).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn find_by_instance_returns_match() {
        let r = Registry {
            claims: vec![rec(0, "a", 1), rec(1, "b", 2)],
        };
        assert_eq!(r.find_by_instance("b").unwrap().slot, Slot::new(1).unwrap());
        assert!(r.find_by_instance("nope").is_none());
    }

    #[test]
    fn find_by_slot_returns_match() {
        let r = Registry {
            claims: vec![rec(0, "a", 1), rec(1, "b", 2)],
        };
        assert_eq!(r.find_by_slot(Slot::new(1).unwrap()).unwrap().instance_id, "b");
        assert!(r.find_by_slot(Slot::new(5).unwrap()).is_none());
    }

    #[test]
    fn remove_by_instance_returns_removed() {
        let mut r = Registry {
            claims: vec![rec(0, "a", 1), rec(1, "b", 2)],
        };
        let removed = r.remove_by_instance("a").unwrap();
        assert_eq!(removed.slot, Slot::new(0).unwrap());
        assert_eq!(r.claims.len(), 1);
        assert!(r.remove_by_instance("a").is_none());
    }

    #[test]
    fn remove_by_slot_returns_removed() {
        let mut r = Registry {
            claims: vec![rec(0, "a", 1), rec(1, "b", 2)],
        };
        let removed = r.remove_by_slot(Slot::new(1).unwrap()).unwrap();
        assert_eq!(removed.instance_id, "b");
        assert_eq!(r.claims.len(), 1);
    }

    #[test]
    fn rejects_unknown_fields() {
        let result = Registry::parse(
            r#"
[[claim]]
slot = 0
instance_id = "x"
pid = 1
claimed_at = 0
bogus = "field"
"#,
        );
        assert!(result.is_err());
    }
}
