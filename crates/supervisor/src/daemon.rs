//! Single-task event-loop daemon.
//!
//! All state — [`Executor`], child processes, log ring buffers, connected
//! clients — lives on one task. The accept loop, per-connection sockets, and
//! per-process line/exit readers run on their own tasks and forward
//! [`InternalEvent`]s through an mpsc into the event loop. Outbound
//! [`ServerMessage`]s go the other way through per-client mpsc senders.
//!
//! See ADR-0003 (daemon lifecycle) and ADR-0010 (architecture).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine;
use devme_config::{Graph, Stack};
use devme_core::{
    ClientMessage, Envelope, ErrorCode, NoticeLevel, ServerMessage, ServiceSnapshot,
    ServiceState, StepSnapshot, StepState,
};
use devme_executor::{Action, Event as ExecEvent, Executor, NodeStatus};
use devme_ipc::FrameCodec;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::codec::Framed;

use crate::process::ChildProcess;

/// Per-service log ring capacity. ~2000 lines is enough to scroll back a
/// minute or two of moderately chatty output without unbounded memory.
const RING_CAPACITY: usize = 2000;

/// Grace period between spawning a service and treating it as healthy when
/// it has no explicit health probe. Long enough to skip the "Starting"
/// flicker for instant-up commands; short enough that the user still sees
/// the transition.
const HEALTH_GRACE_MS: u64 = 150;

type ClientId = u64;
type ClientTx = mpsc::UnboundedSender<ServerMessage>;

/// Events posted to the central event loop. Everything that mutates state
/// flows through this enum.
enum InternalEvent {
    ClientConnected {
        id: ClientId,
        tx: ClientTx,
    },
    ClientMessage {
        id: ClientId,
        msg: ClientMessage,
    },
    ClientDisconnected {
        id: ClientId,
    },
    ProcessLine {
        service: String,
        generation: u64,
        line: String,
    },
    ProcessExited {
        service: String,
        generation: u64,
        exit_code: Option<i32>,
    },
    ServiceGracePassed {
        service: String,
        generation: u64,
    },
}

struct ChildRecord {
    generation: u64,
    pid: u32,
    port: Option<u16>,
    restart_count: u32,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

struct RingBuffer {
    capacity: usize,
    lines: VecDeque<(u64, String)>,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            lines: VecDeque::with_capacity(capacity.min(64)),
        }
    }

    fn push(&mut self, ts: u64, line: String) {
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back((ts, line));
    }

    fn iter(&self) -> impl Iterator<Item = &(u64, String)> {
        self.lines.iter()
    }
}

struct DaemonState {
    stack: Arc<Stack>,
    executor: Executor,
    children: HashMap<String, ChildRecord>,
    logs: HashMap<String, RingBuffer>,
    clients: HashMap<ClientId, ClientTx>,
    /// Per-client subscription filter. Empty vec = subscribed to everything.
    subscriptions: HashMap<ClientId, Vec<String>>,
    /// Services whose next exit should be reported as Stopped, not Failed —
    /// because the user (or a restart sequence) asked them to stop.
    intentional_stops: HashSet<String>,
    /// Services that should be spawned again as soon as their previous
    /// instance has exited. Populated by [`ClientMessage::Restart`].
    pending_restarts: HashSet<String>,
    next_generation: u64,
    next_client_id: u64,
    shutting_down: bool,
}

impl DaemonState {
    fn new(stack: Arc<Stack>) -> Self {
        let graph = Graph::from_stack(&stack);
        Self {
            stack,
            executor: Executor::new(graph),
            children: HashMap::new(),
            logs: HashMap::new(),
            clients: HashMap::new(),
            subscriptions: HashMap::new(),
            intentional_stops: HashSet::new(),
            pending_restarts: HashSet::new(),
            next_generation: 0,
            next_client_id: 0,
            shutting_down: false,
        }
    }

