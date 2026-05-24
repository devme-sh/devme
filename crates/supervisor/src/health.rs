//! Health-probe implementations for `HealthCheck` config variants.
//!
//! Probes return `true` for "service is up and ready to serve" and `false`
//! for anything else. They don't surface specifics — the caller logs the
//! transition; only the boolean drives the executor.

use std::time::Duration;

use devme_core::HealthCheck;
use tokio::net::TcpStream;

/// Probe `target` once and return its current healthy/unhealthy state.
///
/// Errors are mapped to `false` so callers never have to think about
/// "I/O error vs. unhealthy".
pub async fn probe(target: &HealthCheck) -> bool {
    match target {
        HealthCheck::Tcp { tcp } => probe_tcp(tcp).await,
        HealthCheck::Http { http } => probe_http(http).await,
        HealthCheck::Shell { shell } => probe_shell(shell).await,
    }
}

async fn probe_tcp(addr: &str) -> bool {
    let connect = TcpStream::connect(addr);
    matches!(
        tokio::time::timeout(Duration::from_secs(2), connect).await,
        Ok(Ok(_))
    )
}

async fn probe_http(url: &str) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

async fn probe_shell(cmd: &str) -> bool {
    match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
    {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn tcp_probe_passes_for_listening_port() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let target = HealthCheck::Tcp { tcp: addr };
        assert!(probe(&target).await);
    }

    #[tokio::test]
    async fn tcp_probe_fails_for_closed_port() {
        // 127.0.0.1:1 is reserved + nothing listens on it locally.
        let target = HealthCheck::Tcp {
            tcp: "127.0.0.1:1".into(),
        };
        assert!(!probe(&target).await);
    }

    #[tokio::test]
    async fn shell_probe_passes_when_command_exits_zero() {
        let target = HealthCheck::Shell {
            shell: "true".into(),
        };
        assert!(probe(&target).await);
    }

    #[tokio::test]
    async fn shell_probe_fails_when_command_exits_nonzero() {
        let target = HealthCheck::Shell {
            shell: "false".into(),
        };
        assert!(!probe(&target).await);
    }

    #[tokio::test]
    async fn http_probe_fails_for_unreachable_url() {
        let target = HealthCheck::Http {
            http: "http://127.0.0.1:1/".into(),
        };
        assert!(!probe(&target).await);
    }
}
