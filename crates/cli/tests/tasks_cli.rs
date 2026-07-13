use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_devme")
}

fn fixture(config: &str) -> TempDir {
    // Keep daemon socket paths below macOS's SUN_LEN limit while giving every
    // parallel test its own runtime registry.
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

fn interrupt(child: std::process::Child) -> Output {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    child.wait_with_output().unwrap()
}

#[test]
fn list_detail_pass_through_and_aggregate_failure_are_cli_contracts() {
    let dir = fixture(
        r#"schema_version=1
[task.echo]
cmd="printf '%s'"
[task.fail]
cmd="exit 7"
[task.all]
depends_on=["echo","fail"]
"#,
    );
    let list = run(&dir, &["tasks", "--output", "json"]);
    assert!(list.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&list.stdout).unwrap()["schema_version"],
        1
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&list.stdout).unwrap()["count"],
        3
    );
    let detail = run(&dir, &["tasks", "show", "all", "--output", "toon"]);
    assert!(String::from_utf8_lossy(&detail.stdout).contains("dependencies"));
    let echo = run(
        &dir,
        &["run", "echo", "--output", "json", "--", "hello world"],
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&echo.stdout).unwrap()["stdout"],
        "hello world"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&echo.stdout).unwrap()["schema_version"],
        1
    );
    let failed = run(&dir, &["run", "all", "--output", "json"]);
    assert_eq!(failed.status.code(), Some(7));
}

#[test]
fn task_logs_and_doctor_are_redacted_without_a_daemon() {
    let dir = fixture(
        r#"schema_version=1
[logs]
redact=["sword[a-z]+"]
retention_bytes=4096
[task.leak]
cmd="printf 'swordfish'; printf 'bad swordfish' >&2; exit 3"
"#,
    );
    assert_eq!(
        run(&dir, &["run", "leak", "--output", "json"])
            .status
            .code(),
        Some(3)
    );
    let logs = run(&dir, &["logs", "leak", "--json"]);
    let text = String::from_utf8_lossy(&logs.stdout);
    assert!(text.contains("[REDACTED]"));
    assert!(!text.contains("swordfish"));
    let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(first["service"], "task:leak");
    assert_eq!(first["source_kind"], "task");
    let doctor = run(&dir, &["doctor", "leak"]);
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["kind"], "task");
    assert_eq!(report["status"], "failed");
}

#[test]
fn doctor_health_uses_latest_task_result_instead_of_stale_failures() {
    let dir = fixture(
        r#"schema_version=1
[service.keepalive]
cmd="sleep 30"
[task.check]
cmd="test -f passing"
"#,
    );
    assert_eq!(
        run(&dir, &["run", "check", "--output", "json"])
            .status
            .code(),
        Some(1)
    );
    std::fs::write(dir.path().join("passing"), "").unwrap();
    assert!(
        run(&dir, &["run", "check", "--output", "json"])
            .status
            .success()
    );
    assert!(run(&dir, &["up", "-d", "--wait"]).status.success());

    let doctor = run(&dir, &["doctor"]);
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["status"], "healthy");
    assert_eq!(report["tasks"][0]["status"], "passed");
    let _ = run(&dir, &["down"]);
}

