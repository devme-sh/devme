#![cfg(unix)]

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

#[test]
fn guardian_interrupts_task_group_after_owner_disappears() {
    let dir = TempDir::new().unwrap();
    let gate = dir.path().join("gate");
    let completion = dir.path().join("complete");
    let started_at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut owner = Command::new("sleep").arg("30").spawn().unwrap();
    let owner_pid = owner.id();
    let owner_identity = devme_resource_lease::process_identity(owner_pid).unwrap();
    let mut task = Command::new("sleep");
    task.arg("30").stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        task.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut task = task.spawn().unwrap();
    let task_pid = task.id();
    let task_identity = devme_resource_lease::process_identity(task_pid).unwrap();

    let mut guardian = Command::new(env!("CARGO_BIN_EXE_devme"))
        .arg("__task-guardian")
        .arg(owner_pid.to_string())
        .arg(&owner_identity)
        .arg(task_pid.to_string())
        .arg(&task_identity)
        .arg(&gate)
        .arg(&completion)
        .arg(dir.path())
        .arg("ios-test")
        .arg(started_at.to_string())
        .arg("none")
        .arg("")
        .arg("")
        .spawn()
        .unwrap();

    for _ in 0..50 {
        if gate.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(gate.exists(), "guardian never released the Task gate");
    owner.kill().unwrap();
    owner.wait().unwrap();
    let _ = task.wait();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if guardian.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        guardian.try_wait().unwrap().is_some(),
        "guardian did not exit"
    );
    assert_ne!(
        devme_resource_lease::process_identity(task_pid).as_deref(),
        Some(task_identity.as_str()),
        "Task process survived its owner"
    );
    let history = devme_cli::task::read_history(dir.path(), None, None).unwrap();
    assert!(history.iter().any(|result| {
        result.task == "ios-test"
            && result.status == "interrupted"
            && result.interrupted
            && result
                .output_events
                .iter()
                .any(|event| event.text.contains("owner disconnected"))
    }));
}

#[test]
fn guardian_rejects_a_dead_owner_without_opening_the_gate() {
    let dir = TempDir::new().unwrap();
    let gate = dir.path().join("gate");
    let completion = dir.path().join("complete");
    let mut owner = Command::new("true").spawn().unwrap();
    let owner_pid = owner.id();
    let owner_identity = devme_resource_lease::process_identity(owner_pid).unwrap();
    owner.wait().unwrap();
    let mut task = grouped_command("sleep", &["30"]);
    let mut task = task.spawn().unwrap();
    let task_pid = task.id();
    let task_identity = devme_resource_lease::process_identity(task_pid).unwrap();

    let mut guardian = spawn_guardian(
        dir.path(),
        owner_pid,
        &owner_identity,
        task_pid,
        &task_identity,
        &gate,
        &completion,
    );
    let _ = task.wait();
    assert!(!guardian.wait().unwrap().success());
    assert!(!gate.exists(), "guardian opened the gate for a dead owner");
}

#[test]
fn guardian_never_kills_a_process_with_a_mismatched_identity() {
    let dir = TempDir::new().unwrap();
    let gate = dir.path().join("gate");
    let completion = dir.path().join("complete");
    let mut owner = Command::new("sleep").arg("30").spawn().unwrap();
    let owner_pid = owner.id();
    let owner_identity = devme_resource_lease::process_identity(owner_pid).unwrap();
    let mut task = grouped_command("sleep", &["30"]);
    let mut task = task.spawn().unwrap();
    let task_pid = task.id();

    let mut guardian = spawn_guardian(
        dir.path(),
        owner_pid,
        &owner_identity,
        task_pid,
        "not-the-current-process-identity",
        &gate,
        &completion,
    );
    assert!(!guardian.wait().unwrap().success());
    assert!(
        devme_resource_lease::process_identity(task_pid).is_some(),
        "guardian killed a process whose identity did not match"
    );
    task.kill().unwrap();
    task.wait().unwrap();
    owner.kill().unwrap();
    owner.wait().unwrap();
}

