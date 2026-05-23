use serde::{Deserialize, Serialize};

/// Consent policy for running a step's `provision` command.
///
/// See ADR-0002 and the `Trust level` entry in `CONTEXT.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Trust {
    /// Run the provision without asking. Safe operations only.
    Auto,
    /// Ask the user before running. Default for anything mutating.
    #[default]
    Prompt,
    /// Never auto-run; display the suggested command and let the user run it.
    Manual,
}

#[cfg(test)]
mod tests {
    use super::Trust;

    #[test]
    fn default_is_prompt() {
        assert_eq!(Trust::default(), Trust::Prompt);
    }

    #[test]
    fn serializes_to_lowercase() {
        assert_eq!(serde_json::to_string(&Trust::Auto).unwrap(), r#""auto""#);
        assert_eq!(serde_json::to_string(&Trust::Prompt).unwrap(), r#""prompt""#);
        assert_eq!(serde_json::to_string(&Trust::Manual).unwrap(), r#""manual""#);
    }

    #[test]
    fn deserializes_all_variants() {
        assert_eq!(serde_json::from_str::<Trust>(r#""auto""#).unwrap(), Trust::Auto);
        assert_eq!(serde_json::from_str::<Trust>(r#""prompt""#).unwrap(), Trust::Prompt);
        assert_eq!(serde_json::from_str::<Trust>(r#""manual""#).unwrap(), Trust::Manual);
    }

    #[test]
    fn rejects_unknown() {
        assert!(serde_json::from_str::<Trust>(r#""always""#).is_err());
    }
}
