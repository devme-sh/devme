use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_devme")
}

fn fixture(config: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("devme.toml"), config).unwrap();
    dir
}

fn run(dir: &TempDir, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .output()
        .unwrap()
}

fn interrupt(child: Child) -> Output {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    child.wait_with_output().unwrap()
}

fn wait_for(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn aggregate_dependency_failure_is_recorded_for_child_and_requested_root() {
    let dir = fixture(
        r#"schema_version=1
[task.child]
cmd="printf child-error >&2; exit 9"
[task.root]
depends_on=["child"]
"#,
    );

    let output = run(&dir, &["run", "root", "--output", "json"]);
    assert_eq!(output.status.code(), Some(9));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["task"], "root");
    assert_eq!(result["exit_code"], 9);
    assert_eq!(result["status"], "failed");
    assert!(result["stderr"].as_str().unwrap().contains("child"));

    let logs = run(&dir, &["logs", "--tail", "0", "--json"]);
    let records = String::from_utf8_lossy(&logs.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        records
            .iter()
            .any(|record| record["service"] == "task:child")
    );
    assert!(
        records
            .iter()
            .any(|record| record["service"] == "task:root")
    );
}

#[test]
fn invalid_working_directory_is_a_persisted_task_failure() {
    let dir = fixture(
        r#"schema_version=1
[task.broken]
cmd="true"
cwd="missing-directory"
"#,
    );

    let output = run(&dir, &["run", "broken", "--output", "json"]);
    assert_eq!(output.status.code(), Some(1));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["task"], "broken");
    assert_eq!(result["status"], "failed");
    assert!(
        result["stderr"]
            .as_str()
            .unwrap()
            .contains("failed to start")
    );

    let doctor = run(&dir, &["doctor", "broken"]);
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["kind"], "task");
    assert_eq!(report["status"], "failed");
}

#[test]
fn required_service_readiness_failure_is_persisted_before_task_spawn() {
    let dir = fixture(
        r#"schema_version=1
[service.backend]
cmd="sleep 30"
health={ shell="printf 'schema missing' >&2; false" }
readiness={ interval_ms=20, timeout_ms=100, retries=100 }
[task.check]
cmd="touch should-not-run"
services=["backend"]
readiness_timeout=1
"#,
    );

    let output = run(&dir, &["run", "check", "--output", "json"]);
    assert_eq!(output.status.code(), Some(1));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["task"], "check");
    assert_eq!(result["status"], "failed");
    assert!(
        result["stderr"]
            .as_str()
            .unwrap()
            .contains("schema missing")
    );
    assert!(!dir.path().join("should-not-run").exists());

    let doctor = run(&dir, &["doctor", "check", "--full"]);
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["status"], "failed");
    let _ = run(&dir, &["down"]);
}

