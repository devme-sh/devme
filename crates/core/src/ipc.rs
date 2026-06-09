//! Wire protocol for talking to a Daemon over a Unix socket.
//!
//! See ADR-0008 (envelope schema, semantic exit codes, agent-context contract).
//!
//! Wire shape: length-prefixed JSON-lines. Each envelope is one line.
//!
//! ```json
//! { "schema_version": 1, "kind": "log_chunk", "service": "backend", "bytes": "...", "ts": 17_300_000_000 }
//! ```
//!
//! `schema_version` is at the top level so a Client can verify compatibility
//! before parsing the payload; breaking schema changes bump the version.

use serde::{Deserialize, Serialize};

use crate::{ServiceState, StepState};

/// Current wire protocol version. Bumped on any breaking schema change.
pub const SCHEMA_VERSION: u32 = 1;

/// Which standard stream a log line came from. devme runs each service under
/// two PTYs so stdout and stderr stay distinguishable — errors and tracebacks
/// almost always go to stderr — while both fds still see a terminal (so color
/// and progress rendering behave exactly as in a real shell). Carried on every
/// [`ServerMessage::LogChunk`]; `#[serde(default)]` keeps it optional on the
/// wire so a line without it reads as `Stdout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    #[default]
    Stdout,
    Stderr,
}

impl LogStream {
    /// True for the error stream — the cheap, ground-truth signal the error
    /// digest anchors on (no keyword guessing).
    pub fn is_stderr(self) -> bool {
        matches!(self, LogStream::Stderr)
    }
}

/// Wraps a Client or Server message with its protocol version. The version
/// lives in a known field so that downgraded clients can reject mismatches
/// before attempting to parse a payload whose shape they don't know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub schema_version: u32,
    #[serde(flatten)]
    pub payload: T,
}

impl<T> Envelope<T> {
    /// Wrap a payload with the current schema version.
    pub fn new(payload: T) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            payload,
        }
    }
}

/// Messages sent from a Client (TUI, CLI subcommand, agent) to a Daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    /// Subscribe to log + status streams for the named services. Empty list
    /// means all services.
    Subscribe { services: Vec<String> },
    /// Cancel a prior subscription. Does not disconnect.
    Unsubscribe,
    /// Restart the named service (kill, wait, respawn).
    Restart { service: String },
    /// Stop the named service.
    Stop { service: String },
    /// Start the named service, optionally bypassing required dependencies.
    Start {
        service: String,
        #[serde(default)]
        skip_deps: bool,
    },
    /// Re-run health checks across the whole graph. Equivalent to
    /// `devme health --recheck`.
    RecheckHealth,
    /// Graceful daemon shutdown. The daemon stops services and exits.
    /// Distinct from client disconnect; the latter only decrements ref-count.
    Shutdown,
}

/// Identity of the daemon a client is connected to. Carried in `Subscribed`
/// so the TUI can label tabs and route per-instance actions without having
/// to hash the socket path itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceInfo {
    /// Stable hash of the worktree path. Matches
    /// [`devme_config::paths::instance_id`].
    pub id: String,
    /// Human-friendly label — typically the basename of the worktree.
    pub label: String,
    /// Absolute cwd the daemon was started in. Useful for the TUI to show
    /// a tooltip / detail row and for clients to disambiguate identical
    /// labels (e.g. two worktrees of a repo named "api").
    pub cwd: String,
}

/// Messages sent from a Daemon to a Client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    /// Acknowledgement of a successful `Subscribe`. Carries the initial
    /// snapshot of service states so the client can render without first
    /// asking for status.
    Subscribed {
        instance: InstanceInfo,
        services: Vec<ServiceSnapshot>,
        steps: Vec<StepSnapshot>,
    },
    /// A chunk of output from a service's PTY. `bytes` is base64-encoded so
    /// the wire format stays valid UTF-8 even when the PTY emits non-UTF-8
    /// bytes (terminal control sequences, partial multibyte chars).
    LogChunk {
        service: String,
        bytes: String, // base64-encoded
        ts: u64,       // milliseconds since UNIX epoch
        /// Which PTY the line came from (stdout vs stderr). Optional on the
        /// wire (`default = stdout`) so the field is additive within v1.
        #[serde(default)]
        stream: LogStream,
    },
    /// A service's state changed.
    StatusUpdate {
        service: String,
        state: ServiceState,
        pid: Option<u32>,
        port: Option<u16>,
        restart_count: u32,
    },
    /// A step's state changed.
    StepStatusUpdate { step: String, state: StepState },
    /// A non-fatal error or warning the daemon wants the client to surface.
    Notice { level: NoticeLevel, message: String },
    /// Fatal error specific to a Client request (bad service name, etc.).
    /// The connection stays open; the Client may try a different request.
    Error { code: ErrorCode, message: String },
    /// The daemon is exiting; the Client should expect the socket to close.
    Goodbye { reason: String },
}

