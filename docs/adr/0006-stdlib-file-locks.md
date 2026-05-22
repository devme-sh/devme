# ADR-0006: Stdlib file locks + sidecar pattern + PID-and-start-time staleness check

**Status**: Accepted
**Date**: 2026-05-23

## Context

Multiple devstack daemons coexist on one machine (one per worktree). At startup, each must atomically claim a numeric `slot` (0..9) so its services get unique port offsets. Slot claims must be:

- Atomic across concurrent daemon startups
- Stable across restarts (same instance_id reclaims its previous slot when possible)
- Self-healing when a daemon crashes (its slot becomes claimable again)
- Inspectable by humans (`cat slots.toml`)

The historical Rust ecosystem answer was `fs2` or `fd-lock`. As of Rust 1.89 (June 2025), `std::fs::File::lock` is stable and uses `flock(2)` under the hood — the same mechanism Cargo uses.

## Decision

Use stdlib `std::fs::File::lock` (exclusive) on a sidecar file (`slots.toml.lock`) to gate read-modify-write of the data file (`slots.toml`). Write the data file via tempfile + atomic rename so humans can `cat slots.toml` safely at any time.

Each slot record stores `(slot, instance_id, pid, start_time_epoch, claimed_at)`. PID + start-time defeats PID-reuse races: only reclaim a slot if its PID is dead **or** the live PID's start-time differs from the recorded one.

Use the `process_alive` crate (cross-platform, treats `EPERM` as alive, handles Windows correctly) for liveness checks.

Detect NFS at the state directory (`~/.local/share/devstack`) on startup. Warn loudly and refuse to coordinate rather than risk silent broken locks. Document "do not place state on NFS."

## Consequences

- No third-party file-locking crate to depend on. One less crate to track for vulnerabilities and maintenance.
- The sidecar + atomic-rename pattern keeps the data file always-valid for human inspection. No "truncate while locked" footguns.
- The PID + start-time pair prevents the classic stale-PID-record bug where a record points at a recycled PID.
- Cargo's `src/cargo/util/flock.rs` is the canonical reference if we hit edge cases.
- We commit to MSRV 1.89+ for this choice. Older Rust users can't compile devstack.

## Alternatives considered

**`fs2` crate.** Effectively unmaintained as of 2025.

**`fs4` crate.** Active fork of fs2 but redundant now that stdlib covers the use case.

**`fd-lock` crate.** Maintained but redundant; same `flock(2)` underneath.

**`fcntl(F_SETLK)` byte-range locks.** Per-(pid, inode) rather than per-fd; closing an unrelated fd accidentally drops your lock. WSL1's fcntl support is also broken. Cargo deliberately chose flock over fcntl; we follow.

**Shared→exclusive lock upgrade.** Non-atomic on both macOS and Linux. Always take exclusive directly.
