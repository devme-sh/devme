use std::process::Command;

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_devme")
}

#[test]
fn member_directory_lists_and_runs_its_namespaced_task_from_workspace_root() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("apps/ios/Sources")).unwrap();
    std::fs::write(
        dir.path().join("devme.toml"),
        "schema_version = 1\n[workspace.members]\nios = \"apps/ios\"\n[task.root-where]\ncmd = \"pwd\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("apps/ios/devme.toml"),
        "schema_version = 1\n[task.where]\ncmd = \"pwd\"\n",
    )
    .unwrap();

    let invocation = dir.path().join("apps/ios/Sources");
    let list = Command::new(bin())
        .args(["tasks", "--output", "json"])
        .current_dir(&invocation)
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let tasks: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(tasks["tasks"][0]["name"], "ios::where");

    let check = Command::new(bin())
        .args(["--json", "config", "check"])
        .current_dir(&invocation)
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let checked: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(checked["schema_version"], 1);
    assert_eq!(checked["ok"], true);

    let run = Command::new(bin())
        .args(["run", "where", "--output", "json"])
        .current_dir(&invocation)
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(
        result["stdout"].as_str().unwrap().trim(),
        dir.path()
            .join("apps/ios")
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    );
    let history = devme_config::paths::repo_socket_dir(dir.path())
        .unwrap()
        .join(format!(
            "{}-tasks/ios__where.jsonl",
            devme_config::paths::instance_id(dir.path())
        ));
    assert!(
        history.is_file(),
        "missing history at {}",
        history.display()
    );
    assert!(!dir.path().join("apps/ios/.devme").exists());

    let root_run = Command::new(bin())
        .args(["run", "root::root-where", "--output", "json"])
        .current_dir(&invocation)
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(
        root_run.status.success(),
        "{}",
        String::from_utf8_lossy(&root_run.stderr)
    );
    let root_result: serde_json::Value = serde_json::from_slice(&root_run.stdout).unwrap();
    assert_eq!(
        root_result["stdout"].as_str().unwrap().trim(),
        dir.path().canonicalize().unwrap().display().to_string()
    );
}

#[test]
fn bare_noninteractive_devme_is_read_only_focused_agent_context() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("apps/ios")).unwrap();
    std::fs::write(
        dir.path().join("devme.toml"),
        "schema_version = 1\n[workspace.members]\nios = \"apps/ios\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("apps/ios/devme.toml"),
        "schema_version = 1\n[service.app]\ncmd = \"sleep 30\"\n[task.test]\ncmd = \"true\"\n",
    )
    .unwrap();
    let socket = devme_config::paths::supervisor_socket(dir.path()).unwrap();

    let output = Command::new(bin())
        .current_dir(dir.path().join("apps/ios"))
        .env("HOME", dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("focus: \"ios\""), "{stdout}");
    assert!(stdout.contains("tasks: 1 declared"), "{stdout}");
    assert!(
        !socket.exists(),
        "bare agent context unexpectedly started a daemon"
    );
}

#[test]
fn task_auto_converges_declared_dependencies_without_starting_a_daemon() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("devme.toml"),
        r#"schema_version = 1
[step.dependencies]
check = "test -f installed"
provision = "touch installed"
trust = "auto"
[task.check]
cmd = "test -f installed"
steps = ["dependencies"]
"#,
    )
    .unwrap();
    let socket = devme_config::paths::supervisor_socket(dir.path()).unwrap();

    let output = Command::new(bin())
        .args(["run", "check", "--output", "json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("installed").is_file());
    assert!(!socket.exists());
}
