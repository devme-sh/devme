//! Shared, crash-safe allocation of configured scarce Resources.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use devme_config::{Resource, ResourceScope};
use fs2::FileExt;
use indexmap::IndexMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseOwnerKind {
    Task,
    Session,
}

impl LeaseOwnerKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LeaseOwner {
    kind: LeaseOwnerKind,
    name: String,
    root: PathBuf,
}

impl LeaseOwner {
    pub fn task(name: impl Into<String>, root: &Path) -> Self {
        Self {
            kind: LeaseOwnerKind::Task,
            name: name.into(),
            root: root.to_path_buf(),
        }
    }

    pub fn session(name: impl Into<String>, root: &Path) -> Self {
        Self {
            kind: LeaseOwnerKind::Session,
            name: name.into(),
            root: root.to_path_buf(),
        }
    }
}

#[derive(Debug)]
pub struct ResourceLeases {
    files: Vec<File>,
    env: BTreeMap<String, String>,
}

#[derive(Debug)]
pub enum TryAcquire {
    Acquired(ResourceLeases),
    Blocked { resource: String },
}

impl Drop for ResourceLeases {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = FileExt::unlock(file);
        }
    }
}

impl ResourceLeases {
    pub fn try_acquire(
        resources: &IndexMap<String, Resource>,
        root: &Path,
        owner: LeaseOwner,
        names: &[String],
    ) -> Result<Option<Self>> {
        let runtime = devme_config::paths::runtime_dir()?.join("resources");
        Self::try_acquire_at(&runtime, resources, root, owner, names)
    }

    pub fn try_acquire_at(
        runtime: &Path,
        resources: &IndexMap<String, Resource>,
        root: &Path,
        owner: LeaseOwner,
        names: &[String],
    ) -> Result<Option<Self>> {
        Ok(
            match Self::try_acquire_detailed_at(runtime, resources, root, owner, names)? {
                TryAcquire::Acquired(leases) => Some(leases),
                TryAcquire::Blocked { .. } => None,
            },
        )
    }

    pub fn try_acquire_detailed(
        resources: &IndexMap<String, Resource>,
        root: &Path,
        owner: LeaseOwner,
        names: &[String],
    ) -> Result<TryAcquire> {
        let runtime = devme_config::paths::runtime_dir()?.join("resources");
        Self::try_acquire_detailed_at(&runtime, resources, root, owner, names)
    }

    pub fn try_acquire_detailed_at(
        runtime: &Path,
        resources: &IndexMap<String, Resource>,
        root: &Path,
        owner: LeaseOwner,
        names: &[String],
    ) -> Result<TryAcquire> {
        let mut names = names.to_vec();
        names.sort();
        names.dedup();
        let mut files = Vec::with_capacity(names.len());
        let mut env = BTreeMap::new();

        for name in names {
            let resource = resources.get(&name).ok_or_else(|| {
                anyhow!(
                    "{} {:?} requires unknown resource {:?}",
                    owner.kind.as_str(),
                    owner.name,
                    name
                )
            })?;
            if resource.capacity == 0 {
                bail!("resource {name:?} capacity must be at least 1");
            }
            let dir = resource_dir(runtime, root, &name, resource);
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("create resource lease directory {}", dir.display()))?;
            let Some((file, id)) = try_pool(&dir, resource.capacity, &owner)? else {
                for file in &files {
                    let _ = FileExt::unlock(file);
                }
                return Ok(TryAcquire::Blocked { resource: name });
            };
            if let Some(variable) = &resource.env {
                env.insert(variable.clone(), id.to_string());
            }
            files.push(file);
        }
        Ok(TryAcquire::Acquired(Self { files, env }))
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn record_child(&self, pid: u32) -> Result<()> {
        let identity = process_identity(pid)
            .ok_or_else(|| anyhow!("cannot read identity for resource child pid {pid}"))?;
        for file in &self.files {
            let mut file = file;
            writeln!(file, "child={pid}:{identity}")?;
            file.flush()?;
            file.sync_data()?;
        }
        Ok(())
    }

    /// Make these lease descriptors survive exec in only this child process.
    /// The parent keeps FD_CLOEXEC set, so concurrent Task or Service spawns
    /// cannot accidentally inherit another Task's Resource ownership.
    pub fn pass_to_child(&self, command: &mut std::process::Command) {
        configure_child_inheritance(command, &self.files);
    }
}

