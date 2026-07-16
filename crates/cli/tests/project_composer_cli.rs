use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn write(path: impl AsRef<Path>, contents: &str) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn recipe() -> TempDir {
    let source = TempDir::new().unwrap();
    write(
        source.path().join("devme-template.toml"),
        r#"schema_version = 1
name = "native"
version = "2026.7.15"

[base]
path = "base"

[features.auth]
version = "1.0.0"
path = "features/auth"
external_steps = ["Review provider setup \u001b[31mcarefully\u001b[0m"]
"#,
    );
    write(source.path().join("base/README.md"), "# Native app\n");
    write(source.path().join("base/app.txt"), "auth = false\n");
    write(source.path().join("features/auth/app.txt"), "auth = true\n");
    source
}

fn run(cwd: &Path, source: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_devme"))
        .args(args)
        .current_dir(cwd)
        .env("DEVME_TEMPLATE_SOURCE", source)
        .env("HOME", cwd)
        .env("XDG_CONFIG_HOME", cwd.join(".config"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn toon(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!output.stdout.ends_with(b"\n"));
    toon_format::decode_strict(std::str::from_utf8(&output.stdout).unwrap()).unwrap()
}

#[test]
fn create_with_and_feature_lifecycle_are_agent_safe() {
    let source = recipe();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");

    let created = run(
        workspace.path(),
        source.path(),
        &[
            "create",
            "native",
            "groceries",
            "--with",
            "auth",
            "--no-input",
            "--output",
            "toon",
        ],
    );
    let created = toon(&created);
    assert_eq!(created["operation"], "create");
    assert_eq!(created["features"][0], "auth");
    assert_eq!(created["external_steps"][0]["kind"], "manual");
    assert_eq!(created["external_steps"][0]["trusted"], false);
    assert_eq!(
        fs::read_to_string(target.join("app.txt")).unwrap(),
        "auth = true\n"
    );

    let listed = run(
        &target,
        source.path(),
        &["feature", "list", "--no-input", "--output", "toon"],
    );
    let listed = toon(&listed);
    assert_eq!(listed["features"][0]["name"], "auth");
    assert_eq!(listed["features"][0]["version"], "1.0.0");

    write(
        source.path().join("devme-template.toml"),
        r#"schema_version = 1
name = "native"
version = "2026.7.16"

[base]
path = "base"

[features.auth]
version = "1.1.0"
path = "features/auth"
"#,
    );
    write(
        source.path().join("features/auth/app.txt"),
        "auth = refreshed\n",
    );
    let updated = toon(&run(
        &target,
        source.path(),
        &[
            "feature",
            "update",
            "auth",
            "--no-input",
            "--output",
            "toon",
        ],
    ));
    assert_eq!(updated["operation"], "feature_update");
    assert_eq!(updated["recipe"]["version"], "2026.7.16");
    assert_eq!(
        fs::read_to_string(target.join("app.txt")).unwrap(),
        "auth = refreshed\n"
    );

    let removed = run(
        &target,
        source.path(),
        &[
            "feature",
            "remove",
            "auth",
            "--no-input",
            "--output",
            "toon",
        ],
    );
    let removed = toon(&removed);
    assert_eq!(removed["operation"], "feature_remove");
    assert_eq!(
        fs::read_to_string(target.join("app.txt")).unwrap(),
        "auth = false\n"
    );
}

#[test]
fn feature_add_converges_new_runtime_dependencies_without_a_second_command() {
    let source = recipe();
    write(
        source.path().join("base/devme.toml"),
        "schema_version = 1\n",
    );
    write(
        source.path().join("features/auth/devme.toml"),
        r#"schema_version = 1

[step.auth-dependencies]
check = "test -f .auth-ready"
provision = "touch .auth-ready"
trust = "auto"

[service.auth-runtime]
cmd = "sleep 60"
depends_on = ["auth-dependencies"]
"#,
    );
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    toon(&run(
        workspace.path(),
        source.path(),
        &[
            "create",
            "native",
            "groceries",
            "--no-input",
            "--output",
            "toon",
        ],
    ));

    let added = run(
        &target,
        source.path(),
        &["feature", "add", "auth", "--no-input", "--output", "toon"],
    );

    let added = toon(&added);
    assert_eq!(added["operation"], "feature_add");
    assert!(target.join(".auth-ready").is_file());
    assert!(run(&target, source.path(), &["down"]).status.success());
}

#[test]
fn feature_conflicts_use_exit_five_and_structured_recovery() {
    let source = recipe();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    toon(&run(
        workspace.path(),
        source.path(),
        &[
            "create",
            "native",
            "groceries",
            "--no-input",
            "--output",
            "toon",
        ],
    ));
    write(target.join("app.txt"), "auth = custom\n");

    let conflict = run(
        &target,
        source.path(),
        &["feature", "add", "auth", "--no-input", "--output", "toon"],
    );

    assert_eq!(conflict.status.code(), Some(5));
    assert!(
        conflict.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&conflict.stderr)
    );
    let report: serde_json::Value =
        toon_format::decode_strict(std::str::from_utf8(&conflict.stdout).unwrap()).unwrap();
    assert_eq!(report["error"]["code"], "conflict");
    assert_eq!(report["error"]["paths"][0], "app.txt");
    assert!(
        report["error"]["help"]
            .as_str()
            .unwrap()
            .contains("devme feature add auth --dry-run")
    );

    let human = run(
        &target,
        source.path(),
        &["feature", "add", "auth", "--output", "human"],
    );
    assert_eq!(human.status.code(), Some(5));
    let stderr = String::from_utf8(human.stderr).unwrap();
    assert!(stderr.contains("app.txt"));
    assert!(stderr.contains("Help:"));
    assert!(stderr.contains("devme feature add auth --dry-run"));
}

#[test]
fn create_add_is_rejected_with_the_feature_command_hint() {
    let source = recipe();
    let workspace = TempDir::new().unwrap();

    let invalid = run(
        workspace.path(),
        source.path(),
        &["create", "add", "auth", "--no-input", "--output", "toon"],
    );

    assert_eq!(invalid.status.code(), Some(2));
    let report: serde_json::Value =
        toon_format::decode_strict(std::str::from_utf8(&invalid.stdout).unwrap()).unwrap();
    assert!(
        report["error"]["help"]
            .as_str()
            .unwrap()
            .contains("devme feature add auth")
    );
}

#[test]
fn explicit_human_output_is_a_scannable_command_surface() {
    let source = recipe();
    let workspace = TempDir::new().unwrap();

    let discovery = run(
        workspace.path(),
        source.path(),
        &["create", "--output", "human"],
    );

    assert!(discovery.status.success());
    let output = String::from_utf8(discovery.stdout).unwrap();
    assert!(output.starts_with("Templates\n"));
    assert!(output.contains("native"));
    assert!(output.contains("Next: devme create native <path>"));
    assert!(!output.contains("\"schema_version\""));

    let created = run(
        workspace.path(),
        source.path(),
        &[
            "create",
            "native",
            "groceries",
            "--with",
            "auth",
            "--output",
            "human",
        ],
    );
    assert!(created.status.success());
    let output = String::from_utf8(created.stdout).unwrap();
    assert!(output.contains("Untrusted manual recipe guidance"));
    assert!(output.contains("carefully"));
    assert!(!output.contains('\u{1b}'));
}
