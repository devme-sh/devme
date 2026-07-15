# ADR-0021: TUI Home and semantic task kinds

**Status**: Accepted
**Date**: 2026-07-13

## Context

Returning developers remember `devme`, but not necessarily project-specific task names. The service dashboard exposes runtime state well, while one-shot native launch and verification tasks were only discoverable through CLI commands.

## Decision

Bare interactive `devme` opens a Home screen before the service dashboard. Home groups declarative tasks by exactly three semantic kinds: `launch`, `check`, and `utility`. Existing tasks default to `utility`, so metadata does not change execution or invalidate old configuration. Declaration order remains meaningful within each group after workspace composition.

The presentation-free `devme-task-runner` crate owns one deep interface covering setup convergence, typed approval requests, targeted service readiness, Service holds, Resource leases, dependency execution, guardian handoff, cancellation, persistence, and results. Both `devme run <task>`, Session launch Tasks, and Home invoke that interface. A Session launch uses a borrowed context that cannot widen the Session's Services or Resources. The CLI and TUI adapt typed events into their own presentation. They do not shell out to Devme or duplicate orchestration.

When a prompt-trust Step needs provisioning, Home renders the runner's typed approval request as a modal. Enter or `y` approves, `n` or `s` skips, and Escape cancels. The response returns through the runner interface rather than a terminal prompt hidden behind the TUI frame.

Cancellation is an explicit runner input. CLI Ctrl-C and TUI Esc feed the same cancellation path, which stops targeted readiness work, releases resource waits, terminates the spawned process group, persists `cancelled`, and returns the conventional exit code 130. The TUI does not signal its own process.

A successful launch is reported as "last launch succeeded". Home does not claim an app is currently running because Devme does not yet observe the launched native process after the one-shot command completes. The interface can later carry runtime-observation updates without changing the configuration vocabulary.

Bare non-interactive invocation retains the structured agent-context response and never opens the TUI.

## Consequences

- Developers get an arrow-and-Enter path to Run and Check actions, recent results, and service health.
- Keyboard and mouse selection share the same state model and renderer hit map.
- Profiles and recurring startup pickers are unnecessary.
- Runtime observation, stop, and status semantics for launched native apps remain deliberately deferred.
