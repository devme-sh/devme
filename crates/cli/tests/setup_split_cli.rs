use std::hash::{Hash, Hasher};
use std::process::{Command, Output};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_devme")
}

fn runtime(dir: &std::path::Path) -> std::path::PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    dir.hash(&mut hasher);
    std::path::PathBuf::from(format!(
        "/tmp/devme-setup-split-{}-{:x}",
        std::process::id(),
        hasher.finish()
    ))
}

fn run(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("XDG_RUNTIME_DIR", runtime(dir))
        .output()
        .unwrap()
}

fn native_fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    for directory in ["apps/ios", "apps/android", "backend", "web"] {
        std::fs::create_dir_all(dir.path().join(directory)).unwrap();
    }
    std::fs::write(dir.path().join("apps/ios/App.xcworkspace"), "").unwrap();
    std::fs::write(dir.path().join("apps/android/gradlew"), "#!/bin/sh\n").unwrap();
    std::fs::write(
        dir.path().join("apps/android/build.gradle.kts"),
        "plugins { id(\"com.android.application\") }",
    )
    .unwrap();
    std::fs::write(dir.path().join("backend/convex.json"), "").unwrap();
    std::fs::write(dir.path().join("web/vite.config.ts"), "").unwrap();
    std::fs::write(
        dir.path().join("web/package.json"),
        r#"{"scripts":{"dev":"vite"},"devDependencies":{"vite":"7.0.0"}}"#,
    )
    .unwrap();
    dir
}

#[test]
fn split_dry_run_previews_every_file_without_writing() {
    let dir = native_fixture();
    let output = run(dir.path(), &["setup", "split", "--dry-run"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for path in [
        "devme.toml",
        "apps/ios/devme.toml",
        "apps/android/devme.toml",
        "backend/devme.toml",
        "web/devme.toml",
    ] {
        assert!(stdout.contains(&format!("==> {path} <==")), "{stdout}");
        assert!(!dir.path().join(path).exists());
    }
    assert!(stdout.contains("[workspace.members]"));
}

#[test]
fn split_write_creates_a_workspace_that_checks_from_root_and_member() {
    let dir = native_fixture();
    let setup = run(dir.path(), &["setup", "split", "--write"]);
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );

    let root_check = run(dir.path(), &["--json", "config", "check"]);
    assert!(
        root_check.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&root_check.stdout),
        String::from_utf8_lossy(&root_check.stderr)
    );
    let member_check = run(&dir.path().join("apps/ios"), &["--json", "config", "check"]);
    assert!(
        member_check.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&member_check.stdout),
        String::from_utf8_lossy(&member_check.stderr)
    );

    let root = std::fs::read_to_string(dir.path().join("devme.toml")).unwrap();
    assert!(root.contains("ios = \"apps/ios\""));
    let ios = std::fs::read_to_string(dir.path().join("apps/ios/devme.toml")).unwrap();
    assert!(ios.contains("[task.ios-build]"));
    assert!(!ios.contains("cwd ="));
}

#[test]
fn setup_write_remains_the_single_file_compatibility_path() {
    let dir = native_fixture();
    let setup = run(dir.path(), &["setup", "--write"]);
    assert!(setup.status.success());
    assert!(dir.path().join("devme.toml").is_file());
    assert!(!dir.path().join("apps/ios/devme.toml").exists());
    let config = std::fs::read_to_string(dir.path().join("devme.toml")).unwrap();
    assert!(config.contains("cwd = \"apps/ios\""));
    assert!(!config.contains("[workspace.members]"));
}

#[test]
fn root_gradle_wrapper_executes_android_member_task_from_the_build_root() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("apps/android")).unwrap();
    std::fs::write(
        dir.path().join("settings.gradle.kts"),
        "include(\":apps:android\")\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("apps/android/build.gradle.kts"),
        "plugins { id(\"com.android.application\") }\n",
    )
    .unwrap();
    let wrapper = dir.path().join("gradlew");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s' \"$PWD\" > gradle-cwd\nprintf '%s' \"$*\" > gradle-args\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions).unwrap();

    let setup = run(dir.path(), &["setup", "split", "--write"]);
    assert!(
        setup.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&setup.stdout),
        String::from_utf8_lossy(&setup.stderr)
    );
    let android = dir.path().join("apps/android");
    let output = run(&android, &["run", "android-test", "--output", "json"]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["task"], "android::android-test");
    assert_eq!(result["status"], "passed");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("gradle-cwd")).unwrap(),
        dir.path().canonicalize().unwrap().display().to_string()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("gradle-args")).unwrap(),
        "--no-daemon test"
    );
}

#[test]
fn nested_android_module_stays_inside_its_gradle_workspace_member() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("apps/android/app")).unwrap();
    std::fs::write(
        dir.path().join("apps/android/settings.gradle.kts"),
        "include(\":app\")\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("apps/android/gradlew"), "#!/bin/sh\n").unwrap();
    std::fs::write(
        dir.path().join("apps/android/build.gradle.kts"),
        "plugins { id(\"com.android.application\") apply false }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("apps/android/app/build.gradle.kts"),
        "plugins { id(\"com.android.application\") }\n",
    )
    .unwrap();

    let output = run(dir.path(), &["setup", "split", "--dry-run"]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("android = \"apps/android\""), "{stdout}");
    assert!(
        stdout.contains("==> apps/android/devme.toml <=="),
        "{stdout}"
    );
    assert!(!stdout.contains("apps/android/app/devme.toml"), "{stdout}");
}

#[test]
fn split_detection_ignores_generated_devme_state() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("apps/ios")).unwrap();
    std::fs::create_dir_all(
        dir.path()
            .join(".devme/SourcePackages/checkouts/convex-swift"),
    )
    .unwrap();
    std::fs::write(dir.path().join("apps/ios/App.xcodeproj"), "").unwrap();
    std::fs::write(
        dir.path()
            .join(".devme/SourcePackages/checkouts/convex-swift/Package.swift"),
        "",
    )
    .unwrap();

    let output = run(dir.path(), &["setup", "split", "--dry-run"]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("convex-swift"), "{stdout}");
    assert!(!stdout.contains(".devme/"), "{stdout}");
}

#[test]
fn vite_tooling_without_a_dev_script_does_not_invent_a_web_service() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("apps/ios")).unwrap();
    std::fs::write(dir.path().join("apps/ios/App.xcodeproj"), "").unwrap();
    std::fs::write(dir.path().join("vite.config.ts"), "").unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"scripts":{"check":"vp check"},"devDependencies":{"vite-plus":"0.2.4"}}"#,
    )
    .unwrap();

    let output = run(dir.path(), &["setup", "split", "--dry-run"]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("[service.web]"), "{stdout}");
    assert!(!stdout.contains("bun run dev"), "{stdout}");
}
