use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, anyhow, bail};
use devme_config::{Provision, Stack};
use devme_core::{ClientMessage, ServerMessage, ServiceState, Trust};

use crate::{TaskOutputEvent, TaskResult};

const READINESS_ATTEMPT_TAIL: usize = 32;

pub type ApprovalFuture = Pin<Box<dyn Future<Output = Approval> + Send>>;
pub type ApprovalHandler = Arc<dyn Fn(ApprovalRequest) -> ApprovalFuture + Send + Sync>;
pub type DaemonFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
pub type DaemonStarter = Arc<dyn Fn(Vec<String>) -> DaemonFuture + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub step: String,
    pub command: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Approve,
    Skip,
    Cancel,
}

#[derive(Debug, Clone)]
pub enum TaskEvent {
    Progress(String),
    Output(TaskOutputEvent),
    ApprovalRequired(ApprovalRequest),
}

pub struct RunRequest {
    pub task: String,
    pub args: Vec<String>,
    pub approval: ApprovalHandler,
    pub daemon_starter: DaemonStarter,
    pub events: Option<tokio::sync::mpsc::UnboundedSender<TaskEvent>>,
    pub cancellation: Option<tokio::sync::watch::Receiver<bool>>,
}

pub struct BorrowedRunRequest {
    pub session: String,
    pub task: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub services: Vec<String>,
    pub approval: ApprovalHandler,
    pub events: Option<tokio::sync::mpsc::UnboundedSender<TaskEvent>>,
    pub cancellation: Option<tokio::sync::watch::Receiver<bool>>,
}

pub struct TaskRunner<'a> {
    stack: &'a Stack,
    root: &'a Path,
}

impl<'a> TaskRunner<'a> {
    pub fn new(stack: &'a Stack, root: &'a Path) -> Self {
        Self { stack, root }
    }

    pub async fn run(&self, request: RunRequest) -> Result<TaskResult> {
        let activity = crate::activity::TaskActivityWriter::start(self.root, &request.task);
        let result = self.run_observed(request, &activity).await;
        activity.finish(&result);
        result
    }

