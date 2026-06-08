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
//! children and exits.
//!
//! Lifecycle: the daemon exits when its last subscriber disconnects and
//! no new subscriber arrives within a 30-second grace window. An
//! explicit `Shutdown` message also triggers immediate teardown.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use devme_config::{InterpContext, Stack, interpolate};
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
    let repo_services: Vec<ResolvedService> = stack
        .as_ref()
        .map(|s| resolve_repo_services(s, &cwd))
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

/// A repo-scoped service with its `cmd` and `env` already interpolated and
/// ready to spawn. Repo services have no per-instance slot, so they resolve
/// against slot 0 — fixed ports stay fixed and `{port.<name>}` references
/// resolve to those fixed ports.
struct ResolvedService {
    name: String,
    cmd: String,
    env: Vec<(String, String)>,
}

/// Resolve every `scope = "repo"` service into a spawnable form. Builds one
/// shared interpolation context — `{slot}` = 0, `{worktree}`, `{branch}`,
/// and `{port.<service>}` for each repo service that declares a port — then
/// interpolates each service's `cmd` and `env` against it (layering the
/// service's own `{port}` on top). A service whose `cmd` fails to
/// interpolate is dropped with a log line rather than aborting the daemon.
fn resolve_repo_services(stack: &Stack, cwd: &std::path::Path) -> Vec<ResolvedService> {
    // Sibling port map: {port.<name>} for every repo service with a port.
    let worktree = cwd.display().to_string();
    let branch = current_git_branch(cwd).unwrap_or_default();
    let mut base = InterpContext::new()
        .set("slot", "0")
        .set("worktree", worktree)
        .set("branch", branch);
    for (name, svc) in stack.service.iter().filter(|(_, s)| s.scope == Scope::Repo) {
        if let Some(spec) = svc.port {
            base.insert(format!("port.{name}"), spec.resolve(0).to_string());
        }
    }

    let mut resolved = Vec::new();
    for (name, svc) in &stack.service {
        if svc.scope != Scope::Repo {
            continue;
        }
        let mut ctx = base.clone();
        if let Some(spec) = svc.port {
            ctx.insert("port", spec.resolve(0).to_string());
        }
        let cmd = match interpolate(&svc.cmd, &ctx) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(service = %name, error = %e, "cmd interpolation failed; skipping");
                continue;
            }
        };
        let mut env = Vec::with_capacity(svc.env.len());
        for (k, v) in &svc.env {
            match interpolate(v, &ctx) {
                Ok(resolved_val) => env.push((k.clone(), resolved_val)),
                Err(e) => {
                    tracing::warn!(service = %name, var = %k, error = %e, "env interpolation failed; passing through literally");
                    env.push((k.clone(), v.clone()));
                }
            }
        }
        resolved.push(ResolvedService {
            name: name.clone(),
            cmd,
            env,
        });
    }
    resolved
}

/// Current git branch for `cwd` (populates `{branch}`); `None` outside a
/// git checkout or in detached-HEAD state.
fn current_git_branch(cwd: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8(out.stdout).ok()?;
    let trimmed = branch.trim();
    if trimmed.is_empty() || trimmed == "HEAD" {
        return None;
    }
    Some(trimmed.to_string())
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

/// Grace period after the last subscriber disconnects before the shared
/// daemon tears itself down. Long enough that a cargo-watch restart of the
/// TUI doesn't flap the daemon; short enough that genuinely-orphaned daemons
/// don't linger forever.
const IDLE_GRACE_SECS: u64 = 30;

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
    /// Active subscriber count. When this drops to 0, the idle grace timer
    /// starts; if it's still 0 when the timer fires, the daemon exits.
    subscribers: Arc<std::sync::atomic::AtomicUsize>,
    /// Notifies the serve loop when subscriber count changes.
    sub_notify: Arc<tokio::sync::Notify>,
}

impl SharedState {
    async fn spawn_all(cwd: &std::path::Path, services: &[ResolvedService]) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        let services_map: Arc<Mutex<HashMap<String, RunningService>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let shutdown = Arc::new(Mutex::new(false));

        // Spawn each repo-scoped service. Failures are surfaced as Failed
        // snapshots rather than killing the whole daemon, because partial
        // availability is more useful than nothing at all.
        for ResolvedService { name, cmd, env } in services {
            let env_slice: Vec<(&str, &str)> =
                env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            match ChildProcess::spawn_parts::<&str>(cmd, cwd, &env_slice) {
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
            subscribers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            sub_notify: Arc::new(tokio::sync::Notify::new()),
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
    use std::sync::atomic::Ordering;

    // Idle teardown task: when subscriber count drops to 0, wait the grace
    // period then signal shutdown — unless a new subscriber arrives first.
    let idle_state = state.clone();
    tokio::spawn(async move {
        loop {
            idle_state.sub_notify.notified().await;
            if idle_state.subscribers.load(Ordering::SeqCst) == 0 {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(IDLE_GRACE_SECS)) => {
                        if idle_state.subscribers.load(Ordering::SeqCst) == 0 {
                            tracing::info!("no subscribers for {IDLE_GRACE_SECS}s — shutting down");
                            *idle_state.shutdown.lock().await = true;
                            return;
                        }
                    }
                    _ = idle_state.sub_notify.notified() => {
                        // A subscriber arrived during the grace window — cancel.
                    }
                }
            }
        }
    });

    loop {
        if *state.shutdown.lock().await {
            state.shutdown_services().await;
            return Ok(());
        }
        let accept = listener.accept();
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), accept).await;
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
    use std::sync::atomic::Ordering;

    let mut framed = Framed::new(stream, FrameCodec);
    let mut events = state.events.subscribe();
    let mut subscribed = false;

    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
                incoming = framed.next() => {
                    let Some(item) = incoming else { break };
                    let bytes = item?;
                    let env: Envelope<ClientMessage> = serde_json::from_slice(&bytes)?;
                    match env.payload {
                        ClientMessage::Subscribe { .. } => {
                            if !subscribed {
                                subscribed = true;
                                state.subscribers.fetch_add(1, Ordering::SeqCst);
                                state.sub_notify.notify_one();
                            }
                            let services = state.current_snapshot().await;
                            let reply = ServerMessage::Subscribed {
                                instance: info.clone(),
                                services,
                                steps: vec![],
                            };
                            send_msg(&mut framed, reply).await?;
                        }
                        ClientMessage::Unsubscribe => {
                            if subscribed {
                                subscribed = false;
                                state.subscribers.fetch_sub(1, Ordering::SeqCst);
                                state.sub_notify.notify_one();
                            }
                        }
                        ClientMessage::Shutdown => {
                            let _ = send_msg(
                                &mut framed,
                                ServerMessage::Goodbye { reason: "shutdown requested".into() },
                            )
                            .await;
                            *state.shutdown.lock().await = true;
                            return Ok(());
                        }
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
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        Ok(())
    }.await;

    if subscribed {
        state.subscribers.fetch_sub(1, Ordering::SeqCst);
        state.sub_notify.notify_one();
    }
    result
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
