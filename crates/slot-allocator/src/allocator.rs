//! The slot allocator — claim and release slots via a file-locked registry.
//!
//! See ADR-0006. One sidecar lock file (`<path>.lock`) coordinates concurrent
//! claims. The registry itself is written via tempfile + atomic rename so
//! readers never see a torn file.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use devme_core::Slot;
use tempfile::NamedTempFile;

use crate::error::AllocError;
use crate::liveness::{Liveness, SystemLiveness};
use crate::record::{ClaimRecord, Registry};

/// How many slots a freshly-constructed allocator hands out by default.
/// Slots range from 0 (inclusive) to `max_slots` (exclusive).
pub const DEFAULT_MAX_SLOTS: u8 = 10;

/// Coordinate slot ownership across worktrees on the same machine.
pub struct SlotAllocator {
    path: PathBuf,
    lock_path: PathBuf,
    max_slots: u8,
    liveness: Arc<dyn Liveness>,
    pid: u32,
}

impl SlotAllocator {
    /// Open an allocator backed by `path`. Uses `SystemLiveness` and the
    /// default slot count; chain `with_*` to override.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_path = lock_sidecar(&path);
        Self {
            path,
            lock_path,
            max_slots: DEFAULT_MAX_SLOTS,
            liveness: Arc::new(SystemLiveness),
            pid: std::process::id(),
        }
    }

    pub fn with_max_slots(mut self, n: u8) -> Self {
        assert!(n > 0 && n <= Slot::MAX + 1, "max_slots out of range");
        self.max_slots = n;
        self
    }

    pub fn with_liveness(mut self, liveness: Arc<dyn Liveness>) -> Self {
        self.liveness = liveness;
        self
    }

    /// Override the PID recorded in new claims. Tests use this to simulate
    /// claims from other processes.
    #[doc(hidden)]
    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = pid;
        self
    }

    /// Claim a slot for `instance_id`. Idempotent: calling twice from the
    /// same worktree returns the same slot without reordering.
    pub fn claim(&self, instance_id: &str) -> Result<Slot, AllocError> {
        let _guard = self.lock()?;
        let mut registry = self.read()?;

        self.sweep_stale(&mut registry);

        if let Some(existing) = registry.find_by_instance(instance_id) {
            return Ok(existing.slot);
        }

        let slot = self.first_free_slot(&registry)?;
        registry.claims.push(ClaimRecord {
            slot,
            instance_id: instance_id.to_string(),
            pid: self.pid,
            claimed_at: now_seconds(),
        });
        self.write(&registry)?;
        Ok(slot)
    }

    /// Release the slot held by `instance_id`. No-op if the instance isn't
    /// in the registry.
    pub fn release(&self, instance_id: &str) -> Result<(), AllocError> {
        let _guard = self.lock()?;
        let mut registry = self.read()?;
        if registry.remove_by_instance(instance_id).is_some() {
            self.write(&registry)?;
        }
        Ok(())
    }

    /// Read a snapshot of all current claims, after pruning stale ones.
    /// Does not write the pruned registry back — that's the next claim's job.
    pub fn list(&self) -> Result<Vec<ClaimRecord>, AllocError> {
        let _guard = self.lock()?;
        let mut registry = self.read()?;
        self.sweep_stale(&mut registry);
        Ok(registry.claims)
    }

    // --- internals ---

    fn lock(&self) -> Result<LockGuard, AllocError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| AllocError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|source| AllocError::Lock {
                path: self.lock_path.clone(),
                source,
            })?;
        file.lock().map_err(|source| AllocError::Lock {
            path: self.lock_path.clone(),
            source,
        })?;
        Ok(LockGuard { _file: file })
    }

    fn read(&self) -> Result<Registry, AllocError> {
        let bytes = match File::open(&self.path) {
            Ok(mut f) => {
                let mut s = String::new();
                f.read_to_string(&mut s).map_err(|source| AllocError::Io {
                    path: self.path.clone(),
                    source,
                })?;
                s
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(AllocError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        Registry::parse(&bytes).map_err(|source| AllocError::Corrupt {
            path: self.path.clone(),
            source,
        })
    }

    fn write(&self, registry: &Registry) -> Result<(), AllocError> {
        let s = registry.serialize()?;
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = NamedTempFile::new_in(parent).map_err(|source| AllocError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        tmp.write_all(s.as_bytes()).map_err(|source| AllocError::Io {
            path: tmp.path().to_path_buf(),
            source,
        })?;
        tmp.persist(&self.path).map_err(|e| AllocError::Io {
            path: self.path.clone(),
            source: e.error,
        })?;
        Ok(())
    }

    fn sweep_stale(&self, registry: &mut Registry) {
        registry.claims.retain(|c| self.liveness.is_alive(c.pid));
    }

    fn first_free_slot(&self, registry: &Registry) -> Result<Slot, AllocError> {
        for n in 0..self.max_slots {
            let slot = Slot::new(n).expect("max_slots clamped to valid range");
            if registry.find_by_slot(slot).is_none() {
                return Ok(slot);
            }
        }
        Err(AllocError::Exhausted { max: self.max_slots })
    }
}

fn lock_sidecar(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// RAII guard around the locked file. Lock releases when this drops.
struct LockGuard {
    _file: File,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// A liveness probe with a settable set of "dead" PIDs.
    #[derive(Default)]
    struct MockLiveness {
        dead: Mutex<Vec<u32>>,
    }

    impl MockLiveness {
        fn mark_dead(&self, pid: u32) {
            self.dead.lock().unwrap().push(pid);
        }
    }

    impl Liveness for MockLiveness {
        fn is_alive(&self, pid: u32) -> bool {
            !self.dead.lock().unwrap().contains(&pid)
        }
    }

    fn alloc(dir: &TempDir) -> (SlotAllocator, Arc<MockLiveness>) {
        let mock = Arc::new(MockLiveness::default());
        let a = SlotAllocator::open(dir.path().join("slots.toml"))
            .with_liveness(mock.clone())
            .with_pid(1000);
        (a, mock)
    }

    #[test]
    fn claim_from_empty_registry_returns_slot_zero() {
        let dir = TempDir::new().unwrap();
        let (a, _) = alloc(&dir);
        let slot = a.claim("worktree-a").unwrap();
        assert_eq!(slot, Slot::new(0).unwrap());
    }

    #[test]
    fn claim_is_idempotent_for_same_instance_id() {
        let dir = TempDir::new().unwrap();
        let (a, _) = alloc(&dir);
        let s1 = a.claim("worktree-a").unwrap();
        let s2 = a.claim("worktree-a").unwrap();
        assert_eq!(s1, s2);
    }

    #[test]
    fn claim_skips_taken_live_slots() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("slots.toml");
        let mock = Arc::new(MockLiveness::default());

        let a = SlotAllocator::open(&path).with_liveness(mock.clone()).with_pid(1000);
        let b = SlotAllocator::open(&path).with_liveness(mock.clone()).with_pid(1001);
        let c = SlotAllocator::open(&path).with_liveness(mock.clone()).with_pid(1002);

        assert_eq!(a.claim("a").unwrap(), Slot::new(0).unwrap());
        assert_eq!(b.claim("b").unwrap(), Slot::new(1).unwrap());
        assert_eq!(c.claim("c").unwrap(), Slot::new(2).unwrap());
    }

    #[test]
    fn claim_reuses_lowest_freed_slot() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("slots.toml");
        let mock = Arc::new(MockLiveness::default());

        let a = SlotAllocator::open(&path).with_liveness(mock.clone()).with_pid(1000);
        let b = SlotAllocator::open(&path).with_liveness(mock.clone()).with_pid(1001);
        let c = SlotAllocator::open(&path).with_liveness(mock.clone()).with_pid(1002);

        a.claim("a").unwrap();
        b.claim("b").unwrap();
        a.release("a").unwrap();
        // Slot 0 is now free; should get reused before slot 2.
        assert_eq!(c.claim("c").unwrap(), Slot::new(0).unwrap());
    }

    #[test]
    fn claim_reuses_slot_whose_pid_is_dead() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("slots.toml");
        let mock = Arc::new(MockLiveness::default());

        // Dead-process claim from a previous run.
        let stale = SlotAllocator::open(&path).with_liveness(mock.clone()).with_pid(9999);
        stale.claim("ghost").unwrap();
        mock.mark_dead(9999);

        let live = SlotAllocator::open(&path).with_liveness(mock.clone()).with_pid(1000);
        assert_eq!(live.claim("real").unwrap(), Slot::new(0).unwrap());
    }

    #[test]
    fn release_removes_entry() {
        let dir = TempDir::new().unwrap();
        let (a, _) = alloc(&dir);
        a.claim("worktree-a").unwrap();
        a.release("worktree-a").unwrap();
        assert!(a.list().unwrap().is_empty());
    }

    #[test]
    fn release_unknown_instance_id_is_ok() {
        let dir = TempDir::new().unwrap();
        let (a, _) = alloc(&dir);
        a.release("never-claimed").unwrap();
        assert!(a.list().unwrap().is_empty());
    }

    #[test]
    fn claim_with_no_available_slots_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("slots.toml");
        let mock = Arc::new(MockLiveness::default());

        // max=2 — only slots 0 and 1.
        for (i, name) in ["a", "b"].iter().enumerate() {
            let a = SlotAllocator::open(&path)
                .with_max_slots(2)
                .with_liveness(mock.clone())
                .with_pid(1000 + i as u32);
            a.claim(name).unwrap();
        }
        let third = SlotAllocator::open(&path)
            .with_max_slots(2)
            .with_liveness(mock.clone())
            .with_pid(1010);
        let err = third.claim("c").unwrap_err();
        assert!(matches!(err, AllocError::Exhausted { max: 2 }));
    }

    #[test]
    fn list_returns_live_claims_only() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("slots.toml");
        let mock = Arc::new(MockLiveness::default());

        let a = SlotAllocator::open(&path).with_liveness(mock.clone()).with_pid(1000);
        let b = SlotAllocator::open(&path).with_liveness(mock.clone()).with_pid(1001);

        a.claim("a").unwrap();
        b.claim("b").unwrap();
        mock.mark_dead(1000);

        let live = a.list().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].instance_id, "b");
    }

    #[test]
    fn claim_persists_through_a_fresh_allocator() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("slots.toml");
        let mock = Arc::new(MockLiveness::default());

        let first = SlotAllocator::open(&path).with_liveness(mock.clone()).with_pid(1000);
        let s = first.claim("a").unwrap();
        drop(first);

        let second = SlotAllocator::open(&path).with_liveness(mock.clone()).with_pid(1000);
        // Same instance — should be idempotent across allocator instances.
        assert_eq!(second.claim("a").unwrap(), s);
    }

    #[test]
    fn lock_sidecar_path_is_alongside_data_file() {
        let p = PathBuf::from("/some/dir/slots.toml");
        assert_eq!(lock_sidecar(&p), PathBuf::from("/some/dir/slots.toml.lock"));
    }

    /// Cross-process concurrency test. Spawned children re-enter this same
    /// test binary with `SLOT_TEST_CHILD=1` set, do exactly one claim, and
    /// print the resulting slot. If the file lock works, every child sees
    /// a different slot.
    ///
    /// We can't reliably test this across *threads* of one process: on macOS
    /// `fcntl` locks are per-process, so co-threads bypass each other. The
    /// real guarantee is cross-process, which means spawning real processes.
    #[test]
    fn concurrent_claims_across_processes_get_unique_slots() {
        use std::collections::HashSet;
        use std::process::{Command, Stdio};

        // Child mode: claim a slot, announce it, then linger so siblings
        // racing the lock still see us alive (otherwise sweep_stale would
        // reclaim our slot the moment we exit, defeating the test).
        if let Ok(path) = std::env::var("SLOT_TEST_CHILD_PATH") {
            let id = std::env::var("SLOT_TEST_CHILD_ID").unwrap();
            let a = SlotAllocator::open(path);
            let slot = a.claim(&id).unwrap();
            println!("SLOT={}", slot.as_u8());
            std::thread::sleep(std::time::Duration::from_millis(750));
            return;
        }

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("slots.toml");
        let exe = std::env::current_exe().unwrap();
        let n: usize = 5;

        let mut children = Vec::new();
        for i in 0..n {
            let c = Command::new(&exe)
                .args([
                    "--exact",
                    "allocator::tests::concurrent_claims_across_processes_get_unique_slots",
                    "--nocapture",
                ])
                .env("SLOT_TEST_CHILD_PATH", &path)
                .env("SLOT_TEST_CHILD_ID", format!("worktree-{i}"))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn child test binary");
            children.push(c);
        }

        let mut slots = Vec::new();
        for c in children {
            let out = c.wait_with_output().unwrap();
            assert!(out.status.success(), "child failed: {out:?}");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let line = stdout
                .lines()
                .find_map(|l| l.strip_prefix("SLOT="))
                .unwrap_or_else(|| panic!("no SLOT= line in child output: {stdout}"));
            slots.push(line.trim().parse::<u8>().expect("integer slot"));
        }

        let unique: HashSet<u8> = slots.iter().copied().collect();
        assert_eq!(unique.len(), n, "expected {n} unique slots, got {slots:?}");
    }
}