    async fn run_observed(
        &self,
        request: RunRequest,
        activity: &crate::activity::TaskActivityWriter,
    ) -> Result<TaskResult> {
        let started_at = now_ms();
        self.progress(
            &request.events,
            activity,
            format!("Preparing {}", request.task),
        );
        if let Err(error) = self
            .converge_steps(&request.task, &request.approval, &request.events, activity)
            .await
        {
            return if error.downcast_ref::<ApprovalCancelled>().is_some() {
                crate::record_preflight_cancellation_silent(
                    self.stack,
                    self.root,
                    &request.task,
                    &error,
                    started_at,
                    now_ms().saturating_sub(started_at),
                )
            } else {
                crate::record_preflight_failure_silent(self.stack, self.root, &request.task, &error)
            };
        }

        let services = crate::services_for(self.stack, &request.task)?;
        let _hold = if services.is_empty() {
            None
        } else {
            if let Err(error) = (request.daemon_starter)(services.clone()).await {
                return crate::record_preflight_failure_silent(
                    self.stack,
                    self.root,
                    &request.task,
                    &error,
                );
            }
            match self.acquire_service_hold(&request, &services).await {
                Ok(hold) => Some(hold),
                Err(error) => {
                    let cancelled = error.downcast_ref::<ReadinessCancelled>().is_some();
                    return if cancelled {
                        crate::record_preflight_cancellation_silent(
                            self.stack,
                            self.root,
                            &request.task,
                            &error,
                            started_at,
                            now_ms().saturating_sub(started_at),
                        )
                    } else {
                        crate::record_preflight_failure_silent(
                            self.stack,
                            self.root,
                            &request.task,
                            &error,
                        )
                    };
                }
            }
        };

        let (output_tx, output_rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = spawn_output_bridge(output_rx, request.events.clone(), activity.clone());
        self.progress(
            &request.events,
            activity,
            format!("Running {}", request.task),
        );
        let result = crate::execute_streaming(
            self.stack,
            self.root,
            &request.task,
            &request.args,
            output_tx,
            request.cancellation,
            if services.is_empty() {
                crate::GuardianHold::None
            } else {
                crate::GuardianHold::Services {
                    socket: devme_config::paths::supervisor_socket(self.root)?,
                    services,
                }
            },
        )
        .await;
        let _ = bridge.await;
        result
    }

    /// Run a Session launch Task inside the Session's existing ownership
    /// context. The Task may use only Services already held by the Session and
    /// receives the Session's allocated Resource environment without taking a
    /// second Service hold or Resource lease.
    pub async fn run_borrowed(&self, request: BorrowedRunRequest) -> Result<TaskResult> {
        let activity = crate::activity::TaskActivityWriter::start(self.root, &request.task);
        let result = self.run_borrowed_observed(request, &activity).await;
        activity.finish(&result);
        result
    }

    async fn run_borrowed_observed(
        &self,
        request: BorrowedRunRequest,
        activity: &crate::activity::TaskActivityWriter,
    ) -> Result<TaskResult> {
        let started_at = now_ms();
        self.progress(
            &request.events,
            activity,
            format!("Preparing {}", request.task),
        );
        let required = crate::services_for(self.stack, &request.task)?;
        let borrowed_closure = required_service_closure(self.stack, &request.services);
        if let Some(service) = required
            .iter()
            .find(|service| !borrowed_closure.contains(service))
        {
            bail!(
                "Session launch Task {:?} cannot widen its Service context with {service:?}",
                request.task
            );
        }
        if let Err(error) = self
            .converge_steps(&request.task, &request.approval, &request.events, activity)
            .await
        {
            return if error.downcast_ref::<ApprovalCancelled>().is_some() {
                crate::record_preflight_cancellation_silent(
                    self.stack,
                    self.root,
                    &request.task,
                    &error,
                    started_at,
                    now_ms().saturating_sub(started_at),
                )
            } else {
                crate::record_preflight_failure_silent(self.stack, self.root, &request.task, &error)
            };
        }
        let (output_tx, output_rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = spawn_output_bridge(output_rx, request.events.clone(), activity.clone());
        self.progress(
            &request.events,
            activity,
            format!("Running {}", request.task),
        );
        let result = crate::execute_streaming_with_env(
            self.stack,
            self.root,
            &request.task,
            &request.args,
            &request.env,
            output_tx,
            request.cancellation,
            crate::GuardianHold::Session {
                socket: devme_config::paths::supervisor_socket(self.root)?,
                session: request.session,
            },
        )
        .await;
        let _ = bridge.await;
        result
    }

    async fn converge_steps(
        &self,
        task: &str,
        approval: &ApprovalHandler,
        events: &Option<tokio::sync::mpsc::UnboundedSender<TaskEvent>>,
        activity: &crate::activity::TaskActivityWriter,
    ) -> Result<()> {
        for name in crate::steps_for(self.stack, task)? {
            let step = &self.stack.step[&name];
            if check(&step.check, self.root) {
                self.progress(events, activity, format!("Step {name} satisfied"));
                continue;
            }
            let Some(provision) = &step.provision else {
                bail!("required step {name:?} is not satisfied and has no provision");
            };
            let Provision::Shell(command) = provision else {
                bail!("required step {name:?} needs its wizard before this Task can run");
            };
            let approval = match step.trust {
                Trust::Auto => Approval::Approve,
                Trust::Manual => Approval::Skip,
                Trust::Prompt => {
                    let approval_request = ApprovalRequest {
                        step: name.clone(),
                        command: command.clone(),
                        description: step.description.clone(),
                    };
                    if let Some(events) = events {
                        let _ = events.send(TaskEvent::ApprovalRequired(approval_request.clone()));
                    }
                    approval(approval_request).await
                }
            };
            match approval {
                Approval::Approve => {
                    self.progress(events, activity, format!("Provisioning Step {name}"));
                    let status = std::process::Command::new("sh")
                        .args(["-c", command])
                        .current_dir(self.root)
                        .status()?;
                    if !status.success() || !check(&step.check, self.root) {
                        bail!("required step {name:?} remains unsatisfied after provisioning");
                    }
                }
                Approval::Skip => {
                    bail!("required step {name:?} is not satisfied; run this command: {command}")
                }
                Approval::Cancel => return Err(ApprovalCancelled { step: name }.into()),
            }
        }
        Ok(())
    }

    async fn acquire_service_hold(
        &self,
        request: &RunRequest,
        services: &[String],
    ) -> Result<ServiceHold> {
        let closure = required_service_closure(self.stack, services);
        let socket = devme_config::paths::supervisor_socket(self.root)?;
        let mut client = devme_client::Client::connect(&socket).await?;
        client
            .send(ClientMessage::Subscribe {
                services: closure.clone(),
            })
            .await?;
        let mut states = loop {
            match client.next_event().await? {
                Some(ServerMessage::Subscribed { services, .. }) => {
                    break services
                        .into_iter()
                        .map(|service| (service.name, service.state))
                        .collect::<HashMap<_, _>>();
                }
                Some(ServerMessage::Error { message, .. }) => bail!(message),
                Some(_) => {}
                None => bail!("supervisor stopped before the Service snapshot"),
            }
        };
        client
            .send(ClientMessage::AcquireServiceHold {
                services: services.to_vec(),
            })
            .await?;
        loop {
            match client.next_event().await? {
                Some(ServerMessage::ServiceHoldAcquired { .. }) => break,
                Some(ServerMessage::StatusUpdate { service, state, .. }) => {
                    states.insert(service, state);
                }
                Some(ServerMessage::Error { message, .. }) => bail!(message),
                Some(_) => {}
                None => bail!("supervisor stopped before acquiring the Service hold"),
            }
        }
        let timeout = crate::readiness_timeout_for(self.stack, &request.task)?;
        let deadline = Instant::now() + Duration::from_secs(timeout);
        let mut attempts: HashMap<String, Vec<(u32, String)>> = HashMap::new();
        let mut omitted_attempts: HashMap<String, usize> = HashMap::new();
        loop {
            if services
                .iter()
                .all(|name| states.get(name).is_some_and(ServiceState::is_up))
            {
                return Ok(ServiceHold { client });
            }
            if let Some(error) = terminal_service_error(self.stack, &closure, &states, &attempts) {
                return Err(error);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(readiness_timeout_error(
                    self.stack,
                    &closure,
                    &attempts,
                    &omitted_attempts,
                    timeout,
                ));
            }
            let message = tokio::select! {
                message = tokio::time::timeout(remaining, client.next_event()) => {
                    match message {
                        Ok(message) => message?,
                        Err(_) => return Err(readiness_timeout_error(
                            self.stack,
                            &closure,
                            &attempts,
                            &omitted_attempts,
                            timeout,
                        )),
                    }
                }
                _ = wait_for_cancel(request.cancellation.clone()) => {
                    return Err(ReadinessCancelled.into());
                }
            };
            match message {
                Some(ServerMessage::StatusUpdate { service, state, .. }) => {
                    states.insert(service, state);
                }
                Some(ServerMessage::Readiness {
                    service,
                    attempt,
                    last_error: Some(error),
                    ..
                }) => {
                    let service_attempts = attempts.entry(service.clone()).or_default();
                    if service_attempts.len() == READINESS_ATTEMPT_TAIL {
                        service_attempts.remove(0);
                        *omitted_attempts.entry(service).or_default() += 1;
                    }
                    service_attempts.push((attempt, error));
                }
                Some(ServerMessage::Error { message, .. }) => bail!(message),
                Some(_) => {}
                None => bail!("supervisor stopped while waiting for readiness"),
            }
        }
    }

    fn progress(
        &self,
        events: &Option<tokio::sync::mpsc::UnboundedSender<TaskEvent>>,
        activity: &crate::activity::TaskActivityWriter,
        message: String,
    ) {
        activity.progress(&message);
        if let Some(events) = events {
            let _ = events.send(TaskEvent::Progress(message));
        }
    }
}

fn spawn_output_bridge(
    mut output: tokio::sync::mpsc::UnboundedReceiver<TaskOutputEvent>,
    events: Option<tokio::sync::mpsc::UnboundedSender<TaskEvent>>,
    activity: crate::activity::TaskActivityWriter,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = output.recv().await {
            activity.output(&event);
            if let Some(events) = &events {
                let _ = events.send(TaskEvent::Output(event));
            }
        }
    })
}

