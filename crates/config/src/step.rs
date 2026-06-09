//! Step (oneshot graph node) config schema.

use devme_core::{Dependency, Trust};
use serde::{Deserialize, Serialize};

use crate::provision::Provision;

/// One Step in the Stack graph. Satisfied when `check` exits 0.
///
/// In TOML (the key becomes the Step's name):
///
/// ```toml
/// [step.gcloud_installed]
/// check = "command -v gcloud"
/// provision = "brew install --cask google-cloud-sdk"
/// trust = "prompt"
/// depends_on = ["rust_toolchain"]
/// description = "Google Cloud SDK"
/// ```
///
/// See ADR-0001, ADR-0002, and the `Step` entry in `CONTEXT.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Shell command whose exit code 0 means "satisfied."
    pub check: String,

    /// What to run when `check` fails. Optional — a Step without `provision`
    /// is purely diagnostic; it surfaces the failure but doesn't auto-fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<Provision>,

    /// Consent policy for `provision`. Defaults to [`Trust::Prompt`].
    #[serde(default)]
    pub trust: Trust,

    /// Names of other Steps or Services this Step must wait for.
    /// `Vec<Dependency>` parses each entry as either a bare string `"name"`
    /// or `"name?"` for an optional dep.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<Dependency>,

    /// Human-readable description shown in the TUI's Supervisor tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(toml_src: &str) -> Step {
        #[derive(Deserialize)]
        struct Wrap {
            step: indexmap::IndexMap<String, Step>,
        }

        let w: Wrap = toml::from_str(toml_src).unwrap();
        w.step.into_iter().next().expect("at least one step").1
    }

    #[test]
    fn minimal_step_just_check() {
        let s = parse_one(
            r#"
[step.gcloud_installed]
check = "command -v gcloud"
"#,
        );
        assert_eq!(s.check, "command -v gcloud");
        assert!(s.provision.is_none());
        assert_eq!(s.trust, Trust::Prompt);
        assert!(s.depends_on.is_empty());
        assert!(s.description.is_none());
    }

    #[test]
    fn full_step_with_all_fields() {
        let s = parse_one(
            r#"
[step.gcloud_auth]
check = "gcloud auth application-default print-access-token >/dev/null"
provision = "gcloud auth application-default login"
trust = "prompt"
depends_on = ["gcloud_installed"]
description = "gcloud ADC login"
"#,
        );
        assert_eq!(
            s.check,
            "gcloud auth application-default print-access-token >/dev/null"
        );
        assert_eq!(
            s.provision,
            Some(Provision::Shell(
                "gcloud auth application-default login".into()
            ))
        );
        assert_eq!(s.trust, Trust::Prompt);
        assert_eq!(s.depends_on, vec![Dependency::required("gcloud_installed")]);
        assert_eq!(s.description.as_deref(), Some("gcloud ADC login"));
    }

    #[test]
    fn step_with_wizard_provision() {
        let s = parse_one(
            r#"
[step.env_file]
check = "test -f .env"
provision = { wizard = ".devme/setup/env.ts" }
"#,
        );
        assert_eq!(
            s.provision,
            Some(Provision::Wizard {
                wizard: ".devme/setup/env.ts".into()
            })
        );
    }

    #[test]
    fn step_with_optional_dependency() {
        let s = parse_one(
            r#"
[step.run_migrations]
check = "test -f .migrations-done"
provision = "uv run manage.py migrate"
depends_on = ["db", "proxy?"]
"#,
        );
        assert_eq!(
            s.depends_on,
            vec![Dependency::required("db"), Dependency::optional("proxy"),]
        );
    }

    #[test]
    fn step_with_auto_trust() {
        let s = parse_one(
            r#"
[step.tmp_dir]
check = "test -d tmp"
provision = "mkdir -p tmp"
trust = "auto"
"#,
        );
        assert_eq!(s.trust, Trust::Auto);
    }

    #[test]
    fn rejects_unknown_field() {
        let result: Result<Step, _> = toml::from_str(
            r#"
check = "true"
unknown = 42
"#,
        );
        assert!(result.is_err(), "expected unknown field to be rejected");
    }
}
