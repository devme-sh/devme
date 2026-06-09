use serde::{Deserialize, Serialize};

/// Lifetime of a service or step within devme's coordination model.
///
/// See ADR-0001 and the `Scope` entry in `CONTEXT.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Scope {
    /// One copy per Stack instance (per worktree).
    #[default]
    Instance,
    /// One copy per repo, shared across all instances of that repo.
    Repo,
}

#[cfg(test)]
mod tests {
    use super::Scope;

    #[test]
    fn default_is_instance() {
        assert_eq!(Scope::default(), Scope::Instance);
    }

    #[test]
    fn serializes_to_lowercase_string() {
        assert_eq!(
            serde_json::to_string(&Scope::Instance).unwrap(),
            r#""instance""#
        );
        assert_eq!(serde_json::to_string(&Scope::Repo).unwrap(), r#""repo""#);
    }

    #[test]
    fn deserializes_from_lowercase_string() {
        assert_eq!(
            serde_json::from_str::<Scope>(r#""instance""#).unwrap(),
            Scope::Instance
        );
        assert_eq!(
            serde_json::from_str::<Scope>(r#""repo""#).unwrap(),
            Scope::Repo
        );
    }

    #[test]
    fn rejects_unknown_variant() {
        assert!(serde_json::from_str::<Scope>(r#""global""#).is_err());
        assert!(serde_json::from_str::<Scope>(r#""Instance""#).is_err());
    }
}