fn readiness_timeout_error(
    stack: &Stack,
    closure: &[String],
    attempts: &HashMap<String, Vec<(u32, String)>>,
    omitted_attempts: &HashMap<String, usize>,
    timeout_secs: u64,
) -> anyhow::Error {
    let detail = closure
        .iter()
        .filter_map(|name| {
            let service = stack.service.get(name)?;
            let readiness = service.readiness.clone().unwrap_or_default();
            let evidence = attempts
                .get(name)
                .map(|items| {
                    let tail = items
                        .iter()
                        .map(|(attempt, error)| format!("attempt {attempt}: {error}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    match omitted_attempts.get(name).copied().unwrap_or_default() {
                        0 => tail,
                        omitted => format!("{omitted} earlier attempts omitted, {tail}"),
                    }
                })
                .unwrap_or_else(|| "no probe attempt reported".to_string());
            Some(format!(
                "{name}: {evidence} (interval_ms={}, timeout_ms={}, retries={}, deadline_seconds={timeout_secs})",
                readiness.interval_ms, readiness.timeout_ms, readiness.retries
            ))
        })
        .collect::<Vec<_>>()
        .join("; ");
    anyhow!(
        "required services were not ready after {timeout_secs}s{}",
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )
}

fn terminal_service_error(
    stack: &Stack,
    closure: &[String],
    states: &HashMap<String, ServiceState>,
    attempts: &HashMap<String, Vec<(u32, String)>>,
) -> Option<anyhow::Error> {
    closure.iter().find_map(|name| {
        let state = states.get(name)?;
        let failure = match state {
            ServiceState::Failed { exit_code } => match exit_code {
                Some(code) => format!("process exited with code {code}"),
                None => "process was terminated by a signal".to_string(),
            },
            ServiceState::CrashLoop {
                restart_count,
                reason,
            } => format!(
                "entered a crash loop after {restart_count} restarts{}",
                reason
                    .as_deref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ),
            _ => return None,
        };
        let readiness = stack
            .service
            .get(name)
            .and_then(|service| service.readiness.clone())
            .unwrap_or_default();
        let probe = attempts
            .get(name)
            .and_then(|items| items.last())
            .map(|(attempt, error)| format!("; probe attempt {attempt}: {error}"))
            .unwrap_or_default();
        Some(anyhow!(
            "required service {name:?} failed before it became ready: {failure}{probe} (interval_ms={}, timeout_ms={}, retries={}); run `devme doctor {name}` and `devme logs {name}`",
            readiness.interval_ms,
            readiness.timeout_ms,
            readiness.retries
        ))
    })
}

struct ServiceHold {
    client: devme_client::Client,
}

impl Drop for ServiceHold {
    fn drop(&mut self) {
        let _ = &self.client;
    }
}

#[derive(Debug)]
struct ReadinessCancelled;

impl std::fmt::Display for ReadinessCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("task cancelled while waiting for required service readiness")
    }
}

