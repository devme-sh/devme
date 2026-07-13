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
    // Give every shell probe its own process group. `kill_on_drop` only kills
    // the immediate shell, which can leave background children holding ports
    // or files after a readiness timeout or supervisor cancellation.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command
        .spawn()
        .map_err(|error| format!("command probe could not run: {error}"))?;
    let pid = child
        .id()
        .ok_or_else(|| "command probe started without a process id".to_string())?
        as libc::pid_t;
    let mut process_group = ProbeProcessGroup::new(pid);
    let output = child.wait_with_output();
    tokio::pin!(output);

    tokio::select! {
        result = &mut output => {
            process_group.disarm();
            let output = result
                .map_err(|error| format!("command probe could not run: {error}"))?;
            if output.status.success() {
                Ok(())
            } else {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
                Err(if detail.is_empty() {
                    format!("command probe exited {}", output.status)
                } else {
                    detail
                })
            }
        }
        _ = tokio::time::sleep(timeout) => {
            let cleanup = process_group.kill();
            // Reap the group leader and drain stderr before returning. If this
            // future is cancelled while reaping, the armed guard retries the
            // group kill from Drop.
            let _ = output.await;
            process_group.disarm();
            Err(match cleanup {
                Ok(()) => format!(
                    "command probe timed out after {}ms; terminated process group {pid}",
                    timeout.as_millis()
                ),
                Err(error) => format!(
                    "command probe timed out after {}ms; could not terminate process group {pid}: {error}",
                    timeout.as_millis()
                ),
            })
        }
    }
}

/// Owns a shell probe's process group until the group leader has been reaped.
/// Dropping an armed guard handles cancellation of the async probe future.
struct ProbeProcessGroup {
    pid: libc::pid_t,
    armed: bool,
}

impl ProbeProcessGroup {
    fn new(pid: libc::pid_t) -> Self {
        Self { pid, armed: true }
    }

    fn kill(&self) -> std::io::Result<()> {
        // SAFETY: a negative pid asks kill(2) to signal the process group. The
        // pre_exec hook above guarantees this pid is also the group's id.
        let result = unsafe { libc::kill(-self.pid, libc::SIGKILL) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProbeProcessGroup {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio::net::TcpListener;

    fn descendant_probe(pid_file: &Path) -> HealthCheck {
        HealthCheck::Shell {
            shell: format!(
                "sleep 30 & echo $! > '{}'; wait",
                pid_file.display().to_string().replace('\'', "'\\''")
            ),
        }
    }

    async fn read_descendant_pid(pid_file: &Path) -> u32 {
        for _ in 0..40 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        std::fs::read_to_string(pid_file)
            .expect("probe should write the descendant pid")
            .trim()
            .parse()
            .expect("descendant pid should parse")
    }

    async fn process_exited(pid: u32) -> bool {
        for _ in 0..40 {
            if !crate::process::process_is_alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        false
    }

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
    async fn timed_out_shell_probe_terminates_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("descendant.pid");
        let target = descendant_probe(&pid_file);

        let error = probe(&target, Duration::from_millis(100))
            .await
            .expect_err("probe should time out");
        assert!(error.contains("timed out after 100ms"), "{error}");
        assert!(error.contains("terminated process group"), "{error}");

        let descendant = read_descendant_pid(&pid_file).await;
        assert!(
            process_exited(descendant).await,
            "shell probe descendant {descendant} survived timeout"
        );
    }

    #[tokio::test]
    async fn cancelled_shell_probe_terminates_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("cancelled-descendant.pid");
        let target = descendant_probe(&pid_file);
        let probe_task = tokio::spawn(async move { probe(&target, Duration::from_secs(30)).await });

        let descendant = read_descendant_pid(&pid_file).await;

        probe_task.abort();
        let _ = probe_task.await;

        assert!(
            process_exited(descendant).await,
            "shell probe descendant {descendant} survived cancellation"
        );
    }

    #[tokio::test]
    async fn http_probe_fails_for_unreachable_url() {
        let target = HealthCheck::Http {
            http: "http://127.0.0.1:1/".into(),
        };
        assert!(probe(&target, Duration::from_millis(100)).await.is_err());
    }
}
