# ADR-0003: Daemon-per-instance with ref-counted lifecycle

**Status**: Accepted (extended by ADR-0007)
**Date**: 2026-05-23

## Context

devme supervises Services across multiple git worktrees. The IPC architecture has to choose between several shapes: one process owns all services (TUI = supervisor), one daemon per machine, one daemon per worktree, or one daemon per repo. The choice determines crash blast radius, IPC topology, and how clients (TUI, CLI subcommands, agent processes) attach to running services.

We also need to decide when services live and die: should closing the TUI kill services, or should services survive?

## Decision

One daemon per **Stack instance** (one per worktree). Each daemon listens on `~/.local/share/devme/instances/<id>.sock`. Clients (TUI, CLI subcommands, agents) connect over the socket and the daemon ref-counts them.

Default behavior: when the ref count drops to zero, the daemon gracefully stops its services and exits — "the stack lives as long as someone is watching it." Two CLI commands form the explicit opt-in for sticky daemons that survive client disconnect:

- `devme up` — start a daemon with the sticky flag set; services survive until `devme down`.
- `devme` (no args) — foreground mode; attach the TUI; on TUI exit, ref count drops, daemon shuts down.

## Consequences

- Crash isolation per worktree. A panic in one daemon cannot affect another worktree's stack.
- Services follow the user's attention by default — closing the TUI cleans everything up.
- The detached mode is one flag away, not a different architecture.
- Multiple clients can attach simultaneously (TUI in one terminal + `devme logs` tailing in another + an agent calling `devme status`). All share the same daemon's view.
- `repo`-scoped services need a coordinator — see ADR-0007 (shared-services daemon).

## Alternatives considered

**One global daemon per user.** Simpler IPC (single socket), trivial coordination of `repo`-scoped services. But a single point of failure: a bug in one instance's service can wedge the whole daemon, taking down every other worktree. Rejected for blast radius.

**No daemon (TUI owns the services).** Matches mprocs. Services die when the TUI quits; `devme logs` from a second terminal can't reach them. Rejected because `devme up`/`devme logs`/agent CLI commands need a process to talk to even when the TUI isn't running.

**One daemon per machine.** Same crash-isolation problem as one daemon per user.
