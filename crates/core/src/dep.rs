use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A reference to another Step or Service that this one depends on.
///
/// In config, dependencies can be written in three forms:
///
/// ```toml
/// depends_on = ["db"]                            # required
/// depends_on = ["proxy?"]                        # optional (cargo-feature style)
/// depends_on = [{ name = "proxy", required = false }]   # object form
/// ```
///
/// See ADR-0005 and the `Optional dependency` entry in `CONTEXT.md`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dependency {
    pub name: String,
    pub required: bool,
}

impl Dependency {
    /// Construct a required dependency (the default form).
    pub fn required(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            required: true,
        }
    }

    /// Construct an optional dependency. Equivalent to writing `"name?"` in TOML.
    pub fn optional(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            required: false,
        }
    }
}

impl fmt::Display for Dependency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.required {
            f.write_str(&self.name)
        } else {
            write!(f, "{}?", self.name)
        }
    }
}

/// Object form used only for deserialization fallback.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DependencyObject {
    name: String,
    #[serde(default = "default_required")]
    required: bool,
}

fn default_required() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DependencyRepr {
    Short(String),
    Object(DependencyObject),
}

impl<'de> Deserialize<'de> for Dependency {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match DependencyRepr::deserialize(d)? {
            DependencyRepr::Short(s) => Ok(parse_short(&s)),
            DependencyRepr::Object(o) => Ok(Self {
                name: o.name,
                required: o.required,
            }),
        }
    }
}

impl Serialize for Dependency {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

fn parse_short(s: &str) -> Dependency {
    if let Some(name) = s.strip_suffix('?') {
        Dependency::optional(name)
    } else {
        Dependency::required(s)
    }
}

#[cfg(test)]
mod tests {
    use super::Dependency;

    #[test]
    fn required_constructor() {
        let d = Dependency::required("db");
        assert_eq!(d.name, "db");
        assert!(d.required);
    }

    #[test]
    fn optional_constructor() {
        let d = Dependency::optional("proxy");
        assert_eq!(d.name, "proxy");
        assert!(!d.required);
    }

    #[test]
    fn deserializes_required_from_bare_string() {
        let d: Dependency = serde_json::from_str(r#""db""#).unwrap();
        assert_eq!(d, Dependency::required("db"));
    }

    #[test]
    fn deserializes_optional_from_question_suffix() {
        let d: Dependency = serde_json::from_str(r#""proxy?""#).unwrap();
        assert_eq!(d, Dependency::optional("proxy"));
    }

    #[test]
    fn deserializes_required_from_object() {
        let d: Dependency = serde_json::from_str(r#"{"name":"db","required":true}"#).unwrap();
        assert_eq!(d, Dependency::required("db"));
    }

    #[test]
    fn deserializes_optional_from_object() {
        let d: Dependency = serde_json::from_str(r#"{"name":"proxy","required":false}"#).unwrap();
        assert_eq!(d, Dependency::optional("proxy"));
    }

    #[test]
    fn object_form_defaults_required_true() {
        let d: Dependency = serde_json::from_str(r#"{"name":"db"}"#).unwrap();
        assert_eq!(d, Dependency::required("db"));
    }

    #[test]
    fn deserializes_mixed_list() {
        let v: Vec<Dependency> =
            serde_json::from_str(r#"["db", "proxy?", {"name":"redis","required":false}]"#).unwrap();
        assert_eq!(
            v,
            vec![
                Dependency::required("db"),
                Dependency::optional("proxy"),
                Dependency::optional("redis"),
            ]
        );
    }

    #[test]
    fn serializes_to_compact_string_form() {
        assert_eq!(
            serde_json::to_string(&Dependency::required("db")).unwrap(),
            r#""db""#
        );
        assert_eq!(
            serde_json::to_string(&Dependency::optional("proxy")).unwrap(),
            r#""proxy?""#
        );
    }

    #[test]
    fn display_matches_serialized_form() {
        assert_eq!(Dependency::required("db").to_string(), "db");
        assert_eq!(Dependency::optional("proxy").to_string(), "proxy?");
    }

    #[test]
    fn round_trip_required() {
        let d = Dependency::required("db");
        let s = serde_json::to_string(&d).unwrap();
        let back: Dependency = serde_json::from_str(&s).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn round_trip_optional() {
        let d = Dependency::optional("proxy");
        let s = serde_json::to_string(&d).unwrap();
        let back: Dependency = serde_json::from_str(&s).unwrap();
        assert_eq!(d, back);
    }
}
