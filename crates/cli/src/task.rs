//! One-shot task execution, result history, and generic scarce-resource leases.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use devme_config::{ResolvedWorkspace, Resource, ResourceScope, Stack, Task};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::{OutputFormat, TaskAction};

const CAPTURE_LIMIT: usize = 64 * 1024;

#[derive(Debug)]
pub struct UnknownTask {
    name: String,
    available: Vec<String>,
}

impl std::fmt::Display for UnknownTask {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "no task named {:?}; available tasks: {}",
            self.name,
            self.available.join(", ")
        )
    }
}

impl std::error::Error for UnknownTask {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub task: String,
    pub status: String,
    pub exit_code: i32,
    pub started_at: u64,
    pub finished_at: u64,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_events: Vec<TaskOutputEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutputEvent {
    pub ts: u64,
    pub stream: devme_core::LogStream,
    pub text: String,
}

pub fn load(cwd: &Path) -> Result<Stack> {
    Ok(resolve(cwd)?.into_stack())
}

pub fn resolve(cwd: &Path) -> Result<ResolvedWorkspace> {
    ResolvedWorkspace::resolve(cwd).context("could not resolve Devme workspace")
}

pub fn services_for(stack: &Stack, name: &str) -> Result<Vec<String>> {
    let order = execution_order(stack, name)?;
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for task in order {
        for service in &stack.task[task].services {
            if !stack.service.contains_key(service) {
                bail!("task {task:?} requires unknown service {service:?}");
            }
            if seen.insert(service.clone()) {
                result.push(service.clone());
            }
        }
    }
    Ok(result)
}

/// Setup-step closure required by a task and all task dependencies.
pub fn steps_for(stack: &Stack, name: &str) -> Result<Vec<String>> {
    fn visit_step(stack: &Stack, name: &str, seen: &mut HashSet<String>, out: &mut Vec<String>) {
        if !seen.insert(name.to_string()) {
            return;
        }
        let Some(step) = stack.step.get(name) else {
            return;
        };
        for dependency in &step.depends_on {
            if stack.step.contains_key(&dependency.name) {
                visit_step(stack, &dependency.name, seen, out);
            }
        }
        out.push(name.to_string());
    }

    let order = execution_order(stack, name)?;
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for task in order {
        for step in &stack.task[task].steps {
            if !stack.step.contains_key(step) {
                bail!("task {task:?} requires unknown step {step:?}");
            }
            visit_step(stack, step, &mut seen, &mut result);
        }
    }
    Ok(result)
}

pub fn readiness_timeout_for(stack: &Stack, name: &str) -> Result<u64> {
    Ok(execution_order(stack, name)?
        .into_iter()
        .map(|task| stack.task[task].readiness_timeout)
        .max()
        .unwrap_or(60))
}

pub fn show(stack: &Stack, action: Option<TaskAction>, format: OutputFormat) -> Result<()> {
    match action {
        Some(TaskAction::Show { task }) => {
            let value = stack
                .task
                .get(&task)
                .ok_or_else(|| unknown_task(stack, &task))?;
            match format {
                OutputFormat::Json => devme_ui::json(&serde_json::json!({
                    "schema_version": 1,
                    "name": task,
                    "task": value,
                })),
                OutputFormat::Toon => {
                    crate::output::print_toon(&serde_json::json!({
                        "task": {
                            "name": task,
                            "kind": value.kind,
                            "description": value.description,
                            "command": value.cmd,
                            "cwd": value.cwd,
                            "environment": value.env,
                            "dependencies": value.depends_on,
                            "steps": value.steps,
                            "services": value.services,
                            "resources": value.resources,
                            "timeout_seconds": value.timeout,
                            "readiness_timeout_seconds": value.readiness_timeout,
                        }
                    }))?;
                }
                OutputFormat::Human => println!("{}", toml::to_string_pretty(value)?),
            }
        }
        None => match format {
            OutputFormat::Json => {
                let rows: Vec<_> = stack.task.iter().map(|(name, task)| serde_json::json!({
                        "name": name, "task_kind": task.kind, "description": task.description, "has_command": task.cmd.is_some()
                    })).collect();
                devme_ui::json(&serde_json::json!({
                    "schema_version": 1,
                    "count": rows.len(),
                    "tasks": rows,
                }));
            }
            OutputFormat::Toon => {
                let mut output = format!(
                    "count: {}\ntasks[{}]{{name,description,kind,task_kind}}:",
                    stack.task.len(),
                    stack.task.len()
                );
                for (name, task) in &stack.task {
                    let task_kind = match task.kind {
                        devme_config::TaskKind::Launch => "launch",
                        devme_config::TaskKind::Check => "check",
                        devme_config::TaskKind::Utility => "utility",
                    };
                    output.push_str(&format!(
                        "\n  {},{},{},{}",
                        toon_string(name),
                        toon_string(task.description.as_deref().unwrap_or("")),
                        if task.cmd.is_some() {
                            "command"
                        } else {
                            "aggregate"
                        },
                        task_kind,
                    ));
                }
                if stack.task.is_empty() {
                    output.push_str("\nhelp: No tasks are declared in devme.toml");
                } else {
                    output.push_str("\nhelp: Run `devme tasks show <name>` for details");
                }
                print!("{output}");
            }
            OutputFormat::Human => {
                if stack.task.is_empty() {
                    println!("No tasks are declared in devme.toml.");
                }
                for (name, task) in &stack.task {
                    println!(
                        "{name:<20} {}",
                        task.description
                            .as_deref()
                            .unwrap_or(if task.cmd.is_some() {
                                "command"
                            } else {
                                "aggregate"
                            })
                    );
                }
            }
        },
    }
    Ok(())
}

pub async fn execute(
    stack: &Stack,
    root: &Path,
    name: &str,
    args: &[String],
    format: OutputFormat,
) -> Result<TaskResult> {
    execute_inner(
        stack,
        root,
        name,
        args,
        format,
        &BTreeMap::new(),
        true,
        None,
        None,
    )
    .await
}

/// Execute through the same runner semantics without writing over a TUI frame.
pub async fn execute_silent(
    stack: &Stack,
    root: &Path,
    name: &str,
    args: &[String],
) -> Result<TaskResult> {
    execute_inner(
        stack,
        root,
        name,
        args,
        OutputFormat::Human,
        &BTreeMap::new(),
        false,
        None,
        None,
    )
    .await
}

pub async fn execute_streaming(
    stack: &Stack,
    root: &Path,
    name: &str,
    args: &[String],
    updates: tokio::sync::mpsc::UnboundedSender<TaskOutputEvent>,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<TaskResult> {
    execute_inner(
        stack,
        root,
        name,
        args,
        OutputFormat::Human,
        &BTreeMap::new(),
        false,
        Some(updates),
        cancellation,
    )
    .await
}

/// Execute a task with identifiers allocated by an already-held session.
/// Session config validation requires the task itself to declare no resources,
/// so this path cannot reacquire and deadlock on the session's leases.
pub async fn execute_with_env(
    stack: &Stack,
    root: &Path,
    name: &str,
    args: &[String],
    format: OutputFormat,
    injected_env: &BTreeMap<String, String>,
) -> Result<TaskResult> {
    execute_inner(
        stack,
        root,
        name,
        args,
        format,
        injected_env,
        true,
        None,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_inner(
    stack: &Stack,
    root: &Path,
    name: &str,
    args: &[String],
    format: OutputFormat,
    injected_env: &BTreeMap<String, String>,
    emit: bool,
    updates: Option<tokio::sync::mpsc::UnboundedSender<TaskOutputEvent>>,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<TaskResult> {
    let order = execution_order(stack, name)?;
    let retention = retention_bytes(stack);
    let slot = match SlotClaim::acquire(root) {
        Ok(slot) => slot,
        Err(error) => {
            let result = failed_result(name, &error.to_string(), 1, false, now_ms(), 0);
            persist(root, &result, retention)?;
            if emit {
                emit_result(&result, format)?;
            }
            return Ok(result);
        }
    };
    let mut final_result = None;
    for current in order {
        let pass = if current == name { args } else { &[] };
        let result = execute_one(
            stack,
            root,
            current,
            pass,
            format,
            slot.value,
            injected_env,
            emit,
            updates.clone(),
            cancellation.clone(),
        )
        .await?;
        let failed = result.exit_code != 0;
        let failed_dependency = current != name && failed;
        final_result = Some(result);
        if failed_dependency {
            let dependency = final_result.as_ref().expect("result was just assigned");
            let message = format!(
                "dependency {current:?} failed with exit code {}",
                dependency.exit_code
            );
            let root_result = failed_result(
                name,
                &message,
                dependency.exit_code,
                dependency.cancelled,
                dependency.started_at,
                dependency.duration_ms,
            );
            persist(root, &root_result, retention)?;
            final_result = Some(root_result);
            break;
        }
        if failed {
            break;
        }
    }
    let result = final_result.expect("execution order is never empty");
    if emit {
        emit_result(&result, format)?;
    }
    Ok(result)
}

/// Persist and emit a failure that occurs after task selection but before its
/// process can be spawned, such as required-service readiness failure.
pub fn record_preflight_failure(
    stack: &Stack,
    root: &Path,
    name: &str,
    error: &anyhow::Error,
    format: OutputFormat,
) -> Result<TaskResult> {
    record_preflight_result(stack, root, name, error, format, false, None, true)
}

pub fn record_preflight_failure_silent(
    stack: &Stack,
    root: &Path,
    name: &str,
    error: &anyhow::Error,
) -> Result<TaskResult> {
    record_preflight_result(
        stack,
        root,
        name,
        error,
        OutputFormat::Human,
        false,
        None,
        false,
    )
}

/// Persist and emit cancellation that occurs while converging a task's
/// required services. This keeps Ctrl-C compatible with shell cancellation
/// conventions even though the task process itself has not started yet.
pub fn record_preflight_cancellation(
    stack: &Stack,
    root: &Path,
    name: &str,
    error: &anyhow::Error,
    format: OutputFormat,
    started_at: u64,
    duration_ms: u64,
) -> Result<TaskResult> {
    record_preflight_result(
        stack,
        root,
        name,
        error,
        format,
        true,
        Some((started_at, duration_ms)),
        true,
    )
}

pub fn record_preflight_cancellation_silent(
    stack: &Stack,
    root: &Path,
    name: &str,
    error: &anyhow::Error,
    started_at: u64,
    duration_ms: u64,
) -> Result<TaskResult> {
    record_preflight_result(
        stack,
        root,
        name,
        error,
        OutputFormat::Human,
        true,
        Some((started_at, duration_ms)),
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn record_preflight_result(
    stack: &Stack,
    root: &Path,
    name: &str,
    error: &anyhow::Error,
    format: OutputFormat,
    cancelled: bool,
    timing: Option<(u64, u64)>,
    emit: bool,
) -> Result<TaskResult> {
    let (started_at, duration_ms) = timing.unwrap_or_else(|| (now_ms(), 0));
    let redactor =
        devme_config::Redactor::new(&devme_config::persistence_redaction_patterns(stack))
            .context("invalid logs.redact pattern")?;
    let task = stack
        .task
        .get(name)
        .ok_or_else(|| unknown_task(stack, name))?;
    let secret_values = redaction_values(
        stack,
        task,
        &interpolation_context(root, 0),
        &BTreeMap::new(),
    );
    let message = redact(error.to_string().as_bytes(), &secret_values, &redactor);
    let result = failed_result(
        name,
        &message,
        if cancelled { 130 } else { 1 },
        cancelled,
        started_at,
        duration_ms,
    );
    persist(root, &result, retention_bytes(stack))?;
    if emit {
        emit_result(&result, format)?;
    }
    Ok(result)
}

fn execution_order<'a>(stack: &'a Stack, root: &'a str) -> Result<Vec<&'a str>> {
    fn visit<'a>(
        stack: &'a Stack,
        name: &'a str,
        visiting: &mut HashSet<&'a str>,
        done: &mut HashSet<&'a str>,
        out: &mut Vec<&'a str>,
    ) -> Result<()> {
        if done.contains(name) {
            return Ok(());
        }
        let task = stack
            .task
            .get(name)
            .ok_or_else(|| unknown_task(stack, name))?;
        if !visiting.insert(name) {
            bail!("task dependency cycle includes {name:?}");
        }
        for dep in &task.depends_on {
            visit(stack, dep, visiting, done, out)?;
        }
        visiting.remove(name);
        done.insert(name);
        out.push(name);
        Ok(())
    }
    let mut out = Vec::new();
    visit(
        stack,
        root,
        &mut HashSet::new(),
        &mut HashSet::new(),
        &mut out,
    )?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn execute_one(
    stack: &Stack,
    root: &Path,
    name: &str,
    args: &[String],
    format: OutputFormat,
    slot: u8,
    injected_env: &BTreeMap<String, String>,
    emit: bool,
    updates: Option<tokio::sync::mpsc::UnboundedSender<TaskOutputEvent>>,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<TaskResult> {
    let task = &stack.task[name];
    let retention = retention_bytes(stack);
    let capture_limit = CAPTURE_LIMIT.min((retention / 4).max(256) as usize);
    let started_at = now_ms();
    let started = std::time::Instant::now();
    let persistence_patterns = devme_config::persistence_redaction_patterns(stack);
    let redactor = match devme_config::Redactor::new(&persistence_patterns)
        .context("invalid logs.redact pattern")
    {
        Ok(redactor) => redactor,
        Err(error) => {
            let result = failed_result(
                name,
                &error.to_string(),
                1,
                false,
                started_at,
                started.elapsed().as_millis() as u64,
            );
            persist(root, &result, retention)?;
            return Ok(result);
        }
    };
    let ctx = interpolation_context(root, slot);
    let secret_values = redaction_values(stack, task, &ctx, injected_env);
    let attempt = execute_one_attempt(
        stack,
        root,
        name,
        args,
        capture_limit,
        &secret_values,
        &persistence_patterns,
        &ctx,
        started_at,
        &started,
        injected_env,
        updates,
        cancellation,
    )
    .await;
    let result = match attempt {
        Ok(result) => result,
        Err(error) => {
            let cancelled = error.downcast_ref::<ResourceWaitCancelled>().is_some();
            let message = redact(error.to_string().as_bytes(), &secret_values, &redactor);
            failed_result(
                name,
                &message,
                if cancelled { 130 } else { 1 },
                cancelled,
                started_at,
                started.elapsed().as_millis() as u64,
            )
        }
    };
    if emit && format == OutputFormat::Human {
        print!("{}", result.stdout);
        eprint!("{}", result.stderr);
    }
    persist(root, &result, retention)?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn execute_one_attempt(
    stack: &Stack,
    root: &Path,
    name: &str,
    args: &[String],
    capture_limit: usize,
    secret_values: &[String],
    persistence_patterns: &[String],
    ctx: &devme_config::InterpContext,
    started_at: u64,
    started: &std::time::Instant,
    injected_env: &BTreeMap<String, String>,
    updates: Option<tokio::sync::mpsc::UnboundedSender<TaskOutputEvent>>,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<TaskResult> {
    let task = &stack.task[name];
    for step in &task.steps {
        let configured = stack
            .step
            .get(step)
            .ok_or_else(|| anyhow!("task {name:?} requires unknown step {step:?}"))?;
        let status = std::process::Command::new("sh")
            .args(["-c", &configured.check])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            bail!("required step {step:?} is not satisfied; run `devme up` to provision it");
        }
    }
    if task.cmd.is_none() {
        return Ok(empty_result(name));
    }

    let leases =
        acquire_resources(stack, root, name, &task.resources, cancellation.clone()).await?;
    let cwd = match &task.cwd {
        Some(value) => root.join(devme_config::interpolate(value, ctx)?),
        None => root.to_path_buf(),
    };
    let mut command = tokio::process::Command::new("sh");
    let full_command = append_args(
        &devme_config::interpolate(task.cmd.as_deref().unwrap(), ctx)?,
        args,
    );
    command
        .args(["-c", &full_command])
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in &task.env {
        command.env(key, devme_config::interpolate(value, ctx)?);
    }
    // Allocated identifiers are authoritative for the held session and
    // intentionally override static task env with the same key.
    command.envs(injected_env);
    for lease in &leases {
        if let Some(env) = &lease.env {
            command.env(env, lease.id.to_string());
        }
    }
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start task {name:?}"))?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow!("task process has no pid"))? as i32;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out_reader = tokio::spawn(read_stream(
        stdout,
        capture_limit,
        devme_core::LogStream::Stdout,
        secret_values.to_vec(),
        persistence_patterns.to_vec(),
        updates.clone(),
    ));
    let err_reader = tokio::spawn(read_stream(
        stderr,
        capture_limit,
        devme_core::LogStream::Stderr,
        secret_values.to_vec(),
        persistence_patterns.to_vec(),
        updates,
    ));

    let deadline = if task.timeout == 0 {
        Duration::from_secs(365 * 24 * 3600)
    } else {
        Duration::from_secs(task.timeout)
    };
    let (status, timed_out, cancelled) = tokio::select! {
        value = child.wait() => (value?, false, false),
        _ = tokio::time::sleep(deadline) => { terminate_group(pid, &mut child).await?; (child.wait().await?, true, false) },
        _ = tokio::signal::ctrl_c() => { terminate_group(pid, &mut child).await?; (child.wait().await?, false, true) },
        _ = wait_for_cancel(cancellation) => { terminate_group(pid, &mut child).await?; (child.wait().await?, false, true) },
    };
    let out = out_reader.await??;
    let err = err_reader.await??;
    let exit_code = if timed_out {
        124
    } else if cancelled {
        130
    } else {
        raw_exit_code(status)
    };
    let result = TaskResult {
        task: name.to_string(),
        status: if exit_code == 0 {
            "passed".into()
        } else if timed_out {
            "timed_out".into()
        } else if cancelled {
            "cancelled".into()
        } else {
            "failed".into()
        },
        exit_code,
        started_at,
        finished_at: now_ms(),
        duration_ms: started.elapsed().as_millis() as u64,
        timed_out,
        cancelled,
        stdout: out.text,
        stderr: err.text,
        truncated: out.truncated || err.truncated,
        output_events: {
            let mut events = out.events;
            events.extend(err.events);
            events.sort_by_key(|event| event.ts);
            events
        },
    };
    Ok(result)
}

fn raw_exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

struct StreamCapture {
    text: String,
    events: Vec<TaskOutputEvent>,
    truncated: bool,
}

async fn read_stream<R>(
    mut reader: R,
    limit: usize,
    stream: devme_core::LogStream,
    literals: Vec<String>,
    patterns: Vec<String>,
    updates: Option<tokio::sync::mpsc::UnboundedSender<TaskOutputEvent>>,
) -> std::io::Result<StreamCapture>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let redactor = devme_config::Redactor::new(&patterns).map_err(std::io::Error::other)?;
    let redact_all = literals
        .iter()
        .any(|literal| literal.contains(['\n', '\r']));
    let raw_frame_limit = limit.saturating_mul(4).max(8192);
    let mut output = String::with_capacity(limit);
    let mut events = VecDeque::new();
    let mut event_bytes = 0;
    let mut frame = Vec::new();
    let mut dropping_frame = false;
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let mut offset = 0;
        while offset < read {
            let end = buffer[offset..read]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(read, |position| offset + position + 1);
            let segment = &buffer[offset..end];
            if dropping_frame {
                if segment.ends_with(b"\n") {
                    push_redacted_frame(
                        b"[REDACTED oversized output frame]\n",
                        stream,
                        limit,
                        &mut output,
                        &mut events,
                        &mut event_bytes,
                        &mut truncated,
                        updates.as_ref(),
                    );
                    dropping_frame = false;
                }
            } else if frame.len().saturating_add(segment.len()) > raw_frame_limit {
                frame.clear();
                truncated = true;
                dropping_frame = !segment.ends_with(b"\n");
                if !dropping_frame {
                    push_redacted_frame(
                        b"[REDACTED oversized output frame]\n",
                        stream,
                        limit,
                        &mut output,
                        &mut events,
                        &mut event_bytes,
                        &mut truncated,
                        updates.as_ref(),
                    );
                }
            } else {
                frame.extend_from_slice(segment);
                if segment.ends_with(b"\n") {
                    let redacted = if redact_all {
                        b"[REDACTED multiline secret output]\n".to_vec()
                    } else {
                        redact(&frame, &literals, &redactor).into_bytes()
                    };
                    push_redacted_frame(
                        &redacted,
                        stream,
                        limit,
                        &mut output,
                        &mut events,
                        &mut event_bytes,
                        &mut truncated,
                        updates.as_ref(),
                    );
                    frame.clear();
                }
            }
            offset = end;
        }
    }
    if dropping_frame {
        push_redacted_frame(
            b"[REDACTED oversized output frame]",
            stream,
            limit,
            &mut output,
            &mut events,
            &mut event_bytes,
            &mut truncated,
            updates.as_ref(),
        );
    } else if !frame.is_empty() {
        let redacted = if redact_all {
            b"[REDACTED multiline secret output]".to_vec()
        } else {
            redact(&frame, &literals, &redactor).into_bytes()
        };
        push_redacted_frame(
            &redacted,
            stream,
            limit,
            &mut output,
            &mut events,
            &mut event_bytes,
            &mut truncated,
            updates.as_ref(),
        );
    }
    Ok(StreamCapture {
        text: output,
        events: events.into(),
        truncated,
    })
}

#[allow(clippy::too_many_arguments)]
fn push_redacted_frame(
    bytes: &[u8],
    stream: devme_core::LogStream,
    limit: usize,
    output: &mut String,
    events: &mut VecDeque<TaskOutputEvent>,
    event_bytes: &mut usize,
    truncated: &mut bool,
    updates: Option<&tokio::sync::mpsc::UnboundedSender<TaskOutputEvent>>,
) {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if text.len() > limit {
        keep_utf8_tail(&mut text, limit);
        *truncated = true;
    }
    output.push_str(&text);
    if output.len() > limit {
        keep_utf8_tail(output, limit);
        *truncated = true;
    }
    *event_bytes += text.len();
    let event = TaskOutputEvent {
        ts: now_ms(),
        stream,
        text,
    };
    if let Some(updates) = updates {
        let _ = updates.send(event.clone());
    }
    events.push_back(event);
    while *event_bytes > limit && events.len() > 1 {
        if let Some(removed) = events.pop_front() {
            *event_bytes = event_bytes.saturating_sub(removed.text.len());
            *truncated = true;
        }
    }
}

fn keep_utf8_tail(value: &mut String, limit: usize) {
    let mut start = value.len().saturating_sub(limit);
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    value.drain(..start);
}

fn interpolation_context(root: &Path, slot: u8) -> devme_config::InterpContext {
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["branch", "--show-current"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    devme_config::InterpContext::new()
        .set("slot", slot.to_string())
        .set("worktree", root.display().to_string())
        .set("branch", branch)
}

struct SlotClaim {
    value: u8,
    allocator: devme_slot_allocator::SlotAllocator,
    instance: String,
    owned: bool,
}

impl SlotClaim {
    fn acquire(root: &Path) -> Result<Self> {
        let allocator =
            devme_slot_allocator::SlotAllocator::open(devme_config::paths::slot_registry()?);
        let instance = devme_config::paths::instance_id(root);
        let owned = !allocator
            .list()?
            .iter()
            .any(|claim| claim.instance_id == instance);
        let value = allocator.claim(&instance)?.as_u8();
        Ok(Self {
            value,
            allocator,
            instance,
            owned,
        })
    }
}

impl Drop for SlotClaim {
    fn drop(&mut self) {
        if self.owned {
            let _ = self.allocator.release(&self.instance);
        }
    }
}

async fn terminate_group(pid: i32, child: &mut tokio::process::Child) -> Result<()> {
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    if tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    Ok(())
}

fn append_args(command: &str, args: &[String]) -> String {
    let mut value = command.to_string();
    for arg in args {
        value.push(' ');
        value.push_str(&shell_quote(arg));
    }
    value
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct Lease {
    _file: File,
    id: u32,
    env: Option<String>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self._file);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceWaitRecord {
    pub pid: u32,
    pub task: String,
    pub resource: String,
    pub worktree: PathBuf,
    pub waiting_since: u64,
}

struct ResourceWaitGuard {
    path: PathBuf,
    record: ResourceWaitRecord,
}

impl ResourceWaitGuard {
    fn create(root: &Path, task: &str, resource: &str) -> Result<Self> {
        let directory = resource_waiter_dir()?;
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{}-{}.json", std::process::id(), sanitize(task)));
        let guard = Self {
            path,
            record: ResourceWaitRecord {
                pid: std::process::id(),
                task: task.to_string(),
                resource: resource.to_string(),
                worktree: root.to_path_buf(),
                waiting_since: now_ms(),
            },
        };
        guard.persist()?;
        Ok(guard)
    }

    fn update(&mut self, resource: &str) -> Result<()> {
        self.record.resource = resource.to_string();
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec(&self.record)?)?;
        std::fs::rename(temporary, &self.path)?;
        Ok(())
    }
}

impl Drop for ResourceWaitGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("json.tmp"));
    }
}

fn resource_waiter_dir() -> Result<PathBuf> {
    Ok(devme_config::paths::runtime_dir()?
        .join("resources")
        .join("waiters"))
}

/// Return live task waiters, removing records left by crashed owners. When a
/// worktree is supplied, unrelated repositories/worktrees are excluded.
pub fn read_resource_waiters(worktree: Option<&Path>) -> Result<Vec<ResourceWaitRecord>> {
    let directory = resource_waiter_dir()?;
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<ResourceWaitRecord>(&bytes) else {
            let _ = std::fs::remove_file(path);
            continue;
        };
        if !process_is_live(record.pid) {
            let _ = std::fs::remove_file(path);
            continue;
        }
        if worktree.is_none_or(|root| record.worktree == root) {
            records.push(record);
        }
    }
    records.sort_by_key(|record| record.waiting_since);
    Ok(records)
}

fn process_is_live(pid: u32) -> bool {
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
}

#[derive(Debug)]
struct ResourceWaitCancelled {
    resource: String,
}

impl std::fmt::Display for ResourceWaitCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "cancelled while waiting for resource {:?}",
            self.resource
        )
    }
}

