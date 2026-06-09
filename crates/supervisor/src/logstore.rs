//! On-disk spill for service logs — the *history* tier that complements the
//! in-memory ring's *live* tier.
//!
//! The ring (see [`crate::daemon`]) stays the fast path for streaming and
//! replay; this module tees every line to an append-only file so history
//! survives ring eviction *and* a daemon restart. A chatty service can blow the
//! 2 000-line ring in seconds and lose the one line that explains a failure —
//! the disk file keeps it.
//!
//! Findability is bounded, not unbounded: each service has one active file plus
//! one rotated file, size-capped, so a crash-looping service printing a stack
//! trace 100×/s can never fill the disk. The cost of that bound is that the
//! oldest history eventually rotates away — which the CLI surfaces as an
//! explicit truncation marker rather than pretending it had everything.
//!
//! Format is JSON-lines (`{"ts":…,"stream":"stdout","text":"…"}`), one record
//! per line, so `--since` can seek by timestamp and `--json` can stream records
//! straight to `jq`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use devme_core::LogStream;
use serde::{Deserialize, Serialize};

/// Rotate the active file once it reaches this many bytes. Active + one rotated
/// file gives a hard ceiling of ~2× this per service.
const DEFAULT_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// One persisted log line. Matches the `--json` record shape the CLI emits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    pub ts: u64,
    pub stream: LogStream,
    pub text: String,
}

/// Per-instance store: owns a directory of `<service>.log` files and a writer
/// per service. Cheap to construct; writers open lazily on first append.
pub struct LogStore {
    dir: PathBuf,
    rotate_bytes: u64,
    writers: HashMap<String, Writer>,
}

