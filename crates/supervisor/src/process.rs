//! Wraps a portable-pty child in a tokio-friendly handle.
//!
//! The child runs inside a real PTY (so it sees a terminal — full-line
//! buffering, ANSI escapes — exactly like a developer's terminal would).
//! Output is streamed line-by-line, exit status is delivered through a
//! oneshot, and `kill()` works from any thread.

use std::io::{BufRead, BufReader};
use std::path::Path;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("opening pty: {0}")]
    Pty(#[source] anyhow::Error),
    #[error("spawning child: {0}")]
    Spawn(#[source] anyhow::Error),
}

/// A running child process owned by the supervisor.
pub struct ChildProcess {
    pid: u32,
    lines_rx: mpsc::UnboundedReceiver<String>,
    exit_rx: Option<oneshot::Receiver<i32>>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

/// Splittable, task-friendly version of [`ChildProcess`]. The daemon's event
/// loop holds the killer; per-process tasks own the receivers.
pub struct SpawnParts {
    pub pid: u32,
    pub lines: mpsc::UnboundedReceiver<String>,
    pub exit: oneshot::Receiver<i32>,
    pub killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

impl ChildProcess {
    /// Spawn `cmd` via `sh -c`, with `cwd` as the working directory and the
    /// caller's environment.
    pub fn spawn(cmd: &str, cwd: &Path) -> Result<Self, SpawnError> {
        Self::spawn_with_env::<&str>(cmd, cwd, &[])
    }

    /// Spawn into [`SpawnParts`] instead of bundling everything into one
    /// struct. Useful when the caller wants to hand the receivers to
    /// different tasks but keep the killer.
    pub fn spawn_parts<S: AsRef<str>>(
        cmd: &str,
        cwd: &Path,
        extra_env: &[(S, S)],
    ) -> Result<SpawnParts, SpawnError> {
        let cp = Self::spawn_with_env(cmd, cwd, extra_env)?;
        Ok(SpawnParts {
            pid: cp.pid,
            lines: cp.lines_rx,
            exit: cp.exit_rx.expect("exit_rx populated on fresh spawn"),
            killer: cp.killer,
        })
    }

    /// Spawn `cmd` via `sh -c`, with extra environment variables overlaid
    /// on the caller's environment.
    pub fn spawn_with_env<S: AsRef<str>>(
        cmd: &str,
        cwd: &Path,
        extra_env: &[(S, S)],
    ) -> Result<Self, SpawnError> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(SpawnError::Pty)?;

        let mut cmd_builder = CommandBuilder::new("sh");
        cmd_builder.args(["-c", cmd]);
        cmd_builder.cwd(cwd);
        for (k, v) in extra_env {
            cmd_builder.env(k.as_ref(), v.as_ref());
        }

        let mut child = pair.slave.spawn_command(cmd_builder).map_err(SpawnError::Spawn)?;
        let pid = child.process_id().unwrap_or(0);
        let killer = child.clone_killer();
        // Drop slave handle so the PTY closes when the child exits.
        drop(pair.slave);

        // Reader thread: stream lines from the master.
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| SpawnError::Pty(anyhow::Error::msg(e.to_string())))?;
        let (lines_tx, lines_rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut buf = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match buf.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
                        let cleaned = strip_cursor_escapes(&trimmed);
                        if cleaned.is_empty() {
                            continue;
                        }
                        if lines_tx.send(cleaned).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        drop(pair.master);

        // Waiter thread: block on child.wait(), forward exit code.
        let (exit_tx, exit_rx) = oneshot::channel();
        std::thread::spawn(move || {
            let status = child.wait();
            let code = match status {
                Ok(s) => s.exit_code() as i32,
                Err(_) => -1,
            };
            let _ = exit_tx.send(code);
        });

        Ok(Self {
            pid,
            lines_rx,
            exit_rx: Some(exit_rx),
            killer,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Next line of output, or `None` when the PTY closes.
    pub async fn next_line(&mut self) -> Option<String> {
        self.lines_rx.recv().await
    }

    /// Wait for the child to exit. Returns the exit code (`-1` for signal /
    /// internal error). Idempotent — subsequent calls return -1.
    pub async fn wait(&mut self) -> i32 {
        match self.exit_rx.take() {
            Some(rx) => rx.await.unwrap_or(-1),
            None => -1,
        }
    }

    /// Send SIGKILL (or equivalent) to the child. Safe from any task.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.killer
            .kill()
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

/// Send SIGTERM to `pid` — a *graceful* stop request the process can trap to
/// shut down cleanly (flush, remove a docker container, etc.). Best-effort:
/// signalling a dead pid (ESRCH) or pid 0 is a silent no-op. Unix only; on
/// other platforms this does nothing and callers fall back to [`ChildProcess::kill`].
pub fn send_sigterm(pid: u32) {
    send_signal(pid, libc::SIGTERM);
}

/// Send SIGKILL to `pid` — the uncatchable hard stop, used as the fallback
/// after a graceful SIGTERM grace period elapses. Best-effort (see
/// [`send_sigterm`]).
pub fn send_sigkill(pid: u32) {
    send_signal(pid, libc::SIGKILL);
}

/// Best-effort liveness check for `pid` (unix: `kill(pid, 0)`). Lets a caller
/// poll for a graceful exit before escalating to SIGKILL, so a clean shutdown
/// doesn't always burn the full grace period.
pub fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 {
            return false;
        }
        // SAFETY: signal 0 performs only the existence/permission check, no
        // delivery. Returns 0 if alive; EPERM also implies it's alive.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(unix)]
fn send_signal(pid: u32, sig: i32) {
    if pid == 0 {
        return;
    }
    // SAFETY: `kill(2)` with a plain pid and signal number has no memory
    // effects; an invalid/dead pid just returns ESRCH which we ignore.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _sig: i32) {}

/// Strip CSI escape sequences used for cursor positioning (up/down/column,
/// erase, save/restore, show/hide cursor) while keeping SGR color sequences
/// (those ending in `m`). Programs like `docker compose` use cursor movement
/// for in-place progress rendering which looks like garbage in a log buffer.
fn strip_cursor_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if ch == '\x1b' {
            if let Some(&(_, '[')) = chars.peek() {
                // CSI sequence: \x1b[ params final_byte
                let start = i;
                chars.next(); // consume '['
                // skip parameter bytes (0x20-0x3F range in ASCII)
                while let Some(&(_, c)) = chars.peek() {
                    if ('\x20'..='\x3F').contains(&c) {
                        chars.next();
                    } else {
                        break;
                    }
                }
                if let Some(&(end_i, final_ch)) = chars.peek() {
                    chars.next();
                    if final_ch == 'm' {
                        // SGR (color/style) — keep it
                        let seq_end = end_i + final_ch.len_utf8();
                        out.push_str(&s[start..seq_end]);
                    }
                    // else: cursor movement, erase, etc — drop
                }
            } else {
                // Non-CSI escape (e.g. \x1b7, \x1b8) — drop both bytes
                chars.next();
            }
        } else if ch == '\r' {
            // Carriage return without newline — skip
        } else {
            out.push(ch);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_echo_emits_line_then_exits_zero() {
        let mut child = ChildProcess::spawn("echo hello", &std::env::temp_dir()).unwrap();
        let line = child.next_line().await.unwrap();
        assert_eq!(line.trim(), "hello");
        let exit = child.wait().await;
        assert_eq!(exit, 0);
    }

    #[tokio::test]
    async fn multiple_lines_emit_in_order() {
        let mut child =
            ChildProcess::spawn("printf 'one\\ntwo\\nthree\\n'", &std::env::temp_dir()).unwrap();
        let mut lines = Vec::new();
        while let Some(l) = child.next_line().await {
            lines.push(l.trim().to_string());
        }
        assert_eq!(lines, vec!["one", "two", "three"]);
        assert_eq!(child.wait().await, 0);
    }

    #[tokio::test]
    async fn non_zero_exit_code_propagates() {
        let mut child = ChildProcess::spawn("exit 7", &std::env::temp_dir()).unwrap();
        let code = child.wait().await;
        assert_eq!(code, 7);
    }

    #[tokio::test]
    async fn pid_is_nonzero_for_running_child() {
        let child = ChildProcess::spawn("sleep 1", &std::env::temp_dir()).unwrap();
        assert!(child.pid() > 0, "got pid={}", child.pid());
    }

    #[tokio::test]
    async fn env_vars_passed_to_child() {
        let mut child = ChildProcess::spawn_with_env(
            "echo $DEVME_TEST_VAR",
            &std::env::temp_dir(),
            &[("DEVME_TEST_VAR", "winning")],
        )
        .unwrap();
        let line = child.next_line().await.unwrap();
        assert_eq!(line.trim(), "winning");
    }

    #[tokio::test]
    async fn spawn_parts_streams_lines_and_exit_independently() {
        let mut parts = ChildProcess::spawn_parts::<&str>(
            "printf 'a\\nb\\n'",
            &std::env::temp_dir(),
            &[],
        )
        .unwrap();
        let mut lines = Vec::new();
        while let Some(l) = parts.lines.recv().await {
            lines.push(l.trim().to_string());
        }
        assert_eq!(lines, vec!["a", "b"]);
        let exit = parts.exit.await.unwrap();
        assert_eq!(exit, 0);
        assert!(parts.pid > 0);
    }

    #[tokio::test]
    async fn kill_terminates_long_running_child() {
        let mut child = ChildProcess::spawn("sleep 60", &std::env::temp_dir()).unwrap();
        // Give it a moment to actually start before killing.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        child.kill().unwrap();
        // Should exit promptly with a non-zero status (signal terminated).
        let exit = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .expect("wait timed out — kill didn't work");
        assert_ne!(exit, 0, "killed child should not report exit 0");
    }

    #[test]
    fn strip_cursor_escapes_keeps_sgr_colors() {
        let input = "\x1b[32m✔\x1b[0m Container created";
        assert_eq!(strip_cursor_escapes(input), "\x1b[32m✔\x1b[0m Container created");
    }

    #[test]
    fn strip_cursor_escapes_removes_cursor_movement() {
        let input = "\x1b[?25l\x1b[0G[+] up 0/1\r";
        assert_eq!(strip_cursor_escapes(input), "[+] up 0/1");
    }

    #[test]
    fn strip_cursor_escapes_removes_cursor_up_and_erase() {
        let input = "\x1b[2A\x1b[0G\x1b[0KSpinner text";
        assert_eq!(strip_cursor_escapes(input), "Spinner text");
    }

    #[test]
    fn strip_cursor_escapes_handles_empty_and_noise() {
        assert_eq!(strip_cursor_escapes("\x1b[?25h"), "");
        assert_eq!(strip_cursor_escapes("\x1b[2A\x1b[0G\x1b[0K"), "");
    }
}
