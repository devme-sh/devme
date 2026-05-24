//! Wire protocol for custom wizard scripts in `.devme/`.
//!
//! See ADR-0011 and the `Wizard protocol` entry in `CONTEXT.md`.
//!
//! The wizard runner spawns a script as a subprocess. The script writes
//! `WizardEvent` lines to stdout; the runner reads them, renders them in
//! the TUI, and writes the user's `WizardResponse` back to stdin.
//!
//! Same envelope discipline as IPC: `schema_version` at the top level.

use serde::{Deserialize, Serialize};

/// Events the wizard script emits to stdout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WizardEvent {
    /// Ask the user for input. Blocks until the runner writes back a
    /// matching `WizardResponse` on stdin.
    Ask(AskPrompt),
    /// Long-running operation started. `total` is `None` for indeterminate.
    ProgressStart { id: String, label: String, total: Option<u64> },
    /// Progress update. The runner advances the progress bar; if `total` was
    /// `None` initially, this is just a heartbeat.
    ProgressUpdate { id: String, current: u64, message: Option<String> },
    /// Operation finished. The progress widget for `id` collapses to a
    /// single line showing the final message.
    ProgressEnd { id: String, message: Option<String> },
    /// Print a log line in the wizard panel.
    Log { level: WizardLogLevel, message: String },
    /// Persist a key/value pair to `.devme/state.json` so later steps in
    /// the same wizard, or later launches of devme, can read it.
    SetVar { key: String, value: serde_json::Value },
    /// Wizard finished. The runner closes the script's stdin and surfaces
    /// the `summary` in the Supervisor tab.
    Done { summary: String },
}

/// Question kinds the wizard can ask. Discriminator on `type` so each form
/// can carry its own shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AskPrompt {
    /// Single-line text input.
    Text {
        id: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default)]
        required: bool,
    },
    /// Like `Text` but the runner hides the entered characters.
    Password {
        id: String,
        prompt: String,
    },
    /// Pick one option from a list.
    Choice {
        id: String,
        prompt: String,
        choices: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    /// Pick any subset of a list.
    MultiChoice {
        id: String,
        prompt: String,
        choices: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        default: Vec<String>,
    },
    /// Yes/no confirmation.
    Confirm {
        id: String,
        prompt: String,
        #[serde(default)]
        default: bool,
    },
    /// A batched multi-field form rendered on one screen. Fields are
    /// rendered in declaration order; the response carries one value per field.
    Form {
        id: String,
        title: String,
        fields: Vec<FormField>,
    },
}

/// One row in a `Form` prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormField {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum WizardLogLevel {
    Info,
    Warn,
    Error,
}

/// The runner's reply to an `Ask` event. Sent on the script's stdin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WizardResponse {
    /// The user provided a value. `id` must match the `Ask`'s id.
    Value { id: String, value: serde_json::Value },
    /// The user cancelled. The runner closes the script's stdin; the script
    /// should terminate cleanly with a non-zero exit code.
    Cancel { id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_round_trip<T>(value: T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let s = serde_json::to_string(&value).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn text_prompt_round_trips() {
        let p = AskPrompt::Text {
            id: "db_url".into(),
            prompt: "Database URL".into(),
            default: Some("postgres://localhost/dev".into()),
            required: true,
        };
        assert_eq!(json_round_trip(p.clone()), p);
    }

    #[test]
    fn text_prompt_default_omitted_when_none() {
        let p = AskPrompt::Text {
            id: "x".into(),
            prompt: "x".into(),
            default: None,
            required: false,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("default"), "got: {json}");
    }

    #[test]
    fn choice_prompt_round_trips() {
        let p = AskPrompt::Choice {
            id: "region".into(),
            prompt: "Pick region".into(),
            choices: vec!["us-east".into(), "us-west".into(), "eu-west".into()],
            default: Some("eu-west".into()),
        };
        assert_eq!(json_round_trip(p.clone()), p);
    }

    #[test]
    fn form_prompt_round_trips() {
        let p = AskPrompt::Form {
            id: "env".into(),
            title: "Configure .env".into(),
            fields: vec![
                FormField {
                    key: "DB_URL".into(),
                    label: "Database URL".into(),
                    default: Some("postgres://localhost/dev".into()),
                    required: true,
                    help: Some("Connection string for the dev database".into()),
                },
                FormField {
                    key: "GCS_BUCKET".into(),
                    label: "GCS bucket".into(),
                    default: None,
                    required: true,
                    help: None,
                },
            ],
        };
        assert_eq!(json_round_trip(p.clone()), p);
    }

    #[test]
    fn confirm_default_false() {
        let p: AskPrompt = serde_json::from_str(
            r#"{"type":"confirm","id":"q","prompt":"continue?"}"#,
        )
        .unwrap();
        assert_eq!(p, AskPrompt::Confirm { id: "q".into(), prompt: "continue?".into(), default: false });
    }

    #[test]
    fn wizard_event_round_trips_every_variant() {
        let cases: Vec<WizardEvent> = vec![
            WizardEvent::Ask(AskPrompt::Text {
                id: "x".into(),
                prompt: "x".into(),
                default: None,
                required: false,
            }),
            WizardEvent::ProgressStart {
                id: "p1".into(),
                label: "Installing".into(),
                total: Some(100),
            },
            WizardEvent::ProgressStart {
                id: "p2".into(),
                label: "Working".into(),
                total: None,
            },
            WizardEvent::ProgressUpdate {
                id: "p1".into(),
                current: 47,
                message: Some("downloading binary".into()),
            },
            WizardEvent::ProgressEnd {
                id: "p1".into(),
                message: Some("done".into()),
            },
            WizardEvent::Log {
                level: WizardLogLevel::Warn,
                message: "could not infer project name".into(),
            },
            WizardEvent::SetVar {
                key: "selected_region".into(),
                value: serde_json::json!("eu-west"),
            },
            WizardEvent::Done {
                summary: "Configured 4 services".into(),
            },
        ];
        for ev in cases {
            assert_eq!(json_round_trip(ev.clone()), ev);
        }
    }

    #[test]
    fn wizard_response_round_trips() {
        let cases = vec![
            WizardResponse::Value {
                id: "db_url".into(),
                value: serde_json::json!("postgres://localhost/dev"),
            },
            WizardResponse::Value {
                id: "env".into(),
                value: serde_json::json!({"DB_URL": "x", "GCS_BUCKET": "y"}),
            },
            WizardResponse::Cancel { id: "q".into() },
        ];
        for r in cases {
            assert_eq!(json_round_trip(r.clone()), r);
        }
    }

    #[test]
    fn rejects_unknown_event_kind() {
        let json = r#"{"kind":"bogus"}"#;
        assert!(serde_json::from_str::<WizardEvent>(json).is_err());
    }

    #[test]
    fn rejects_unknown_ask_type() {
        let json = r#"{"kind":"ask","type":"emoji_picker","id":"x","prompt":"x"}"#;
        assert!(serde_json::from_str::<WizardEvent>(json).is_err());
    }

    #[test]
    fn ask_event_wraps_prompt_with_kind_discriminator() {
        let ev = WizardEvent::Ask(AskPrompt::Confirm {
            id: "q".into(),
            prompt: "Proceed?".into(),
            default: true,
        });
        let json = serde_json::to_string(&ev).unwrap();
        // Outer kind = "ask", inner type = "confirm"
        assert!(json.contains(r#""kind":"ask""#), "got: {json}");
        assert!(json.contains(r#""type":"confirm""#), "got: {json}");
    }
}