impl std::error::Error for ResourceWaitCancelled {}

async fn acquire_resources(
    stack: &Stack,
    root: &Path,
    task: &str,
    names: &[String],
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<Vec<Lease>> {
    let mut ordered = names.to_vec();
    ordered.sort();
    ordered.dedup();
    for name in &ordered {
        let resource = stack
            .resource
            .get(name)
            .ok_or_else(|| anyhow!("task {task:?} requires unknown resource {name:?}"))?;
        if resource.capacity == 0 {
            bail!("resource {name:?} capacity must be at least 1");
        }
        std::fs::create_dir_all(resource_dir(root, name, resource)?)?;
    }

    let mut announced = std::collections::HashSet::new();
    let mut wait_record: Option<ResourceWaitGuard> = None;
    loop {
        let mut leases = Vec::with_capacity(ordered.len());
        let mut blocked = None;
        for name in &ordered {
            let resource = &stack.resource[name];
            let dir = resource_dir(root, name, resource)?;
            match try_acquire(&dir, resource, task, root)? {
                Some(lease) => leases.push(lease),
                None => {
                    blocked = Some(name.clone());
                    break;
                }
            }
        }
        if let Some(resource) = blocked {
            // Atomic means all or none. Releasing this partial attempt before
            // waiting prevents a task blocked on B from starving users of A.
            drop(leases);
            if announced.insert(resource.clone()) {
                devme_ui::info(format!("task {task}: waiting for resource {resource}"));
            }
            match &mut wait_record {
                Some(record) if record.record.resource != resource => record.update(&resource)?,
                Some(_) => {}
                None => wait_record = Some(ResourceWaitGuard::create(root, task, &resource)?),
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                _ = tokio::signal::ctrl_c() => {
                    return Err(ResourceWaitCancelled { resource }.into());
                },
                _ = wait_for_cancel(cancellation.clone()) => {
                    return Err(ResourceWaitCancelled { resource }.into());
                },
            }
        } else {
            return Ok(leases);
        }
    }
}

async fn wait_for_cancel(mut cancellation: Option<tokio::sync::watch::Receiver<bool>>) {
    let Some(receiver) = cancellation.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

fn try_acquire(dir: &Path, resource: &Resource, task: &str, root: &Path) -> Result<Option<Lease>> {
    for id in 0..resource.capacity {
        let path = dir.join(format!("{id}.lease"));
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                write!(
                    file,
                    "pid={}\ntask={}\nworktree={}\nacquired_at={}\n",
                    std::process::id(),
                    task,
                    root.display(),
                    now_ms()
                )?;
                file.flush()?;
                inherit_lease_into_task_group(&file)?;
                return Ok(Some(Lease {
                    _file: file,
                    id,
                    env: resource.env.clone(),
                }));
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || matches!(error.raw_os_error(), Some(libc::EAGAIN)) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

/// Keep the kernel lock alive in the spawned task process group. If the
/// orchestrating CLI is killed abruptly, descendants retain the lease until
/// they exit instead of allowing the same scarce resource to be allocated
/// twice. Normal completion and process-group cancellation close the inherited
/// descriptor and release it automatically.
#[cfg(unix)]
fn inherit_lease_into_task_group(file: &File) -> std::io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: fcntl only reads and updates descriptor flags for this valid,
    // open file descriptor. Clearing FD_CLOEXEC is intentional here.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn inherit_lease_into_task_group(_file: &File) -> std::io::Result<()> {
    Ok(())
}

fn resource_dir(root: &Path, name: &str, resource: &Resource) -> Result<PathBuf> {
    let runtime = devme_config::paths::runtime_dir()?.join("resources");
    Ok(match resource.scope {
        ResourceScope::Host => runtime.join("host").join(sanitize(name)),
        ResourceScope::Repo => runtime
            .join("repo")
            .join(devme_config::paths::repo_id(root))
            .join(sanitize(name)),
        ResourceScope::Worktree => runtime
            .join("worktree")
            .join(devme_config::paths::instance_id(root))
            .join(sanitize(name)),
    })
}

fn persist(root: &Path, result: &TaskResult, max: u64) -> Result<()> {
    let dir = devme_config::paths::repo_socket_dir(root)?
        .join(format!("{}-tasks", devme_config::paths::instance_id(root)));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.jsonl", sanitize(&result.task)));
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    serde_json::to_writer(&mut file, result)?;
    writeln!(file)?;
    let max = max.max(1024);
    if file.metadata()?.len() > max {
        drop(file);
        let mut bytes = Vec::new();
        File::open(&path)?.read_to_end(&mut bytes)?;
        let start = bytes.len().saturating_sub(max as usize);
        let start = bytes[start..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|p| start + p + 1)
            .unwrap_or(start);
        std::fs::write(path, &bytes[start..])?;
    }
    Ok(())
}

pub fn read_history(
    root: &Path,
    names: Option<&HashSet<String>>,
    since: Option<u64>,
) -> Result<Vec<TaskResult>> {
    let dir = devme_config::paths::repo_socket_dir(root)?
        .join(format!("{}-tasks", devme_config::paths::instance_id(root)));
    let mut results = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(results),
        Err(e) => return Err(e.into()),
    };
    for entry in entries.flatten() {
        let file = match File::open(entry.path()) {
            Ok(file) => file,
            Err(_) => continue,
        };
        for line in std::io::BufRead::lines(std::io::BufReader::new(file)).map_while(Result::ok) {
            if let Ok(result) = serde_json::from_str::<TaskResult>(&line)
                && names.is_none_or(|set| set.contains(&result.task))
                && since.is_none_or(|floor| result.finished_at >= floor)
            {
                results.push(result);
            }
        }
    }
    results.sort_by_key(|result| result.finished_at);
    Ok(results)
}

fn redaction_values(
    stack: &Stack,
    task: &Task,
    ctx: &devme_config::InterpContext,
    injected_env: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut values = task
        .env
        .iter()
        .filter(|(key, value)| devme_config::is_sensitive_key(key) && value.len() >= 4)
        .filter_map(|(_, value)| devme_config::interpolate(value, ctx).ok())
        .chain(
            injected_env
                .iter()
                .filter(|(key, value)| devme_config::is_sensitive_key(key) && value.len() >= 4)
                .map(|(_, value)| value.clone()),
        )
        .collect::<Vec<_>>();
    for name in stack
        .env
        .keys()
        .filter(|name| devme_config::is_sensitive_key(name))
    {
        if let Ok(value) = std::env::var(name)
            && value.len() >= 4
        {
            values.push(value);
        }
    }
    values.sort();
    values.dedup();
    values
}

fn redact(bytes: &[u8], literals: &[String], redactor: &devme_config::Redactor) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    for value in literals {
        if !value.is_empty() {
            text = text.replace(value, "[REDACTED]");
        }
    }
    redactor.apply(&text)
}

fn retention_bytes(stack: &Stack) -> u64 {
    stack
        .logs
        .as_ref()
        .map(|policy| policy.retention_bytes)
        .unwrap_or(8 * 1024 * 1024)
}

fn failed_result(
    name: &str,
    message: &str,
    exit_code: i32,
    cancelled: bool,
    started_at: u64,
    duration_ms: u64,
) -> TaskResult {
    let finished_at = now_ms();
    TaskResult {
        task: name.into(),
        status: if cancelled {
            "cancelled".into()
        } else {
            "failed".into()
        },
        exit_code,
        started_at,
        finished_at,
        duration_ms,
        timed_out: false,
        cancelled,
        stdout: String::new(),
        stderr: message.into(),
        truncated: false,
        output_events: vec![TaskOutputEvent {
            ts: finished_at,
            stream: devme_core::LogStream::Stderr,
            text: message.into(),
        }],
    }
}

fn emit_result(result: &TaskResult, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Human => devme_ui::info(format!(
            "task {} {} in {}ms",
            result.task, result.status, result.duration_ms
        )),
        OutputFormat::Json => {
            let mut value = serde_json::to_value(result)?;
            let object = value
                .as_object_mut()
                .expect("task result serializes as an object");
            // Per-line events are an internal persistence detail. Keep the
            // established run-result schema compact and compatible.
            object.remove("output_events");
            object.insert("schema_version".into(), serde_json::json!(1));
            devme_ui::json(&value);
        }
        OutputFormat::Toon => {
            print!(
                "result:\n  task: {}\n  status: {}\n  exit_code: {}\n  duration_ms: {}\n  timed_out: {}\n  cancelled: {}\n  truncated: {}\n  stdout: {}\n  stderr: {}",
                toon_string(&result.task),
                result.status,
                result.exit_code,
                result.duration_ms,
                result.timed_out,
                result.cancelled,
                result.truncated,
                toon_string(&result.stdout),
                toon_string(&result.stderr)
            );
        }
    }
    Ok(())
}