#[test]
fn service_required_task_starts_only_target_closure_and_waits_for_readiness() {
    let dir = fixture(
        r#"schema_version=1
[service.database]
cmd="touch database-ready; sleep 30"
health={ shell="test -f database-ready" }
readiness={ interval_ms=25, timeout_ms=100, retries=20 }
[service.backend]
cmd="printf 'backend event\\n'; touch backend-ready; sleep 30"
health={ shell="test -f backend-ready" }
readiness={ interval_ms=25, timeout_ms=100, retries=20 }
depends_on=["database"]
[service.unrelated]
cmd="touch unrelated-started; sleep 30"
[task.check]
cmd="test -f backend-ready; printf 'task event\\n'"
services=["backend"]
readiness_timeout=5
"#,
    );
    let output = run(&dir, &["run", "check", "--output", "json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!dir.path().join("unrelated-started").exists());
    assert!(dir.path().join("database-ready").exists());
    let status = run(&dir, &["--json", "status"]);
    assert!(status.status.success());
    let status_json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let backend = status_json["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|service| service["name"] == "backend")
        .unwrap();
    assert_eq!(backend["readiness"]["ready"], true);
    let logs = run(&dir, &["logs", "--tail", "0", "--json"]);
    let records = String::from_utf8_lossy(&logs.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        records
            .iter()
            .any(|record| record["source_kind"] == "service")
    );
    assert!(records.iter().any(|record| record["source_kind"] == "task"));
    assert!(
        records
            .windows(2)
            .all(|pair| pair[0]["ts"].as_u64() <= pair[1]["ts"].as_u64())
    );
    let _ = run(&dir, &["down"]);
}

#[test]
fn task_readiness_deadline_reports_the_last_probe_error() {
    let dir = fixture(
        r#"schema_version=1
[service.backend]
cmd="sleep 30"
health={ shell="echo expected schema is not published >&2; false" }
readiness={ interval_ms=20, timeout_ms=100, retries=100 }
[task.check]
cmd="true"
services=["backend"]
readiness_timeout=1
"#,
    );
    let output = run(&dir, &["run", "check", "--output", "toon"]);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("expected schema is not published"),
        "{stdout}"
    );
    for expected in [
        "earlier attempts omitted",
        "interval_ms=20",
        "timeout_ms=100",
        "retries=100",
        "deadline_seconds=1",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout}"
        );
    }
    assert!(output.stderr.is_empty());

    let doctor = run(&dir, &["doctor", "backend"]);
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["readiness"]["ready"], false);
    assert!(report["readiness"]["attempt"].as_u64().unwrap() > 0);
    let _ = run(&dir, &["down"]);
}

