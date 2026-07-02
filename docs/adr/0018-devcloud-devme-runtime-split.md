# ADR-0018: Split devcloud remote context from devme runtime supervision

**Status**: Accepted
**Date**: 2026-07-02
**Supersedes**: ADR-0014 (Cloud relay)

## Context

devme's original cloud relay design tried to make remote development seamless:
continuous Mutagen sync, remote stack proxying, watchdog takeover, and managed
agent session handoff lived under the devme umbrella. That was coherent for the
"close the laptop and keep working" goal, but it made devme responsible for too
many unrelated concerns.

The current workflow is more explicit. devme should be a deep module for
project runtime supervision: Stack, Step, Service, logs, status, URLs, TUI, and
runtime lifecycle. Remote project context belongs in a separate command that
can be composed with SSH, Herdr, Codex, or a shell wrapper without coupling
those tools back into the stack supervisor.

For v1, remote work should use Git as the durable sync boundary. Live file sync,
Mutagen conflict handling, Codex or Claude session transfer, and Herdr session
control are outside devme's runtime contract.

## Decision

Split the remote workflow into two tools:

- `devme` remains the stack/runtime supervisor. It owns local project services,
  setup steps, logs, status, URLs, the TUI, and future on-demand wake/sleep
  behavior.
- `devcloud` becomes the remote project context adapter. It resolves the current
  local Git repository to a canonical remote clone, verifies that the clone
  points at the same origin, and runs commands or shells over SSH in that remote
  project directory.

Do not continue the old remote-primary direction in new work. devme should not
own transparent remote proxying, Mutagen sync orchestration, Herdr attach
presets, agent session handoff, or remote URL rewriting. The old cloud relay ADR
remains as historical context, but this ADR is the active direction.

## Consequences

### Positive

- devme keeps a narrow runtime boundary and does not become a global session
  orchestrator.
- devcloud can be tested and evolved around one concern: Git-derived remote
  project context plus SSH execution.
- Herdr, Codex, and shell functions can compose the two tools without either
  tool depending on their internals.
- Git is the v1 sync boundary, which avoids hidden file flows, Mutagen conflict
  states, and local/remote divergence surprises.
- Future live sync can be added as a separate devcloud/devsync concern if a real
  need appears.

### Negative

- v1 remote work is less seamless than automatic laptop sleep takeover.
- Developers must commit, push, fetch, or otherwise use Git intentionally to
  move state between machines.
- Existing remote-primary config and habits need migration warnings instead of
  silent compatibility.
- The split introduces a second CLI surface that agents and docs must describe
  clearly.

## Migration note

Existing cloud relay and remote-primary docs describe a superseded design. New
documentation should point users to the split:

- use Git as the v1 source of truth for remote project state;
- use devcloud for project identity, remote path resolution, clone convergence,
  and SSH command execution;
- use devme for local stack/runtime supervision and service diagnostics;
- let Herdr and Codex own their own sessions, auth, and attach/resume behavior.

Old remote config should fail or warn with a migration hint when later runtime
issues remove the active remote-primary surface.

## Alternatives considered

**Keep cloud relay inside devme.** Rejected because it couples service
supervision to file sync, session transfer, URL rewriting, and terminal
orchestration. Those concerns change for different reasons.

**Keep live sync but move only session control out.** Rejected for v1 because
Mutagen conflicts and hidden file flow remain the hard operational problem.
Git is enough for the initial VPS workflow.

**Make devcloud a separate repository immediately.** Rejected for v1. Keeping it
in this workspace lets the split share release infrastructure and domain
language while the interface is still settling.

**Make devme launch Herdr or Codex through presets.** Rejected because Herdr and
Codex already own their session models. devcloud can print names and paths that
shell functions consume without embedding either tool in devme.