fn empty_result(name: &str) -> TaskResult {
    let now = now_ms();
    TaskResult {
        task: name.into(),
        status: "passed".into(),
        exit_code: 0,
        started_at: now,
        finished_at: now,
        duration_ms: 0,
        timed_out: false,
        cancelled: false,
        stdout: String::new(),
        stderr: String::new(),
        truncated: false,
        output_events: Vec::new(),
    }
}
fn unknown_task(stack: &Stack, name: &str) -> anyhow::Error {
    UnknownTask {
        name: name.to_string(),
        available: stack.task.keys().cloned().collect(),
    }
    .into()
}
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn toon_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('\"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn timeout_kills_the_spawned_process_group_and_records_distinction() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("devme.toml"),
            "schema_version=1\n[task.slow]\ncmd=\"sleep 30 & wait\"\ntimeout=1\n",
        )
        .unwrap();
        let stack = load(dir.path()).unwrap();
        let result = execute(&stack, dir.path(), "slow", &[], OutputFormat::Json)
            .await
            .unwrap();
        assert_eq!(result.exit_code, 124);
        assert!(result.timed_out);
        assert!(!result.cancelled);
    }

    #[test]
    fn output_redacts_secret_values_before_bounding() {
        let text = redact(
            b"token=swordfish",
            &["swordfish".into()],
            &devme_config::Redactor::default(),
        );
        assert_eq!(text, "token=[REDACTED]");
    }

    #[tokio::test]
    async fn stream_capture_is_bounded_to_the_tail() {
        let input = tokio::io::BufReader::new(&b"0123456789"[..]);
        let capture = read_stream(
            input,
            4,
            devme_core::LogStream::Stdout,
            Vec::new(),
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(capture.text, "6789");
        assert!(capture.truncated);
    }

    #[tokio::test]
    async fn stream_capture_publishes_redacted_output_before_completion() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(64);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let capture = tokio::spawn(read_stream(
            reader,
            64,
            devme_core::LogStream::Stdout,
            vec!["secret".into()],
            vec![],
            Some(tx),
        ));
        writer.write_all(b"token=secret\n").await.unwrap();
        let event = rx.recv().await.unwrap();
        assert_eq!(event.text, "token=[REDACTED]\n");
        drop(writer);
        capture.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn tui_cancellation_token_uses_the_same_process_group_semantics() {
        let root = tempfile::tempdir().unwrap();
        let stack = Stack::parse("schema_version=1\n[task.slow]\ncmd=\"sleep 30\"\n").unwrap();
        let (updates, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel, cancellation) = tokio::sync::watch::channel(false);
        let run = tokio::spawn(async move {
            execute_streaming(
                &stack,
                root.path(),
                "slow",
                &[],
                updates,
                Some(cancellation),
            )
            .await
            .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.send(true).unwrap();
        let result = run.await.unwrap();
        assert_eq!(result.exit_code, 130);
        assert!(result.cancelled);
    }

    #[test]
    fn task_history_retention_keeps_a_parseable_bounded_tail() {
        let dir = TempDir::new().unwrap();
        for index in 0..20 {
            let mut result = empty_result("check");
            result.finished_at += index;
            result.stdout = "x".repeat(300);
            persist(dir.path(), &result, 1024).unwrap();
        }
        let history = read_history(dir.path(), None, None).unwrap();
        assert!(!history.is_empty());
        assert_eq!(history.last().unwrap().task, "check");
        let history_dir = devme_config::paths::repo_socket_dir(dir.path())
            .unwrap()
            .join(format!(
                "{}-tasks",
                devme_config::paths::instance_id(dir.path())
            ));
        assert!(
            std::fs::metadata(history_dir.join("check.jsonl"))
                .unwrap()
                .len()
                <= 1024
        );
    }

    #[test]
    fn host_resource_contends_across_two_worktrees_and_recovers_after_release() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let resource = Resource {
            capacity: 1,
            scope: ResourceScope::Host,
            env: Some("DEVICE_SLOT".into()),
        };
        let name = format!(
            "device-fixture-{}-{}",
            std::process::id(),
            a.path()
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("temp")
        );
        let dir_a = resource_dir(a.path(), &name, &resource).unwrap();
        let dir_b = resource_dir(b.path(), &name, &resource).unwrap();
        assert_eq!(dir_a, dir_b);
        std::fs::create_dir_all(&dir_a).unwrap();
        let first = try_acquire(&dir_a, &resource, "worktree-a", a.path())
            .unwrap()
            .unwrap();
        assert!(
            try_acquire(&dir_b, &resource, "worktree-b", b.path())
                .unwrap()
                .is_none()
        );
        drop(first);
        let second = try_acquire(&dir_b, &resource, "worktree-b", b.path())
            .unwrap()
            .unwrap();
        assert_eq!(second.id, 0);
    }
}