#[test]
fn cancellation_returns_130_and_kills_process_tree() {
    let dir =
        fixture("schema_version=1\n[task.wait]\ncmd=\"sleep 30 & echo $! > child.pid; wait\"\n");
    let child = Command::new(bin())
        .args(["run", "wait", "--output", "json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if dir.path().join("child.pid").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    let pid: i32 = std::fs::read_to_string(dir.path().join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
}

#[test]
fn timeout_returns_124_and_kills_process_tree() {
    let dir = fixture(
        "schema_version=1\n[task.wait]\ncmd=\"sleep 30 & echo $! > timeout-child.pid; wait\"\ntimeout=1\n",
    );
    let output = run(&dir, &["run", "wait", "--output", "json"]);
    assert_eq!(output.status.code(), Some(124));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["timed_out"], true);
    assert_eq!(result["cancelled"], false);

    let pid: i32 = std::fs::read_to_string(dir.path().join("timeout-child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
}

#[test]
fn host_lease_waits_across_two_worktrees() {
    let runtime = std::path::PathBuf::from(format!("/tmp/devme-contention-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let config = "schema_version=1\n[resource.device]\nscope=\"host\"\ncapacity=1\nenv=\"SLOT\"\n[task.use]\ncmd=\"sleep 1; printf $SLOT\"\nresources=[\"device\"]\n";
    let a = fixture(config);
    let b = fixture(config);
    let started = Instant::now();
    let mut first = Command::new(bin())
        .args(["run", "use", "--output", "json"])
        .current_dir(a.path())
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let second = Command::new(bin())
        .args(["run", "use", "--output", "json"])
        .current_dir(b.path())
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(first.wait().unwrap().success());
    assert!(second.status.success());
    assert!(started.elapsed() >= Duration::from_millis(1800));
    let _ = std::fs::remove_dir_all(runtime);
}

#[test]
fn multi_resource_wait_releases_partial_leases_atomically() {
    let runtime = TempDir::new().unwrap();
    let dir = fixture(
        r#"schema_version=1
[resource.a]
scope="host"
[resource.b]
scope="host"
[task.hold-b]
cmd="touch holding-b; sleep 30"
resources=["b"]
[task.wait-both]
cmd="sleep 30"
resources=["a","b"]
[task.use-a]
cmd="true"
resources=["a"]
"#,
    );
    let holder = Command::new(bin())
        .args(["run", "hold-b", "--output", "json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if dir.path().join("holding-b").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(dir.path().join("holding-b").exists());

    let waiter = Command::new(bin())
        .args(["run", "wait-both", "--output", "json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(250));

    let started = Instant::now();
    let use_a = Command::new(bin())
        .args(["run", "use-a", "--output", "json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .unwrap();
    assert!(use_a.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "task waiting on b retained its partial a lease"
    );

    let _ = interrupt(waiter);
    let _ = interrupt(holder);
}

#[test]
fn host_lease_recovers_after_owner_process_is_killed() {
    let runtime =
        std::path::PathBuf::from(format!("/tmp/devme-crash-recovery-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let dir = fixture(
        "schema_version=1\n[resource.device]\nscope=\"host\"\ncapacity=1\n[task.hold]\ncmd=\"echo $$ > holder.pid; sleep 30\"\nresources=[\"device\"]\n[task.quick]\ncmd=\"true\"\nresources=[\"device\"]\n",
    );
    let mut owner = Command::new(bin())
        .args(["run", "hold", "--output", "json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let task_pid = (0..100)
        .find_map(|_| {
            let pid = std::fs::read_to_string(dir.path().join("holder.pid"))
                .ok()
                .and_then(|value| value.trim().parse::<i32>().ok());
            if pid.is_none() {
                std::thread::sleep(Duration::from_millis(20));
            }
            pid
        })
        .expect("task did not persist its process id");
    unsafe {
        libc::kill(owner.id() as i32, libc::SIGKILL);
    }
    let _ = owner.wait();

    let started = Instant::now();
    let recovered = Command::new(bin())
        .args(["run", "quick", "--output", "json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(recovered.status.success());
    assert!(started.elapsed() < Duration::from_secs(2));

    unsafe {
        libc::kill(-task_pid, libc::SIGKILL);
    }
    let _ = std::fs::remove_dir_all(runtime);
}

#[test]
fn setup_detects_native_monorepo_and_written_config_passes_check() {
    let dir = TempDir::new().unwrap();
    for file in [
        "Package.swift",
        "App.xcworkspace",
        "settings.gradle.kts",
        "convex.json",
        "vite.config.ts",
    ] {
        std::fs::write(dir.path().join(file), "").unwrap();
    }
    let setup = run(&dir, &["setup", "--write"]);
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let check = run(&dir, &["--json", "config", "check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
}

#[test]
fn agent_integrations_are_explicit_idempotent_and_removable() {
    let dir = fixture("schema_version=1\n[task.check]\ncmd=\"true\"\n");
    assert!(
        run(&dir, &["agent", "setup", "--target", "all"])
            .status
            .success()
    );
    assert!(
        run(&dir, &["agent", "setup", "--target", "all"])
            .status
            .success()
    );
    let status = run(&dir, &["agent", "status", "--target", "all"]);
    assert_eq!(
        String::from_utf8_lossy(&status.stdout)
            .matches("installed")
            .count(),
        3
    );
    let context = run(&dir, &["agent", "context"]);
    assert!(context.status.success());
    assert!(!context.stdout.ends_with(b"\n"));
    assert!(
        run(&dir, &["agent", "remove", "--target", "all"])
            .status
            .success()
    );
}

#[test]
fn checked_in_native_mobile_example_passes_config_check() {
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/native-mobile-monorepo");
    let output = Command::new(bin())
        .args(["--json", "config", "check"])
        .current_dir(example)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn portable_native_mobile_fixture_executes_end_to_end() {
    let dir = fixture(include_str!("fixtures/native-mobile/devme.toml"));
    let check = run(&dir, &["--json", "config", "check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let output = run(&dir, &["run", "check", "--output", "json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "passed");
    let logs = run(&dir, &["logs", "--tail", "0", "--json"]);
    let history = String::from_utf8_lossy(&logs.stdout);
    assert!(history.contains("backend ready"));
    assert!(history.contains("task:ios-test"));
    assert!(history.contains("task:android-test"));
    let _ = run(&dir, &["down"]);
}

#[test]
fn toon_surfaces_have_no_trailing_newline_and_usage_errors_are_structured() {
    let dir = fixture("schema_version=1\n[task.check]\ncmd=\"true\"\n");
    for args in [
        &["tasks", "--output", "toon"][..],
        &["tasks", "show", "check", "--output", "toon"][..],
        &["run", "check", "--output", "toon"][..],
    ] {
        let output = run(&dir, args);
        assert!(output.status.success());
        assert!(!output.stdout.ends_with(b"\n"));
        assert!(output.stderr.is_empty());
    }

    let unknown = run(
        &dir,
        &["run", "check", "--output", "toon", "--unknown-flag"],
    );
    assert_eq!(unknown.status.code(), Some(2));
    assert!(unknown.stderr.is_empty());
    let unknown_error: serde_json::Value =
        toon_format::decode_strict(std::str::from_utf8(&unknown.stdout).unwrap()).unwrap();
    assert_eq!(unknown_error["error"]["code"], "invalid_arguments");
    assert!(
        unknown_error["error"]["help"]
            .as_str()
            .unwrap()
            .contains("Valid flags for `devme run`")
    );

    let automatic = run(&dir, &["run", "missing"]);
    assert_eq!(automatic.status.code(), Some(3));
    assert!(automatic.stderr.is_empty());
    let automatic_error: serde_json::Value =
        toon_format::decode_strict(std::str::from_utf8(&automatic.stdout).unwrap()).unwrap();
    assert_eq!(automatic_error["error"]["code"], "not_found");

    let missing = run(&dir, &["run", "missing", "--output", "json"]);
    assert_eq!(missing.status.code(), Some(3));
    let error: serde_json::Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(error["schema_version"], 1);
    assert_eq!(error["error"]["code"], "not_found");
    assert!(missing.stderr.is_empty());
}

#[test]
fn task_detail_is_complete_and_status_doctor_formats_are_compatible() {
    let dir = fixture(
        r#"schema_version=1
[step.tool]
check="true"
[service.api]
cmd="sleep 30"
[resource.device]
capacity=2
[task.detail]
description="Native verification"
cmd="true"
cwd="."
env={ MODE="test" }
steps=["tool"]
services=["api"]
resources=["device"]
timeout=10
readiness_timeout=17
"#,
    );

    let detail = run(&dir, &["tasks", "show", "detail", "--output", "toon"]);
    assert!(detail.status.success());
    let decoded: serde_json::Value =
        toon_format::decode_strict(std::str::from_utf8(&detail.stdout).unwrap()).unwrap();
    assert_eq!(decoded["task"]["description"], "Native verification");
    assert_eq!(decoded["task"]["cwd"], ".");
    assert_eq!(decoded["task"]["environment"]["MODE"], "test");
    assert_eq!(decoded["task"]["steps"][0], "tool");
    assert_eq!(decoded["task"]["readiness_timeout_seconds"], 17);

    let json_alias = run(&dir, &["doctor", "--json"]);
    let output_json = run(&dir, &["doctor", "--output", "json"]);
    let alias_value: serde_json::Value = serde_json::from_slice(&json_alias.stdout).unwrap();
    let output_value: serde_json::Value = serde_json::from_slice(&output_json.stdout).unwrap();
    assert_eq!(alias_value, output_value);

    let toon = run(&dir, &["doctor", "--output", "toon"]);
    let toon_value: serde_json::Value =
        toon_format::decode_strict(std::str::from_utf8(&toon.stdout).unwrap()).unwrap();
    assert_eq!(toon_value, output_value);
}

#[test]
fn status_json_alias_and_official_toon_output_are_equivalent() {
    let dir = fixture("schema_version=1\n");
    let alias = run(&dir, &["status", "--json"]);
    assert!(alias.status.success());
    let explicit = run(&dir, &["status", "--output", "json"]);
    assert!(explicit.status.success());
    let alias_value: serde_json::Value = serde_json::from_slice(&alias.stdout).unwrap();
    let explicit_value: serde_json::Value = serde_json::from_slice(&explicit.stdout).unwrap();
    assert_eq!(alias_value, explicit_value);
    assert_eq!(explicit_value["sessions"], serde_json::json!([]));
    assert_eq!(explicit_value["resource_waiters"], serde_json::json!([]));

    let toon = run(&dir, &["status", "--output", "toon"]);
    assert!(toon.status.success());
    assert!(!toon.stdout.ends_with(b"\n"));
    let toon_value: serde_json::Value =
        toon_format::decode_strict(std::str::from_utf8(&toon.stdout).unwrap()).unwrap();
    assert_eq!(toon_value, explicit_value);
    let _ = run(&dir, &["down"]);
}

#[test]
fn agent_status_honors_json_and_context_uses_all_canonical_guidance() {
    let dir = fixture("schema_version=1\n[task.check]\ncmd=\"true\"\n");
    let status = run(&dir, &["agent", "status", "--json"]);
    assert!(status.status.success());
    let value: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["integrations"].as_array().unwrap().len(), 3);

    std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
    std::fs::write(dir.path().join(".claude/settings.json"), "not json").unwrap();
    let error = run(&dir, &["agent", "setup", "--target", "claude", "--json"]);
    assert_eq!(error.status.code(), Some(1));
    let error_value: serde_json::Value = serde_json::from_slice(&error.stdout).unwrap();
    assert_eq!(error_value["schema_version"], 1);
    assert_eq!(error_value["error"]["code"], "operation_failed");
    assert!(error.stderr.is_empty());

    let context = run(&dir, &["agent", "context"]);
    let text = String::from_utf8_lossy(&context.stdout);
    assert!(text.contains("logs --since 5m --json"));
    assert!(text.contains("only after explicit user approval"));
}
