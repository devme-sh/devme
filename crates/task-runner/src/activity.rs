use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{TaskOutputEvent, TaskResult};

const ACTIVITY_SCHEMA_VERSION: u32 = 1;
const MAX_ACTIVITY_FILES: usize = 256;
const MAX_MESSAGE_CHARS: usize = 240;
const OUTPUT_PERSIST_INTERVAL: Duration = Duration::from_millis(100);
static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskActivityOutcome {
    Succeeded,
    Cancelled,
    Interrupted,
    TimedOut,
    Failed,
}

impl TaskActivityOutcome {
    fn from_status(status: &str) -> Self {
        match status {
            "passed" => Self::Succeeded,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            "timed_out" => Self::TimedOut,
            _ => Self::Failed,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
            Self::TimedOut => "timed out",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskActivityState {
    Running {
        message: String,
    },
    Finished {
        outcome: TaskActivityOutcome,
        finished_at: u64,
        duration_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskActivity {
    pub schema_version: u32,
    pub run_id: String,
    pub instance_id: String,
    pub cwd: String,
    pub task: String,
    pub owner_pid: u32,
    pub owner_identity: Option<String>,
    pub started_at: u64,
    pub updated_at: u64,
    pub revision: u64,
    pub state: TaskActivityState,
}

impl TaskActivity {
    pub fn is_running(&self) -> bool {
        matches!(self.state, TaskActivityState::Running { .. })
    }

    pub fn message(&self) -> &str {
        match &self.state {
            TaskActivityState::Running { message } => message,
            TaskActivityState::Finished { outcome, .. } => outcome.label(),
        }
    }

    pub fn outcome(&self) -> Option<TaskActivityOutcome> {
        match self.state {
            TaskActivityState::Running { .. } => None,
            TaskActivityState::Finished { outcome, .. } => Some(outcome),
        }
    }

    pub fn finished_at(&self) -> Option<u64> {
        match self.state {
            TaskActivityState::Running { .. } => None,
            TaskActivityState::Finished { finished_at, .. } => Some(finished_at),
        }
    }

    pub fn owner_is_live(&self) -> bool {
        if !self.is_running() {
            return false;
        }
        self.owner_identity.as_deref().is_none_or(|identity| {
            devme_resource_lease::process_identity(self.owner_pid).as_deref() == Some(identity)
        })
    }
}

#[derive(Clone)]
pub(crate) struct TaskActivityWriter {
    path: Option<PathBuf>,
    activity: Arc<Mutex<TaskActivity>>,
    last_output_persisted: Arc<Mutex<Option<Instant>>>,
}

impl TaskActivityWriter {
    pub(crate) fn start(root: &Path, task: &str) -> Self {
        let now = now_ms();
        let pid = std::process::id();
        let sequence = NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("{now}-{pid}-{sequence}");
        let activity = TaskActivity {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            run_id: run_id.clone(),
            instance_id: devme_config::paths::instance_id(root),
            cwd: fs::canonicalize(root)
                .unwrap_or_else(|_| root.to_path_buf())
                .to_string_lossy()
                .into_owned(),
            task: task.to_string(),
            owner_pid: pid,
            owner_identity: devme_resource_lease::process_identity(pid),
            started_at: now,
            updated_at: now,
            revision: 0,
            state: TaskActivityState::Running {
                message: format!("Preparing {task}"),
            },
        };
        let path = task_activity_dir(root)
            .and_then(|dir| {
                fs::create_dir_all(&dir)?;
                cleanup_activity_dir(&dir);
                Ok(dir.join(format!("{run_id}.json")))
            })
            .ok();
        let writer = Self {
            path,
            activity: Arc::new(Mutex::new(activity)),
            last_output_persisted: Arc::new(Mutex::new(None)),
        };
        writer.persist();
        writer
    }

    pub(crate) fn progress(&self, message: &str) {
        self.update(|activity| {
            if let TaskActivityState::Running {
                message: activity_message,
            } = &mut activity.state
            {
                *activity_message = compact_message(message);
            }
        });
    }

    pub(crate) fn output(&self, event: &TaskOutputEvent) {
        let now = Instant::now();
        let should_persist = self.last_output_persisted.lock().map_or(true, |mut last| {
            if last.is_some_and(|last| now.duration_since(last) < OUTPUT_PERSIST_INTERVAL) {
                false
            } else {
                *last = Some(now);
                true
            }
        });
        if should_persist {
            self.progress(&event.text);
        }
    }

    pub(crate) fn finish(&self, result: &Result<TaskResult>) {
        self.update(|activity| {
            let now = now_ms();
            let (outcome, finished_at, duration_ms) = result.as_ref().map_or_else(
                |_| {
                    (
                        TaskActivityOutcome::Failed,
                        now,
                        now.saturating_sub(activity.started_at),
                    )
                },
                |result| {
                    (
                        TaskActivityOutcome::from_status(&result.status),
                        result.finished_at,
                        result.duration_ms,
                    )
                },
            );
            activity.state = TaskActivityState::Finished {
                outcome,
                finished_at,
                duration_ms,
            };
        });
    }

    fn update(&self, mutate: impl FnOnce(&mut TaskActivity)) {
        if let Ok(mut activity) = self.activity.lock() {
            mutate(&mut activity);
            activity.revision = activity.revision.saturating_add(1);
            activity.updated_at = now_ms();
        }
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(activity) = self.activity.lock() else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec(&*activity) else {
            return;
        };
        let temp = path.with_extension(format!("tmp-{}-{}", std::process::id(), activity.revision));
        if fs::write(&temp, bytes).is_ok() {
            let _ = fs::rename(&temp, path);
        }
    }
}

pub fn task_activity_dir(root: &Path) -> Result<PathBuf> {
    Ok(devme_config::paths::repo_socket_dir(root)?.join(format!(
        "{}-task-activity",
        devme_config::paths::instance_id(root)
    )))
}

pub fn read_task_activities(root: &Path) -> Result<Vec<TaskActivity>> {
    read_task_activities_from_dir(&task_activity_dir(root)?)
}

pub fn read_task_activities_from_dir(dir: &Path) -> Result<Vec<TaskActivity>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut activities = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        if let Ok(activity) = serde_json::from_slice::<TaskActivity>(&bytes) {
            activities.push(activity);
        }
    }
    activities.sort_by_key(|activity| (activity.started_at, activity.run_id.clone()));
    Ok(activities)
}

fn cleanup_activity_dir(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|value| value.to_str()) == Some("json"))
                .then(|| (entry.metadata().and_then(|meta| meta.modified()).ok(), path))
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|(modified, path)| (*modified, path.clone()));
    let remove = files
        .len()
        .saturating_sub(MAX_ACTIVITY_FILES.saturating_sub(1));
    for (_, path) in files.into_iter().take(remove) {
        let _ = fs::remove_file(path);
    }
}

fn compact_message(message: &str) -> String {
    let line = message
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(message)
        .trim();
    line.chars()
        .filter(|character| !character.is_control())
        .take(MAX_MESSAGE_CHARS)
        .collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
