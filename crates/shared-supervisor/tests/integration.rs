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
        let p = std::path::PathBuf::from(format!(
            "/tmp/devme-test-{}-{}",
            std::process::id(),
            n
        ));
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
        ServerMessage::Subscribed { instance, services, steps } => {
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
