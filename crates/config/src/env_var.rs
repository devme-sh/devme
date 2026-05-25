//! Declarative environment variable schema for `devme.toml`.
//!
//! See ADR-0014. Each `[env.<NAME>]` entry declares an expected env var
//! with optional metadata used to prompt developers on first run or when
//! a new variable appears.

use serde::{Deserialize, Serialize};

/// One declared environment variable in `[env.<NAME>]`.
///
/// ```toml
/// [env.DATABASE_URL]
/// required = true
/// default = "postgresql://devme:devme@localhost:5432/devme_web"
/// help = "Connection string. Default matches the docker container devme starts."
///
/// [env.BETTER_AUTH_SECRET]
/// generate = "npx -y @better-auth/cli secret"
/// help = "Auth signing key. Auto-generated on first run."
///
/// [env.VITE_POSTHOG_HOST]
/// choices = ["https://us.i.posthog.com", "https://eu.i.posthog.com"]
/// default = "https://us.i.posthog.com"
/// help = "PostHog ingestion endpoint."
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvVar {
    /// If true, devme blocks startup until this var has a value.
    /// If false, the user can skip the prompt.
    #[serde(default)]
    pub required: bool,

    /// Pre-filled value shown in the prompt. Supports `{service.port}`
    /// interpolation (resolved after port allocation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    /// Shown below the prompt — explains where to find the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,

    /// Shell command whose stdout becomes the value. Runs automatically
    /// if the var is missing and the user doesn't provide manual input.
    /// Gated by the same consent model as step provisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate: Option<String>,

    /// Renders a selector instead of free-text input. The user picks one.
    /// If `default` is set, it's pre-selected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_env(toml_src: &str) -> indexmap::IndexMap<String, EnvVar> {
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default)]
            env: indexmap::IndexMap<String, EnvVar>,
        }
        let w: Wrap = toml::from_str(toml_src).unwrap();
        w.env
    }

    #[test]
    fn minimal_env_var() {
        let vars = parse_env(r#"
[env.API_KEY]
"#);
        let v = &vars["API_KEY"];
        assert!(!v.required);
        assert!(v.default.is_none());
        assert!(v.help.is_none());
        assert!(v.generate.is_none());
        assert!(v.choices.is_empty());
    }

    #[test]
    fn full_env_var_with_all_fields() {
        let vars = parse_env(r#"
[env.DATABASE_URL]
required = true
default = "postgres://localhost/dev"
help = "Connection string for the dev database"
generate = "echo postgres://localhost/dev"
choices = ["postgres://localhost/dev", "postgres://localhost/test"]
"#);
        let v = &vars["DATABASE_URL"];
        assert!(v.required);
        assert_eq!(v.default.as_deref(), Some("postgres://localhost/dev"));
        assert_eq!(v.help.as_deref(), Some("Connection string for the dev database"));
        assert_eq!(v.generate.as_deref(), Some("echo postgres://localhost/dev"));
        assert_eq!(v.choices, vec!["postgres://localhost/dev", "postgres://localhost/test"]);
    }

    #[test]
    fn env_var_with_generate_only() {
        let vars = parse_env(r#"
[env.SECRET_KEY]
generate = "openssl rand -hex 32"
help = "Auto-generated signing key"
"#);
        let v = &vars["SECRET_KEY"];
        assert!(!v.required);
        assert_eq!(v.generate.as_deref(), Some("openssl rand -hex 32"));
    }

    #[test]
    fn env_var_with_choices() {
        let vars = parse_env(r#"
[env.REGION]
choices = ["us-east-1", "eu-west-1", "ap-southeast-1"]
default = "eu-west-1"
help = "AWS region for the dev environment"
"#);
        let v = &vars["REGION"];
        assert_eq!(v.choices.len(), 3);
        assert_eq!(v.default.as_deref(), Some("eu-west-1"));
    }

    #[test]
    fn declaration_order_preserved() {
        let vars = parse_env(r#"
[env.FIRST]
help = "first"

[env.SECOND]
help = "second"

[env.THIRD]
help = "third"
"#);
        let names: Vec<&str> = vars.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["FIRST", "SECOND", "THIRD"]);
    }

    #[test]
    fn rejects_unknown_field() {
        let result: Result<EnvVar, _> = toml::from_str(r#"
required = true
bogus_field = "oops"
"#);
        assert!(result.is_err());
    }

    #[test]
    fn empty_choices_omitted_from_serialization() {
        let v = EnvVar {
            required: false,
            default: None,
            help: None,
            generate: None,
            choices: Vec::new(),
        };
        let s = toml::to_string(&v).unwrap();
        assert!(!s.contains("choices"), "got: {s}");
    }
}