#[test]
fn guardian_waits_for_the_full_task_group_before_releasing_ownership() {
    let dir = TempDir::new().unwrap();
    let gate = dir.path().join("gate");
    let completion = dir.path().join("complete");
    let child_pid_file = dir.path().join("child-pid");
    let mut owner = Command::new("sleep").arg("30").spawn().unwrap();
    let owner_pid = owner.id();
    let owner_identity = devme_resource_lease::process_identity(owner_pid).unwrap();
    let script = "while [ ! -e \"$1\" ]; do sleep 0.01; done; sleep 30 & echo $! > \"$2\"";
    let mut task = grouped_command(
        "sh",
        &[
            "-c",
            script,
            "guardian-task",
            gate.to_str().unwrap(),
            child_pid_file.to_str().unwrap(),
        ],
    );
    let mut task = task.spawn().unwrap();
    let task_pid = task.id();
    let task_identity = devme_resource_lease::process_identity(task_pid).unwrap();
    let mut guardian = spawn_guardian(
        dir.path(),
        owner_pid,
        &owner_identity,
        task_pid,
        &task_identity,
        &gate,
        &completion,
    );

    for _ in 0..100 {
        if child_pid_file.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let child_pid = std::fs::read_to_string(&child_pid_file)
        .expect("Task descendant pid was not written")
        .trim()
        .parse::<u32>()
        .unwrap();
    let child_identity = devme_resource_lease::process_identity(child_pid).unwrap();
    task.wait().unwrap();
    assert!(
        guardian.try_wait().unwrap().is_none(),
        "guardian exited while a Task descendant was alive"
    );
    owner.kill().unwrap();
    owner.wait().unwrap();
    assert!(guardian.wait().unwrap().success());
    assert_ne!(
        devme_resource_lease::process_identity(child_pid).as_deref(),
        Some(child_identity.as_str()),
        "Task descendant survived its owner"
    );
}

#[test]
fn guardian_requires_completion_acknowledgement_for_a_clean_exit() {
    let dir = TempDir::new().unwrap();
    let gate = dir.path().join("gate");
    let completion = dir.path().join("complete");
    let mut owner = Command::new("sleep").arg("30").spawn().unwrap();
    let owner_pid = owner.id();
    let owner_identity = devme_resource_lease::process_identity(owner_pid).unwrap();
    let script = "while [ ! -e \"$1\" ]; do sleep 0.01; done";
    let mut task = grouped_command(
        "sh",
        &["-c", script, "guardian-task", gate.to_str().unwrap()],
    );
    let mut task = task.spawn().unwrap();
    let task_pid = task.id();
    let task_identity = devme_resource_lease::process_identity(task_pid).unwrap();
    let mut guardian = spawn_guardian(
        dir.path(),
        owner_pid,
        &owner_identity,
        task_pid,
        &task_identity,
        &gate,
        &completion,
    );

    task.wait().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        guardian.try_wait().unwrap().is_none(),
        "guardian exited before Task completion was persisted"
    );
    std::fs::write(&completion, b"persisted").unwrap();
    assert!(guardian.wait().unwrap().success());
    owner.kill().unwrap();
    owner.wait().unwrap();
    assert!(
        devme_cli::task::read_history(dir.path(), None, None)
            .unwrap()
            .is_empty()
    );
}

fn grouped_command(program: &str, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command
}

#[allow(clippy::too_many_arguments)]
fn spawn_guardian(
    root: &std::path::Path,
    owner_pid: u32,
    owner_identity: &str,
    task_pid: u32,
    task_identity: &str,
    gate: &std::path::Path,
    completion: &std::path::Path,
) -> std::process::Child {
    let started_at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    Command::new(env!("CARGO_BIN_EXE_devme"))
        .arg("__task-guardian")
        .arg(owner_pid.to_string())
        .arg(owner_identity)
        .arg(task_pid.to_string())
        .arg(task_identity)
        .arg(gate)
        .arg(completion)
        .arg(root)
        .arg("ios-test")
        .arg(started_at.to_string())
        .arg("none")
        .arg("")
        .arg("")
        .spawn()
        .unwrap()
}