impl LogStore {
    /// Open (creating the directory) a store rooted at `dir`.
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            rotate_bytes: DEFAULT_ROTATE_BYTES,
            writers: HashMap::new(),
        }
    }

    /// Test/tuning hook: set a smaller rotation threshold.
    pub fn with_rotate_bytes(mut self, bytes: u64) -> Self {
        self.rotate_bytes = bytes;
        self
    }

    /// Append one line for `service`. Best-effort: I/O errors are dropped on the
    /// floor rather than killing the daemon's event loop — the ring is still
    /// authoritative for live streaming.
    pub fn append(&mut self, service: &str, ts: u64, stream: LogStream, text: &str) {
        let dir = self.dir.clone();
        let rotate_bytes = self.rotate_bytes;
        let writer = self
            .writers
            .entry(service.to_string())
            .or_insert_with(|| Writer::open(&dir, service, rotate_bytes));
        writer.append(ts, stream, text);
    }

    /// Read persisted records for `service`, oldest-first. Applies `since`
    /// (keep `ts >= since`) then `tail` (keep at most the last N). Returns
    /// `(records, truncated)` where `truncated` is true when older history was
    /// rotated away below the requested window — the caller surfaces that as a
    /// marker so an agent never mistakes a clipped window for the whole story.
    pub fn read(
        &self,
        service: &str,
        since: Option<u64>,
        tail: Option<usize>,
    ) -> (Vec<LogRecord>, bool) {
        let active = active_path(&self.dir, service);
        let rotated = rotated_path(&self.dir, service);

        let mut all: Vec<LogRecord> = Vec::new();
        // Rotated file holds the older half; read it first.
        for path in [rotated, active] {
            read_into(&path, &mut all);
        }

        // Anything on disk at all? Track the earliest ts before filtering so we
        // can tell whether `since` reached past the start of what we still hold.
        let earliest = all.first().map(|r| r.ts);

        if let Some(since) = since {
            all.retain(|r| r.ts >= since);
        }

        // `since` asked for history older than the oldest record we still keep.
        let truncated_by_since = match (since, earliest) {
            (Some(since), Some(earliest)) => since < earliest,
            _ => false,
        };

        let mut truncated_by_tail = false;
        if let Some(tail) = tail {
            if all.len() > tail {
                truncated_by_tail = true;
                all.drain(0..all.len() - tail);
            }
        }

        (all, truncated_by_since || truncated_by_tail)
    }

    /// Remove every file this store wrote, then forget its writers. Called on
    /// graceful shutdown / teardown so a worktree's logs don't outlive it.
    pub fn purge(&mut self) {
        self.writers.clear();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `<dir>/<service>.log`.
fn active_path(dir: &Path, service: &str) -> PathBuf {
    dir.join(format!("{}.log", sanitize(service)))
}

/// `<dir>/<service>.log.1` — the single rotated generation.
fn rotated_path(dir: &Path, service: &str) -> PathBuf {
    dir.join(format!("{}.log.1", sanitize(service)))
}

/// Service names are TOML table keys (usually plain identifiers), but be defensive
/// about path separators so a name can't escape the log directory.
fn sanitize(service: &str) -> String {
    service
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

/// Parse a JSONL log file into `out`, skipping unparseable lines. A missing
/// file is not an error — the service may simply have produced no output.
fn read_into(path: &Path, out: &mut Vec<LogRecord>) {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<LogRecord>(&line) {
            out.push(rec);
        }
    }
}

struct Writer {
    file: Option<File>,
    bytes: u64,
    rotate_bytes: u64,
    active: PathBuf,
    rotated: PathBuf,
}

impl Writer {
    fn open(dir: &Path, service: &str, rotate_bytes: u64) -> Self {
        let active = active_path(dir, service);
        let rotated = rotated_path(dir, service);
        let (file, bytes) = match OpenOptions::new().create(true).append(true).open(&active) {
            Ok(f) => {
                let bytes = f.metadata().map(|m| m.len()).unwrap_or(0);
                (Some(f), bytes)
            }
            Err(_) => (None, 0),
        };
        Self {
            file,
            bytes,
            rotate_bytes,
            active,
            rotated,
        }
    }

    fn append(&mut self, ts: u64, stream: LogStream, text: &str) {
        let rec = LogRecord {
            ts,
            stream,
            text: text.to_string(),
        };
        let mut line = match serde_json::to_string(&rec) {
            Ok(s) => s,
            Err(_) => return,
        };
        line.push('\n');
        if let Some(file) = self.file.as_mut() {
            if file.write_all(line.as_bytes()).is_ok() {
                self.bytes += line.len() as u64;
            }
        }
        if self.bytes >= self.rotate_bytes {
            self.rotate();
        }
    }

    /// Active → rotated (replacing the previous rotated generation), then reopen
    /// a fresh active file. On any failure we keep writing to the existing file
    /// rather than lose the stream.
    fn rotate(&mut self) {
        self.file = None; // close before rename
        if std::fs::rename(&self.active, &self.rotated).is_err() {
            // Couldn't rotate — reopen the original and carry on.
            self.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.active)
                .ok();
            return;
        }
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.active)
        {
            Ok(f) => {
                self.file = Some(f);
                self.bytes = 0;
            }
            Err(_) => self.file = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store(dir: &TempDir) -> LogStore {
        LogStore::new(dir.path().join("logs"))
    }

    #[test]
    fn append_then_read_round_trips_in_order() {
        let dir = TempDir::new().unwrap();
        let mut s = store(&dir);
        s.append("web", 1, LogStream::Stdout, "first");
        s.append("web", 2, LogStream::Stderr, "boom");
        s.append("web", 3, LogStream::Stdout, "third");

        let (recs, truncated) = s.read("web", None, None);
        assert!(!truncated);
        assert_eq!(recs.len(), 3);
        assert_eq!(
            recs[0],
            LogRecord {
                ts: 1,
                stream: LogStream::Stdout,
                text: "first".into()
            }
        );
        assert_eq!(recs[1].stream, LogStream::Stderr);
        assert_eq!(recs[2].text, "third");
    }

    #[test]
    fn read_survives_a_fresh_store_over_the_same_dir() {
        // Simulates a daemon restart: a new LogStore on the same directory must
        // still see what a previous process wrote.
        let dir = TempDir::new().unwrap();
        {
            let mut s = store(&dir);
            s.append("api", 10, LogStream::Stdout, "before restart");
        }
        let s2 = store(&dir);
        let (recs, _) = s2.read("api", None, None);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].text, "before restart");
    }

    #[test]
    fn since_filters_by_timestamp() {
        let dir = TempDir::new().unwrap();
        let mut s = store(&dir);
        for ts in 1..=5 {
            s.append("web", ts, LogStream::Stdout, &format!("line {ts}"));
        }
        let (recs, _) = s.read("web", Some(3), None);
        assert_eq!(recs.iter().map(|r| r.ts).collect::<Vec<_>>(), vec![3, 4, 5]);
    }

    #[test]
    fn tail_keeps_only_the_last_n_and_flags_truncation() {
        let dir = TempDir::new().unwrap();
        let mut s = store(&dir);
        for ts in 1..=10 {
            s.append("web", ts, LogStream::Stdout, &format!("line {ts}"));
        }
        let (recs, truncated) = s.read("web", None, Some(3));
        assert!(truncated, "dropping 7 of 10 lines should flag truncation");
        assert_eq!(
            recs.iter().map(|r| r.ts).collect::<Vec<_>>(),
            vec![8, 9, 10]
        );
    }

    #[test]
    fn since_past_the_start_flags_truncation() {
        // `since` predates the oldest record we still hold → the window is clipped.
        let dir = TempDir::new().unwrap();
        let mut s = store(&dir);
        s.append("web", 100, LogStream::Stdout, "only line");
        let (recs, truncated) = s.read("web", Some(50), None);
        assert_eq!(recs.len(), 1);
        assert!(truncated);
    }

    #[test]
    fn rotation_caps_history_at_two_files() {
        let dir = TempDir::new().unwrap();
        // Tiny threshold so a handful of lines forces several rotations.
        let mut s = LogStore::new(dir.path().join("logs")).with_rotate_bytes(64);
        for ts in 1..=200 {
            s.append("web", ts, LogStream::Stdout, &format!("line number {ts}"));
        }
        // Only active + .1 survive — never an unbounded pile of files.
        let entries: Vec<_> = std::fs::read_dir(dir.path().join("logs"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("web.log"))
            .collect();
        assert!(entries.len() <= 2, "expected ≤2 files, got {entries:?}");
        // The most recent lines are still readable; the oldest rotated away.
        let (recs, _) = s.read("web", None, None);
        assert!(recs.last().unwrap().text.contains("200"));
        assert!(
            recs.first().unwrap().ts > 1,
            "oldest lines should have rotated off"
        );
    }

    #[test]
    fn purge_removes_the_directory() {
        let dir = TempDir::new().unwrap();
        let mut s = store(&dir);
        s.append("web", 1, LogStream::Stdout, "x");
        s.purge();
        assert!(!dir.path().join("logs").exists());
    }
}
