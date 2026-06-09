//! Spawns a child under **two PTYs** — one for stdout, one for stderr — so the
//! supervisor can tell the streams apart (errors and tracebacks almost always
//! go to stderr) while both file descriptors still see a real terminal. Because
//! each fd is a genuine PTY, `isatty(1)` *and* `isatty(2)` are true, so dev
//! servers keep emitting color and progress exactly as in a developer's shell —
//! we just read the two masters separately and tag every line with its
//! [`LogStream`].
//!
//! `portable-pty`'s high-level spawn wires stdin/stdout/stderr all onto a single
//! slave, so it can't split the streams. We drop to a Unix `openpty(3)` ×2 plus
//! a `pre_exec` hook (`setsid` + `TIOCSCTTY` + `dup2`) instead. `setsid` makes
//! the child its own session/process-group leader (`pid == pgid`), which is what
//! lets [`send_sigkill`]'s group-signal reap wrapper shells and their
//! grandchildren.
//!
//! Cross-stream ordering is best-effort (two reader threads, timestamped by the
//! daemon on receipt) — the same model already used to interleave across
//! services. Within a single stream, order is exact.

use std::io::{BufRead, BufReader};
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;

use devme_core::LogStream;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("opening pty: {0}")]
    Pty(#[source] anyhow::Error),
    #[error("spawning child: {0}")]
    Spawn(#[source] anyhow::Error),
}

/// A line of output paired with the stream it came from.
pub type TaggedLine = (LogStream, String);

/// A running child process owned by the supervisor.
pub struct ChildProcess {
    pid: u32,
    lines_rx: mpsc::UnboundedReceiver<TaggedLine>,
    exit_rx: Option<oneshot::Receiver<i32>>,
}

/// Splittable, task-friendly version of [`ChildProcess`]. The daemon's event
/// loop kills by pid (see [`send_sigkill`]); per-process tasks own the receivers.
pub struct SpawnParts {
    pub pid: u32,
    pub lines: mpsc::UnboundedReceiver<TaggedLine>,
    pub exit: oneshot::Receiver<i32>,
}

impl ChildProcess {
    /// Spawn `cmd` via `sh -c`, with `cwd` as the working directory and the
    /// caller's environment.
    pub fn spawn(cmd: &str, cwd: &Path) -> Result<Self, SpawnError> {
        Self::spawn_with_env::<&str>(cmd, cwd, &[])
    }

    /// Spawn into [`SpawnParts`] instead of bundling everything into one
    /// struct. Useful when the caller wants to hand the receivers to
    /// different tasks.
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
        })
    }

    /// Spawn `cmd` via `sh -c`, with extra environment variables overlaid
    /// on the caller's environment.
    pub fn spawn_with_env<S: AsRef<str>>(
        cmd: &str,
        cwd: &Path,
        extra_env: &[(S, S)],
    ) -> Result<Self, SpawnError> {
        // One PTY per stream. Both slaves become the child's terminal so
        // isatty() holds on stdout and stderr alike.
        let (master_out, slave_out) = open_pty()?;
        let (master_err, slave_err) = open_pty().inspect_err(|_| unsafe {
            libc::close(master_out);
            libc::close(slave_out);
        })?;

        // Masters are parent-only — CLOEXEC so the forked child never inherits
        // a read handle to its own terminal.
        unsafe {
            libc::fcntl(master_out, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(master_err, libc::F_SETFD, libc::FD_CLOEXEC);
        }

        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(cmd).current_dir(cwd);
        // Std sets up stdio (dup2 onto 0/1/2) *before* running pre_exec hooks,
        // so point the builder at /dev/null and let pre_exec install the PTYs.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Tame `docker compose`'s interactive TTY rendering. Because we give the
        // child a real PTY, compose otherwise draws its navigation menu
        // ("w Enable Watch  d Detach") as a sticky footer and animates progress
        // in place with cursor movement — both turn into garbage in an
        // append-only log buffer (the footer gets prepended to every line; each
        // spinner frame becomes a duplicate line). `COMPOSE_MENU=false` drops
        // the footer; `COMPOSE_PROGRESS=plain` emits clean sequential progress
        // lines instead of redraws. Both are compose-only and ignored by every
        // other program. Set before `extra_env` so a user's devme.toml can still
        // override them.
        command.env("COMPOSE_MENU", "false");
        command.env("COMPOSE_PROGRESS", "plain");
        for (k, v) in extra_env {
            command.env(k.as_ref(), v.as_ref());
        }

        // SAFETY: the closure runs in the forked child between fork and exec and
        // uses only async-signal-safe libc calls (setsid/dup2/ioctl/close) and
        // no allocation. `slave_out`/`slave_err` are plain fds captured by value.
        unsafe {
            command.pre_exec(move || {
                // New session: child is its own session + process-group leader
                // (pid == pgid), which makes the group-kill in send_signal()
                // reap `sh -c …` wrappers and their grandchildren.
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // stdout PTY drives stdin+stdout; stderr PTY drives fd 2.
                if libc::dup2(slave_out, 0) == -1
                    || libc::dup2(slave_out, 1) == -1
                    || libc::dup2(slave_err, 2) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                // Acquire the stdout PTY as controlling terminal (job control,
                // SIGWINCH, /dev/tty). Best-effort — not fatal if it fails.
                libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0);
                if slave_out > 2 {
                    libc::close(slave_out);
                }
                if slave_err > 2 {
                    libc::close(slave_err);
                }
                Ok(())
            });
        }

        let spawn_result = command.spawn();
        // Parent's slave copies are redundant once the child holds them; closing
        // them lets the masters see EOF when the child exits.
        unsafe {
            libc::close(slave_out);
            libc::close(slave_err);
        }
        let child = match spawn_result {
            Ok(c) => c,
            Err(e) => {
                unsafe {
                    libc::close(master_out);
                    libc::close(master_err);
                }
                return Err(SpawnError::Spawn(anyhow::Error::msg(e.to_string())));
            }
        };
        let pid = child.id();

        // One reader thread per stream, each tagging its lines.
        let (lines_tx, lines_rx) = mpsc::unbounded_channel();
        spawn_reader(master_out, LogStream::Stdout, lines_tx.clone());
        spawn_reader(master_err, LogStream::Stderr, lines_tx);

        // Waiter thread: block on child.wait(), forward exit code. `code()` is
        // None when signal-terminated → -1, matching the old behavior.
        let (exit_tx, exit_rx) = oneshot::channel();
        let mut child = child;
        std::thread::spawn(move || {
            let code = match child.wait() {
                Ok(s) => s.code().unwrap_or(-1),
                Err(_) => -1,
            };
            let _ = exit_tx.send(code);
        });

        Ok(Self {
            pid,
            lines_rx,
            exit_rx: Some(exit_rx),
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Next line of output tagged with its stream, or `None` once both PTYs
    /// have closed.
    pub async fn next_line(&mut self) -> Option<TaggedLine> {
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

    /// Send SIGKILL to the child's process group. Safe from any task.
    pub fn kill(&mut self) -> std::io::Result<()> {
        send_sigkill(self.pid);
        Ok(())
    }
}

/// Open a PTY pair sized like a developer's terminal. Returns `(master, slave)`
/// raw fds; the caller owns both.
fn open_pty() -> Result<(RawFd, RawFd), SpawnError> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut winsize = libc::winsize {
        ws_row: 24,
        ws_col: 200,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty writes two valid fds on success; a null termios uses
    // sane defaults; winsize is a stack value valid for the call.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut winsize,
        )
    };
    if rc != 0 {
        return Err(SpawnError::Pty(anyhow::Error::msg(
            std::io::Error::last_os_error().to_string(),
        )));
    }
    Ok((master, slave))
}

/// Stream lines from a PTY master, cleaning cursor escapes and tagging each
/// with `stream`. Ends (and drops its sender) when the master hits EOF.
fn spawn_reader(master: RawFd, stream: LogStream, tx: mpsc::UnboundedSender<TaggedLine>) {
    // SAFETY: we own `master` and hand it to exactly one File, which closes it.
    let file = unsafe { std::fs::File::from_raw_fd(master) };
    std::thread::spawn(move || {
        let mut buf = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match buf.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end_matches(['\n', '\r']);
                    let cleaned = strip_cursor_escapes(trimmed);
                    if cleaned.is_empty() {
                        continue;
                    }
                    if tx.send((stream, cleaned)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
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
    let pid = pid as libc::pid_t;
    // SAFETY: `getpgid`/`kill` with a plain pid have no memory effects; a
    // dead/invalid pid just returns ESRCH which we ignore.
    unsafe {
        // Each child runs in its own PTY session (portable-pty calls setsid),
        // so the child is its own process-group leader: pid == pgid. When that
        // holds, signal the whole group (`kill(-pgid)`) so wrapper shells like
        // `sh -c …`, `bun x vite`, and `docker compose` take their
        // grandchildren down with them instead of leaving orphans. The
        // pgid == pid guard means we never signal the supervisor's own group
        // if the child somehow wasn't a session leader.
        if libc::getpgid(pid) == pid {
            libc::kill(-pid, sig);
        } else {
            libc::kill(pid, sig);
        }
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
        let (stream, line) = child.next_line().await.unwrap();
        assert_eq!(line.trim(), "hello");
        assert_eq!(stream, LogStream::Stdout);
        let exit = child.wait().await;
        assert_eq!(exit, 0);
    }

    #[tokio::test]
    async fn multiple_lines_emit_in_order() {
        let mut child =
            ChildProcess::spawn("printf 'one\\ntwo\\nthree\\n'", &std::env::temp_dir()).unwrap();
        let mut lines = Vec::new();
        while let Some((_, l)) = child.next_line().await {
            lines.push(l.trim().to_string());
        }
        assert_eq!(lines, vec!["one", "two", "three"]);
        assert_eq!(child.wait().await, 0);
    }

    #[tokio::test]
    async fn stdout_and_stderr_lines_are_tagged_by_stream() {
        let mut child = ChildProcess::spawn(
            "printf 'to_out\\n'; printf 'to_err\\n' 1>&2",
            &std::env::temp_dir(),
        )
        .unwrap();
        let mut seen = std::collections::HashMap::new();
        while let Some((stream, line)) = child.next_line().await {
            seen.insert(line.trim().to_string(), stream);
        }
        assert_eq!(
            seen.get("to_out"),
            Some(&LogStream::Stdout),
            "got: {seen:?}"
        );
        assert_eq!(
            seen.get("to_err"),
            Some(&LogStream::Stderr),
            "got: {seen:?}"
        );
    }

    #[tokio::test]
    async fn both_stdout_and_stderr_are_real_ttys() {
        // The whole point of dual-PTY: isatty() holds on *both* fds, so tools
        // keep coloring/animating on stdout and stderr alike.
        let mut child = ChildProcess::spawn(
            "test -t 1 && echo OUT_TTY; test -t 2 && echo ERR_TTY 1>&2",
            &std::env::temp_dir(),
        )
        .unwrap();
        let mut got = Vec::new();
        while let Some((s, l)) = child.next_line().await {
            got.push((s, l.trim().to_string()));
        }
        assert!(
            got.contains(&(LogStream::Stdout, "OUT_TTY".to_string())),
            "stdout was not a tty: {got:?}"
        );
        assert!(
            got.contains(&(LogStream::Stderr, "ERR_TTY".to_string())),
            "stderr was not a tty: {got:?}"
        );
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
        let (_, line) = child.next_line().await.unwrap();
        assert_eq!(line.trim(), "winning");
    }

    #[tokio::test]
    async fn spawn_parts_streams_lines_and_exit_independently() {
        let mut parts =
            ChildProcess::spawn_parts::<&str>("printf 'a\\nb\\n'", &std::env::temp_dir(), &[])
                .unwrap();
        let mut lines = Vec::new();
        while let Some((_, l)) = parts.lines.recv().await {
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

    #[tokio::test]
    async fn sigkill_reaps_grandchildren_via_process_group() {
        // `sh` forks a backgrounded `sleep` (a grandchild), prints its pid,
        // then waits. Killing the sh pid must also take the grandchild down,
        // which only happens if we signal the whole process group.
        let mut parts = ChildProcess::spawn_parts::<&str>(
            "sleep 30 & echo $!; wait",
            &std::env::temp_dir(),
            &[],
        )
        .unwrap();
        let parent = parts.pid;
        let grandchild: u32 = parts
            .lines
            .recv()
            .await
            .expect("grandchild pid line")
            .1
            .trim()
            .parse()
            .expect("pid parses");
        // Let both settle.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(process_is_alive(grandchild), "grandchild should be alive");

        send_sigkill(parent);

        // Poll for the grandchild to die (group kill), up to ~2s.
        let mut reaped = false;
        for _ in 0..40 {
            if !process_is_alive(grandchild) {
                reaped = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(reaped, "grandchild {grandchild} survived a group SIGKILL");
    }

    #[test]
    fn strip_cursor_escapes_keeps_sgr_colors() {
        let input = "\x1b[32m✔\x1b[0m Container created";
        assert_eq!(
            strip_cursor_escapes(input),
            "\x1b[32m✔\x1b[0m Container created"
        );
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
