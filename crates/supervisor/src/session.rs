//! Crash-safe, atomic resource leases held by resource-bound sessions.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use devme_config::{Resource, ResourceScope};
use fs2::FileExt;
use indexmap::IndexMap;

/// A complete set of locks for one session. Dropping this value releases all
/// locks, including after an unwinding daemon failure or normal shutdown.
#[derive(Debug)]
pub struct SessionLeases {
    files: Vec<File>,
    env: BTreeMap<String, String>,
}

impl Drop for SessionLeases {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = FileExt::unlock(file);
        }
    }
}

impl SessionLeases {
    /// Try to acquire the complete resource set atomically. A contended pool
    /// returns `Ok(None)` and releases every lock obtained during this attempt.
    pub fn try_acquire(
        resources: &IndexMap<String, Resource>,
        root: &Path,
        session: &str,
        names: &[String],
    ) -> Result<Option<Self>> {
        let runtime = devme_config::paths::runtime_dir()?.join("resources");
        Self::try_acquire_at(&runtime, resources, root, session, names)
    }

    fn try_acquire_at(
        runtime: &Path,
        resources: &IndexMap<String, Resource>,
        root: &Path,
        session: &str,
        names: &[String],
    ) -> Result<Option<Self>> {
        let mut names = names.to_vec();
        names.sort();
        names.dedup();
        let mut files = Vec::with_capacity(names.len());
        let mut env = BTreeMap::new();

        for name in names {
            let resource = resources
                .get(&name)
                .ok_or_else(|| anyhow!("session {session:?} requires unknown resource {name:?}"))?;
            if resource.capacity == 0 {
                bail!("resource {name:?} capacity must be at least 1");
            }
            let dir = resource_dir(runtime, root, &name, resource);
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("create resource lease directory {}", dir.display()))?;

            let Some((file, id)) = try_pool(&dir, resource.capacity, session, root)? else {
                // Release partial acquisition explicitly before another
                // attempt in this process. Relying on descriptor drop alone
                // can leave flock visibility racy on macOS.
                for file in &files {
                    let _ = FileExt::unlock(file);
                }
                return Ok(None);
            };
            if let Some(variable) = &resource.env {
                env.insert(variable.clone(), id.to_string());
            }
            files.push(file);
        }

        Ok(Some(Self { files, env }))
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

fn try_pool(dir: &Path, capacity: u32, session: &str, root: &Path) -> Result<Option<(File, u32)>> {
    for id in 0..capacity {
        let path = dir.join(format!("{id}.lease"));
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open resource lease {}", path.display()))?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                write!(
                    file,
                    "pid={}\nsession={}\nworktree={}\nacquired_at={}\n",
                    std::process::id(),
                    session,
                    root.display(),
                    now_ms()
                )?;
                file.flush()?;
                return Ok(Some((file, id)));
            }
            Err(error) if lock_contended(&error) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

fn lock_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || matches!(error.raw_os_error(), Some(libc::EAGAIN))
}

fn resource_dir(runtime: &Path, root: &Path, name: &str, resource: &Resource) -> PathBuf {
    match resource.scope {
        ResourceScope::Host => runtime.join("host").join(sanitize(name)),
        ResourceScope::Repo => runtime
            .join("repo")
            .join(devme_config::paths::repo_id(root))
            .join(sanitize(name)),
        ResourceScope::Worktree => runtime
            .join("worktree")
            .join(devme_config::paths::instance_id(root))
            .join(sanitize(name)),
    }
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
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

#[cfg(test)]
mod tests {
    use super::*;
    use devme_config::ResourceScope;
    use tempfile::TempDir;

    fn resource(scope: ResourceScope) -> Resource {
        Resource {
            capacity: 1,
            scope,
            env: None,
        }
    }

    #[test]
    fn contention_releases_partial_acquisition_atomically() {
        let runtime = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let mut resources = IndexMap::new();
        resources.insert("a".into(), resource(ResourceScope::Host));
        resources.insert("b".into(), resource(ResourceScope::Host));

        let held = SessionLeases::try_acquire_at(
            runtime.path(),
            &resources,
            root.path(),
            "holder",
            &["b".into()],
        )
        .unwrap()
        .unwrap();
        assert!(
            SessionLeases::try_acquire_at(
                runtime.path(),
                &resources,
                root.path(),
                "waiting",
                &["a".into(), "b".into()],
            )
            .unwrap()
            .is_none()
        );
        let a = SessionLeases::try_acquire_at(
            runtime.path(),
            &resources,
            root.path(),
            "other",
            &["a".into()],
        )
        .unwrap()
        .expect("partial resource a was released");
        drop(a);
        drop(held);
    }

    #[test]
    fn drop_releases_lock_and_allocated_id_is_exposed() {
        let runtime = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let mut resources = IndexMap::new();
        let mut device = resource(ResourceScope::Host);
        device.env = Some("DEVICE_ID".into());
        resources.insert("device".into(), device);

        let first = SessionLeases::try_acquire_at(
            runtime.path(),
            &resources,
            root.path(),
            "first",
            &["device".into()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(first.env()["DEVICE_ID"], "0");
        assert!(
            SessionLeases::try_acquire_at(
                runtime.path(),
                &resources,
                root.path(),
                "second",
                &["device".into()],
            )
            .unwrap()
            .is_none()
        );
        drop(first);
        assert!(
            SessionLeases::try_acquire_at(
                runtime.path(),
                &resources,
                root.path(),
                "second",
                &["device".into()],
            )
            .unwrap()
            .is_some()
        );
    }
}