    fn alloc_generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    fn current_service_state(&self, name: &str) -> ServiceState {
        match self.executor.state(name) {
            Some(NodeStatus::Service(s)) => s.clone(),
            _ => ServiceState::Stopped,
        }
    }

    fn current_step_state(&self, name: &str) -> StepState {
        match self.executor.state(name) {
            Some(NodeStatus::Step(s)) => *s,
            _ => StepState::Unknown,
        }
    }

    fn snapshot(&self) -> (Vec<ServiceSnapshot>, Vec<StepSnapshot>) {
        let services = self
            .stack
            .service
            .iter()
            .map(|(name, _)| {
                let rec = self.children.get(name);
                ServiceSnapshot {
                    name: name.clone(),
                    state: self.current_service_state(name),
                    pid: rec.map(|r| r.pid),
                    port: rec.and_then(|r| r.port),
                    restart_count: rec.map(|r| r.restart_count).unwrap_or(0),
                }
            })
            .collect();
        let steps = self
            .stack
            .step
            .iter()
            .map(|(name, _)| StepSnapshot {
                name: name.clone(),
                state: self.current_step_state(name),
            })
            .collect();
        (services, steps)
    }

    /// Send `msg` to every connected client whose subscription matches `svc`.
    /// `svc = None` means "broadcast unconditionally" (e.g. Notice/Goodbye).
    fn broadcast(&self, svc: Option<&str>, msg: ServerMessage) {
        for (id, tx) in &self.clients {
            if let Some(s) = svc {
                let subs = self.subscriptions.get(id);
                let included = match subs {
                    None => true, // not yet subscribed; harmless
                    Some(v) if v.is_empty() => true,
                    Some(v) => v.iter().any(|x| x == s),
                };
                if !included {
                    continue;
                }
            }
            let _ = tx.send(msg.clone());
        }
    }
}

/// Server-side handle to the supervisor's IPC socket.
pub struct DaemonServer {
    listener: UnixListener,
    path: PathBuf,
    stack: Arc<Stack>,
}

impl DaemonServer {
    /// Bind with an empty stack — useful only for handshake-shape tests.
    pub fn bind(path: &Path) -> std::io::Result<Self> {
        Self::bind_with_stack(path, Stack::parse("schema_version = 1\n").unwrap())
    }

    /// Bind a Unix socket and prepare to serve `stack`.
    pub fn bind_with_stack(path: &Path, stack: Stack) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            stack: Arc::new(stack),
        })
    }

    /// Long-running serve loop. Returns when a client requests Shutdown or
    /// the listener errors.
    pub async fn serve(self) -> std::io::Result<()> {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel::<InternalEvent>();
        let accept_tx = internal_tx.clone();
        let DaemonServer { listener, path, stack } = self;

        tokio::spawn(accept_loop(listener, accept_tx));

        let result = run_event_loop(internal_tx, internal_rx, stack).await;
        // Best-effort socket cleanup. The next bind() also removes a stale
        // file, so a daemon crash leaves no permanent debris.
        let _ = std::fs::remove_file(&path);
        result
    }
}

/// Per-connection task spawned from the accept loop. Owns both halves of the
/// framed socket: parses inbound, forwards as `ClientMessage` events, and
/// sends `ServerMessage`s pulled from `client_rx`.
async fn handle_connection(
    stream: UnixStream,
    id: ClientId,
    internal_tx: mpsc::UnboundedSender<InternalEvent>,
    mut client_rx: mpsc::UnboundedReceiver<ServerMessage>,
) {
    let mut conn = framed(stream);
    loop {
        tokio::select! {
            biased;
            outgoing = client_rx.recv() => match outgoing {
                Some(msg) => {
                    let goodbye = matches!(msg, ServerMessage::Goodbye { .. });
                    let env = Envelope::new(msg);
                    let bytes = match serde_json::to_vec(&env) {
                        Ok(b) => b,
                        Err(_) => break,
                    };
                    if conn.send(bytes.as_slice()).await.is_err() {
                        break;
                    }
                    if goodbye {
                        // Give the kernel a beat to flush, then close.
                        break;
                    }
                }
                None => break,
            },
            incoming = conn.next() => match incoming {
                Some(Ok(bytes)) => {
                    match serde_json::from_slice::<Envelope<ClientMessage>>(&bytes) {
                        Ok(env) => {
                            if internal_tx
                                .send(InternalEvent::ClientMessage { id, msg: env.payload })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            let err = ServerMessage::Error {
                                code: ErrorCode::Usage,
                                message: format!("malformed request: {e}"),
                            };
                            let env = Envelope::new(err);
                            if let Ok(b) = serde_json::to_vec(&env) {
                                let _ = conn.send(b.as_slice()).await;
                            }
                        }
                    }
                }
                Some(Err(_)) | None => break,
            },
        }
    }
    let _ = internal_tx.send(InternalEvent::ClientDisconnected { id });
}

