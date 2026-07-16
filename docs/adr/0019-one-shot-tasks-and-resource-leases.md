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
Task and Session acquisition use one shared Resource lease module and metadata
format. A foreground Task hands its lease descriptors to an exact-binary Task
guardian before it starts running. The Task remains behind a gate until the
guardian has recorded both the Task process-group identity and the owning CLI
identity and acquired an independent copy of the Task plan's Service hold. A
Session launch guardian independently joins the existing Session instead. If
the CLI disappears, the guardian terminates and waits for the full Task group,
persists an `interrupted` result, and only then releases the Service or Session
hold and Resource leases. On clean completion, the runner persists the result
and waits for the guardian to acknowledge it before either owner releases its
hold. This prevents leaked Tasks, premature Service shutdown, and premature
Resource reallocation.
Session sidecars start behind a supervisor-owned gate: Devme records each
process-group PID and OS start-time identity before releasing the gate. After a
supervisor crash, the replacement verifies those identities, kills and waits
for matching orphan groups, and only then reassigns the lease. PID reuse is
treated as an already-gone orphan rather than permission to signal an unrelated
process. Runtime state is private to the current user with mode 0700.
The `devme` executable embeds both instance and repository supervisor entry
points and launches its own exact build, so an older separately installed daemon
cannot parse or execute a newer CLI's workspace configuration.

`[session.<name>]` is a narrow composition over existing service closures,
resources, and an optional launch task. It holds resource leases while
session-scoped log or device sidecars and the launch task use the allocated
environment. Multiple clients join idempotently. On final disconnect, a
configurable linger permits reconnection; teardown stops sidecars before
releasing leases. Sessions do not define steps or a second dependency graph.
The optional launch Task borrows the Session's Service and Resource context. It
cannot widen that context by requesting another Service or Resource.

Every active Task, Session, or explicit runtime owner contributes a reference-
counted Service hold for its required Service closure. A Task DAG contributes
one hold for the aggregate plan, while each executable Task acquires its own
Resource leases. Releasing one hold stops only hold-managed Services that no
remaining owner requires. Pre-existing or explicitly managed Services are not
claimed for teardown merely because a Task used them.

Task and service history share one retention and redaction policy. Redaction
patterns are compiled as regular expressions and applied before disk writes.
`devme logs` correlates service and `task:<name>` records by timestamp, while
`devme doctor` includes the latest task results and readiness failure details.

Required service startup is a targeted supervisor operation. It advances only
the required dependency closure and reports each readiness attempt, the last
actionable error, and the configured interval, probe timeout, retries, and
overall task deadline. HTTP, TCP, and shell probes remain generic, so a shell
probe can prove that backend schema and functions are published.

`devme agent setup|status|remove` manages explicit project-scoped session
integrations for Claude Code, Codex, and OpenCode. It never installs them
implicitly. `devme agent context` emits compact directory-scoped TOON state and
next commands. The embedded skill and this context use the same canonical live
guidance, guarded by a freshness test.

`devme setup` conservatively detects Xcode projects/workspaces, Package.swift,
Gradle Kotlin/Android, Convex, and Vite+ markers. It emits explicit delegated
commands and does not infer or reproduce native build graphs.

An explicit root `[workspace.members]` table may compose one level of child
`devme.toml` files. Devme flattens them into the same worktree runtime with
stable `member::node` names. An invocation inside a member can use local names,
while cross-member dependencies remain qualified and can converge the required
backend closure. Paths stay relative to the file that declared them. See
ADR-0020 for ownership, focus, and boundary details.

## Consequences

Devme becomes the single root orchestration contract without learning native
build graphs or adding caching. Ordinary services can represent simulator and
adb log streams. Existing service health probes, including HTTP and shell
checks, remain the readiness authority, so a shell probe can verify published
backend schema or functions rather than only an open port.

The orchestration remains deliberately shallow: Xcode, Gradle, Vite+, Bun, and
Convex own compilation and runtime semantics. Devme owns ordering, isolation,
leases, readiness, history, redaction, and diagnostics. There is no second root
task runner, package graph, remote cache, or native build-graph model.

Tasks may declare path-only `artifacts`. Devme resolves them to absolute
project paths and reports them with the task result. It does not decide
retention, upload, rendering, or CI policy.

Splitting the root file is organizational only. It does not create nested
supervisors or independent resource and history domains. Existing single-file
configs retain their unqualified names and behavior.
