use serde::{Deserialize, Serialize};

/// Port allocation spec for a Service.
///
/// Two forms:
///
/// ```toml
/// port = { base = 8080, slot_offset = 10 }   # slot-aware: 8080, 8090, 8100, ...
/// port = { fixed = 15432 }                    # always 15432, slot ignored
/// ```
///
/// See the `Slot` entry in `CONTEXT.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum PortSpec {
    SlotOffset { base: u16, slot_offset: u16 },
    Fixed { fixed: u16 },
}

impl PortSpec {
    /// Resolve to a concrete port given the current slot.
    pub fn resolve(self, slot: u8) -> u16 {
        match self {
            PortSpec::SlotOffset { base, slot_offset } => {
                base.saturating_add(u16::from(slot).saturating_mul(slot_offset))
            }
            PortSpec::Fixed { fixed } => fixed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PortSpec;

    #[test]
    fn slot_offset_resolves_to_base_at_slot_zero() {
        let p = PortSpec::SlotOffset {
            base: 8080,
            slot_offset: 10,
        };
        assert_eq!(p.resolve(0), 8080);
    }

    #[test]
    fn slot_offset_resolves_to_base_plus_slot_times_offset() {
        let p = PortSpec::SlotOffset {
            base: 8080,
            slot_offset: 10,
        };
        assert_eq!(p.resolve(1), 8090);
        assert_eq!(p.resolve(3), 8110);
        assert_eq!(p.resolve(9), 8170);
    }

    #[test]
    fn fixed_ignores_slot() {
        let p = PortSpec::Fixed { fixed: 15432 };
        assert_eq!(p.resolve(0), 15432);
        assert_eq!(p.resolve(5), 15432);
        assert_eq!(p.resolve(9), 15432);
    }

    #[test]
    fn deserialize_slot_offset() {
        let p: PortSpec = serde_json::from_str(r#"{"base":8080,"slot_offset":10}"#).unwrap();
        assert_eq!(
            p,
            PortSpec::SlotOffset {
                base: 8080,
                slot_offset: 10
            }
        );
    }

    #[test]
    fn deserialize_fixed() {
        let p: PortSpec = serde_json::from_str(r#"{"fixed":15432}"#).unwrap();
        assert_eq!(p, PortSpec::Fixed { fixed: 15432 });
    }

    #[test]
    fn slot_offset_saturates_on_overflow() {
        // base near u16::MAX should saturate, not wrap or panic
        let p = PortSpec::SlotOffset {
            base: 65000,
            slot_offset: 1000,
        };
        assert_eq!(p.resolve(9), u16::MAX);
    }
}