async fn accept_loop(listener: UnixListener, internal_tx: mpsc::UnboundedSender<InternalEvent>) {
    // Client ids are issued by the event loop; each accept here just
    // creates the client's outbound mpsc and posts ClientConnected so the
    // event loop can register both. The id is allocated by the event loop
    // before sending the tx half back via a oneshot? Cleaner: allocate the
    // id from a local counter here. The event loop's `next_client_id` is
    // only used for diagnostics; the only invariant is uniqueness.
    let mut next_id: u64 = 1;
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => return,
        };
        let id = next_id;
        next_id += 1;
        let (client_tx, client_rx) = mpsc::unbounded_channel::<ServerMessage>();
        if internal_tx
            .send(InternalEvent::ClientConnected { id, tx: client_tx })
            .is_err()
        {
            return;
        }
        let conn_tx = internal_tx.clone();
        tokio::spawn(handle_connection(stream, id, conn_tx, client_rx));
    }
}

async fn run_event_loop(
    internal_tx: mpsc::UnboundedSender<InternalEvent>,
    mut internal_rx: mpsc::UnboundedReceiver<InternalEvent>,
    stack: Arc<Stack>,
) -> std::io::Result<()> {
    let mut state = DaemonState::new(stack);

    while let Some(event) = internal_rx.recv().await {
        process_event(&mut state, event, &internal_tx);
        if state.shutting_down && state.children.is_empty() {
            break;
        }
    }
    Ok(())
}

fn process_event(
    state: &mut DaemonState,
    event: InternalEvent,
    tx: &mpsc::UnboundedSender<InternalEvent>,
) {
    match event {
        InternalEvent::ClientConnected { id, tx: ctx } => {
            state.clients.insert(id, ctx);
            // Override the local counter so diagnostics line up if anyone
            // logs it.
            if id > state.next_client_id {
                state.next_client_id = id;
            }
        }
        InternalEvent::ClientDisconnected { id } => {
            state.clients.remove(&id);
            state.subscriptions.remove(&id);
        }
        InternalEvent::ClientMessage { id, msg } => handle_client_message(state, id, msg, tx),
        InternalEvent::ProcessLine {
            service,
            generation,
            line,
        } => handle_process_line(state, service, generation, line),
        InternalEvent::ProcessExited {
            service,
            generation,
            exit_code,
        } => handle_process_exited(state, service, generation, exit_code, tx),
        InternalEvent::ServiceGracePassed {
            service,
            generation,
        } => handle_grace_passed(state, service, generation, tx),
    }
}

