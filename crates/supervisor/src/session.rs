//! Crash-safe, atomic resource leases held by resource-bound sessions.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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

    /// Persist every session-owned process group beside the lock owner. A new
    /// supervisor terminates these orphans before reassigning the resource.
    pub fn record_child(&self, pid: u32) -> Result<()> {
        let identity = process_identity(pid)
            .ok_or_else(|| anyhow!("cannot read identity for session child pid {pid}"))?;
        for file in &self.files {
            let mut file = file;
            writeln!(file, "child={pid}:{identity}")?;
            file.flush()?;
            file.sync_data()?;
        }
        Ok(())
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
                if !recover_stale_owner(&mut file)? {
                    FileExt::unlock(&file)?;
                    continue;
                }
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                write!(
                    file,
                    "pid={}\nowner_start={}\nsession={}\nworktree={}\nacquired_at={}\n",
                    std::process::id(),
                    process_identity(std::process::id()).unwrap_or_else(|| "unknown".into()),
                    session,
                    root.display(),
                    now_ms()
                )?;
                file.flush()?;
                file.sync_data()?;
                return Ok(Some((file, id)));
            }
            Err(error) if lock_contended(&error) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

fn recover_stale_owner(file: &mut File) -> Result<bool> {
    file.seek(SeekFrom::Start(0))?;
    let mut metadata = String::new();
    file.read_to_string(&mut metadata)?;
    let owner = metadata
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .and_then(|value| value.parse::<u32>().ok());
    let owner_start = metadata
        .lines()
        .find_map(|line| line.strip_prefix("owner_start="));
    let children = metadata
        .lines()
        .filter_map(|line| line.strip_prefix("child="))
        .filter_map(|value| value.split_once(':'))
        .filter_map(|(pid, identity)| Some((pid.parse::<u32>().ok()?, identity.to_string())))
        .collect::<Vec<_>>();
    let owner_matches = owner
        .zip(owner_start)
        .is_some_and(|(pid, identity)| process_identity(pid).as_deref() == Some(identity));
    if owner.is_none() {
        return Ok(true);
    }
    if owner_matches {
        // The kernel lock is authoritative in normal operation, but do not
        // silently reuse a slot if unexpectedly unlocked metadata from this
        // live supervisor still identifies a live child.
        return Ok(children
            .iter()
            .all(|(pid, identity)| process_identity(*pid).as_deref() != Some(identity)));
    }
    for (pid, identity) in &children {
        if process_identity(*pid).as_deref() == Some(identity) {
            kill_process_group(*pid);
        }
    }
    for _ in 0..40 {
        if children
            .iter()
            .all(|(pid, identity)| process_identity(*pid).as_deref() != Some(identity))
        {
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(children
        .iter()
        .all(|(pid, identity)| process_identity(*pid).as_deref() != Some(identity)))
}

#[cfg(target_os = "macos")]
fn process_identity(pid: u32) -> Option<String> {
    // SAFETY: proc_pidinfo initializes the provided proc_bsdinfo buffer for a
    // live process. The return size is checked before any fields are read.
    unsafe {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let written = libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        );
        if written != size {
            return None;
        }
        let info = info.assume_init();
        Some(format!(
            "{}-{}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    // Field 22 is process start time. `after_comm` starts at field 3.
    after_comm.split_whitespace().nth(19).map(str::to_string)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_identity(_pid: u32) -> Option<String> {
    None
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    if pid == 0 {
        return;
    }
    let pid = pid as libc::pid_t;
    // SAFETY: kill only targets the recorded session process. The group form
    // is used when the process is still its own process-group leader.
    unsafe {
        if libc::getpgid(pid) == pid {
            libc::kill(-pid, libc::SIGKILL);
        } else {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

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
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    #[cfg(unix)]
    use std::process::{Command, Stdio};
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

    #[cfg(unix)]
    #[test]
    fn stale_owner_children_are_killed_before_resource_reassignment() {
        let runtime = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let device = resource(ResourceScope::Host);
        let mut resources = IndexMap::new();
        resources.insert("device".into(), device.clone());

        let mut dead_owner = Command::new("true").spawn().unwrap();
        let dead_owner_pid = dead_owner.id();
        let dead_owner_start = process_identity(dead_owner_pid).unwrap();
        dead_owner.wait().unwrap();

        let mut child = Command::new("sleep");
        child.arg("30").stdout(Stdio::null()).stderr(Stdio::null());
        // SAFETY: this pre-exec closure only creates a fresh session in the
        // child immediately before exec.
        unsafe {
            child.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = child.spawn().unwrap();
        let child_pid = child.id();
        let child_identity = process_identity(child_pid).unwrap();
        let waiter = std::thread::spawn(move || {
            let mut child = child;
            child.wait().unwrap()
        });

        let lease_dir = resource_dir(runtime.path(), root.path(), "device", &device);
        std::fs::create_dir_all(&lease_dir).unwrap();
        std::fs::write(
            lease_dir.join("0.lease"),
            format!(
                "pid={dead_owner_pid}\nowner_start={dead_owner_start}\nchild={child_pid}:{child_identity}\n"
            ),
        )
        .unwrap();

        let recovered = SessionLeases::try_acquire_at(
            runtime.path(),
            &resources,
            root.path(),
            "recovered",
            &["device".into()],
        )
        .unwrap()
        .expect("resource is reassigned after its stale child is gone");
        assert!(process_identity(child_pid).is_none());
        assert!(!waiter.join().unwrap().success());
        drop(recovered);
    }

    #[cfg(unix)]
    #[test]
    fn stale_metadata_never_kills_a_reused_process_id() {
        let runtime = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let device = resource(ResourceScope::Host);
        let mut resources = IndexMap::new();
        resources.insert("device".into(), device.clone());

        let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
        let unrelated_pid = unrelated.id();
        let lease_dir = resource_dir(runtime.path(), root.path(), "device", &device);
        std::fs::create_dir_all(&lease_dir).unwrap();
        std::fs::write(
            lease_dir.join("0.lease"),
            format!(
                "pid=999999\nowner_start=gone\nchild={unrelated_pid}:not-the-current-start-time\n"
            ),
        )
        .unwrap();

        let recovered = SessionLeases::try_acquire_at(
            runtime.path(),
            &resources,
            root.path(),
            "recovered",
            &["device".into()],
        )
        .unwrap()
        .expect("a mismatched identity is treated as an already-gone orphan");
        assert!(process_identity(unrelated_pid).is_some());
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        drop(recovered);
    }

    #[cfg(unix)]
    #[test]
    fn live_owner_with_live_child_is_not_treated_as_a_free_lease() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let child_pid = child.id();
        let child_identity = process_identity(child_pid).unwrap();
        std::fs::write(
            file.path(),
            format!(
                "pid={}\nowner_start={}\nchild={child_pid}:{child_identity}\n",
                std::process::id(),
                process_identity(std::process::id()).unwrap()
            ),
        )
        .unwrap();

        let mut lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(file.path())
            .unwrap();
        assert!(!recover_stale_owner(&mut lease).unwrap());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(recover_stale_owner(&mut lease).unwrap());
    }
}
