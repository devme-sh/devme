//! `devme-shared-supervisor` — per-repo daemon for `scope = "repo"` services.
//!
//! Bound at `paths::shared_socket(cwd)` (under the repo-hash directory),
//! it owns every service in cwd's `devme.toml` marked `scope = "repo"`.
//! Instance daemons connect as clients to see the shared services
//! alongside their own. See ADR-0007 for the lifecycle model.
//!
//! Today's scope (#24): spawn-then-stream. The shared daemon reads
//! `devme.toml`, finds repo-scoped services, spawns each via
//! [`devme_supervisor::process::ChildProcess`], and broadcasts every
//! line of output as a `LogChunk` to all subscribers. Shutdown kills
//! children and exits. Ref-counted teardown (auto-exit when the last
//! subscriber disconnects) is a follow-up; users currently quit
//! explicitly via the TUI's `q`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use devme_config::Stack;
use devme_core::{
    ClientMessage, Envelope, InstanceInfo, ServerMessage, ServiceSnapshot, ServiceState,
    Scope,
};
use devme_ipc::FrameCodec;
use devme_supervisor::process::ChildProcess;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::codec::Framed;

fn main() {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("devme-shared-supervisor: tokio init failed: {e}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async {
        match real_main().await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("devme-shared-supervisor: {e}");
                1
            }
        }
    });
    std::process::exit(code);
}

async fn real_main() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let repo_id = devme_config::paths::repo_id(&cwd);
    let sock = devme_config::paths::shared_socket(&cwd)?;

    let listener = match try_bind(&sock).await? {
        Some(l) => l,
        None => {
            return Err(anyhow::anyhow!(
                "shared daemon already running for repo {} at {}",
                repo_id,
                sock.display()
            ));
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let info = InstanceInfo {
        id: format!("shared::{repo_id}"),
        label: format!("shared ({})", repo_id_short(&repo_id)),
        cwd: cwd.display().to_string(),
    };

    // Parse the stack and pick the repo-scoped services. If devme.toml is
    // missing the daemon still runs (responding to Subscribe with an
    // empty snapshot), so instance daemons can attach harmlessly.
    let stack = read_stack(&cwd).ok();
    let repo_services: Vec<(String, String)> = stack
        .as_ref()
        .map(|s| {
            s.service
                .iter()
                .filter(|(_, svc)| svc.scope == Scope::Repo)
                .map(|(name, svc)| (name.clone(), svc.cmd.clone()))
                .collect()
        })
        .unwrap_or_default();

    tracing::info!(
        repo_id = %repo_id,
        socket = %sock.display(),
        services = repo_services.len(),
        "shared supervisor up"
    );

    let state = SharedState::spawn_all(&cwd, &repo_services).await;
    let result = serve(listener, info, state).await;
    let _ = std::fs::remove_file(&sock);
    result
}

async fn try_bind(path: &std::path::Path) -> anyhow::Result<Option<UnixListener>> {
    if UnixStream::connect(path).await.is_ok() {
        return Ok(None);
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    Ok(Some(listener))
}

fn read_stack(cwd: &std::path::Path) -> anyhow::Result<Stack> {
    let raw = std::fs::read_to_string(cwd.join("devme.toml"))?;
    let stack = Stack::parse(&raw)?;
    Ok(stack)
}

fn repo_id_short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// One spawned child + its derived snapshot.
struct RunningService {
    snapshot: ServiceSnapshot,
    /// Best-effort killer; held so we can stop the process on Shutdown.
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

/// Live state shared between the accept loop and connection handlers.
struct SharedState {
    /// Service name → running handle. `Mutex` because Shutdown reaches in
    /// from any handler.
    services: Arc<Mutex<HashMap<String, RunningService>>>,
    /// Broadcast every server message so each subscriber's writer task can
    /// forward it. `LogChunk` and `StatusUpdate` go through here.
    events: broadcast::Sender<ServerMessage>,
    /// Set to true on Shutdown so the accept loop exits.
    shutdown: Arc<Mutex<bool>>,
}

impl SharedState {
    async fn spawn_all(cwd: &std::path::Path, services: &[(String, String)]) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        let services_map: Arc<Mutex<HashMap<String, RunningService>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let shutdown = Arc::new(Mutex::new(false));

        // Spawn each repo-scoped service. Failures are surfaced as Failed
        // snapshots rather than killing the whole daemon, because partial
        // availability is more useful than nothing at all.
        for (name, cmd) in services {
            match ChildProcess::spawn_parts::<&str>(cmd, cwd, &[]) {
                Ok(parts) => {
                    let pid = parts.pid;
                    spawn_log_forwarder(name.clone(), parts.lines, events.clone());
                    spawn_exit_forwarder(
                        name.clone(),
                        parts.exit,
                        events.clone(),
                        services_map.clone(),
                    );
                    let snapshot = ServiceSnapshot {
                        name: name.clone(),
                        state: ServiceState::Running { degraded: false, started_without: vec![] },
                        pid: Some(pid),
                        port: None,
                        restart_count: 0,
                    };
                    services_map.lock().await.insert(
                        name.clone(),
                        RunningService { snapshot: snapshot.clone(), killer: parts.killer },
                    );
                }
                Err(e) => {
                    tracing::error!(service = %name, error = %e, "spawn failed");
                }
            }
        }

        Arc::new(Self {
            services: services_map,
            events,
            shutdown,
        })
    }

    async fn current_snapshot(&self) -> Vec<ServiceSnapshot> {
        self.services
            .lock()
            .await
            .values()
            .map(|r| r.snapshot.clone())
            .collect()
    }

    async fn shutdown_services(&self) {
        let mut svcs = self.services.lock().await;
        for (_, r) in svcs.iter_mut() {
            let _ = r.killer.kill();
        }
        svcs.clear();
    }
}

/// Per-spawn task: reads PTY lines and broadcasts them as `LogChunk`s.
/// Empty lines are dropped because the renderer treats them as visual
/// padding and a noisy service would just push real content off-screen.
fn spawn_log_forwarder(
    name: String,
    mut lines: mpsc::UnboundedReceiver<String>,
    events: broadcast::Sender<ServerMessage>,
) {
    tokio::spawn(async move {
        while let Some(line) = lines.recv().await {
            if line.is_empty() {
                continue;
            }
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let bytes = base64::engine::general_purpose::STANDARD.encode(line.as_bytes());
            let _ = events.send(ServerMessage::LogChunk { service: name.clone(), bytes, ts });
        }
    });
}

/// Per-spawn task: waits on the exit oneshot and emits a Failed/Stopped
/// snapshot so subscribers can see when a service dies. Removes the
/// service from the running set so a subsequent shutdown doesn't try to
/// kill a dead PID (Apple sometimes returns ESRCH; cheap to avoid).
fn spawn_exit_forwarder(
    name: String,
    exit: tokio::sync::oneshot::Receiver<i32>,
    events: broadcast::Sender<ServerMessage>,
    services: Arc<Mutex<HashMap<String, RunningService>>>,
) {
    tokio::spawn(async move {
        let code = exit.await.unwrap_or(-1);
        services.lock().await.remove(&name);
        let state = if code == 0 {
            ServiceState::Stopped
        } else {
            ServiceState::Failed { exit_code: Some(code) }
        };
        let _ = events.send(ServerMessage::StatusUpdate {
            service: name,
            state,
            pid: None,
            port: None,
            restart_count: 0,
        });
    });
}

async fn serve(
    listener: UnixListener,
    info: InstanceInfo,
    state: Arc<SharedState>,
) -> anyhow::Result<()> {
    loop {
        if *state.shutdown.lock().await {
            state.shutdown_services().await;
            return Ok(());
        }
        let accept = listener.accept();
        // Wake periodically to check the shutdown flag — a long-idle daemon
        // with no inbound traffic would otherwise block here forever.
        let res = tokio::time::timeout(std::time::Duration::from_secs(60), accept).await;
        match res {
            Ok(Ok((stream, _))) => {
                let info = info.clone();
                let state_t = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, info, state_t).await {
                        tracing::debug!(?e, "connection ended");
                    }
                });
            }
            Ok(Err(e)) => {
                tracing::error!(?e, "accept failed");
                return Err(e.into());
            }
            Err(_) => {} // timeout — loop back and recheck shutdown
        }
    }
}