fn handle_client_message(
    state: &mut DaemonState,
    id: ClientId,
    msg: ClientMessage,
    tx: &mpsc::UnboundedSender<InternalEvent>,
) {
    match msg {
        ClientMessage::Subscribe { services } => {
            let (svcs, steps) = state.snapshot();
            if let Some(client) = state.clients.get(&id) {
                let _ = client.send(ServerMessage::Subscribed {
                    services: svcs,
                    steps,
                });
                // Replay buffered logs for subscribed services.
                let names: Vec<String> = if services.is_empty() {
                    state.logs.keys().cloned().collect()
                } else {
                    services.clone()
                };
                for name in names {
                    if let Some(buf) = state.logs.get(&name) {
                        for (ts, line) in buf.iter() {
                            let _ = client.send(ServerMessage::LogChunk {
                                service: name.clone(),
                                bytes: encode_line(line),
                                ts: *ts,
                            });
                        }
                    }
                }
            }
            state.subscriptions.insert(id, services);
        }
        ClientMessage::Unsubscribe => {
            state.subscriptions.remove(&id);
        }
        ClientMessage::Start { service: _, .. } => {
            // For v1, any Start triggers a global Event::Start. The service
            // argument is informational; the executor advances the whole
            // graph in topo order.
            let actions = state.executor.handle(ExecEvent::Start);
            enact_actions(state, actions, tx);
        }
        ClientMessage::Stop { service } => {
            if state.children.contains_key(&service) {
                state.intentional_stops.insert(service.clone());
            }
            let actions = state.executor.handle(ExecEvent::UserStop {
                name: service.clone(),
            });
            enact_actions(state, actions, tx);
            // If there was no running child, still emit a status update so
            // subscribers see the transition.
            if !state.children.contains_key(&service) {
                state.broadcast(
                    Some(&service),
                    ServerMessage::StatusUpdate {
                        service: service.clone(),
                        state: ServiceState::Stopped,
                        pid: None,
                        port: None,
                        restart_count: 0,
                    },
                );
            }
        }
        ClientMessage::Restart { service } => {
            state.pending_restarts.insert(service.clone());
            if state.children.contains_key(&service) {
                state.intentional_stops.insert(service.clone());
                let actions = state.executor.handle(ExecEvent::UserStop {
                    name: service.clone(),
                });
                enact_actions(state, actions, tx);
            } else {
                // Not running: just trigger restart immediately by resetting.
                let actions = state.executor.reset(&service);
                enact_actions(state, actions, tx);
                state.pending_restarts.remove(&service);
            }
        }
        ClientMessage::RecheckHealth => {
            // Stub for v1 — no probe loop running yet.
        }
        ClientMessage::Shutdown => {
            // Reply to all clients, kill children. Loop exits once children
            // map drains.
            state.broadcast(
                None,
                ServerMessage::Goodbye {
                    reason: "shutdown requested".into(),
                },
            );
            for (name, mut rec) in state.children.drain() {
                state.intentional_stops.insert(name);
                let _ = rec.killer.kill();
            }
            state.shutting_down = true;
        }
    }
}

fn handle_process_line(state: &mut DaemonState, service: String, generation: u64, line: String) {
    let current_gen = state
        .children
        .get(&service)
        .map(|r| r.generation)
        .unwrap_or(0);
    if current_gen != generation {
        // Stale line from a killed instance — drop.
        return;
    }
    let ts = now_ms();
    let buf = state
        .logs
        .entry(service.clone())
        .or_insert_with(|| RingBuffer::new(RING_CAPACITY));
    buf.push(ts, line.clone());
    state.broadcast(
        Some(&service),
        ServerMessage::LogChunk {
            service: service.clone(),
            bytes: encode_line(&line),
            ts,
        },
    );
}

fn handle_process_exited(
    state: &mut DaemonState,
    service: String,
    generation: u64,
    exit_code: Option<i32>,
    tx: &mpsc::UnboundedSender<InternalEvent>,
) {
    // Drop the record only if the generation still matches — otherwise this
    // is an exit from an already-replaced instance.
    let drop_it = state
        .children
        .get(&service)
        .map(|r| r.generation == generation)
        .unwrap_or(false);
    if drop_it {
        state.children.remove(&service);
    }
    let intentional = state.intentional_stops.remove(&service);

    if intentional {
        state.broadcast(
            Some(&service),
            ServerMessage::StatusUpdate {
                service: service.clone(),
                state: ServiceState::Stopped,
                pid: None,
                port: None,
                restart_count: 0,
            },
        );
        // We don't fire Event::ServiceExited because the user asked for this.
        // The executor was already told via UserStop (in handle_client_message).
    } else {
        let actions = state.executor.handle(ExecEvent::ServiceExited {
            name: service.clone(),
            exit_code,
        });
        state.broadcast(
            Some(&service),
            ServerMessage::StatusUpdate {
                service: service.clone(),
                state: ServiceState::Failed { exit_code },
                pid: None,
                port: None,
                restart_count: 0,
            },
        );
        enact_actions(state, actions, tx);
    }

    if state.pending_restarts.remove(&service) {
        let actions = state.executor.reset(&service);
        enact_actions(state, actions, tx);
    }
}

