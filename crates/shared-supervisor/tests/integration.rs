//! End-to-end: spawn the shared-supervisor binary, connect a client,
//! subscribe, and verify the response carries a "shared::<repo>" identity
//! anchored to the binary's cwd. Also verify a second instance refuses to
//! bind when one is already running, and Shutdown actually exits.

use std::process::{Command, Stdio};
use std::time::Duration;

use devme_client::Client;
use devme_core::{ClientMessage, ServerMessage};
use std::sync::atomic::{AtomicU32, Ordering};

/// Short tempdir under `/tmp` — system TempDir on macOS lives under
/// `/var/folders/...` which blows past Unix socket `SUN_LEN` (104 chars)
/// once we nest `devme/repos/<hash>/shared.sock` inside.
struct ShortTmp(std::path::PathBuf);
impl ShortTmp {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::path::PathBuf::from(format!("/tmp/devme-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for ShortTmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Walks `${runtime}/devme/repos/*/shared.sock` until one exists. If the
/// child dies first, dumps its stderr into the panic message so the test
/// log shows *why* it didn't bind instead of just a timeout.
async fn wait_for_shared_socket_or_diag(
    runtime: &std::path::Path,
    child: &mut std::process::Child,
) -> std::path::PathBuf {
    use std::io::Read;
    let repos = runtime.join("devme").join("repos");
    let started = std::time::Instant::now();
    let timeout = Duration::from_secs(2);
    loop {
        if let Ok(rd) = std::fs::read_dir(&repos) {
            for entry in rd.flatten() {
                let sock = entry.path().join("shared.sock");
                // `exists()` follows symlinks but returns false for some
                // ENOENTs on socket files when the directory race-loses;
                // use `metadata()` to be explicit.
                if sock.metadata().is_ok() {
                    return sock;
                }
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            let mut stderr = String::new();
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_string(&mut stderr);
            }
            panic!(
                "shared-supervisor exited early ({status:?}) before binding\n--- stderr ---\n{stderr}"
            );
        }
        if started.elapsed() > timeout {
            let listing = std::fs::read_dir(&repos)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.path().display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|e| format!("(read_dir err: {e})"));
            let mut stderr = String::new();
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_string(&mut stderr);
            }
            panic!(
                "shared.sock did not appear under {} within {timeout:?}\nentries: [{listing}]\n--- stderr ---\n{stderr}",
                repos.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn binary_path() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo for integration tests of bin
    // targets — resolves to the freshly-built artifact for this profile.
    let p = env!("CARGO_BIN_EXE_devme-shared-supervisor");
    std::path::PathBuf::from(p)
}

#[tokio::test]
async fn binds_and_responds_to_subscribe_with_shared_identity() {
    // Isolated runtime + temp HOME so we don't collide with other tests
    // or the user's own daemons.
    let tmp = ShortTmp::new();
    let runtime = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut child = Command::new(binary_path())
        .current_dir(&cwd)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Wait for the socket to appear under our isolated runtime dir.
    let sock = wait_for_shared_socket_or_diag(&runtime, &mut child).await;

    let mut client = Client::connect(&sock).await.unwrap();
    let resp = client
        .request(ClientMessage::Subscribe { services: vec![] })
        .await
        .unwrap();
    let id = match resp {
        ServerMessage::Subscribed {
            instance,
            services,
            steps,
        } => {
            assert!(services.is_empty(), "shared daemon has no services yet");
            assert!(steps.is_empty(), "shared daemon has no steps yet");
            instance.id
        }
        other => panic!("expected Subscribed, got {other:?}"),
    };
    assert!(
        id.starts_with("shared::"),
        "instance id should be shared::<repo-hash>, got {id}"
    );

    // Clean shutdown.
    let _ = client.send(ClientMessage::Shutdown).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let _ = child.kill();
}

#[tokio::test]
async fn spawns_repo_scoped_services_from_devme_toml_and_streams_logs() {
    use base64::Engine;
    let tmp = ShortTmp::new();
    let runtime = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();

    // Two services: one repo-scoped (must spawn), one instance-scoped
    // (must NOT spawn — proves filtering).
    std::fs::write(
        cwd.join("devme.toml"),
        r#"
schema_version = 1

[service.cache]
cmd = "while true; do echo SHARED-MARKER; sleep 0.2; done"
scope = "repo"

[service.web]
cmd = "while true; do echo INSTANCE-MARKER; sleep 0.2; done"
"#,
    )
    .unwrap();

    let mut child = Command::new(binary_path())
        .current_dir(&cwd)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let sock = wait_for_shared_socket_or_diag(&runtime, &mut child).await;
    let mut client = Client::connect(&sock).await.unwrap();

    // Subscribe; the snapshot must contain `cache` but not `web`.
    let resp = client
        .request(ClientMessage::Subscribe { services: vec![] })
        .await
        .unwrap();
    let services = match resp {
        ServerMessage::Subscribed { services, .. } => services,
        other => panic!("expected Subscribed, got {other:?}"),
    };
    let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"cache"),
        "cache must be spawned, got {names:?}"
    );
    assert!(
        !names.contains(&"web"),
        "instance-scoped 'web' must NOT be spawned by shared daemon, got {names:?}"
    );

    // Drain events until we see the SHARED-MARKER from the cache service.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut saw_marker = false;
    while std::time::Instant::now() < deadline && !saw_marker {
        let next = tokio::time::timeout(Duration::from_millis(200), client.next_event()).await;
        if let Ok(Ok(Some(ServerMessage::LogChunk { service, bytes, .. }))) = next {
            if service != "cache" {
                continue;
            }
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(bytes.as_bytes())
                .unwrap();
            let line = String::from_utf8_lossy(&decoded);
            if line.contains("SHARED-MARKER") {
                saw_marker = true;
            }
        }
    }
    assert!(
        saw_marker,
        "did not receive SHARED-MARKER LogChunk within 3s"
    );

    let _ = client.send(ClientMessage::Shutdown).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let _ = child.kill();
}

#[tokio::test]
async fn stop_hook_runs_on_shutdown_and_process_is_killed() {
    // A repo service that never exits on its own (`sleep 1000`) with a `stop`
    // teardown command. On Shutdown the supervisor must run the stop command
    // (proven by a marker file) and then signal the process dead.
    let tmp = ShortTmp::new();
    let runtime = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    let marker = cwd.join("stop-ran");

    std::fs::write(
        cwd.join("devme.toml"),
        format!(
            r#"
schema_version = 1

[service.db]
cmd = "sleep 1000"
scope = "repo"
stop = "touch {}"
"#,
            marker.display()
        ),
    )
    .unwrap();

    let mut child = Command::new(binary_path())
        .current_dir(&cwd)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let sock = wait_for_shared_socket_or_diag(&runtime, &mut child).await;
    let mut client = Client::connect(&sock).await.unwrap();

    // Grab the spawned service's pid so we can assert it's gone afterwards.
    let resp = client
        .request(ClientMessage::Subscribe { services: vec![] })
        .await
        .unwrap();
    let pid = match resp {
        ServerMessage::Subscribed { services, .. } => services
            .iter()
            .find(|s| s.name == "db")
            .and_then(|s| s.pid)
            .expect("db service should have a pid"),
        other => panic!("expected Subscribed, got {other:?}"),
    };
    assert!(
        !marker.exists(),
        "stop hook must not have run before shutdown"
    );

    let _ = client.send(ClientMessage::Shutdown).await;

    // The daemon process should exit (its teardown awaits the stop command).
    let exited = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        exited.is_ok(),
        "shared-supervisor did not exit after Shutdown"
    );

    assert!(marker.exists(), "stop hook command did not run on shutdown");
    // The `sleep 1000` must have been signalled dead, not orphaned.
    assert!(
        !pid_alive(pid),
        "service process {pid} survived shutdown (was not signalled)"
    );

    let _ = child.kill();
}

/// Best-effort liveness check via `kill -0` (dependency-free) for the
/// assertion above. Exit 0 = the process exists.
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn second_instance_refuses_to_bind_when_one_is_running() {
    let tmp = ShortTmp::new();
    let runtime = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut first = Command::new(binary_path())
        .current_dir(&cwd)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let sock = wait_for_shared_socket_or_diag(&runtime, &mut first).await;

    let second = Command::new(binary_path())
        .current_dir(&cwd)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        !second.status.success(),
        "second instance must refuse to bind, got status {:?}",
        second.status
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("already running"),
        "expected 'already running' in stderr, got: {stderr}"
    );

    // Cleanup the first daemon.
    let mut client = Client::connect(&sock).await.unwrap();
    let _ = client.send(ClientMessage::Shutdown).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(Some(_)) = first.try_wait() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let _ = first.kill();
}