impl std::error::Error for ReadinessCancelled {}

#[derive(Debug)]
struct ApprovalCancelled {
    step: String,
}

impl std::fmt::Display for ApprovalCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "task cancelled before provisioning Step {}",
            self.step
        )
    }
}

impl std::error::Error for ApprovalCancelled {}

fn check(command: &str, root: &Path) -> bool {
    std::process::Command::new("sh")
        .args(["-c", command])
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn required_service_closure(stack: &Stack, targets: &[String]) -> Vec<String> {
    fn visit(stack: &Stack, graph: &devme_config::Graph, name: &str, found: &mut Vec<String>) {
        if found.iter().any(|candidate| candidate == name) || !stack.service.contains_key(name) {
            return;
        }
        found.push(name.to_string());
        for dependency in graph
            .dependencies(name)
            .iter()
            .filter(|dependency| dependency.required)
        {
            visit(stack, graph, &dependency.name, found);
        }
    }
    let graph = devme_config::Graph::from_stack(stack);
    let mut found = Vec::new();
    for target in targets {
        visit(stack, &graph, target, &mut found);
    }
    found
}

async fn wait_for_cancel(mut cancellation: Option<tokio::sync::watch::Receiver<bool>>) {
    match cancellation.as_mut() {
        Some(receiver) => loop {
            if *receiver.borrow() {
                return;
            }
            tokio::select! {
                _ = tokio::signal::ctrl_c() => return,
                changed = receiver.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        },
        None => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval(value: Approval) -> ApprovalHandler {
        Arc::new(move |_| Box::pin(async move { value }))
    }

    #[tokio::test]
    async fn run_publishes_process_shared_activity_until_completion() {
        let root = tempfile::tempdir().unwrap();
        let stack = Stack::parse(
            "schema_version=1\n[task.verify]\ncmd=\"echo compiling; sleep 0.3; echo done\"\n",
        )
        .unwrap();
        let runner = TaskRunner::new(&stack, root.path());
        let run = runner.run(RunRequest {
            task: "verify".into(),
            args: Vec::new(),
            approval: approval(Approval::Approve),
            daemon_starter: Arc::new(|_| Box::pin(async { Ok(()) })),
            events: None,
            cancellation: None,
        });
        tokio::pin!(run);

        tokio::select! {
            result = &mut run => panic!("task finished before activity could be observed: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(80)) => {}
        }
        let live = crate::read_task_activities(root.path()).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].task, "verify");
        assert!(live[0].is_running());
        assert!(live[0].owner_is_live());

        let result = run.await.unwrap();
        assert_eq!(result.status, "passed");
        let finished = crate::read_task_activities(root.path()).unwrap();
        assert_eq!(finished.len(), 1);
        assert_eq!(
            finished[0].outcome(),
            Some(crate::TaskActivityOutcome::Succeeded)
        );
        assert!(!finished[0].is_running());
    }

    #[tokio::test]
    async fn borrowed_session_run_cannot_widen_services() {
        let root = tempfile::tempdir().unwrap();
        let stack = Stack::parse(
            "schema_version=1\n[service.backend]\ncmd=\"sleep 1\"\n[task.launch]\ncmd=\"true\"\nservices=[\"backend\"]\n",
        )
        .unwrap();
        let error = TaskRunner::new(&stack, root.path())
            .run_borrowed(BorrowedRunRequest {
                session: "dev".into(),
                task: "launch".into(),
                args: Vec::new(),
                env: Default::default(),
                services: Vec::new(),
                approval: approval(Approval::Approve),
                events: None,
                cancellation: None,
            })
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot widen its Service context")
        );
    }

    #[tokio::test]
    async fn borrowed_session_run_accepts_services_in_the_borrowed_closure() {
        let root = tempfile::tempdir().unwrap();
        let stack = Stack::parse(
            "schema_version=1\n[service.db]\ncmd=\"sleep 1\"\n[service.api]\ncmd=\"sleep 1\"\ndepends_on=[\"db\"]\n[task.launch]\ncmd=\"true\"\nservices=[\"db\"]\n",
        )
        .unwrap();
        let result = TaskRunner::new(&stack, root.path())
            .run_borrowed(BorrowedRunRequest {
                session: "dev".into(),
                task: "launch".into(),
                args: Vec::new(),
                env: Default::default(),
                services: vec!["api".into()],
                approval: approval(Approval::Approve),
                events: None,
                cancellation: None,
            })
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn borrowed_session_run_uses_typed_step_approval() {
        let root = tempfile::tempdir().unwrap();
        let stack = Stack::parse(
            "schema_version=1\n[step.toolchain]\ncheck=\"test -f ready\"\nprovision=\"touch ready\"\ntrust=\"prompt\"\n[task.launch]\ncmd=\"true\"\nsteps=[\"toolchain\"]\n",
        )
        .unwrap();
        let result = TaskRunner::new(&stack, root.path())
            .run_borrowed(BorrowedRunRequest {
                session: "dev".into(),
                task: "launch".into(),
                args: Vec::new(),
                env: Default::default(),
                services: Vec::new(),
                approval: Arc::new(|request| {
                    Box::pin(async move {
                        assert_eq!(request.step, "toolchain");
                        assert_eq!(request.command, "touch ready");
                        Approval::Approve
                    })
                }),
                events: None,
                cancellation: None,
            })
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(root.path().join("ready").exists());
    }

    #[tokio::test]
    async fn cancelling_typed_step_approval_persists_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let stack = Stack::parse(
            "schema_version=1\n[step.toolchain]\ncheck=\"false\"\nprovision=\"true\"\ntrust=\"prompt\"\n[task.launch]\ncmd=\"true\"\nsteps=[\"toolchain\"]\n",
        )
        .unwrap();
        let result = TaskRunner::new(&stack, root.path())
            .run_borrowed(BorrowedRunRequest {
                session: "dev".into(),
                task: "launch".into(),
                args: Vec::new(),
                env: Default::default(),
                services: Vec::new(),
                approval: approval(Approval::Cancel),
                events: None,
                cancellation: None,
            })
            .await
            .unwrap();
        assert_eq!(result.exit_code, 130);
        assert!(result.cancelled);
    }
}
