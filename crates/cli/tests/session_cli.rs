use std::process::{Command, Output};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_devme")
}

fn fixture(config: &str) -> TempDir {
    let dir = TempDir::new_in("/tmp").unwrap();
    std::fs::write(dir.path().join("devme.toml"), config).unwrap();
    dir
}

fn run(dir: &TempDir, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", dir.path().join(".runtime"))
        .output()
        .unwrap()
}

fn run_with_runtime(dir: &TempDir, runtime: &TempDir, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .unwrap()
}

#[test]
fn session_cli_injects_allocated_env_into_run_task_and_stops_idempotently() {
    let dir = fixture(
        r#"schema_version=1
[resource.device]
scope="worktree"
env="DEVICE_ID"
[task.launch]
cmd="printf x >> launch-count; printf \"$DEVICE_ID\""
[session.dev]
resources=["device"]
run="launch"
linger=30
"#,
    );

    let opened = run(&dir, &["session", "dev", "--output", "json"]);
    assert!(
        opened.status.success(),
        "{}",
        String::from_utf8_lossy(&opened.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&opened.stdout).unwrap();
    assert_eq!(result["task"], "launch");
    assert_eq!(result["stdout"], "0");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("launch-count")).unwrap(),
        "x"
    );

    let joined = run(&dir, &["session", "dev", "--output", "json"]);
    assert!(joined.status.success());
    let joined_result: serde_json::Value = serde_json::from_slice(&joined.stdout).unwrap();
    assert_eq!(joined_result["session"], "dev");
    assert_eq!(joined_result["joined"], true);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("launch-count")).unwrap(),
        "x",
        "joining a live session reran its optional launch task"
    );

    let listed = run(&dir, &["sessions", "--output", "json"]);
    assert!(listed.status.success());
    let sessions: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(sessions["count"], 1);
    assert_eq!(sessions["sessions"][0]["name"], "dev");
    assert_eq!(sessions["sessions"][0]["status"], "ready");

    let stopped = run(&dir, &["session", "dev", "--stop", "--output", "toon"]);
    assert!(stopped.status.success());
    let text = String::from_utf8_lossy(&stopped.stdout);
    assert!(text.contains("status: stopped"), "{text}");
    let stopped_again = run(&dir, &["session", "dev", "--stop", "--output", "json"]);
    assert!(stopped_again.status.success());
    let second: serde_json::Value = serde_json::from_slice(&stopped_again.stdout).unwrap();
    assert_eq!(second["already_stopped"], true);
    let _ = run(&dir, &["down"]);
}

#[test]
fn killed_supervisor_cleans_orphan_sidecar_before_reassigning_session_lease() {
    let runtime = TempDir::new_in("/tmp").unwrap();
    let dir = fixture(
        r#"schema_version=1
[resource.device]
scope="worktree"
[service.sidecar]
cmd="echo $$ > sidecar.pid; sleep 30 & echo $! > grandchild.pid; wait"
scope="session"
[session.dev]
needs=["sidecar"]
resources=["device"]
linger=30
"#,
    );
    let first = run_with_runtime(&dir, &runtime, &["session", "dev", "--output", "json"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let orphan_pid: i32 = std::fs::read_to_string(dir.path().join("sidecar.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let orphan_grandchild_pid: i32 = std::fs::read_to_string(dir.path().join("grandchild.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let registry = std::fs::read_to_string(runtime.path().join("devme/slots.json")).unwrap();
    let supervisor_pid: i32 = registry
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .and_then(|value| value.parse().ok())
        .expect("slot registry contains the supervisor pid");

    // SAFETY: the test owns this isolated supervisor process.
    unsafe {
        libc::kill(supervisor_pid, libc::SIGKILL);
    }
    for _ in 0..100 {
        if unsafe { libc::kill(supervisor_pid, 0) } == -1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let recovered = run_with_runtime(&dir, &runtime, &["session", "dev", "--output", "json"]);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        unsafe { libc::kill(orphan_pid, 0) },
        -1,
        "the replacement supervisor reassigned a lease before killing its orphan"
    );
    for _ in 0..100 {
        if unsafe { libc::kill(orphan_grandchild_pid, 0) } == -1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        unsafe { libc::kill(orphan_grandchild_pid, 0) },
        -1,
        "the orphan process group retained a live grandchild"
    );
    let replacement_pid: i32 = std::fs::read_to_string(dir.path().join("sidecar.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(replacement_pid, orphan_pid);

    let _ = run_with_runtime(&dir, &runtime, &["down"]);
}

#[test]
fn sessions_has_definitive_empty_state_without_starting_a_daemon() {
    let dir = fixture("schema_version=1\n");
    let output = run(&dir, &["sessions", "--output", "toon"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("count: 0"), "{stdout}");
    assert!(
        stdout.contains("No sessions are declared in devme.toml"),
        "{stdout}"
    );
    let socket = devme_config::paths::supervisor_socket(dir.path()).unwrap();
    assert!(!socket.exists(), "read-only listing spawned a supervisor");
}

#[test]
fn unknown_session_is_structured_and_uses_not_found_exit_code() {
    let dir = fixture("schema_version=1\n");
    let output = run(&dir, &["session", "missing", "--output", "json"]);
    assert_eq!(output.status.code(), Some(3));
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "not_found");
    assert!(output.stderr.is_empty());

    let stopped = run(&dir, &["session", "missing", "--stop", "--output", "json"]);
    assert_eq!(stopped.status.code(), Some(3));
    let stop_error: serde_json::Value = serde_json::from_slice(&stopped.stdout).unwrap();
    assert_eq!(stop_error["error"]["code"], "not_found");
}