fn handle_grace_passed(
    state: &mut DaemonState,
    service: String,
    generation: u64,
    tx: &mpsc::UnboundedSender<InternalEvent>,
) {
    let still_running = state
        .children
        .get(&service)
        .map(|r| r.generation == generation)
        .unwrap_or(false);
    if !still_running {
        return;
    }
    let actions = state.executor.handle(ExecEvent::ServiceHealthy {
        name: service.clone(),
    });
    let rec = state.children.get(&service).expect("checked");
    state.broadcast(
        Some(&service),
        ServerMessage::StatusUpdate {
            service: service.clone(),
            state: ServiceState::Running {
                degraded: false,
                started_without: vec![],
            },
            pid: Some(rec.pid),
            port: rec.port,
            restart_count: rec.restart_count,
        },
    );
    enact_actions(state, actions, tx);
}

fn enact_actions(
    state: &mut DaemonState,
    actions: Vec<Action>,
    tx: &mpsc::UnboundedSender<InternalEvent>,
) {
    let mut work: Vec<Action> = actions;
    while let Some(action) = work.pop() {
        match action {
            Action::StartService(name) => {
                let svc = match state.stack.service.get(&name) {
                    Some(s) => s.clone(),
                    None => continue,
                };
                let generation = state.alloc_generation();
                let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                let env: Vec<(String, String)> = svc
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let env_slice: Vec<(&str, &str)> =
                    env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

                let parts = match ChildProcess::spawn_parts::<&str>(&svc.cmd, &cwd, &env_slice) {
                    Ok(p) => p,
                    Err(e) => {
                        state.broadcast(
                            None,
                            ServerMessage::Notice {
                                level: NoticeLevel::Error,
                                message: format!("spawn {name}: {e}"),
                            },
                        );
                        continue;
                    }
                };

                state.children.insert(
                    name.clone(),
                    ChildRecord {
                        generation,
                        pid: parts.pid,
                        port: None,
                        restart_count: 0,
                        killer: parts.killer,
                    },
                );
                state.broadcast(
                    Some(&name),
                    ServerMessage::StatusUpdate {
                        service: name.clone(),
                        state: ServiceState::Starting,
                        pid: Some(parts.pid),
                        port: None,
                        restart_count: 0,
                    },
                );

                // Per-process tasks: line reader and exit waiter.
                let lines_tx = tx.clone();
                let lines_name = name.clone();
                let mut lines_rx = parts.lines;
                tokio::spawn(async move {
                    while let Some(line) = lines_rx.recv().await {
                        if lines_tx
                            .send(InternalEvent::ProcessLine {
                                service: lines_name.clone(),
                                generation,
                                line,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                });

                let exit_tx = tx.clone();
                let exit_name = name.clone();
                let exit_rx = parts.exit;
                tokio::spawn(async move {
                    let code = exit_rx.await.ok();
                    let _ = exit_tx.send(InternalEvent::ProcessExited {
                        service: exit_name,
                        generation,
                        exit_code: code,
                    });
                });

                // Grace timer for the "no probe → healthy" path.
                let grace_tx = tx.clone();
                let grace_name = name.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(HEALTH_GRACE_MS)).await;
                    let _ = grace_tx.send(InternalEvent::ServiceGracePassed {
                        service: grace_name,
                        generation,
                    });
                });
            }
            Action::StopService(name) => {
                if let Some(mut rec) = state.children.remove(&name) {
                    let _ = rec.killer.kill();
                }
            }
            Action::RunCheck(name) => {
                // V1: no real check spawning yet — treat every check as
                // passing so dependents can advance. Real spawning will
                // mirror the service path and route StepCheckCompleted.
                let follow_up = state.executor.handle(ExecEvent::StepCheckCompleted {
                    name: name.clone(),
                    passed: true,
                });
                state.broadcast(
                    None,
                    ServerMessage::StepStatusUpdate {
                        step: name.clone(),
                        state: StepState::Passed,
                    },
                );
                work.extend(follow_up);
            }
            Action::RunProvision(_) => {
                // V1: provisioning is out of scope for this first wiring pass.
            }
        }
    }
}

