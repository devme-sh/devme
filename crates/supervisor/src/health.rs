//! Health-probe implementations for `HealthCheck` config variants.
//!
//! Probes return actionable errors for status, doctor, and task-readiness
//! diagnostics. The caller remains responsible for retry policy.

use std::time::Duration;

use devme_core::HealthCheck;
use tokio::net::TcpStream;

/// Probe `target` once and return its current healthy/unhealthy state.
///
pub async fn probe(target: &HealthCheck, timeout: Duration) -> Result<(), String> {
    match target {
        HealthCheck::Tcp { tcp } => probe_tcp(tcp, timeout).await,
        HealthCheck::Http { http } => probe_http(http, timeout).await,
        HealthCheck::Shell { shell } => probe_shell(shell, timeout).await,
    }
}

async fn probe_tcp(addr: &str, timeout: Duration) -> Result<(), String> {
    let connect = TcpStream::connect(addr);
    match tokio::time::timeout(timeout, connect).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("TCP {addr} is not ready: {e}")),
        Err(_) => Err(format!(
            "TCP {addr} timed out after {}ms",
            timeout.as_millis()
        )),
    }
}

async fn probe_http(url: &str, timeout: Duration) -> Result<(), String> {
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => return Err(format!("could not create HTTP probe: {e}")),
    };
    match client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => Err(format!("HTTP {url} returned {}", resp.status())),
        Err(e) => Err(format!("HTTP {url} is not ready: {e}")),
    }
}

async fn probe_shell(cmd: &str, timeout: Duration) -> Result<(), String> {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.output();
    match tokio::time::timeout(timeout, child).await {
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if detail.is_empty() {
                format!("command probe exited {}", output.status)
            } else {
                detail
            })
        }
        Ok(Err(e)) => Err(format!("command probe could not run: {e}")),
        Err(_) => Err(format!(
            "command probe timed out after {}ms",
            timeout.as_millis()
        )),
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
        assert!(probe(&target, Duration::from_secs(2)).await.is_ok());
    }

    #[tokio::test]
    async fn tcp_probe_fails_for_closed_port() {
        // 127.0.0.1:1 is reserved + nothing listens on it locally.
        let target = HealthCheck::Tcp {
            tcp: "127.0.0.1:1".into(),
        };
        assert!(probe(&target, Duration::from_millis(100)).await.is_err());
    }

    #[tokio::test]
    async fn shell_probe_passes_when_command_exits_zero() {
        let target = HealthCheck::Shell {
            shell: "true".into(),
        };
        assert!(probe(&target, Duration::from_secs(2)).await.is_ok());
    }

    #[tokio::test]
    async fn shell_probe_fails_when_command_exits_nonzero() {
        let target = HealthCheck::Shell {
            shell: "false".into(),
        };
        assert!(probe(&target, Duration::from_secs(2)).await.is_err());
    }

    #[tokio::test]
    async fn http_probe_fails_for_unreachable_url() {
        let target = HealthCheck::Http {
            http: "http://127.0.0.1:1/".into(),
        };
        assert!(probe(&target, Duration::from_millis(100)).await.is_err());
    }
}
