use std::io::Write;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_devme")
}

fn fixture() -> TempDir {
    let dir = TempDir::new_in("/tmp").unwrap();
    std::fs::write(
        dir.path().join("devme.toml"),
        r#"schema_version = 1

[stack]
env_file = ".env.auth.local"

[env.GOOGLE_WEB_CLIENT_ID]
required = true
setup_url = "https://console.example.test/credentials"
help = "Google web OAuth client ID"

[env.GOOGLE_CLIENT_SECRET]
required = true
secret = true
setup_url = "https://console.example.test/credentials"
help = "Google web OAuth client secret"
"#,
    )
    .unwrap();
    dir
}

fn command(dir: &TempDir) -> Command {
    let mut command = Command::new(bin());
    command
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", dir.path().join(".runtime"));
    command
}

fn run(dir: &TempDir, args: &[&str]) -> Output {
    command(dir).args(args).output().unwrap()
}

fn run_with_stdin(dir: &TempDir, args: &[&str], input: &str) -> Output {
    let mut child = command(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn setup_status_is_agent_readable_and_never_exposes_values() {
    let dir = fixture();
    std::fs::write(
        dir.path().join(".env.auth.local"),
        "GOOGLE_CLIENT_SECRET=do-not-print\n",
    )
    .unwrap();

    let output = run(&dir, &["setup", "status", "--json"]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["status"], "incomplete");
    assert_eq!(report["missing_required"], 1);
    assert!(
        report["env_file"]
            .as_str()
            .unwrap()
            .ends_with(".env.auth.local")
    );
    assert_eq!(report["variables"][0]["state"], "missing");
    assert_eq!(
        report["variables"][0]["setup_url"],
        "https://console.example.test/credentials"
    );
    assert_eq!(report["variables"][1]["state"], "configured");
    assert_eq!(report["variables"][1]["secret"], true);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("do-not-print"));
}

#[test]
fn setup_status_supports_toon_output() {
    let dir = fixture();

    let output = run(&dir, &["setup", "status", "--output", "toon"]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        toon_format::decode_strict(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["status"], "incomplete");
    assert_eq!(report["missing_required"], 2);
    assert_eq!(report["variables"][0]["name"], "GOOGLE_WEB_CLIENT_ID");
}

#[test]
fn setup_status_defaults_to_toon_when_stdout_is_not_a_terminal() {
    let dir = fixture();

    let output = run(&dir, &["setup", "status"]);

    assert!(output.status.success());
    let report: serde_json::Value =
        toon_format::decode_strict(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    assert_eq!(report["status"], "incomplete");
    assert_eq!(report["missing_required"], 2);
}

#[test]
fn setup_help_describes_the_terminal_sensitive_output_default() {
    let dir = fixture();

    let output = run(&dir, &["setup", "status", "--help"]);

    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("Defaults to human on a terminal and TOON otherwise"));
    assert!(!help.contains("[default: human]"));
}

#[test]
fn setup_set_accepts_secrets_only_on_stdin_and_updates_status() {
    let dir = fixture();
    let client = run(
        &dir,
        &[
            "setup",
            "set",
            "GOOGLE_WEB_CLIENT_ID",
            "--value",
            "web.apps.googleusercontent.com",
            "--json",
        ],
    );
    assert!(
        client.status.success(),
        "{}",
        String::from_utf8_lossy(&client.stderr)
    );

    let rejected = run(
        &dir,
        &[
            "setup",
            "set",
            "GOOGLE_CLIENT_SECRET",
            "--value",
            "leaked-argument",
            "--json",
        ],
    );
    assert!(!rejected.status.success());
    assert!(
        !std::fs::read_to_string(dir.path().join(".env.auth.local"))
            .unwrap()
            .contains("leaked-argument")
    );
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("stdin")
            || String::from_utf8_lossy(&rejected.stdout).contains("stdin")
    );

    let secret = run_with_stdin(
        &dir,
        &["setup", "set", "GOOGLE_CLIENT_SECRET", "--json"],
        "local-secret\n",
    );
    assert!(
        secret.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&secret.stdout),
        String::from_utf8_lossy(&secret.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&secret.stdout).unwrap();
    assert_eq!(report["status"], "complete");
    assert!(!String::from_utf8_lossy(&secret.stdout).contains("local-secret"));
    let env = std::fs::read_to_string(dir.path().join(".env.auth.local")).unwrap();
    assert!(env.contains("GOOGLE_WEB_CLIENT_ID=web.apps.googleusercontent.com"));
    assert!(env.contains("GOOGLE_CLIENT_SECRET=local-secret"));
    assert!(!env.contains("leaked-argument"));
}

#[test]
fn setup_set_supports_toon_output() {
    let dir = fixture();

    let output = run(
        &dir,
        &[
            "setup",
            "set",
            "GOOGLE_WEB_CLIENT_ID",
            "--value",
            "web-client",
            "--output",
            "toon",
        ],
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        toon_format::decode_strict(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    assert_eq!(report["status"], "incomplete");
    assert_eq!(report["missing_required"], 1);
    assert_eq!(report["variables"][0]["state"], "configured");
}

#[test]
fn setup_set_uses_semantic_exit_codes() {
    let dir = fixture();

    let unknown = run(
        &dir,
        &["setup", "set", "UNKNOWN", "--value", "value", "--json"],
    );
    assert_eq!(unknown.status.code(), Some(3));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&unknown.stdout).unwrap()["error"]["code"],
        "not_found"
    );

    let secret_argument = run(
        &dir,
        &[
            "setup",
            "set",
            "GOOGLE_CLIENT_SECRET",
            "--value",
            "secret",
            "--json",
        ],
    );
    assert_eq!(secret_argument.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&secret_argument.stdout).unwrap()["error"]["code"],
        "invalid_arguments"
    );

    let empty = run(
        &dir,
        &[
            "setup",
            "set",
            "GOOGLE_WEB_CLIENT_ID",
            "--value",
            "",
            "--json",
        ],
    );
    assert_eq!(empty.status.code(), Some(2));

    let first = run(
        &dir,
        &[
            "setup",
            "set",
            "GOOGLE_WEB_CLIENT_ID",
            "--value",
            "first",
            "--json",
        ],
    );
    assert!(first.status.success());
    let retry = run(
        &dir,
        &[
            "setup",
            "set",
            "GOOGLE_WEB_CLIENT_ID",
            "--value",
            "first",
            "--json",
        ],
    );
    assert_eq!(retry.status.code(), Some(0));
    let env = std::fs::read_to_string(dir.path().join(".env.auth.local")).unwrap();
    assert_eq!(env.matches("GOOGLE_WEB_CLIENT_ID=").count(), 1);

    let conflict = run(
        &dir,
        &[
            "setup",
            "set",
            "GOOGLE_WEB_CLIENT_ID",
            "--value",
            "second",
            "--output",
            "toon",
        ],
    );
    assert_eq!(conflict.status.code(), Some(5));
    let conflict_report: serde_json::Value =
        toon_format::decode_strict(std::str::from_utf8(&conflict.stdout).unwrap()).unwrap();
    assert_eq!(conflict_report["error"]["code"], "conflict");
}

#[test]
fn explicit_human_output_applies_to_setup_errors() {
    let dir = fixture();

    let output = run(
        &dir,
        &[
            "setup", "set", "UNKNOWN", "--value", "value", "--output", "human",
        ],
    );

    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not declared"));
}