fn encode_line(line: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(line.as_bytes())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn framed(stream: UnixStream) -> Framed<UnixStream, FrameCodec> {
    Framed::new(stream, FrameCodec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use devme_core::ClientMessage;
    use tempfile::TempDir;

    async fn connect(sock: &Path) -> Framed<UnixStream, FrameCodec> {
        for _ in 0..50 {
            if let Ok(s) = UnixStream::connect(sock).await {
                return framed(s);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("could not connect to {}", sock.display());
    }

    async fn send_msg(
        conn: &mut Framed<UnixStream, FrameCodec>,
        msg: ClientMessage,
    ) {
        let env = Envelope::new(msg);
        let bytes = serde_json::to_vec(&env).unwrap();
        conn.send(bytes.as_slice()).await.unwrap();
    }

    async fn recv_msg(conn: &mut Framed<UnixStream, FrameCodec>) -> ServerMessage {
        let bytes = conn.next().await.expect("frame").unwrap();
        let env: Envelope<ServerMessage> = serde_json::from_slice(&bytes).unwrap();
        env.payload
    }

    fn make_stack(s: &str) -> Stack {
        Stack::parse(s).unwrap()
    }

    #[tokio::test]
    async fn subscribe_returns_snapshot_of_configured_services() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        let server = DaemonServer::bind_with_stack(
            &sock,
            make_stack(
                r#"
schema_version = 1

[service.db]
cmd = "true"

[service.api]
cmd = "true"
"#,
            ),
        )
        .unwrap();
        let task = tokio::spawn(server.serve());

        let mut conn = connect(&sock).await;
        send_msg(&mut conn, ClientMessage::Subscribe { services: vec![] }).await;
        match recv_msg(&mut conn).await {
            ServerMessage::Subscribed { services, .. } => {
                let names: Vec<_> = services.iter().map(|s| s.name.as_str()).collect();
                assert_eq!(names, vec!["db", "api"]);
                assert!(services.iter().all(|s| matches!(s.state, ServiceState::Stopped)));
            }
            other => panic!("expected Subscribed, got {other:?}"),
        }

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = recv_msg(&mut conn).await; // Goodbye
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }

    #[tokio::test]
    async fn start_spawns_service_and_reports_running() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        let server = DaemonServer::bind_with_stack(
            &sock,
            make_stack(
                r#"
schema_version = 1

[service.tick]
cmd = "sleep 5"
"#,
            ),
        )
        .unwrap();
        let task = tokio::spawn(server.serve());

        let mut conn = connect(&sock).await;
        send_msg(&mut conn, ClientMessage::Subscribe { services: vec![] }).await;
        let _ = recv_msg(&mut conn).await; // Subscribed snapshot

        send_msg(
            &mut conn,
            ClientMessage::Start {
                service: "tick".into(),
                skip_deps: false,
            },
        )
        .await;

        // Expect Starting then Running.
        let mut saw_starting = false;
        let mut saw_running = false;
        for _ in 0..6 {
            match tokio::time::timeout(Duration::from_secs(2), recv_msg(&mut conn))
                .await
                .expect("timed out waiting for status updates")
            {
                ServerMessage::StatusUpdate { service, state, .. } if service == "tick" => {
                    if matches!(state, ServiceState::Starting) {
                        saw_starting = true;
                    }
                    if matches!(state, ServiceState::Running { .. }) {
                        saw_running = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(saw_starting, "expected to see Starting");
        assert!(saw_running, "expected to see Running");

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        // Drain — Goodbye may arrive interleaved with final status updates.
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn process_lines_arrive_as_log_chunks() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        let server = DaemonServer::bind_with_stack(
            &sock,
            make_stack(
                r#"
schema_version = 1

[service.echo]
cmd = "printf 'one\\ntwo\\n'; sleep 5"
"#,
            ),
        )
        .unwrap();
        let task = tokio::spawn(server.serve());

        let mut conn = connect(&sock).await;
        send_msg(&mut conn, ClientMessage::Subscribe { services: vec![] }).await;
        let _ = recv_msg(&mut conn).await;
        send_msg(
            &mut conn,
            ClientMessage::Start {
                service: "echo".into(),
                skip_deps: false,
            },
        )
        .await;

        let mut got = Vec::new();
        for _ in 0..20 {
            let msg = tokio::time::timeout(Duration::from_secs(2), recv_msg(&mut conn))
                .await
                .expect("timed out waiting for log chunks");
            if let ServerMessage::LogChunk {
                service,
                bytes,
                ..
            } = msg
            {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(bytes.as_bytes())
                    .unwrap();
                let line = String::from_utf8(decoded).unwrap();
                let trimmed = line.trim().to_string();
                if service == "echo" {
                    got.push(trimmed);
                }
            }
            if got.contains(&"one".to_string()) && got.contains(&"two".to_string()) {
                break;
            }
        }
        assert!(got.contains(&"one".to_string()), "missing 'one': {got:?}");
        assert!(got.contains(&"two".to_string()), "missing 'two': {got:?}");

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn stop_kills_service_and_reports_stopped() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        let server = DaemonServer::bind_with_stack(
            &sock,
            make_stack(
                r#"
schema_version = 1

[service.tick]
cmd = "sleep 30"
"#,
            ),
        )
        .unwrap();
        let task = tokio::spawn(server.serve());

        let mut conn = connect(&sock).await;
        send_msg(&mut conn, ClientMessage::Subscribe { services: vec![] }).await;
        let _ = recv_msg(&mut conn).await;
        send_msg(
            &mut conn,
            ClientMessage::Start {
                service: "tick".into(),
                skip_deps: false,
            },
        )
        .await;

        // Wait until Running.
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(2), recv_msg(&mut conn))
                .await
                .expect("timed out waiting for Running");
            if matches!(
                msg,
                ServerMessage::StatusUpdate {
                    ref service,
                    state: ServiceState::Running { .. },
                    ..
                } if service == "tick"
            ) {
                break;
            }
        }

        send_msg(
            &mut conn,
            ClientMessage::Stop {
                service: "tick".into(),
            },
        )
        .await;

        let mut saw_stopped = false;
        for _ in 0..6 {
            let msg = tokio::time::timeout(Duration::from_secs(2), recv_msg(&mut conn))
                .await
                .expect("timed out waiting for Stopped");
            if matches!(
                msg,
                ServerMessage::StatusUpdate {
                    ref service,
                    state: ServiceState::Stopped,
                    ..
                } if service == "tick"
            ) {
                saw_stopped = true;
                break;
            }
        }
        assert!(saw_stopped, "expected Stopped after Stop");

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn shutdown_sends_goodbye_and_exits() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        let server = DaemonServer::bind_with_stack(
            &sock,
            make_stack("schema_version = 1\n"),
        )
        .unwrap();
        let task = tokio::spawn(server.serve());

        let mut conn = connect(&sock).await;
        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let msg = recv_msg(&mut conn).await;
        assert!(matches!(msg, ServerMessage::Goodbye { .. }));

        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("serve didn't exit");
        result.unwrap().unwrap();
    }
}