#[test]
fn cancellation_during_probe_retries_is_persisted_and_stops_only_started_closure() {
    let dir = fixture(
        r#"schema_version=1
[service.unrelated]
cmd="echo $$ > unrelated.pid; sleep 30"
[service.backend]
cmd="echo $$ > backend.pid; sleep 30"
health={ shell="printf 'schema still publishing' >&2; false" }
readiness={ interval_ms=20, timeout_ms=100, retries=1000 }
[task.check]
cmd="touch should-not-run"
services=["backend"]
readiness_timeout=30
"#,
    );

    let unrelated = run(&dir, &["up", "unrelated", "-d"]);
    assert!(
        unrelated.status.success(),
        "{}",
        String::from_utf8_lossy(&unrelated.stderr)
    );
    wait_for(&dir.path().join("unrelated.pid"));
    let unrelated_pid: i32 = std::fs::read_to_string(dir.path().join("unrelated.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let child = Command::new(bin())
        .args(["run", "check", "--output", "json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&dir.path().join("backend.pid"));
    std::thread::sleep(Duration::from_millis(150));
    let backend_pid: i32 = std::fs::read_to_string(dir.path().join("backend.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let output = interrupt(child);
    assert_eq!(output.status.code(), Some(130));
    assert!(output.stderr.is_empty());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["task"], "check");
    assert_eq!(result["status"], "cancelled");
    assert_eq!(result["exit_code"], 130);
    assert_eq!(result["cancelled"], true);
    assert!(result["duration_ms"].as_u64().unwrap() >= 100);
    assert!(result["finished_at"].as_u64().unwrap() > result["started_at"].as_u64().unwrap());
    assert!(!dir.path().join("should-not-run").exists());

    let stopped_deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(backend_pid, 0) } == 0 && Instant::now() < stopped_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        unsafe { libc::kill(backend_pid, 0) },
        -1,
        "task-started backend survived cancellation"
    );
    assert_eq!(
        unsafe { libc::kill(unrelated_pid, 0) },
        0,
        "pre-existing unrelated service was stopped"
    );

    let doctor = run(&dir, &["doctor", "check", "--full"]);
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["kind"], "task");
    assert_eq!(report["status"], "cancelled");
    assert_eq!(report["latest"]["exit_code"], 130);
    assert_eq!(report["latest"]["cancelled"], true);

    let logs = run(&dir, &["logs", "check", "--tail", "0", "--json"]);
    let records = String::from_utf8_lossy(&logs.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(records.iter().any(|record| {
        record["service"] == "task:check"
            && record["text"]
                .as_str()
                .is_some_and(|text| text.contains("cancelled while waiting"))
    }));
    let _ = run(&dir, &["down"]);
}

#[test]
fn required_service_process_failure_aborts_immediately_with_actionable_history() {
    let dir = fixture(
        r#"schema_version=1
[service.backend]
cmd="printf 'fatal backend boot' >&2; exit 17"
health={ shell="printf 'schema unavailable' >&2; false" }
readiness={ interval_ms=100, timeout_ms=500, retries=1000 }
[task.check]
cmd="touch should-not-run"
services=["backend"]
readiness_timeout=30
"#,
    );

    let started = Instant::now();
    let output = run(&dir, &["run", "check", "--output", "json"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "service failure waited for the 30 second readiness deadline"
    );
    assert!(output.stderr.is_empty());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["task"], "check");
    assert_eq!(result["status"], "failed");
    let diagnostic = result["stderr"].as_str().unwrap();
    assert!(diagnostic.contains("required service \"backend\" failed"));
    assert!(diagnostic.contains("exited with code 17"));
    assert!(diagnostic.contains("devme doctor backend"));
    assert!(diagnostic.contains("devme logs backend"));
    assert!(!dir.path().join("should-not-run").exists());

    let doctor = run(&dir, &["doctor", "check", "--full"]);
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["kind"], "task");
    assert_eq!(report["status"], "failed");
    assert_eq!(report["latest"]["exit_code"], 1);
    assert!(
        report["latest"]["stderr"]
            .as_str()
            .unwrap()
            .contains("exited with code 17")
    );

    let logs = run(&dir, &["logs", "check", "--tail", "0", "--json"]);
    let history = String::from_utf8_lossy(&logs.stdout);
    assert!(history.contains("exited with code 17"), "{history}");
    assert!(history.contains("task:check"), "{history}");
    let _ = run(&dir, &["down"]);
}

#[test]
fn cancellation_while_waiting_for_a_resource_returns_130_and_is_persisted() {
    let runtime = TempDir::new().unwrap();
    let dir = fixture(
        r#"schema_version=1
[resource.device]
scope="host"
capacity=1
[task.hold]
cmd="touch holding; sleep 30"
resources=["device"]
[task.wait]
cmd="true"
resources=["device"]
"#,
    );
    let holder = Command::new(bin())
        .args(["run", "hold", "--output", "json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&dir.path().join("holding"));

    let waiter = Command::new(bin())
        .args(["run", "wait", "--output", "json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let context = Command::new(bin())
        .args(["agent", "context", "--json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .unwrap();
    let context: serde_json::Value = serde_json::from_slice(&context.stdout).unwrap();
    assert_eq!(context["state"]["resource_waits"], 1);
    assert_eq!(context["resource_waiters"][0]["task"], "wait");
    assert_eq!(context["resource_waiters"][0]["resource"], "device");

    let output = interrupt(waiter);
    assert_eq!(output.status.code(), Some(130));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["task"], "wait");
    assert_eq!(result["cancelled"], true);
    assert_eq!(result["status"], "cancelled");

    let doctor = Command::new(bin())
        .args(["doctor", "wait"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["status"], "cancelled");
    let _ = interrupt(holder);
}

#[test]
fn output_is_timestamped_and_redacted_before_the_bounded_tail_is_retained() {
    let dir = fixture(
        r#"schema_version=1
[logs]
redact=["secret-[A-Z]+"]
retention_bytes=1024
[task.events]
cmd="printf 'first\\n'; sleep 1; printf 'second\\n' >&2"
[task.secret]
cmd="printf '%0300d' 0; printf secret-TOPSECRET; printf '%0244d' 0"
"#,
    );

    let events = run(&dir, &["run", "events", "--output", "json"]);
    assert!(events.status.success());
    let result: serde_json::Value = serde_json::from_slice(&events.stdout).unwrap();
    assert_eq!(result["stdout"], "first\n");
    assert_eq!(result["stderr"], "second\n");
    assert!(result.get("output_events").is_none());

    let secret = run(&dir, &["run", "secret", "--output", "json"]);
    assert!(secret.status.success());
    let text = String::from_utf8_lossy(&secret.stdout);
    assert!(!text.contains("TOPSECRET"), "{text}");

    let logs = run(&dir, &["logs", "events", "--tail", "0", "--json"]);
    let records = String::from_utf8_lossy(&logs.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 2);
    assert!(records[0]["ts"].as_u64() < records[1]["ts"].as_u64());
    assert_eq!(records[0]["stream"], "stdout");
    assert_eq!(records[1]["stream"], "stderr");
}

#[test]
fn credential_shaped_task_and_declared_env_values_never_reach_persistence() {
    let dir = fixture(
        r#"schema_version=1
[env.API_TOKEN]
required=true
[task.leak]
cmd="printf '%s %s' \"$CERTIFICATE_DATA\" \"$API_TOKEN\""
env={ CERTIFICATE_DATA="ultra-secret-cert" }
"#,
    );
    let output = Command::new(bin())
        .args(["run", "leak", "--output", "json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("API_TOKEN", "runtime-secret-token")
        .output()
        .unwrap();
    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(!rendered.contains("ultra-secret-cert"), "{rendered}");
    assert!(!rendered.contains("runtime-secret-token"), "{rendered}");
    assert!(rendered.contains("[REDACTED]"), "{rendered}");

    let history = devme_cli::task::read_history(dir.path(), None, None).unwrap();
    let persisted = serde_json::to_string(&history).unwrap();
    assert!(!persisted.contains("ultra-secret-cert"), "{persisted}");
    assert!(!persisted.contains("runtime-secret-token"), "{persisted}");

    let logs = Command::new(bin())
        .args(["logs", "leak", "--tail", "0", "--json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("API_TOKEN", "runtime-secret-token")
        .output()
        .unwrap();
    let log_text = String::from_utf8_lossy(&logs.stdout);
    assert!(!log_text.contains("ultra-secret-cert"), "{log_text}");
    assert!(!log_text.contains("runtime-secret-token"), "{log_text}");

    let doctor = Command::new(bin())
        .args(["doctor", "leak"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("API_TOKEN", "runtime-secret-token")
        .output()
        .unwrap();
    let doctor_text = String::from_utf8_lossy(&doctor.stdout);
    assert!(!doctor_text.contains("ultra-secret-cert"), "{doctor_text}");
    assert!(
        !doctor_text.contains("runtime-secret-token"),
        "{doctor_text}"
    );
}
