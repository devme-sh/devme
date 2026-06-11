# ADR-0005: Override-aware failure model

**Status**: Accepted
**Date**: 2026-05-23

## Context

Three kinds of failure happen during a typical devme run:

1. A Step's `check` fails (the prerequisite isn't satisfied).
2. A Step's `provision` errors out (the install command failed).
3. A Service crashes after startup.

Each needs its own response. And there are real-world cases where the user knows better than devme — checks can be wrong (gcloud installed via nix; not on PATH the way our probe expects), dependencies can be temporarily unavailable but the user wants to develop anyway, services can be intentionally external.

A rigid "fail loud, halt the graph" model is safe but punishing. A permissive "skip whatever fails" model is dangerous. The right model gives the user multiple, visible, reversible escape hatches.

## Decision

Three mechanisms, each addressing a different failure case:

1. **Mark-as-installed Override.** When a Step's check fails, the TUI offers `i` (mark as installed). This persists an Override in `.devme/overrides.toml`. Future runs treat the Step as satisfied without running its check. The Override is visible in the TUI (`step.gcloud · ✓ (overridden)`) and via `devme overrides list`; cleared via `devme overrides clear <step>` or wholesale via `devme health --recheck`.

2. **Optional dependencies.** A Service's `depends_on` edge can be marked optional with the `?` suffix (`depends_on = ["db", "proxy?"]`) or `required = false`. The dependent Service starts even if the dep is in `waiting_on_dependency` state.

3. **Forced start.** A runtime override for required deps: `devme start backend --skip-deps` (CLI) or `f` in the TUI when focused on a waiting Service. The Service runs in a visibly degraded state — `running (started without proxy)` — surfaced in `devme status`, `devme errors --json`, and the TUI.

`provision` failures halt the affected subtree (not the entire graph) and show an inline retry/skip/abort overlay. Service crashes follow systemd-style policies: `restart = "on-failure"` default, exponential backoff (0.5s → 30s), crash-loop guard after 5 *consecutive rapid exits* (process died within 5s of spawn; a run that survives longer resets the streak). Counting consecutive rapid exits rather than exits-per-wall-clock-window keeps the guard effective once the backoff saturates — a fixed window shorter than threshold × max-backoff could never trip. A tripped guard parks the Service in a terminal `crash-loop` state (visible in `devme status` and the TUI, with the diagnosed reason — e.g. "port 3011 already in use by tailscaled (pid 1234)" — when the port probe can name one); `devme restart <svc>` resets the breaker.

## Consequences

- Three mechanisms means three things to document and test, but each addresses a real and different failure mode. Collapsing them would lose expressiveness.
- Overrides are visible by design — they never silently hide regressions.
- The optional/forced-start distinction (config-time vs runtime) prevents the user from having to edit checked-in config every time they want to develop offline.
- "Started in degraded state" is a first-class concept the agent and TUI can reason about, not a silent fact buried in logs.

## Alternatives considered

**Single mechanism: just allow `--skip-deps` at runtime.** Misses the case where the *check* itself is wrong, not the dep. Misses the case where the config author knows the dep is non-load-bearing. Out.

**Auto-recover with retries.** Hides real failures and burns CPU. The crash-loop guard is the only auto-recovery we ship.

**Configuration-only (`required = false` everywhere, no runtime overrides).** Forces config edits for transient developer needs (today I want to skip the proxy). Bad ergonomics.
