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
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use devme_config::{Graph, InterpContext, Stack, interpolate};
use devme_core::{
    ClientMessage, Envelope, ErrorCode, HealthCheck, InstanceInfo, NoticeLevel, RestartPolicy,
    ServerMessage, ServiceSnapshot, ServiceState, Slot, StepSnapshot, StepState,
};
use devme_executor::{Action, Event as ExecEvent, Executor, NodeStatus};
use devme_ipc::FrameCodec;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::codec::Framed;

use crate::health::probe;
use crate::process::{ChildProcess, process_is_alive, send_sigkill, send_sigterm};

/// Per-service log ring capacity. ~2000 lines is enough to scroll back a
/// minute or two of moderately chatty output without unbounded memory.
const RING_CAPACITY: usize = 2000;

/// Grace period between spawning a service and treating it as healthy when
/// it has no explicit health probe. Long enough to skip the "Starting"
/// flicker for instant-up commands; short enough that the user still sees
/// the transition.
const HEALTH_GRACE_MS: u64 = 150;

/// Grace between a graceful SIGTERM and the SIGKILL fallback when a stopping
/// service ignores SIGTERM. Under `devme down`'s default 10s client wait so it
/// still observes the resulting Stopped events.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How often the health probe re-runs while a service is in `Starting`.
const PROBE_INTERVAL_MS: u64 = 1000;

/// Base restart backoff. Real delay = `BASE * 2^min(count, BACKOFF_CAP_POW)`,
/// capped at [`RESTART_BACKOFF_MAX_MS`].
const RESTART_BACKOFF_BASE_MS: u64 = 500;
const RESTART_BACKOFF_MAX_MS: u64 = 30_000;
const BACKOFF_CAP_POW: u32 = 6;

/// Crash-loop detection: if a service exits N times in W seconds, stop
/// trying to restart it and mark it `CrashLoop`. Tuned to be permissive
/// enough for normal flaky-during-startup services but tight enough that a
/// truly broken cmd doesn't burn CPU forever.
const CRASH_LOOP_THRESHOLD: usize = 5;
const CRASH_LOOP_WINDOW_SECS: u64 = 60;

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
    /// An `external` service's health probe passed. Carries no generation —
    /// devme doesn't own the process, so there's no child to match against.
    ExternalHealthy {
        service: String,
    },
    /// Restart-policy backoff timer expired — try to bring the service back.
    AutoRestart {
        service: String,
    },
    /// A step's `check` command finished. `passed=true` for exit code 0.
    StepCheckResult {
        step: String,
        passed: bool,
    },
    /// A step's `provision` command finished.
    StepProvisionResult {
        step: String,
        passed: bool,
    },
    /// A line of output from a step's check or provision subprocess.
    /// Streamed as it arrives so the user can watch slow setup commands.
    StepLine {
        step: String,
        line: String,
    },
}

struct ChildRecord {
    generation: u64,
    pid: u32,
    port: Option<u16>,
    restart_count: u32,
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
    /// Identity the daemon advertises to every Subscriber — instance id,
    /// label, cwd. Lets multi-instance TUIs tab between worktrees.
    instance: InstanceInfo,
    /// Slot assigned by the cross-worktree allocator. Drives the `{slot}`
    /// and `{port}` interpolation for every service.
    slot: Slot,
    /// Absolute worktree path, exposed as `{worktree}` in interpolation.
    /// Derived from the daemon's [`InstanceInfo::cwd`].
    worktree: String,
    /// Current git branch of the worktree, exposed as `{branch}`. Empty
    /// when the worktree isn't a git checkout (the var is still present so
    /// configs referencing `{branch}` validate; a typo like `{branchh}`
    /// still errors).
    branch: String,
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
    /// Cumulative restart counter per service. Incremented when an exit
    /// causes an auto-restart; reset when the user explicitly Starts/Stops.
    /// Powers the backoff curve and the `restart_count` in StatusUpdates.
    restart_counts: HashMap<String, u32>,
    /// Rolling window of recent exit timestamps per service. Used to detect
    /// "crashing every few seconds" patterns. Trimmed on every exit so the
    /// memory stays bounded.
    recent_exits: HashMap<String, VecDeque<Instant>>,
    /// Services whose auto-restart loop has been disabled because they're
    /// crash-looping. Reset on user Start/Stop/Restart.
    crash_looped: HashSet<String>,
    next_generation: u64,
    next_client_id: u64,
    shutting_down: bool,
}