/// One service's state in a `Subscribed` snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSnapshot {
    pub name: String,
    pub state: ServiceState,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    /// Copy/open URL template (`{host}`/`{port}` placeholders) for the
    /// open/copy-URL actions, or `None` when there's nothing to point at. See
    /// `Service::url_template`. Carried only in the `Subscribed` snapshot — it
    /// derives from static config, so `StatusUpdate` need not repeat it; the
    /// client patches port/state onto the existing entry and keeps this.
    #[serde(default)]
    pub url: Option<String>,
    pub restart_count: u32,
}

/// One step's state in a `Subscribed` snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepSnapshot {
    pub name: String,
    pub state: StepState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

/// Discriminated error categories carried in `ServerMessage::Error`. These
/// align with the CLI's semantic exit codes (see ADR-0008) so that
/// `devme`'s exit code is a direct projection of the last server error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ErrorCode {
    /// Bad arguments to a request. Exit code 2.
    Usage,
    /// Service, step, or instance not found. Exit code 3.
    NotFound,
    /// Permission denied. Exit code 4.
    Permission,
    /// Conflict (port in use, slot collision, ownership conflict). Exit 5.
    Conflict,
    /// Generic internal error. Exit code 1.
    Internal,
}

impl ErrorCode {
    /// The CLI exit code that should be emitted when this error reaches the
    /// command line. See ADR-0008.
    pub fn cli_exit_code(self) -> i32 {
        match self {
            ErrorCode::Usage => 2,
            ErrorCode::NotFound => 3,
            ErrorCode::Permission => 4,
            ErrorCode::Conflict => 5,
            ErrorCode::Internal => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_round_trip<T>(msg: T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let json = serde_json::to_string(&msg).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn envelope_includes_schema_version_at_top_level() {
        let env = Envelope::new(ClientMessage::Unsubscribe);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""schema_version":1"#), "got: {json}");
        assert!(json.contains(r#""kind":"unsubscribe""#), "got: {json}");
    }

    #[test]
    fn envelope_flattens_payload_no_nested_object() {
        // Verifies the flatten attribute: kind is at top level, not under "payload"
        let env = Envelope::new(ClientMessage::Restart {
            service: "backend".into(),
        });
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains(r#""payload":"#), "got: {json}");
        assert!(json.contains(r#""service":"backend""#));
    }

    #[test]
    fn client_message_subscribe_round_trips() {
        let m = ClientMessage::Subscribe {
            services: vec!["backend".into(), "frontend".into()],
        };
        assert_eq!(json_round_trip(m.clone()), m);
    }

    #[test]
    fn client_message_start_skip_deps_defaults_false() {
        let m: ClientMessage =
            serde_json::from_str(r#"{"kind":"start","service":"backend"}"#).unwrap();
        assert_eq!(
            m,
            ClientMessage::Start {
                service: "backend".into(),
                skip_deps: false
            }
        );
    }

    #[test]
    fn client_message_start_with_skip_deps_true() {
        let m: ClientMessage =
            serde_json::from_str(r#"{"kind":"start","service":"backend","skip_deps":true}"#)
                .unwrap();
        assert_eq!(
            m,
            ClientMessage::Start {
                service: "backend".into(),
                skip_deps: true
            }
        );
    }

    #[test]
    fn client_message_round_trips_every_variant() {
        let cases = vec![
            ClientMessage::Subscribe { services: vec![] },
            ClientMessage::Subscribe {
                services: vec!["a".into(), "b".into()],
            },
            ClientMessage::Unsubscribe,
            ClientMessage::Restart {
                service: "backend".into(),
            },
            ClientMessage::Stop {
                service: "backend".into(),
            },
            ClientMessage::Start {
                service: "backend".into(),
                skip_deps: false,
            },
            ClientMessage::Start {
                service: "backend".into(),
                skip_deps: true,
            },
            ClientMessage::RecheckHealth,
            ClientMessage::Shutdown,
        ];
        for m in cases {
            assert_eq!(json_round_trip(m.clone()), m);
        }
    }

    #[test]
    fn server_message_round_trips_every_variant() {
        let test_instance = InstanceInfo {
            id: "abc".into(),
            label: "demo".into(),
            cwd: "/tmp/demo".into(),
        };
        let cases = vec![
            ServerMessage::Subscribed {
                instance: test_instance.clone(),
                services: vec![],
                steps: vec![],
            },
            ServerMessage::Subscribed {
                instance: test_instance.clone(),
                services: vec![ServiceSnapshot {
                    name: "backend".into(),
                    state: ServiceState::Running {
                        degraded: false,
                        started_without: vec![],
                    },
                    pid: Some(12345),
                    port: Some(8080),
                    url: Some("http://{host}:{port}".into()),
                    restart_count: 0,
                }],
                steps: vec![StepSnapshot {
                    name: "gcloud".into(),
                    state: StepState::Passed,
                }],
            },
            ServerMessage::LogChunk {
                service: "backend".into(),
                bytes: "aGVsbG8=".into(),
                ts: 1_730_000_000_000,
                stream: LogStream::Stderr,
            },
            ServerMessage::StatusUpdate {
                service: "backend".into(),
                state: ServiceState::Starting,
                pid: None,
                port: None,
                restart_count: 0,
            },
            ServerMessage::StepStatusUpdate {
                step: "gcloud".into(),
                state: StepState::Passed,
            },
            ServerMessage::Notice {
                level: NoticeLevel::Warn,
                message: "stale cache invalidated".into(),
            },
            ServerMessage::Error {
                code: ErrorCode::NotFound,
                message: "service 'redis' not declared in config".into(),
            },
            ServerMessage::Goodbye {
                reason: "user requested shutdown".into(),
            },
        ];
        for m in cases {
            assert_eq!(json_round_trip(m.clone()), m);
        }
    }

    #[test]
    fn error_code_maps_to_exit_codes_per_adr_0008() {
        assert_eq!(ErrorCode::Internal.cli_exit_code(), 1);
        assert_eq!(ErrorCode::Usage.cli_exit_code(), 2);
        assert_eq!(ErrorCode::NotFound.cli_exit_code(), 3);
        assert_eq!(ErrorCode::Permission.cli_exit_code(), 4);
        assert_eq!(ErrorCode::Conflict.cli_exit_code(), 5);
    }

    #[test]
    fn envelope_rejects_payload_with_unknown_kind() {
        let json = r#"{"schema_version":1,"kind":"bogus"}"#;
        let result: Result<Envelope<ClientMessage>, _> = serde_json::from_str(json);
        assert!(result.is_err(), "expected unknown kind to be rejected");
    }

    #[test]
    fn rejects_unknown_top_level_field_in_client_messages() {
        // deny_unknown_fields on the enum should reject extra fields per variant
        let json = r#"{"kind":"restart","service":"backend","unexpected":42}"#;
        let result: Result<ClientMessage, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn log_chunk_stream_defaults_to_stdout_when_absent() {
        // Additive field: a v1 LogChunk emitted before `stream` existed (no
        // such field on the wire) must still parse, defaulting to stdout.
        let json = r#"{"kind":"log_chunk","service":"web","bytes":"aGk=","ts":1}"#;
        let m: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(
            m,
            ServerMessage::LogChunk {
                service: "web".into(),
                bytes: "aGk=".into(),
                ts: 1,
                stream: LogStream::Stdout,
            }
        );
    }

    #[test]
    fn log_stream_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&LogStream::Stderr).unwrap(),
            r#""stderr""#
        );
        assert_eq!(
            serde_json::to_string(&LogStream::Stdout).unwrap(),
            r#""stdout""#
        );
    }

    #[test]
    fn schema_version_is_one_at_v1() {
        // Bumping this constant requires a breaking-change checklist;
        // the assertion guards against accidental edits.
        assert_eq!(SCHEMA_VERSION, 1);
    }
}