#[cfg(unix)]
fn configure_child_inheritance(command: &mut std::process::Command, files: &[File]) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let descriptors = files.iter().map(File::as_raw_fd).collect::<Vec<_>>();
    unsafe {
        command.pre_exec(move || {
            for descriptor in &descriptors {
                let flags = libc::fcntl(*descriptor, libc::F_GETFD);
                if flags == -1
                    || libc::fcntl(*descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_child_inheritance(_command: &mut std::process::Command, _files: &[File]) {}

fn try_pool(dir: &Path, capacity: u32, owner: &LeaseOwner) -> Result<Option<(File, u32)>> {
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
                    "version=1\npid={}\nowner_start={}\nkind={}\nname={}\nworktree={}\nacquired_at={}\n",
                    std::process::id(),
                    process_identity(std::process::id()).unwrap_or_else(|| "unknown".into()),
                    owner.kind.as_str(),
                    owner.name,
                    owner.root.display(),
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
pub fn process_identity(pid: u32) -> Option<String> {
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
pub fn process_identity(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    after_comm.split_whitespace().nth(19).map(str::to_string)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn process_identity(_pid: u32) -> Option<String> {
    None
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    if pid == 0 {
        return;
    }
    let pid = pid as libc::pid_t;
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
    use devme_config::{Resource, ResourceScope};
    use indexmap::IndexMap;
    #[cfg(unix)]
    use std::fs::OpenOptions;
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    #[cfg(unix)]
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    use super::{LeaseOwner, ResourceLeases, process_identity, recover_stale_owner, resource_dir};

    fn resource(scope: ResourceScope) -> Resource {
        Resource {
            capacity: 1,
            scope,
            env: Some("DEVICE_SLOT".into()),
        }
    }

    #[test]
    fn task_and_session_owners_contend_for_the_same_host_resource() {
        let runtime = TempDir::new().unwrap();
        let task_root = TempDir::new().unwrap();
        let session_root = TempDir::new().unwrap();
        let resources = IndexMap::from([("simulator".to_string(), resource(ResourceScope::Host))]);

        let task = ResourceLeases::try_acquire_at(
            runtime.path(),
            &resources,
            task_root.path(),
            LeaseOwner::task("ios-test", task_root.path()),
            &["simulator".into()],
        )
        .unwrap()
        .expect("task acquires simulator");
        assert_eq!(task.env()["DEVICE_SLOT"], "0");

        let blocked = ResourceLeases::try_acquire_at(
            runtime.path(),
            &resources,
            session_root.path(),
            LeaseOwner::session("ios", session_root.path()),
            &["simulator".into()],
        )
        .unwrap();
        assert!(blocked.is_none());

        drop(task);
        assert!(
            ResourceLeases::try_acquire_at(
                runtime.path(),
                &resources,
                session_root.path(),
                LeaseOwner::session("ios", session_root.path()),
                &["simulator".into()],
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn contention_releases_partial_acquisition_atomically() {
        let runtime = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let resources = IndexMap::from([
            ("a".to_string(), resource(ResourceScope::Host)),
            ("b".to_string(), resource(ResourceScope::Host)),
        ]);

        let held = ResourceLeases::try_acquire_at(
            runtime.path(),
            &resources,
            root.path(),
            LeaseOwner::session("holder", root.path()),
            &["b".into()],
        )
        .unwrap()
        .unwrap();
        assert!(
            ResourceLeases::try_acquire_at(
                runtime.path(),
                &resources,
                root.path(),
                LeaseOwner::task("waiting", root.path()),
                &["a".into(), "b".into()],
            )
            .unwrap()
            .is_none()
        );
        let a = ResourceLeases::try_acquire_at(
            runtime.path(),
            &resources,
            root.path(),
            LeaseOwner::task("other", root.path()),
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
        let resources = IndexMap::from([("device".to_string(), resource(ResourceScope::Host))]);

        let first = ResourceLeases::try_acquire_at(
            runtime.path(),
            &resources,
            root.path(),
            LeaseOwner::task("first", root.path()),
            &["device".into()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(first.env()["DEVICE_SLOT"], "0");
        assert!(
            ResourceLeases::try_acquire_at(
                runtime.path(),
                &resources,
                root.path(),
                LeaseOwner::session("second", root.path()),
                &["device".into()],
            )
            .unwrap()
            .is_none()
        );
        drop(first);
        assert!(
            ResourceLeases::try_acquire_at(
                runtime.path(),
                &resources,
                root.path(),
                LeaseOwner::session("second", root.path()),
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
        let resources = IndexMap::from([("device".to_string(), device.clone())]);

        let mut dead_owner = Command::new("true").spawn().unwrap();
        let dead_owner_pid = dead_owner.id();
        let dead_owner_start = process_identity(dead_owner_pid).unwrap();
        dead_owner.wait().unwrap();

        let mut child = Command::new("sleep");
        child.arg("30").stdout(Stdio::null()).stderr(Stdio::null());
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

        let recovered = ResourceLeases::try_acquire_at(
            runtime.path(),
            &resources,
            root.path(),
            LeaseOwner::task("recovered", root.path()),
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
        let resources = IndexMap::from([("device".to_string(), device.clone())]);

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

        let recovered = ResourceLeases::try_acquire_at(
            runtime.path(),
            &resources,
            root.path(),
            LeaseOwner::session("recovered", root.path()),
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
