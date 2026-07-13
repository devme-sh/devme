//! One-shot task execution, result history, and generic scarce-resource leases.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use devme_config::{Resource, ResourceScope, Stack, Task};
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
}

pub fn load(cwd: &Path) -> Result<Stack> {
    let path = cwd.join("devme.toml");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    Stack::parse(&text).with_context(|| format!("invalid {}", path.display()))
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
                    print!(
                        "task:\n  name: {}\n  command: {}\n  dependencies: {}\n  services: {}\n  resources: {}\n  timeout_seconds: {}",
                        toon_string(&task),
                        toon_string(value.cmd.as_deref().unwrap_or("")),
                        toon_array(&value.depends_on),
                        toon_array(&value.services),
                        toon_array(&value.resources),
                        value.timeout
                    );
                }
                OutputFormat::Human => println!("{}", toml::to_string_pretty(value)?),
            }
        }
        None => match format {
            OutputFormat::Json => {
                let rows: Vec<_> = stack.task.iter().map(|(name, task)| serde_json::json!({
                        "name": name, "description": task.description, "has_command": task.cmd.is_some()
                    })).collect();
                devme_ui::json(&serde_json::json!({
                    "schema_version": 1,
                    "count": rows.len(),
                    "tasks": rows,
                }));
            }
            OutputFormat::Toon => {
                let mut output = format!(
                    "count: {}\ntasks[{}]{{name,description,kind}}:",
                    stack.task.len(),
                    stack.task.len()
                );
                for (name, task) in &stack.task {
                    let kind = if task.cmd.is_some() {
                        "command"
                    } else {
                        "aggregate"
                    };
                    output.push_str(&format!(
                        "\n  {},{},{}",
                        toon_string(name),
                        toon_string(task.description.as_deref().unwrap_or("")),
                        kind
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
    let order = execution_order(stack, name)?;
    let slot = SlotClaim::acquire(root)?;
    let mut final_result = None;
    for current in order {
        let pass = if current == name { args } else { &[] };
        let result = execute_one(stack, root, current, pass, format, slot.value).await?;
        let failed = result.exit_code != 0;
        final_result = Some(result);
        if failed {
            break;
        }
    }
    let result = final_result.expect("execution order is never empty");
    emit_result(&result, format)?;
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

async fn execute_one(
    stack: &Stack,
    root: &Path,
    name: &str,
    args: &[String],
    format: OutputFormat,
    slot: u8,
) -> Result<TaskResult> {
    let task = &stack.task[name];
    let retention = stack
        .logs
        .as_ref()
        .map(|policy| policy.retention_bytes)
        .unwrap_or(8 * 1024 * 1024);
    let capture_limit = CAPTURE_LIMIT.min((retention / 4).max(256) as usize);
    let redactor = devme_config::Redactor::new(
        &stack
            .logs
            .as_ref()
            .map(|policy| policy.redact.clone())
            .unwrap_or_default(),
    )
    .context("invalid logs.redact pattern")?;
    for step in &task.steps {
        let configured = stack
            .step
            .get(step)
            .ok_or_else(|| anyhow!("task {name:?} requires unknown step {step:?}"))?;
        let status = std::process::Command::new("sh")
            .args(["-c", &configured.check])
            .current_dir(root)
            .status()?;
        if !status.success() {
            bail!("required step {step:?} is not satisfied; run `devme up` to provision it");
        }
    }
    if task.cmd.is_none() {
        let result = empty_result(name);
        persist(root, &result, retention)?;
        return Ok(result);
    }

    let leases = acquire_resources(stack, root, name, &task.resources).await?;
    let started_at = now_ms();
    let started = std::time::Instant::now();
    let ctx = interpolation_context(root, slot);
    let secret_values = redaction_values(task, &ctx);
    let cwd = match &task.cwd {
        Some(value) => root.join(devme_config::interpolate(value, &ctx)?),
        None => root.to_path_buf(),
    };
    let mut command = tokio::process::Command::new("sh");
    let full_command = append_args(
        &devme_config::interpolate(task.cmd.as_deref().unwrap(), &ctx)?,
        args,
    );
    command
        .args(["-c", &full_command])
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in &task.env {
        command.env(key, devme_config::interpolate(value, &ctx)?);
    }
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
    let out_reader = tokio::spawn(read_tail(stdout, capture_limit));
    let err_reader = tokio::spawn(read_tail(stderr, capture_limit));

    let deadline = if task.timeout == 0 {
        Duration::from_secs(365 * 24 * 3600)
    } else {
        Duration::from_secs(task.timeout)
    };
    let (status, timed_out, cancelled) = tokio::select! {
        value = child.wait() => (value?, false, false),
        _ = tokio::time::sleep(deadline) => { terminate_group(pid, &mut child).await?; (child.wait().await?, true, false) },
        _ = tokio::signal::ctrl_c() => { terminate_group(pid, &mut child).await?; (child.wait().await?, false, true) },
    };
    let (raw_out, out_cut) = out_reader.await??;
    let (raw_err, err_cut) = err_reader.await??;
    let stdout = redact(&raw_out, &secret_values, &redactor);
    let stderr = redact(&raw_err, &secret_values, &redactor);
    if format == OutputFormat::Human {
        print!("{stdout}");
        eprint!("{stderr}");
    }
    let exit_code = if timed_out {
        124
    } else if cancelled {
        130
    } else {
        status.code().unwrap_or(1)
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
        stdout,
        stderr,
        truncated: out_cut || err_cut,
    };
    persist(root, &result, retention)?;
    Ok(result)
}

async fn read_tail<R>(mut reader: R, limit: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(limit);
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if output.len() + read > limit {
            truncated = true;
            let overflow = output.len() + read - limit;
            if overflow >= output.len() {
                output.clear();
            } else {
                output.drain(..overflow);
            }
        }
        let start = read.saturating_sub(limit);
        output.extend_from_slice(&buffer[start..read]);
    }
    Ok((output, truncated))
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

async fn acquire_resources(
    stack: &Stack,
    root: &Path,
    task: &str,
    names: &[String],
) -> Result<Vec<Lease>> {
    let mut ordered = names.to_vec();
    ordered.sort();
    let mut leases = Vec::new();
    for name in ordered {
        let resource = stack
            .resource
            .get(&name)
            .ok_or_else(|| anyhow!("task {task:?} requires unknown resource {name:?}"))?;
        if resource.capacity == 0 {
            bail!("resource {name:?} capacity must be at least 1");
        }
        let dir = resource_dir(root, &name, resource)?;
        std::fs::create_dir_all(&dir)?;
        let mut announced = false;
        loop {
            if let Some(lease) = try_acquire(&dir, resource, task)? {
                leases.push(lease);
                break;
            }
            if !announced {
                devme_ui::info(format!("task {task}: waiting for resource {name}"));
                announced = true;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                _ = tokio::signal::ctrl_c() => bail!("cancelled while waiting for resource {name:?}"),
            }
        }
    }
    Ok(leases)
}

fn try_acquire(dir: &Path, resource: &Resource, task: &str) -> Result<Option<Lease>> {
    for id in 0..resource.capacity {
        let path = dir.join(format!("{id}.lease"));
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        if file.try_lock_exclusive().is_ok() {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            write!(
                file,
                "pid={}\ntask={}\nacquired_at={}\n",
                std::process::id(),
                task,
                now_ms()
            )?;
            file.flush()?;
            return Ok(Some(Lease {
                _file: file,
                id,
                env: resource.env.clone(),
            }));
        }
    }
    Ok(None)
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

fn redaction_values(task: &Task, ctx: &devme_config::InterpContext) -> Vec<String> {
    task.env
        .iter()
        .filter(|(k, v)| {
            v.len() >= 4
                && ["SECRET", "TOKEN", "PASSWORD", "KEY", "CREDENTIAL"]
                    .iter()
                    .any(|needle| k.to_ascii_uppercase().contains(needle))
        })
        .filter_map(|(_, value)| devme_config::interpolate(value, ctx).ok())
        .collect()
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

fn emit_result(result: &TaskResult, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Human => devme_ui::info(format!(
            "task {} {} in {}ms",
            result.task, result.status, result.duration_ms
        )),
        OutputFormat::Json => {
            let mut value = serde_json::to_value(result)?;
            value
                .as_object_mut()
                .expect("task result serializes as an object")
                .insert("schema_version".into(), serde_json::json!(1));
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
fn toon_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|v| toon_string(v))
            .collect::<Vec<_>>()
            .join(",")
    )
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
        let (bytes, truncated) = read_tail(input, 4).await.unwrap();
        assert_eq!(bytes, b"6789");
        assert!(truncated);
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
        let name = format!("device-fixture-{}", std::process::id());
        let dir_a = resource_dir(a.path(), &name, &resource).unwrap();
        let dir_b = resource_dir(b.path(), &name, &resource).unwrap();
        assert_eq!(dir_a, dir_b);
        std::fs::create_dir_all(&dir_a).unwrap();
        let first = try_acquire(&dir_a, &resource, "worktree-a")
            .unwrap()
            .unwrap();
        assert!(
            try_acquire(&dir_b, &resource, "worktree-b")
                .unwrap()
                .is_none()
        );
        drop(first);
        let second = try_acquire(&dir_b, &resource, "worktree-b")
            .unwrap()
            .unwrap();
        assert_eq!(second.id, 0);
    }
}