impl DaemonState {
    fn new(stack: Arc<Stack>, slot: Slot, instance: InstanceInfo) -> Self {
        let graph = Graph::from_stack(&stack);
        let worktree = instance.cwd.clone();
        let branch = current_git_branch(Path::new(&worktree)).unwrap_or_default();
        Self {
            stack,
            executor: Executor::new(graph),
            instance,
            slot,
            worktree,
            branch,
            children: HashMap::new(),
            logs: HashMap::new(),
            clients: HashMap::new(),
            subscriptions: HashMap::new(),
            intentional_stops: HashSet::new(),
            pending_restarts: HashSet::new(),
            restart_counts: HashMap::new(),
            recent_exits: HashMap::new(),
            crash_looped: HashSet::new(),
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
                // restart_count persists across crash/restart cycles, so a
                // momentarily-Failed service still shows its accumulated
                // count — readers can tell apart a fresh failure from a
                // service that's been bouncing for a while.
                let count = *self.restart_counts.get(name).unwrap_or(&0);
                ServiceSnapshot {
                    name: name.clone(),
                    state: self.current_service_state(name),
                    pid: rec.map(|r| r.pid),
                    port: rec.and_then(|r| r.port),
                    restart_count: count,
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

    /// Resolved port for every service that declares a port spec, keyed by
    /// service name. Computed from the stack + slot, so it's stable for the
    /// daemon's lifetime and identical for every node that reads it.
    fn service_ports(&self) -> HashMap<&str, u16> {
        self.stack
            .service
            .iter()
            .filter_map(|(name, svc)| {
                svc.port
                    .map(|spec| (name.as_str(), spec.resolve(self.slot.as_u8())))
            })
            .collect()
    }

    /// The interpolation context shared by every node: `{slot}`,
    /// `{worktree}`, `{branch}`, and `{port.<service>}` for each sibling
    /// service that declares a port. A node's own `{port}` is layered on
    /// top by [`node_ctx`](Self::node_ctx).
    fn base_ctx(&self) -> InterpContext {
        let mut ctx = InterpContext::new()
            .set("slot", self.slot.to_string())
            .set("worktree", self.worktree.clone())
            .set("branch", self.branch.clone());
        for (name, port) in self.service_ports() {
            ctx.insert(format!("port.{name}"), port.to_string());
        }
        ctx
    }

    /// Context for a specific node: the shared [`base_ctx`](Self::base_ctx)
    /// plus the node's own `{port}` when it has one. Steps pass `None`;
    /// they still see every sibling's `{port.<service>}`.
    fn node_ctx(&self, own_port: Option<u16>) -> InterpContext {
        let mut ctx = self.base_ctx();
        if let Some(p) = own_port {
            ctx.insert("port", p.to_string());
        }
        ctx
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
    slot: Slot,
    instance: InstanceInfo,
}

impl DaemonServer {
    /// Bind with an empty stack and slot 0. Useful for handshake-shape tests
    /// that don't need real port allocation.
    pub fn bind(path: &Path) -> std::io::Result<Self> {
        Self::bind_with_stack(path, Stack::parse("schema_version = 1\n").unwrap())
    }

    /// Bind a Unix socket and prepare to serve `stack`. Uses slot 0 — for
    /// real multi-instance behaviour the supervisor's `main` chooses a slot
    /// via the cross-worktree allocator.
    pub fn bind_with_stack(path: &Path, stack: Stack) -> std::io::Result<Self> {
        Self::bind_with_stack_and_slot(path, stack, Slot::ZERO)
    }

    /// Bind a Unix socket and prepare to serve `stack` at the given `slot`.
    /// `slot` drives `{port}` and `{slot}` interpolation across services.
    pub fn bind_with_stack_and_slot(
        path: &Path,
        stack: Stack,
        slot: Slot,
    ) -> std::io::Result<Self> {
        // Default identity derived from the socket path's basename. Real
        // daemons should call [`bind_with_instance`] so the InstanceInfo
        // matches the worktree, not the socket file.
        let instance = InstanceInfo {
            id: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("anon")
                .to_string(),
            label: "(unknown)".to_string(),
            cwd: ".".to_string(),
        };
        Self::bind_with_instance(path, stack, slot, instance)
    }

    /// Bind a Unix socket with full control over identity. The supervisor
    /// binary calls this with an InstanceInfo derived from its own cwd so
    /// connected clients can label the stack in their UI.
    pub fn bind_with_instance(
        path: &Path,
        stack: Stack,
        slot: Slot,
        instance: InstanceInfo,
    ) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            stack: Arc::new(stack),
            slot,
            instance,
        })
    }

    /// Long-running serve loop. Returns when a client requests Shutdown or
    /// the listener errors.
    pub async fn serve(self) -> std::io::Result<()> {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel::<InternalEvent>();
        let accept_tx = internal_tx.clone();
        let DaemonServer { listener, path, stack, slot, instance } = self;

        tokio::spawn(accept_loop(listener, accept_tx));

        let result = run_event_loop(internal_tx, internal_rx, stack, slot, instance).await;
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
    slot: Slot,
    instance: InstanceInfo,
) -> std::io::Result<()> {
    let mut state = DaemonState::new(stack, slot, instance);

    while let Some(event) = internal_rx.recv().await {
        process_event(&mut state, event, &internal_tx);
        if state.shutting_down && state.children.is_empty() {
            // All children have drained — say goodbye now (not at Shutdown
            // receipt) so clients render real Stopped events first.
            state.broadcast(
                None,
                ServerMessage::Goodbye {
                    reason: "shutdown complete".into(),
                },
            );
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
        InternalEvent::ExternalHealthy { service } => handle_external_healthy(state, service, tx),
        InternalEvent::AutoRestart { service } => handle_auto_restart(state, service, tx),
        InternalEvent::StepCheckResult { step, passed } => {
            let actions = state.executor.handle(ExecEvent::StepCheckCompleted {
                name: step.clone(),
                passed,
            });
            let new_state = match state.executor.state(&step) {
                Some(NodeStatus::Step(s)) => *s,
                _ if passed => StepState::Passed,
                _ => StepState::Failed,
            };
            state.broadcast(
                None,
                ServerMessage::StepStatusUpdate {
                    step: step.clone(),
                    state: new_state,
                },
            );
            enact_actions(state, actions, tx);
        }
        InternalEvent::StepLine { step, line } => {
            let ts = now_ms();
            let bytes = encode_line(&line);
            let buf = state
                .logs
                .entry(step.clone())
                .or_insert_with(|| RingBuffer::new(RING_CAPACITY));
            buf.push(ts, line);
            let msg = ServerMessage::LogChunk {
                service: step.clone(),
                bytes,
                ts,
            };
            state.broadcast(Some(&step), msg);
        }
        InternalEvent::StepProvisionResult { step, passed } => {
            let actions = state.executor.handle(ExecEvent::StepProvisionCompleted {
                name: step.clone(),
                passed,
            });
            let new_state = match state.executor.state(&step) {
                Some(NodeStatus::Step(s)) => *s,
                _ if passed => StepState::Unknown,
                _ => StepState::ProvisionFailed,
            };
            state.broadcast(
                None,
                ServerMessage::StepStatusUpdate {
                    step,
                    state: new_state,
                },
            );
            enact_actions(state, actions, tx);
        }
    }
}

fn handle_auto_restart(
    state: &mut DaemonState,
    service: String,
    tx: &mpsc::UnboundedSender<InternalEvent>,
) {
    // Bail if a child is already running, the user stopped it in the
    // meantime, or shutdown is in progress.
    if state.shutting_down
        || state.children.contains_key(&service)
        || state.intentional_stops.contains(&service)
    {
        return;
    }
    let actions = state.executor.reset(&service);
    enact_actions(state, actions, tx);
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
                    instance: state.instance.clone(),
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
        ClientMessage::Start { service, .. } => {
            // For v1, any Start triggers a global Event::Start. The service
            // argument is informational; the executor advances the whole
            // graph in topo order.
            // If the user is explicitly starting a service, clear its restart
            // counter so the backoff resets after intentional interaction.
            if !service.is_empty() {
                state.restart_counts.remove(&service);
                state.recent_exits.remove(&service);
                state.crash_looped.remove(&service);
                // If the service is currently parked in CrashLoop, reset its
                // executor entry so advance() picks it up again.
                if matches!(
                    state.executor.state(&service),
                    Some(NodeStatus::Service(ServiceState::CrashLoop { .. }))
                        | Some(NodeStatus::Service(ServiceState::Failed { .. }))
                ) {
                    let actions = state.executor.reset(&service);
                    enact_actions(state, actions, tx);
                    return;
                }
            }
            let actions = state.executor.handle(ExecEvent::Start);
            enact_actions(state, actions, tx);
        }
        ClientMessage::Stop { service } => {
            state.restart_counts.remove(&service);
            state.recent_exits.remove(&service);
            state.crash_looped.remove(&service);
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
            state.recent_exits.remove(&service);
            state.crash_looped.remove(&service);
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
            // Gracefully stop every child (run `stop` hooks, then SIGTERM →
            // SIGKILL). Children stay in the map and drain via ProcessExited;
            // the loop broadcasts Goodbye and exits once the map empties, so
            // clients see real per-service Stopped events with real timing.
            let names: Vec<String> = state.children.keys().cloned().collect();
            for name in &names {
                state.intentional_stops.insert(name.clone());
            }
            for name in &names {
                begin_stop(state, name, tx);
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
        state.restart_counts.remove(&service);
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
    } else {
        let actions = state.executor.handle(ExecEvent::ServiceExited {
            name: service.clone(),
            exit_code,
        });
        let restart_count = *state.restart_counts.get(&service).unwrap_or(&0);
        state.broadcast(
            Some(&service),
            ServerMessage::StatusUpdate {
                service: service.clone(),
                state: ServiceState::Failed { exit_code },
                pid: None,
                port: None,
                restart_count,
            },
        );
        enact_actions(state, actions, tx);

        // Decide whether to auto-restart per the service's policy. User-
        // initiated restarts (pending_restarts) take precedence below and
        // skip this branch.
        if !state.pending_restarts.contains(&service)
            && should_auto_restart(&state.stack, &service, exit_code)
        {
            // Record this exit in the crash-loop window and trim old entries.
            let now = Instant::now();
            let exits = state
                .recent_exits
                .entry(service.clone())
                .or_insert_with(|| VecDeque::with_capacity(CRASH_LOOP_THRESHOLD + 1));
            let window = Duration::from_secs(CRASH_LOOP_WINDOW_SECS);
            while exits.front().is_some_and(|&t| now.duration_since(t) > window) {
                exits.pop_front();
            }
            exits.push_back(now);

            if exits.len() >= CRASH_LOOP_THRESHOLD {
                // Trip the breaker: stop trying to restart this service and
                // surface a Notice so clients can flag it loudly. The
                // service stays in CrashLoop until the user explicitly
                // intervenes via Start / Restart / Stop.
                state.crash_looped.insert(service.clone());
                let count = *state.restart_counts.get(&service).unwrap_or(&0);
                state.broadcast(
                    Some(&service),
                    ServerMessage::StatusUpdate {
                        service: service.clone(),
                        state: ServiceState::CrashLoop {
                            restart_count: count,
                        },
                        pid: None,
                        port: None,
                        restart_count: count,
                    },
                );
                state.broadcast(
                    None,
                    ServerMessage::Notice {
                        level: NoticeLevel::Error,
                        message: format!(
                            "service {service} crash-looped \
                             ({CRASH_LOOP_THRESHOLD} exits in \
                             {CRASH_LOOP_WINDOW_SECS}s) — auto-restart disabled"
                        ),
                    },
                );
            } else {
                let prev = state.restart_counts.entry(service.clone()).or_insert(0);
                *prev = prev.saturating_add(1);
                let delay_ms = backoff_for(*prev);
                let svc_name = service.clone();
                let restart_tx = tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    let _ = restart_tx.send(InternalEvent::AutoRestart { service: svc_name });
                });
            }
        }
    }

    if state.pending_restarts.remove(&service) {
        let actions = state.executor.reset(&service);
        enact_actions(state, actions, tx);
    }
}

fn should_auto_restart(stack: &Stack, name: &str, exit_code: Option<i32>) -> bool {
    let Some(svc) = stack.service.get(name) else {
        return false;
    };
    match svc.restart {
        RestartPolicy::Never => false,
        RestartPolicy::Always => true,
        RestartPolicy::OnFailure => !matches!(exit_code, Some(0)),
    }
}

fn backoff_for(count: u32) -> u64 {
    let pow = count.saturating_sub(1).min(BACKOFF_CAP_POW);
    let raw = RESTART_BACKOFF_BASE_MS.saturating_mul(1u64 << pow);
    raw.min(RESTART_BACKOFF_MAX_MS)
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

/// An external service's probe passed. There's no child process to track —
/// devme only health-checks it — so we transition the executor straight to
/// `External { healthy: true }` and let dependents advance.
fn handle_external_healthy(
    state: &mut DaemonState,
    service: String,
    tx: &mpsc::UnboundedSender<InternalEvent>,
) {
    // Already healthy? Nothing to do (the probe loop only fires once, but a
    // re-subscribe or duplicate event must stay idempotent).
    if matches!(
        state.executor.state(&service),
        Some(NodeStatus::Service(ServiceState::External { healthy: true }))
    ) {
        return;
    }
    let actions = state.executor.handle(ExecEvent::ExternalHealthy {
        name: service.clone(),
    });
    let port = state
        .stack
        .service
        .get(&service)
        .and_then(|s| s.port)
        .map(|spec| spec.resolve(state.slot.as_u8()));
    state.broadcast(
        Some(&service),
        ServerMessage::StatusUpdate {
            service: service.clone(),
            state: ServiceState::External { healthy: true },
            pid: None,
            port,
            restart_count: 0,
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
                // Honor the service's `cwd` (joined to the worktree root). A
                // relative value like `[service.ai] cwd = "ai-service"` would
                // otherwise be ignored and the service would spawn from the
                // repo root, failing to find its entrypoint. An absolute cwd
                // replaces the base per `Path::join` semantics.
                let base_cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                let cwd = match &svc.cwd {
                    Some(rel) => base_cwd.join(rel),
                    None => base_cwd,
                };

                // Resolve the service's port (if it declared one), then build
                // an interpolation context with {slot}, {port}, {worktree},
                // {branch}, and {port.<sibling>} so cmd / env / health all see
                // the same numbers — including every other service's port.
                let port = svc.port.map(|spec| spec.resolve(state.slot.as_u8()));
                let ctx = state.node_ctx(port);

                let cmd = match interpolate(&svc.cmd, &ctx) {
                    Ok(s) => s,
                    Err(e) => {
                        state.broadcast(
                            None,
                            ServerMessage::Notice {
                                level: NoticeLevel::Error,
                                message: format!("service {name}: cmd interpolation: {e}"),
                            },
                        );
                        continue;
                    }
                };
                // Resolve env values the same way. A bad `{var}` here (e.g. a
                // typo'd `{port.backed}`) fails the spawn loudly rather than
                // leaking a literal `{...}` into the child's environment.
                let mut env: Vec<(String, String)> = Vec::with_capacity(svc.env.len());
                let mut env_error: Option<String> = None;
                for (k, v) in &svc.env {
                    match interpolate(v, &ctx) {
                        Ok(resolved) => env.push((k.clone(), resolved)),
                        Err(e) => {
                            env_error = Some(format!("service {name}: env {k}: {e}"));
                            break;
                        }
                    }
                }
                if let Some(message) = env_error {
                    state.broadcast(
                        None,
                        ServerMessage::Notice {
                            level: NoticeLevel::Error,
                            message,
                        },
                    );
                    continue;
                }
                let env_slice: Vec<(&str, &str)> =
                    env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

                let parts = match ChildProcess::spawn_parts::<&str>(&cmd, &cwd, &env_slice) {
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

                let restart_count = *state.restart_counts.get(&name).unwrap_or(&0);
                state.children.insert(
                    name.clone(),
                    ChildRecord {
                        generation,
                        pid: parts.pid,
                        port,
                        restart_count,
                    },
                );
                state.broadcast(
                    Some(&name),
                    ServerMessage::StatusUpdate {
                        service: name.clone(),
                        state: ServiceState::Starting,
                        pid: Some(parts.pid),
                        port,
                        restart_count,
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

                // Readiness path: if the service declared a health probe,
                // poll it until it passes. Otherwise fall back to a short
                // grace timer.
                if let Some(check) = svc.health.clone() {
                    let check = interpolate_health(&check, &ctx);
                    let probe_tx = tx.clone();
                    let probe_name = name.clone();
                    tokio::spawn(async move {
                        let mut ticker =
                            tokio::time::interval(Duration::from_millis(PROBE_INTERVAL_MS));
                        ticker.set_missed_tick_behavior(
                            tokio::time::MissedTickBehavior::Delay,
                        );
                        // Skip the immediate first tick — give the service a
                        // beat to actually start listening.
                        ticker.tick().await;
                        loop {
                            ticker.tick().await;
                            if probe(&check).await {
                                let _ = probe_tx.send(InternalEvent::ServiceGracePassed {
                                    service: probe_name,
                                    generation,
                                });
                                return;
                            }
                        }
                    });
                } else {
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
            }
            Action::ProbeExternal(name) => {
                // External service: never spawn — the process is owned by
                // someone else (the shared supervisor, or a developer's own
                // daemon). Poll its health probe until it passes, then report
                // ExternalHealthy. No ChildRecord, no exit waiter, so `devme
                // down` never tries to signal a PID we don't own.
                let svc = match state.stack.service.get(&name) {
                    Some(s) => s.clone(),
                    None => continue,
                };
                let port = svc.port.map(|spec| spec.resolve(state.slot.as_u8()));
                let ctx = state.node_ctx(port);

                state.broadcast(
                    Some(&name),
                    ServerMessage::StatusUpdate {
                        service: name.clone(),
                        state: ServiceState::External { healthy: false },
                        pid: None,
                        port,
                        restart_count: 0,
                    },
                );

                // No health probe declared → treat as healthy immediately
                // (nothing to wait on). In practice the instance supervisor
                // attaches a default TCP probe to repo-scoped services, so
                // this branch is the degenerate "external with no health" case.
                let Some(check) = svc.health.clone() else {
                    let _ = tx.send(InternalEvent::ExternalHealthy { service: name });
                    continue;
                };
                let check = interpolate_health(&check, &ctx);
                let probe_tx = tx.clone();
                let probe_name = name.clone();
                tokio::spawn(async move {
                    let mut ticker =
                        tokio::time::interval(Duration::from_millis(PROBE_INTERVAL_MS));
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        ticker.tick().await;
                        if probe(&check).await {
                            let _ = probe_tx
                                .send(InternalEvent::ExternalHealthy { service: probe_name });
                            return;
                        }
                    }
                });
            }
            Action::StopService(name) => {
                begin_stop(state, &name, tx);
            }
            Action::RunCheck(name) => {
                let cmd = state
                    .stack
                    .step
                    .get(&name)
                    .map(|s| s.check.clone());
                let Some(cmd) = cmd else { continue };
                let ctx = state.node_ctx(None);
                let cmd = interpolate(&cmd, &ctx).unwrap_or(cmd);
                let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                let check_tx = tx.clone();
                let step_name = name.clone();
                tokio::spawn(async move {
                    let passed =
                        run_shell_streaming(&cmd, &cwd, &step_name, &check_tx).await;
                    let _ = check_tx.send(InternalEvent::StepCheckResult {
                        step: step_name,
                        passed,
                    });
                });
            }
            Action::RunProvision(name) => {
                let provision = state
                    .stack
                    .step
                    .get(&name)
                    .and_then(|s| s.provision.clone());
                let Some(provision) = provision else { continue };
                let ctx = state.node_ctx(None);
                let cmd = match provision {
                    devme_config::Provision::Shell(c) => interpolate(&c, &ctx).unwrap_or(c),
                    devme_config::Provision::Wizard { wizard } => {
                        state.broadcast(
                            None,
                            ServerMessage::Notice {
                                level: NoticeLevel::Warn,
                                message: format!(
                                    "step {name}: wizard provision ({wizard}) not yet \
                                     supported — treating as failed"
                                ),
                            },
                        );
                        let _ = tx.send(InternalEvent::StepProvisionResult {
                            step: name,
                            passed: false,
                        });
                        continue;
                    }
                };
                // `trust = "manual"` (ADR-0002): never auto-run inside the
                // daemon — surface the command and report the step unmet so
                // the user runs it themselves. `auto`/`prompt` both run here:
                // the detached daemon has no terminal to prompt on, so the
                // consent gate for these lives in the foreground preflight.
                let trust = state
                    .stack
                    .step
                    .get(&name)
                    .map(|s| s.trust)
                    .unwrap_or_default();
                if trust == devme_core::Trust::Manual {
                    state.broadcast(
                        None,
                        ServerMessage::Notice {
                            level: NoticeLevel::Warn,
                            message: format!(
                                "step {name}: manual provision — run it yourself: {cmd}"
                            ),
                        },
                    );
                    let _ = tx.send(InternalEvent::StepProvisionResult {
                        step: name,
                        passed: false,
                    });
                    continue;
                }
                let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                let provision_tx = tx.clone();
                let step_name = name.clone();
                tokio::spawn(async move {
                    let passed =
                        run_shell_streaming(&cmd, &cwd, &step_name, &provision_tx).await;
                    let _ = provision_tx.send(InternalEvent::StepProvisionResult {
                        step: step_name,
                        passed,
                    });
                });
            }
        }
    }
}

fn encode_line(line: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(line.as_bytes())
}

/// Current git branch for `cwd`, used to populate `{branch}`. Returns
/// `None` outside a git checkout or in detached-HEAD state, in which case
/// `{branch}` resolves to an empty string.
fn current_git_branch(cwd: &Path) -> Option<String> {
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

/// Interpolate `{port}` / `{slot}` inside a health-check target. Falls back
/// to the original string if interpolation errors out (rather than failing
/// the spawn) — the probe will just be slightly wrong and report unhealthy.
fn interpolate_health(check: &HealthCheck, ctx: &InterpContext) -> HealthCheck {
    let resolve = |s: &str| interpolate(s, ctx).unwrap_or_else(|_| s.to_string());
    match check {
        HealthCheck::Tcp { tcp } => HealthCheck::Tcp { tcp: resolve(tcp) },
        HealthCheck::Http { http } => HealthCheck::Http { http: resolve(http) },
        HealthCheck::Shell { shell } => HealthCheck::Shell {
            shell: resolve(shell),
        },
    }
}

/// Begin stopping a running service: run its optional `stop` teardown command
/// (e.g. `docker compose down`, which removes a dockerd-owned container that
/// SIGKILL of the `up` client would leave running), then signal the process —
/// SIGTERM, and SIGKILL after [`STOP_GRACE`] if it ignores it. The child is
/// left in `state.children` and reaped by the normal `ProcessExited` path
/// (which reports Stopped and, for a restart, relaunches), so a graceful
/// shutdown keeps the daemon alive until the process actually exits.
fn begin_stop(state: &DaemonState, name: &str, tx: &mpsc::UnboundedSender<InternalEvent>) {
    let Some(rec) = state.children.get(name) else {
        return;
    };
    let pid = rec.pid;
    let port = rec.port;
    let svc = state.stack.service.get(name);
    let stop_cmd = svc.and_then(|s| s.stop.clone());
    // Honor the service's cwd for the stop command, same as spawn.
    let base_cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let cwd = match svc.and_then(|s| s.cwd.clone()) {
        Some(rel) => base_cwd.join(rel),
        None => base_cwd,
    };

    if let Some(stop_cmd) = stop_cmd {
        let ctx = state.node_ctx(port);
        let cmd = interpolate(&stop_cmd, &ctx).unwrap_or(stop_cmd);
        let label = name.to_string();
        let txc = tx.clone();
        tokio::spawn(async move {
            run_shell_streaming(&cmd, &cwd, &label, &txc).await;
            graceful_signal(pid).await;
        });
    } else {
        tokio::spawn(async move {
            graceful_signal(pid).await;
        });
    }
}

/// SIGTERM, then SIGKILL after [`STOP_GRACE`] if the process is still alive.
/// Polls so a clean exit returns near-instantly. No-op once the pid is gone.
async fn graceful_signal(pid: u32) {
    if !process_is_alive(pid) {
        return;
    }
    send_sigterm(pid);
    let deadline = tokio::time::Instant::now() + STOP_GRACE;
    while tokio::time::Instant::now() < deadline {
        if !process_is_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    send_sigkill(pid);
}

/// Run `cmd` under `sh -c`, streaming both stdout and stderr back to the
/// event loop as `StepLine`s tagged with `step_name`. Returns true on
/// exit code 0.
async fn run_shell_streaming(
    cmd: &str,
    cwd: &Path,
    step_name: &str,
    tx: &mpsc::UnboundedSender<InternalEvent>,
) -> bool {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut child = match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    async fn pump<R>(reader: R, tx: mpsc::UnboundedSender<InternalEvent>, name: String)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx
                .send(InternalEvent::StepLine {
                    step: name.clone(),
                    line,
                })
                .is_err()
            {
                return;
            }
        }
    }
    if let Some(out) = child.stdout.take() {
        tokio::spawn(pump(out, tx.clone(), step_name.to_string()));
    }
    if let Some(err) = child.stderr.take() {
        tokio::spawn(pump(err, tx.clone(), step_name.to_string()));
    }

    match child.wait().await {
        Ok(status) => status.success(),
        Err(_) => false,
    }
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
    async fn service_port_is_interpolated_into_cmd_and_reported() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        let marker = dir.path().join("port-seen");
        let marker_path = marker.to_string_lossy().to_string();
        // Service echoes its port into a file, then sleeps; we read the
        // file back to verify {port} was substituted before spawn.
        let cmd = format!("echo {{port}} > {marker_path}; sleep 30");
        let toml = format!(
            r#"
schema_version = 1

[service.app]
cmd = "{cmd}"
port = {{ base = 9000, slot_offset = 10 }}
"#,
        );

        let stack = make_stack(&toml);
        // Slot 3 → 9000 + 3*10 = 9030.
        let server = DaemonServer::bind_with_stack_and_slot(
            &sock,
            stack,
            devme_core::Slot::new(3).unwrap(),
        )
        .unwrap();
        let task = tokio::spawn(server.serve());

        let mut conn = connect(&sock).await;
        send_msg(&mut conn, ClientMessage::Subscribe { services: vec![] }).await;
        let _ = recv_msg(&mut conn).await;
        send_msg(
            &mut conn,
            ClientMessage::Start {
                service: String::new(),
                skip_deps: false,
            },
        )
        .await;

        // Wait for the marker file to exist and contain the resolved port.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got: Option<String> = None;
        while std::time::Instant::now() < deadline && got.is_none() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(s) = std::fs::read_to_string(&marker) {
                got = Some(s.trim().to_string());
            }
        }
        assert_eq!(got.as_deref(), Some("9030"), "expected port 9030 in marker");

        // The Running StatusUpdate should also carry the resolved port.
        let mut saw_port_in_status = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline && !saw_port_in_status {
            match tokio::time::timeout(Duration::from_millis(500), recv_msg(&mut conn)).await {
                Ok(ServerMessage::StatusUpdate {
                    service,
                    port: Some(p),
                    state: ServiceState::Running { .. },
                    ..
                }) if service == "app" && p == 9030 => {
                    saw_port_in_status = true;
                }
                _ => {}
            }
        }
        assert!(saw_port_in_status, "Running status didn't carry port 9030");

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn service_cwd_is_honored_when_spawning() {
        // A service with `cwd` set must spawn in that directory. We point cwd
        // at an absolute temp subdir and have the service write a *relative*
        // file; it must land in the subdir (proving cwd was applied), not in
        // the daemon's own working directory.
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let sock = dir.path().join("d.sock");
        let cwd_str = sub.to_string_lossy().to_string();
        // Relative marker — only resolves correctly if the child's cwd == sub.
        let cmd = "echo here > cwd-seen; sleep 30";
        let toml = format!(
            r#"
schema_version = 1

[service.app]
cmd = "{cmd}"
cwd = "{cwd_str}"
"#,
        );

        let stack = make_stack(&toml);
        let server = DaemonServer::bind_with_stack(&sock, stack).unwrap();
        let task = tokio::spawn(server.serve());

        let mut conn = connect(&sock).await;
        send_msg(&mut conn, ClientMessage::Subscribe { services: vec![] }).await;
        let _ = recv_msg(&mut conn).await;
        send_msg(
            &mut conn,
            ClientMessage::Start {
                service: String::new(),
                skip_deps: false,
            },
        )
        .await;

        let expected = sub.join("cwd-seen");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while std::time::Instant::now() < deadline && !found {
            tokio::time::sleep(Duration::from_millis(100)).await;
            found = expected.exists();
        }
        assert!(found, "service did not spawn in its configured cwd");

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn sibling_port_worktree_and_branch_resolve_in_service() {
        // A frontend service references the backend's resolved port via
        // {port.backend}, plus {worktree} and {branch}. All three must
        // resolve before spawn — proving the shared per-service port map
        // and the {worktree}/{branch} context are wired through.
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        let marker = dir.path().join("ctx-seen");
        let marker_path = marker.to_string_lossy().to_string();
        // {worktree} resolves to "." here (bind_with_stack_and_slot sets
        // InstanceInfo::cwd = "."). {branch} resolves to whatever git
        // reports for the test's cwd (possibly empty); we only assert it
        // didn't error out the spawn, so we don't pin its value.
        // Single-quote the echo payload so the shell treats `|` literally
        // (devme has already substituted the {vars} by spawn time, so the
        // quotes don't suppress anything we need).
        let cmd =
            format!("echo '{{port.backend}}|{{worktree}}' > {marker_path}; echo {{branch}}; sleep 30");
        let toml = format!(
            r#"
schema_version = 1

[service.backend]
cmd = "sleep 30"
port = {{ base = 8080, slot_offset = 10 }}

[service.frontend]
cmd = "{cmd}"
port = {{ base = 5173, slot_offset = 10 }}
env = {{ VITE_API_BASE_URL = "http://localhost:{{port.backend}}" }}
"#,
        );

        let stack = make_stack(&toml);
        // Slot 2 → backend = 8080 + 2*10 = 8100.
        let server = DaemonServer::bind_with_stack_and_slot(
            &sock,
            stack,
            devme_core::Slot::new(2).unwrap(),
        )
        .unwrap();
        let task = tokio::spawn(server.serve());

        let mut conn = connect(&sock).await;
        send_msg(&mut conn, ClientMessage::Subscribe { services: vec![] }).await;
        let _ = recv_msg(&mut conn).await;
        send_msg(
            &mut conn,
            ClientMessage::Start {
                service: "frontend".into(),
                skip_deps: false,
            },
        )
        .await;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got: Option<String> = None;
        while std::time::Instant::now() < deadline && got.is_none() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(s) = std::fs::read_to_string(&marker) {
                got = Some(s.trim().to_string());
            }
        }
        // Backend's resolved port is visible to the frontend, and {worktree}
        // resolved to the daemon's cwd (".").
        assert_eq!(
            got.as_deref(),
            Some("8100|."),
            "expected sibling backend port 8100 and worktree '.' in marker"
        );

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn unknown_sibling_port_var_fails_spawn_with_notice() {
        // A typo'd {port.backed} (missing 'n') must surface as an error
        // Notice, not leak a literal "{port.backed}" into the child.
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        let toml = r#"
schema_version = 1

[service.backend]
cmd = "sleep 30"
port = { base = 8080, slot_offset = 10 }

[service.frontend]
cmd = "echo {port.backed}; sleep 30"
"#;
        let server =
            DaemonServer::bind_with_stack(&sock, make_stack(toml)).unwrap();
        let task = tokio::spawn(server.serve());

        let mut conn = connect(&sock).await;
        send_msg(&mut conn, ClientMessage::Subscribe { services: vec![] }).await;
        let _ = recv_msg(&mut conn).await;
        send_msg(
            &mut conn,
            ClientMessage::Start {
                service: "frontend".into(),
                skip_deps: false,
            },
        )
        .await;

        let mut saw_error = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline && !saw_error {
            match tokio::time::timeout(Duration::from_millis(500), recv_msg(&mut conn)).await {
                Ok(ServerMessage::Notice {
                    level: NoticeLevel::Error,
                    message,
                }) if message.contains("port.backed") => {
                    saw_error = true;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(saw_error, "expected an error Notice naming the unknown var");

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn step_with_passing_check_unblocks_dependent_service() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        let server = DaemonServer::bind_with_stack(
            &sock,
            make_stack(
                r#"
schema_version = 1

[step.tools]
check = "true"

[service.app]
cmd = "sleep 30"
depends_on = ["tools"]
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
                service: String::new(),
                skip_deps: false,
            },
        )
        .await;

        // Expect a StepStatusUpdate (tools → Passed), then app reaches Running.
        let mut saw_step_passed = false;
        let mut saw_app_running = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && (!saw_step_passed || !saw_app_running) {
            match tokio::time::timeout(Duration::from_millis(500), recv_msg(&mut conn)).await {
                Ok(ServerMessage::StepStatusUpdate {
                    step,
                    state: StepState::Passed,
                }) if step == "tools" => {
                    saw_step_passed = true;
                }
                Ok(ServerMessage::StatusUpdate {
                    service,
                    state: ServiceState::Running { .. },
                    ..
                }) if service == "app" => {
                    saw_app_running = true;
                }
                _ => {}
            }
        }
        assert!(saw_step_passed, "step never reported Passed");
        assert!(saw_app_running, "service blocked on step never reached Running");

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn step_output_is_streamed_to_subscribers_as_log_chunks() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        let server = DaemonServer::bind_with_stack(
            &sock,
            make_stack(
                r#"
schema_version = 1

[step.greeting]
check = "echo hello from check; echo line two"
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
                service: String::new(),
                skip_deps: false,
            },
        )
        .await;

        let mut got: Vec<String> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline
            && !got.iter().any(|l| l.contains("hello from check"))
        {
            match tokio::time::timeout(Duration::from_millis(500), recv_msg(&mut conn)).await {
                Ok(ServerMessage::LogChunk {
                    service, bytes, ..
                }) if service == "greeting" => {
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(bytes.as_bytes())
                        .unwrap();
                    got.push(String::from_utf8(decoded).unwrap());
                }
                _ => {}
            }
        }
        assert!(
            got.iter().any(|l| l.contains("hello from check")),
            "step output missing 'hello from check', got: {got:?}"
        );

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn step_with_failing_check_and_passing_provision_reruns_check() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        // Marker-file pattern: check passes only after provision creates the marker.
        let marker = dir.path().join("marker");
        let marker_path = marker.to_string_lossy().to_string();
        let check = format!("test -f {marker_path}");
        let provision = format!("touch {marker_path}");
        let toml = format!(
            r#"
schema_version = 1

[step.bootstrap]
check = "{check}"
provision = "{provision}"
"#,
        );
        let server = DaemonServer::bind_with_stack(&sock, make_stack(&toml)).unwrap();
        let task = tokio::spawn(server.serve());

        let mut conn = connect(&sock).await;
        send_msg(&mut conn, ClientMessage::Subscribe { services: vec![] }).await;
        let _ = recv_msg(&mut conn).await;
        send_msg(
            &mut conn,
            ClientMessage::Start {
                service: String::new(),
                skip_deps: false,
            },
        )
        .await;

        let mut last_state: Option<StepState> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !matches!(last_state, Some(StepState::Passed))
        {
            match tokio::time::timeout(Duration::from_millis(500), recv_msg(&mut conn)).await {
                Ok(ServerMessage::StepStatusUpdate { step, state }) if step == "bootstrap" => {
                    last_state = Some(state);
                }
                _ => {}
            }
        }
        assert_eq!(
            last_state,
            Some(StepState::Passed),
            "step never reached Passed after provision"
        );

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn on_failure_service_is_auto_restarted() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        let server = DaemonServer::bind_with_stack(
            &sock,
            make_stack(
                r#"
schema_version = 1

[service.flaky]
cmd = "exit 1"
restart = "on-failure"
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
                service: String::new(),
                skip_deps: false,
            },
        )
        .await;

        let mut starting_count = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline && starting_count < 2 {
            match tokio::time::timeout(Duration::from_millis(500), recv_msg(&mut conn)).await {
                Ok(ServerMessage::StatusUpdate {
                    service,
                    state: ServiceState::Starting,
                    ..
                }) if service == "flaky" => {
                    starting_count += 1;
                }
                Ok(_) | Err(_) => {}
            }
        }
        assert!(
            starting_count >= 2,
            "expected ≥2 Starting transitions (auto-restart), got {starting_count}"
        );

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn relentless_crasher_is_quarantined_as_crash_loop() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        // Instant-failing cmd with a flat-ish backoff window — within the
        // 60s detection window we'll see 5 exits before the test deadline.
        let server = DaemonServer::bind_with_stack(
            &sock,
            make_stack(
                r#"
schema_version = 1

[service.brokenloop]
cmd = "exit 1"
restart = "always"
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
                service: String::new(),
                skip_deps: false,
            },
        )
        .await;

        let mut saw_crash_loop = false;
        // 5 restarts with backoff 0.5, 1, 2, 4, 8s would take 15.5s. But the
        // 5th attempt's failure trips the breaker, not the 5th attempt
        // starting — so by ~7.5s we should see CrashLoop.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline && !saw_crash_loop {
            match tokio::time::timeout(Duration::from_secs(2), recv_msg(&mut conn)).await {
                Ok(ServerMessage::StatusUpdate {
                    service,
                    state: ServiceState::CrashLoop { .. },
                    ..
                }) if service == "brokenloop" => {
                    saw_crash_loop = true;
                }
                Ok(_) | Err(_) => {}
            }
        }
        assert!(saw_crash_loop, "service never reached CrashLoop");

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn never_policy_does_not_restart() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        let server = DaemonServer::bind_with_stack(
            &sock,
            make_stack(
                r#"
schema_version = 1

[service.boom]
cmd = "exit 1"
restart = "never"
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
                service: String::new(),
                skip_deps: false,
            },
        )
        .await;

        // Collect StatusUpdates for ~2s; should see only one Starting then Failed.
        let mut starting_count = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(400), recv_msg(&mut conn)).await {
                Ok(ServerMessage::StatusUpdate {
                    service,
                    state: ServiceState::Starting,
                    ..
                }) if service == "boom" => {
                    starting_count += 1;
                }
                Ok(_) | Err(_) => {}
            }
        }
        assert_eq!(
            starting_count, 1,
            "expected exactly one Starting transition with restart=never"
        );

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[tokio::test]
    async fn health_probe_gates_running_transition() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");
        // Use a shell probe that passes only after the marker file exists.
        let marker = dir.path().join("ready");
        let marker_path = marker.to_string_lossy().to_string();
        let cmd = format!("sleep 1.5; touch {marker_path}; sleep 30");
        let probe_cmd = format!("test -f {marker_path}");
        let toml = format!(
            r#"
schema_version = 1

[service.app]
cmd = "{cmd}"
health = {{ shell = "{probe_cmd}" }}
"#,
        );

        let server = DaemonServer::bind_with_stack(&sock, make_stack(&toml)).unwrap();
        let task = tokio::spawn(server.serve());

        let mut conn = connect(&sock).await;
        send_msg(&mut conn, ClientMessage::Subscribe { services: vec![] }).await;
        let _ = recv_msg(&mut conn).await;
        send_msg(
            &mut conn,
            ClientMessage::Start {
                service: String::new(),
                skip_deps: false,
            },
        )
        .await;

        let start = std::time::Instant::now();
        let mut running_at: Option<Duration> = None;
        let deadline = start + Duration::from_secs(8);
        while std::time::Instant::now() < deadline && running_at.is_none() {
            match tokio::time::timeout(Duration::from_secs(2), recv_msg(&mut conn)).await {
                Ok(ServerMessage::StatusUpdate {
                    service,
                    state: ServiceState::Running { .. },
                    ..
                }) if service == "app" => {
                    running_at = Some(start.elapsed());
                }
                Ok(_) | Err(_) => {}
            }
        }
        let running_at = running_at.expect("service never reached Running");
        // Must be after the marker is touched (~1.5s); not from the 150ms grace.
        assert!(
            running_at >= Duration::from_millis(1200),
            "Running too soon ({running_at:?}); probe was bypassed"
        );

        send_msg(&mut conn, ClientMessage::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff_for(0), RESTART_BACKOFF_BASE_MS);
        assert_eq!(backoff_for(1), RESTART_BACKOFF_BASE_MS);
        assert_eq!(backoff_for(2), RESTART_BACKOFF_BASE_MS * 2);
        assert_eq!(backoff_for(3), RESTART_BACKOFF_BASE_MS * 4);
        assert_eq!(backoff_for(6), RESTART_BACKOFF_BASE_MS * 32);
        // 500 * 64 = 32000 would exceed the 30s cap.
        assert_eq!(backoff_for(7), RESTART_BACKOFF_MAX_MS);
        assert_eq!(backoff_for(100), RESTART_BACKOFF_MAX_MS);
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
