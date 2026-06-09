use serde::{Deserialize, Serialize};

/// When to restart a Service that has exited.
///
/// See ADR-0005.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum RestartPolicy {
    /// Do not restart. The Service stays stopped after exit.
    Never,
    /// Restart only if the Service exited with a non-zero code.
    #[default]
    OnFailure,
    /// Restart regardless of exit code.
    Always,
}

#[cfg(test)]
mod tests {
    use super::RestartPolicy;

    #[test]
    fn default_is_on_failure() {
        assert_eq!(RestartPolicy::default(), RestartPolicy::OnFailure);
    }

    #[test]
    fn serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&RestartPolicy::Never).unwrap(),
            r#""never""#
        );
        assert_eq!(
            serde_json::to_string(&RestartPolicy::OnFailure).unwrap(),
            r#""on-failure""#
        );
        assert_eq!(
            serde_json::to_string(&RestartPolicy::Always).unwrap(),
            r#""always""#
        );
    }

    #[test]
    fn deserializes_kebab_case() {
        assert_eq!(
            serde_json::from_str::<RestartPolicy>(r#""never""#).unwrap(),
            RestartPolicy::Never
        );
        assert_eq!(
            serde_json::from_str::<RestartPolicy>(r#""on-failure""#).unwrap(),
            RestartPolicy::OnFailure
        );
        assert_eq!(
            serde_json::from_str::<RestartPolicy>(r#""always""#).unwrap(),
            RestartPolicy::Always
        );
    }

    #[test]
    fn rejects_snake_case_variant() {
        assert!(serde_json::from_str::<RestartPolicy>(r#""on_failure""#).is_err());
    }
}
