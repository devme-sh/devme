use std::collections::HashSet;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_devme")
}

struct Fixture {
    root: TempDir,
    runtime: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("devme-shared-logs-")
            .tempdir_in("/tmp")
            .unwrap();
        let runtime = tempfile::Builder::new()
            .prefix("devme-shared-runtime-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(
            root.path().join("devme.toml"),
            r#"schema_version = 1

[logs]
redact = ["secret-[A-Z]+"]
retention_bytes = 1024

[service.repo]
scope = "repo"
cmd = "i=0; while [ $i -lt 80 ]; do printf 'repo-%03d payload-payload-payload-payload\\n' $i; i=$((i + 1)); done; printf 'repo-END secret-REPO\\n' >&2; sleep 1; printf 'repo-follow\\n'; sleep 30"
health = { shell = "true" }

[service.local]
cmd = "printf 'local-ready secret-LOCAL\\n' >&2; sleep 1; printf 'local-follow\\n'; sleep 30"

[task.verify]
cmd = "printf 'task-ready secret-TASK\\n'"
"#,
        )
        .unwrap();
        Self { root, runtime }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .args(args)
            .current_dir(self.root.path())
            .env("HOME", self.root.path())
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .output()
            .unwrap()
    }

    fn json_logs(&self, args: &[&str]) -> Vec<serde_json::Value> {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(bin());
        command
            .current_dir(self.root.path())
            .env("HOME", self.root.path())
            .env("XDG_RUNTIME_DIR", self.runtime.path());
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["down", "--all"]);
    }
}

fn wait_for_correlated_logs(fixture: &Fixture) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let records = fixture.json_logs(&["logs", "--tail", "0", "--json"]);
        let sources = records
            .iter()
            .filter_map(|record| record["service"].as_str())
            .collect::<HashSet<_>>();
        if sources.contains("repo")
            && sources.contains("local")
            && sources.contains("task:verify")
            && records
                .iter()
                .any(|record| record["text"] == "repo-END [REDACTED]")
        {
            return records;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for all log sources"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn all_source_logs_merge_repo_instance_and_task_history_with_global_filters() {
    let fixture = Fixture::new();
    let task = fixture.run(&["run", "verify", "--output", "json"]);
    assert!(task.status.success());
    let up = fixture.run(&["up", "-d"]);
    assert!(
        up.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&up.stdout),
        String::from_utf8_lossy(&up.stderr)
    );

    let records = wait_for_correlated_logs(&fixture);
    assert!(
        records
            .windows(2)
            .all(|pair| pair[0]["ts"].as_u64() <= pair[1]["ts"].as_u64()),
        "combined history was not timestamp ordered: {records:?}"
    );
    let rendered = serde_json::to_string(&records).unwrap();
    assert!(!rendered.contains("secret-REPO"), "{rendered}");
    assert!(!rendered.contains("secret-LOCAL"), "{rendered}");
    assert!(!rendered.contains("secret-TASK"), "{rendered}");
    assert!(rendered.contains("[REDACTED]"), "{rendered}");

    // The repo source wrote enough to rotate more than two generations. The
    // configured bounded retention keeps its latest marker and drops its
    // earliest records.
    assert!(
        records
            .iter()
            .any(|record| record["text"] == "repo-END [REDACTED]")
    );
    assert!(!records.iter().any(|record| {
        record["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("repo-000 "))
    }));

    std::thread::sleep(Duration::from_millis(1200));
    let retained = fixture.json_logs(&["logs", "--tail", "0", "--json"]);
    let tail = fixture.json_logs(&["logs", "--tail", "3", "--json"]);
    assert_eq!(tail.len(), 3);
    assert_eq!(tail, retained[retained.len() - 3..]);

    let since = tail[0]["ts"].as_u64().unwrap().to_string();
    let recent = fixture.json_logs(&["logs", "--tail", "0", "--since", &since, "--json"]);
    assert!(
        recent
            .iter()
            .all(|record| record["ts"].as_u64().unwrap() >= since.parse().unwrap())
    );

    // No logical record may be duplicated by the instance and shared views.
    let unique = records
        .iter()
        .map(|record| {
            (
                record["ts"].as_u64().unwrap(),
                record["service"].as_str().unwrap(),
                record["stream"].as_str().unwrap(),
                record["text"].as_str().unwrap(),
            )
        })
        .collect::<HashSet<_>>();
    assert_eq!(unique.len(), records.len());
}

#[test]
fn doctor_includes_repo_owned_state_and_redacted_history() {
    let fixture = Fixture::new();
    assert!(
        fixture
            .run(&["run", "verify", "--output", "json"])
            .status
            .success()
    );
    assert!(fixture.run(&["up", "-d"]).status.success());
    let _ = wait_for_correlated_logs(&fixture);

    let digest = fixture.run(&["doctor"]);
    assert!(digest.status.success());
    let report: serde_json::Value = serde_json::from_slice(&digest.stdout).unwrap();
    let names = report["services"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|service| service["name"].as_str())
        .collect::<HashSet<_>>();
    assert!(names.contains("repo"), "{report}");
    assert!(names.contains("local"), "{report}");
    assert!(
        report["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["name"] == "verify")
    );

    let zoom = fixture.run(&["doctor", "repo", "--tail", "0"]);
    assert!(zoom.status.success());
    let repo: serde_json::Value = serde_json::from_slice(&zoom.stdout).unwrap();
    assert_eq!(repo["kind"], "service");
    let rendered = serde_json::to_string(&repo).unwrap();
    assert!(rendered.contains("repo-END [REDACTED]"), "{rendered}");
    assert!(!rendered.contains("secret-REPO"), "{rendered}");
}

#[test]
fn named_queries_stay_scoped_and_follow_streams_both_supervisors() {
    use std::process::Stdio;

    let fixture = Fixture::new();
    assert!(
        fixture
            .run(&["run", "verify", "--output", "json"])
            .status
            .success()
    );
    assert!(fixture.run(&["up", "-d"]).status.success());
    let _ = wait_for_correlated_logs(&fixture);

    let repo = fixture.json_logs(&["logs", "repo", "--tail", "0", "--json"]);
    assert!(!repo.is_empty());
    assert!(repo.iter().all(|record| record["service"] == "repo"));
    let local = fixture.json_logs(&["logs", "local", "--tail", "0", "--json"]);
    assert!(!local.is_empty());
    assert!(local.iter().all(|record| record["service"] == "local"));

    let child = fixture
        .command()
        .args(["logs", "--follow", "--tail", "0", "--json"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1400));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let rendered = String::from_utf8(output.stdout).unwrap();
    assert!(rendered.contains("repo-follow"), "{rendered}");
    assert!(rendered.contains("local-follow"), "{rendered}");
}
