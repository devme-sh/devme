# ADR-0007: Two-tier daemon architecture (per-instance + per-repo shared-services daemon)

**Status**: Accepted (extends ADR-0003)
**Date**: 2026-05-23

## Context

ADR-0003 establishes a daemon per Stack instance. That works for `instance`-scoped Services (backend, frontend, db — each worktree gets its own). But `repo`-scoped Services (a cloud-sql-proxy shared across all worktrees of a repo) need a single owner. Three sub-problems:

1. When worktree A starts the proxy, how does worktree B discover and attach to it?
2. When A's daemon exits, what happens to the proxy? It's A's child process.
3. When the proxy's owner is hard-killed (SIGKILL, OOM, segfault), the process can't run cleanup — handoff to another instance daemon is impossible.

Handoff via "I'm leaving, take over" only works on graceful exit; SIGKILL gives the process no opportunity to run anything. Detached/reparent tricks (`setsid` so the proxy survives independently) work but make the proxy an orphan we don't really control.

## Decision

Split the daemon role into two tiers:

- **Instance daemon** — per-worktree, owns `instance`-scoped Services. Same as ADR-0003.
- **Shared-services daemon** — per-repo, owns `repo`-scoped Services. Listens at `~/.local/share/devstack/repos/<repo-hash>/shared.sock`. Spawned on demand by the first Instance daemon that needs a `repo`-scoped Service. Instance daemons attach as clients; ref-counted. Exits when the last instance daemon disconnects.

Lifecycle:

1. Worktree A's instance daemon starts. Sees `[service.proxy] scope = "repo"`. Looks for `shared.sock`. Doesn't exist → spawns shared-services daemon, waits for it, connects.
2. Shared daemon spawns the proxy as its own child (it owns the PTY). Logs streamed back to instance daemons.
3. Worktree B's instance daemon starts. `shared.sock` exists. B connects. No second proxy.
4. Worktree A's instance daemon dies (graceful or hard). Shared daemon notices the dropped connection, decrements ref count. Proxy keeps running as long as anyone is attached.
5. Last instance daemon disconnects. Shared daemon ref count = 0. Gracefully stops proxy, exits.
6. Shared daemon crashes. Instance daemons detect dropped socket. Lowest-slot instance daemon elects itself, respawns shared daemon, which starts a fresh proxy.

## Consequences

- One robust code path for all owner-exit scenarios (graceful, hard kill, crash). No "handoff via signal" complexity.
- The PTY of a `repo`-scoped Service is always owned by a process devstack controls (the shared daemon). We never depend on init reparenting tricks.
- One extra binary mode to build and ship (`devstack-shared-supervisor`). About 200-400 lines of additional code.
- Crash blast radius stays per-repo: a bug in one repo's shared daemon cannot affect another repo's Services. (A global daemon would couple unrelated repos.)
- Discovery is filesystem-based (socket file present + lock-protected claim). Same primitive as slot allocation; familiar.

## Alternatives considered

**Pure handoff on owner exit.** Doesn't survive SIGKILL/OOM. Out.

**Detached + reparented (`setsid` the Service, let init own it).** Single code path but the PTY's stdout/stderr are no longer ours to capture — would need a separate log-writer process or rely on tail-following files. More moving parts than the shared daemon, and the orphan-process taste is bad.

**Restart on every handoff.** Brief downtime for shared services, which matters when those services hold connections (cloud-sql-proxy with active queries). Out.

**One global daemon per user.** Trivial coordination but blast-radius cost. See ADR-0003 alternatives.
