use std::path::{Path, PathBuf};

use devme_task_runner::TaskActivity;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

pub struct ObservedActivity {
    pub activity: TaskActivity,
    pub initial: bool,
}

pub struct ActivityFeed {
    rx: mpsc::UnboundedReceiver<ObservedActivity>,
    _watcher: RecommendedWatcher,
}

impl ActivityFeed {
    pub fn bind(repo_dir: &Path) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        for activity in scan(repo_dir)? {
            let _ = tx.send(ObservedActivity {
                activity,
                initial: true,
            });
        }

        let (path_tx, mut path_rx) = mpsc::unbounded_channel::<PathBuf>();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event {
                for path in event.paths {
                    if is_activity_file(&path) {
                        let _ = path_tx.send(path);
                    }
                }
            }
        })
        .map_err(|error| std::io::Error::other(format!("task activity watch: {error}")))?;
        let mut watcher = watcher;
        watcher
            .watch(repo_dir, RecursiveMode::Recursive)
            .map_err(|error| std::io::Error::other(format!("task activity watch: {error}")))?;

        tokio::spawn(async move {
            while let Some(path) = path_rx.recv().await {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let Ok(bytes) = std::fs::read(path) else {
                    continue;
                };
                let Ok(activity) = serde_json::from_slice::<TaskActivity>(&bytes) else {
                    continue;
                };
                if tx
                    .send(ObservedActivity {
                        activity,
                        initial: false,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        Ok(Self {
            rx,
            _watcher: watcher,
        })
    }

    pub async fn recv(&mut self) -> Option<ObservedActivity> {
        self.rx.recv().await
    }
}

fn scan(repo_dir: &Path) -> std::io::Result<Vec<TaskActivity>> {
    let mut activities = Vec::new();
    let entries = match std::fs::read_dir(repo_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(activities),
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir()
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("-task-activity"))
        {
            continue;
        }
        activities
            .extend(devme_task_runner::read_task_activities_from_dir(&path).unwrap_or_default());
    }
    activities.sort_by_key(|activity| (activity.started_at, activity.run_id.clone()));
    Ok(activities)
}

fn is_activity_file(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("json")
        && path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("-task-activity"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_reads_only_task_activity_directories() {
        let root = tempfile::tempdir().unwrap();
        let activity_dir = root.path().join("abc-task-activity");
        std::fs::create_dir_all(&activity_dir).unwrap();
        let activity = TaskActivity {
            schema_version: 1,
            run_id: "run-1".into(),
            instance_id: "abc".into(),
            cwd: "/repo".into(),
            task: "verify".into(),
            owner_pid: 1,
            owner_identity: None,
            started_at: 1,
            updated_at: 1,
            revision: 0,
            state: devme_task_runner::TaskActivityState::Running,
            message: "Preparing verify".into(),
            status: None,
            finished_at: None,
            duration_ms: None,
        };
        std::fs::write(
            activity_dir.join("run-1.json"),
            serde_json::to_vec(&activity).unwrap(),
        )
        .unwrap();
        std::fs::write(root.path().join("ignored.json"), b"{}").unwrap();

        assert_eq!(scan(root.path()).unwrap(), vec![activity]);
    }
}
