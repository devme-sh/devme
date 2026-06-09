use std::fmt;

use serde::{Deserialize, Serialize};

/// A small integer (0..MAX_SLOT) assigned to a Stack instance at startup, used
/// to offset port allocations across coexisting worktrees.
///
/// See ADR-0006 and the `Slot` entry in `CONTEXT.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Slot(u8);

impl Slot {
    /// The maximum slot index permitted by default. Configurable via user
    /// global config; this is the compile-time ceiling.
    pub const MAX: u8 = 15;

    /// Construct a slot, returning `None` if the value exceeds `Slot::MAX`.
    pub fn new(value: u8) -> Option<Self> {
        if value <= Self::MAX {
            Some(Self(value))
        } else {
            None
        }
    }

    /// The first slot (0). Used by single-worktree projects.
    pub const ZERO: Slot = Slot(0);

    pub fn as_u8(self) -> u8 {
        self.0
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::Slot;

    #[test]
    fn new_accepts_zero_through_max() {
        for v in 0..=Slot::MAX {
            assert!(Slot::new(v).is_some(), "expected Slot::new({v}) to succeed");
        }
    }

    #[test]
    fn new_rejects_above_max() {
        assert!(Slot::new(Slot::MAX + 1).is_none());
        assert!(Slot::new(100).is_none());
        assert!(Slot::new(u8::MAX).is_none());
    }

    #[test]
    fn zero_constant_is_slot_zero() {
        assert_eq!(Slot::ZERO.as_u8(), 0);
    }

    #[test]
    fn display_formats_as_integer() {
        assert_eq!(Slot::new(0).unwrap().to_string(), "0");
        assert_eq!(Slot::new(7).unwrap().to_string(), "7");
    }

    #[test]
    fn serializes_transparently_as_integer() {
        assert_eq!(serde_json::to_string(&Slot::new(3).unwrap()).unwrap(), "3");
    }

    #[test]
    fn deserializes_from_integer() {
        let s: Slot = serde_json::from_str("5").unwrap();
        assert_eq!(s.as_u8(), 5);
    }

    #[test]
    fn deserialize_allows_above_max_without_panicking() {
        // We rely on validation at the construction site (slot allocator),
        // not at deserialization — this lets bad data round-trip and surface
        // through validation rather than serde-level errors.
        let s: Slot = serde_json::from_str("200").unwrap();
        assert_eq!(s.as_u8(), 200);
    }

    #[test]
    fn ordering() {
        assert!(Slot::new(0).unwrap() < Slot::new(1).unwrap());
        assert!(Slot::new(9).unwrap() < Slot::new(10).unwrap());
    }
}
