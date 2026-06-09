//! Health check kinds used by Services (and by `external = true` Services as
//! the only signal devme has).

use serde::{Deserialize, Serialize};

/// How devme determines whether a long-running Service is healthy.
///
/// In TOML, one of:
///
/// ```toml
/// health = { tcp = "localhost:{port}" }
/// health = { http = "http://localhost:{port}/health" }
/// health = { shell = "curl -fsS http://localhost:8080/ready" }
/// ```
///
/// `{port}` interpolates the service's resolved port; the supervisor performs
/// substitution before issuing the check.
///
/// See `External service` in `CONTEXT.md`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum HealthCheck {
    /// Open a TCP socket; healthy if `connect()` succeeds.
    Tcp { tcp: String },
    /// Issue an HTTP GET; healthy if response code is 2xx.
    Http { http: String },
    /// Run an arbitrary shell command; healthy if exit code is 0.
    Shell { shell: String },
}

#[cfg(test)]
mod tests {
    use super::HealthCheck;

    #[test]
    fn parse_tcp_form() {
        let h: HealthCheck = serde_json::from_str(r#"{"tcp":"localhost:8080"}"#).unwrap();
        assert_eq!(
            h,
            HealthCheck::Tcp {
                tcp: "localhost:8080".into()
            }
        );
    }

    #[test]
    fn parse_http_form() {
        let h: HealthCheck =
            serde_json::from_str(r#"{"http":"http://localhost:8080/health"}"#).unwrap();
        assert_eq!(
            h,
            HealthCheck::Http {
                http: "http://localhost:8080/health".into()
            }
        );
    }

    #[test]
    fn parse_shell_form() {
        let h: HealthCheck = serde_json::from_str(r#"{"shell":"true"}"#).unwrap();
        assert_eq!(
            h,
            HealthCheck::Shell {
                shell: "true".into()
            }
        );
    }

    #[test]
    fn round_trip_each_form() {
        let cases = vec![
            HealthCheck::Tcp {
                tcp: "localhost:5432".into(),
            },
            HealthCheck::Http {
                http: "http://localhost:8080/health".into(),
            },
            HealthCheck::Shell {
                shell: "pg_isready".into(),
            },
        ];
        for h in cases {
            let json = serde_json::to_string(&h).unwrap();
            let back: HealthCheck = serde_json::from_str(&json).unwrap();
            assert_eq!(h, back);
        }
    }

    #[test]
    fn rejects_unknown_form() {
        // An object with no known discriminator field fails.
        assert!(serde_json::from_str::<HealthCheck>(r#"{"ping":"localhost"}"#).is_err());
        // An object with multiple discriminator fields also fails because
        // untagged only matches one variant.
        assert!(serde_json::from_str::<HealthCheck>(r#"{"tcp":"a","http":"b"}"#).is_err());
    }
}
