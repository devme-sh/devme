# ADR-0021: TUI Home and semantic task kinds

**Status**: Accepted
**Date**: 2026-07-13

## Context

Returning developers remember `devme`, but not necessarily project-specific task names. The service dashboard exposes runtime state well, while one-shot native launch and verification tasks were only discoverable through CLI commands.

## Decision

Bare interactive `devme` opens a Home screen before the service dashboard. Home groups declarative tasks by exactly three semantic kinds: `launch`, `check`, and `utility`. Existing tasks default to `utility`, so metadata does not change execution or invalidate old configuration. Declaration order remains meaningful within each group after workspace composition.

The CLI owns one deep task-runner interface covering setup convergence, targeted service readiness, resources, dependency execution, cancellation, persistence, and results. Both `devme run <task>` and Home invoke that interface. The TUI owns selection, progress presentation, and recent-result wording only. It does not shell out to Devme or duplicate orchestration.

Cancellation is an explicit runner input. CLI Ctrl-C and TUI Esc feed the same cancellation path, which stops targeted readiness work, releases resource waits, terminates the spawned process group, persists `cancelled`, and returns the conventional exit code 130. The TUI does not signal its own process.

A successful launch is reported as "last launch succeeded". Home does not claim an app is currently running because Devme does not yet observe the launched native process after the one-shot command completes. The interface can later carry runtime-observation updates without changing the configuration vocabulary.

Bare non-interactive invocation retains the structured agent-context response and never opens the TUI.

## Consequences

- Developers get an arrow-and-Enter path to Run and Check actions, recent results, and service health.
- Keyboard and mouse selection share the same state model and renderer hit map.
- Profiles and recurring startup pickers are unnecessary.
- Runtime observation, stop, and status semantics for launched native apps remain deliberately deferred.
