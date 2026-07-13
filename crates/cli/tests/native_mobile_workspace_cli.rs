use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_devme")
}

struct Fixture {
    root: TempDir,
    runtime: TempDir,
}

impl Fixture {
    fn portable() -> Self {
        let root = TempDir::new().unwrap();
        copy_tree(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/native-mobile-workspace"),
            root.path(),
        );
        let runtime = tempfile::Builder::new()
            .prefix("devme-native-")
            .tempdir_in("/tmp")
            .unwrap();
        Self { root, runtime }
    }

    fn run_from(&self, relative: &str, args: &[&str]) -> Output {
        self.run_from_with_env(relative, args, std::iter::empty::<(&str, &str)>())
    }

    fn run_from_with_env<K, V, I>(&self, relative: &str, args: &[&str], env: I) -> Output
    where
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
        I: IntoIterator<Item = (K, V)>,
    {
        Command::new(bin())
            .args(args)
            .current_dir(self.root.path().join(relative))
            .env("HOME", self.root.path())
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .envs(env)
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run_from(".", &["down"]);
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    assert!(source.is_dir(), "missing fixture at {}", source.display());
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn contains_socket(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            entry.path().extension().is_some_and(|ext| ext == "sock")
                || (entry.file_type().is_ok_and(|kind| kind.is_dir())
                    && contains_socket(&entry.path()))
        })
    })
}

fn wait_until_no_socket(path: &Path) {
    for _ in 0..100 {
        if !contains_socket(path) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("supervisor socket remained under {}", path.display());
}

#[test]
fn native_workspace_configs_resolve_from_root_and_member() {
    let fixture = Fixture::portable();

    for directory in [".", "apps/ios", "apps/android", "backend"] {
        let output = fixture.run_from(directory, &["--json", "config", "check"]);
        assert!(
            output.status.success(),
            "config check from {directory}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["ok"], true, "config check from {directory}");
    }

    let tasks = fixture.run_from("apps/ios", &["tasks", "--output", "json"]);
    assert!(tasks.status.success());
    let report: serde_json::Value = serde_json::from_slice(&tasks.stdout).unwrap();
    let names = report["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(names.contains(&"ios::compile"));
    assert!(names.contains(&"ios::verify"));
}

#[test]
fn member_alias_runs_locally_without_starting_a_supervisor() {
    let fixture = Fixture::portable();

    let output = fixture.run_from("apps/ios/Sources", &["run", "compile", "--output", "json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["task"], "ios::compile");
    assert!(fixture.root.path().join("apps/ios/.compiled").is_file());
    wait_until_no_socket(fixture.runtime.path());
}

#[test]
fn ios_task_starts_only_its_backend_service_closure_and_cleans_up() {
    let fixture = Fixture::portable();

    let output = fixture.run_from("apps/ios", &["run", "verify", "--output", "json"]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["task"], "ios::verify");
    assert!(
        fixture
            .root
            .path()
            .join("backend/.schema-published")
            .is_file()
    );
    assert!(fixture.root.path().join("apps/ios/.verified").is_file());
    assert!(!fixture.root.path().join("apps/android/.started").exists());

    let down = fixture.run_from("apps/ios", &["down"]);
    assert!(
        down.status.success(),
        "{}",
        String::from_utf8_lossy(&down.stderr)
    );
    wait_until_no_socket(fixture.runtime.path());
}

#[test]
fn ios_session_cli_holds_a_resource_for_backend_logs_and_launch() {
    let fixture = Fixture::portable();
    let launched = fixture.run_from("apps/ios", &["session", "dev", "--output", "json"]);
    assert!(
        launched.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&launched.stdout),
        String::from_utf8_lossy(&launched.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&launched.stdout).unwrap();
    assert_eq!(result["task"], "ios::launch");
    assert_eq!(result["status"], "passed");
    assert!(fixture.root.path().join("apps/ios/.logs-started").is_file());
    assert!(fixture.root.path().join("apps/ios/.launched").is_file());
    assert!(
        fixture
            .root
            .path()
            .join("backend/.schema-published")
            .is_file()
    );
    assert!(!fixture.root.path().join("apps/android/.started").exists());

    let sessions = fixture.run_from("apps/ios", &["sessions", "--output", "json"]);
    let sessions: serde_json::Value = serde_json::from_slice(&sessions.stdout).unwrap();
    assert_eq!(sessions["sessions"][0]["name"], "ios::dev");
    assert_eq!(sessions["sessions"][0]["status"], "ready");

    let stopped = fixture.run_from(
        "apps/ios",
        &["session", "dev", "--stop", "--output", "json"],
    );
    assert!(stopped.status.success());
}

#[test]
fn checked_in_native_example_is_a_valid_nested_workspace() {
    let fixture = TempDir::new().unwrap();
    copy_tree(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/native-mobile-monorepo"),
        fixture.path(),
    );
    let output = Command::new(bin())
        .args(["--json", "config", "check"])
        .current_dir(fixture.path().join("apps/ios"))
        .env("HOME", fixture.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
