use std::process::{Command, Output};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_devme")
}

fn fixture() -> TempDir {
    let dir = tempfile::Builder::new()
        .prefix("devme-down-nongit-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::write(
        dir.path().join("devme.toml"),
        r#"schema_version = 1

[service.worker]
cmd = "touch started; sleep 30"
stop = "touch stopped"
"#,
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("runtime")).unwrap();
    dir
}

fn run(dir: &TempDir, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", dir.path().join("runtime"))
        .output()
        .unwrap()
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
fn down_all_stops_the_current_non_git_project() {
    let dir = fixture();

    let up = run(&dir, &["up", "-d"]);
    assert!(
        up.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&up.stdout),
        String::from_utf8_lossy(&up.stderr),
    );
    wait_for(&dir.path().join("started"));

    let down = run(&dir, &["down", "--all"]);
    assert!(
        down.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&down.stdout),
        String::from_utf8_lossy(&down.stderr),
    );
    wait_for(&dir.path().join("stopped"));
}