async fn handle(
    stream: UnixStream,
    info: InstanceInfo,
    state: Arc<SharedState>,
) -> anyhow::Result<()> {
    let mut framed = Framed::new(stream, FrameCodec);
    let mut events = state.events.subscribe();

    loop {
        tokio::select! {
            incoming = framed.next() => {
                let Some(item) = incoming else { break };
                let bytes = item?;
                let env: Envelope<ClientMessage> = serde_json::from_slice(&bytes)?;
                match env.payload {
                    ClientMessage::Subscribe { .. } => {
                        let services = state.current_snapshot().await;
                        let reply = ServerMessage::Subscribed {
                            instance: info.clone(),
                            services,
                            steps: vec![],
                        };
                        send_msg(&mut framed, reply).await?;
                    }
                    ClientMessage::Unsubscribe => {}
                    ClientMessage::Shutdown => {
                        let _ = send_msg(
                            &mut framed,
                            ServerMessage::Goodbye { reason: "shutdown requested".into() },
                        )
                        .await;
                        *state.shutdown.lock().await = true;
                        return Ok(());
                    }
                    // Restart/Stop/Start of repo-scoped services arrives
                    // here once instance daemons learn to route them.
                    // Today: acknowledge with a Notice so the client sees
                    // we received it but didn't act.
                    _ => {
                        let _ = send_msg(
                            &mut framed,
                            ServerMessage::Notice {
                                level: devme_core::NoticeLevel::Warn,
                                message: "shared supervisor: per-service control not yet routed"
                                    .into(),
                            },
                        )
                        .await;
                    }
                }
            }
            broadcast = events.recv() => match broadcast {
                Ok(msg) => send_msg(&mut framed, msg).await?,
                // Lagged means the broadcast buffer dropped some messages;
                // forward what's still available rather than disconnecting.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    Ok(())
}

async fn send_msg(
    framed: &mut Framed<UnixStream, FrameCodec>,
    msg: ServerMessage,
) -> anyhow::Result<()> {
    let env = Envelope::new(msg);
    let bytes = serde_json::to_vec(&env)?;
    framed.send(bytes.as_slice()).await?;
    Ok(())
}

#[allow(dead_code)]
fn _unused(_: PathBuf) {}
