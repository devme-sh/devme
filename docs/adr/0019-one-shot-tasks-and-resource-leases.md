# ADR-0019: One-shot tasks and scoped resource leases

**Status**: Accepted
**Date**: 2026-07-13

## Context

Native-mobile monorepos need one root interface for dependency convergence,
services, builds, tests, app launch, and diagnostics. Their authoritative tools
remain xcodebuild, Gradle, Bun, Vite+, and Convex. A second root task runner
duplicates command configuration and gives agents two competing interfaces.
Concurrent worktrees also contend for host resources that ports cannot model,
such as simulators, emulator identities, and signing access.

## Decision

`devme.toml` gains `[task.<name>]` one-shot commands and `[resource.<name>]`
bounded pools. `devme run <task>` executes task dependencies in declaration
order, checks required setup steps, starts and waits for required services, and
then delegates the command to its native build tool. Aggregate tasks omit
`cmd` and contain dependencies only.

Tasks without services do not start a supervisor. Every task process is a
process-group leader. Timeout and cancellation terminate the whole group and
retain distinct result states and conventional exit codes 124 and 130. Normal
command failures preserve the wrapped command's exit code.

Resource leases are file-locked slots with host, repository, or worktree
scope. The OS releases locks after crashes, owner metadata remains inspectable,
and a configured environment variable exposes the allocated zero-based slot.
Names are acquired in sorted order to prevent multi-resource deadlock.

Task results are appended as bounded JSONL history beside instance runtime
state. Captured stdout and stderr are bounded and values from secret-shaped
task environment keys are redacted before persistence. Human mode replays raw
task output; `--output toon` provides compact agent output and `--output json`
plus the global `--json` alias preserve JSON consumers.

## Consequences

Devme becomes the single root orchestration contract without learning native
build graphs or adding caching. Ordinary services can represent simulator and
adb log streams. Existing service health probes, including HTTP and shell
checks, remain the readiness authority, so a shell probe can verify published
backend schema or functions rather than only an open port.

This tracer bullet does not install agent session hooks or implement project
detection. Those surfaces need separate compatibility work for each supported
agent and init workflow. Task history is persisted and structured but is not
yet merged into the existing `logs` and `doctor` query surfaces.

## Follow-up tasks

1. **Task diagnostics and configurable redaction** - merge task records into
   `devme logs` and `devme doctor`, add explicit redaction patterns and shared
   retention policy, and test correlation across service and task timestamps.
2. **Targeted supervisor readiness** - add an IPC operation that starts only a
   task's required service closure, reports per-probe attempts and last errors,
   and supports configured probe timeout, interval, retries, and overall task
   readiness deadline. The current tracer uses `up --wait`, whose v1 executor
   advances the whole service graph.
3. **Agent session integration and generated guidance** - add explicit,
   idempotent setup/status/remove commands for Claude Code, Codex, and OpenCode
   after confirming each current hook contract. Generate both the home surface
   and installable skill from one guidance source and add a CI freshness check.
4. **Native-monorepo init detection** - extend init/setup detection for Xcode
   projects and workspaces, Package.swift, Gradle Kotlin/Android, Convex, and
   Vite+ while keeping emitted commands explicit and delegating all build graph
   knowledge to the native tools.
