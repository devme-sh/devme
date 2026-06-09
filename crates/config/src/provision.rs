//! How devme satisfies a failed Step `check`. Either runs a shell command
//! or invokes a wizard script at a relative path.

use serde::{Deserialize, Serialize};

/// In TOML:
///
/// ```toml
/// # Shell form (string):
/// provision = "brew install --cask google-cloud-sdk"
///
/// # Wizard form (table with `wizard`):
/// provision = { wizard = ".devme/setup/env.ts" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Provision {
    /// Run an arbitrary shell command. The supervisor spawns it with the
    /// repo's environment.
    Shell(String),
    /// Invoke a wizard script. Path is relative to the repo root.
    Wizard { wizard: String },
}

#[cfg(test)]
mod tests {
    use super::Provision;

    #[test]
    fn parse_shell_form_from_bare_string() {
        #[derive(serde::Deserialize)]
        struct Wrap {
            provision: Provision,
        }

        let w: Wrap = toml::from_str(r#"provision = "brew install gcloud""#).unwrap();
        let Provision::Shell(cmd) = &w.provision else {
            panic!("expected shell")
        };
        assert_eq!(cmd, "brew install gcloud");
    }

    #[test]
    fn parse_wizard_form_from_table() {
        // wrap in a key so we can use toml::from_str
        let toml_src = r#"
provision = { wizard = ".devme/setup/env.ts" }
"#;
        #[derive(serde::Deserialize)]
        struct Wrap {
            provision: Provision,
        }

        let w: Wrap = toml::from_str(toml_src).unwrap();
        let Provision::Wizard { wizard } = &w.provision else {
            panic!("expected wizard")
        };
        assert_eq!(wizard, ".devme/setup/env.ts");
    }

    #[test]
    fn round_trip_shell_form_via_json() {
        let p = Provision::Shell("mkdir -p tmp".into());
        let json = serde_json::to_string(&p).unwrap();
        let back: Provision = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn round_trip_wizard_form_via_json() {
        let p = Provision::Wizard {
            wizard: ".devme/setup.ts".into(),
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: Provision = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn rejects_unknown_form() {
        // A table with an unknown key fails (untagged + deny_unknown_fields)
        let result: Result<Provision, _> = serde_json::from_str(r#"{"unknown":"foo"}"#);
        assert!(result.is_err());
    }
}
