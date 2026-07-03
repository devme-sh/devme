# PRD: devcloud Split And devme On-Demand Runtime

Status: ready-for-agent
Tracker target: Linear `devme-sh`
Local draft: `.scratch/devcloud-devme-runtime-split/`

## Problem Statement

devme currently owns too much of the remote development workflow. It supervises project services, but it also contains remote sync, remote command proxying, Mutagen sessions, Herdr attach behavior, URL rewriting, and remote-first defaults. That made the original "close the laptop and keep working" idea powerful, but the current workflow has moved in a different direction.

The desired workflow is now simpler and more explicit:

1. devme should own project service orchestration.
2. A new devcloud command should own remote project context and SSH execution.
3. Git should be the source of truth for project sync in v1.
4. Herdr and Codex should own agent/session control.
5. A small fish command named `dev` can compose these tools into the daily workflow.

The current devme remote surface makes this harder because it couples local stack supervision to remote sync and Herdr. It also starts services more eagerly than needed. When agents are doing development, the service stack should be asleep by default and wake only when a command actually needs runtime services, logs, URLs, or a running TUI view.

The user wants a minimal, reliable system where a developer can `cd` into a project on the Mac, run one command to work on the VPS when needed, and let agents wake or sleep project services on demand without devme becoming the global session orchestrator.

## Solution

Split the system into two clear tools:

1. **devme** remains the local stack supervisor. It owns Steps, Services, logs, status, URLs, startup timing, idle sleep, and the TUI.
2. **devcloud** becomes the remote project context adapter. It resolves a local Git project into a canonical remote path, ensures the remote clone exists, and runs commands or shells on the remote host in the correct directory.

For v1, devcloud does not implement live sync. It requires a Git repository with an `origin` remote. It derives project identity from Git, creates or verifies a canonical remote clone under the configured root, and runs SSH commands in that clone. This makes Git the durable sync boundary and avoids Mutagen conflict handling until there is a proven need for live mirroring.

devme becomes more on-demand:

- Running `devme` attaches the TUI/dashboard but does not wake services by itself.
- `devme up -d` wakes or starts the stack, like Docker Compose.
- `devme down` sleeps/stops the stack.
- `devme status` is read-only and does not wake the stack.
- Runtime commands such as `logs`, `url`, `doctor`, `start`, `restart`, or opening service detail/log views wake the stack when needed and clearly say that services are starting.
- devme tracks internal activity and sleeps services after a default idle window, initially 30 minutes.
- The idle mechanism is internal. There is no public lease CLI in v1.
- devme records service startup timing so it can show realistic startup estimates when waking a sleeping stack.

Herdr and Codex stay outside devme. The user-facing `dev` workflow can be a shell function or small wrapper that composes devcloud output:

- `devcloud name` gives the default Herdr session name.
- `devcloud path` gives the remote project directory.
- `devcloud run <cmd...>` runs commands on the VPS in that directory.
- A fish `dev` command can use those values to open a Herdr session or start Codex remotely without devme knowing about Herdr.

## User Stories

1. As a developer, I want `devme` to focus on services, so that remote session orchestration does not complicate the stack supervisor.
2. As a developer, I want a separate `devcloud` command for remote project context, so that I can use the same remote path and SSH behavior with Herdr, Codex, or plain shell commands.
3. As a developer, I want devcloud to derive project identity from Git origin, so that I do not maintain a separate project registry.
4. As a developer, I want devcloud to require Git origin in v1, so that remote state is anchored in a durable source of truth.
5. As a developer, I want devcloud to create or verify the canonical remote clone, so that the VPS directory convention stays consistent.
6. As a developer, I want `devcloud path` to print the canonical remote project path, so that scripts can compose it without parsing prose.
7. As a developer, I want `devcloud name` to print a clean project name, so that Herdr sessions can be named consistently.
8. As a developer, I want `devcloud status` to explain the local Git origin, remote host, and remote path, so that I can diagnose misconfiguration quickly.
9. As a developer, I want `devcloud doctor` to check Git origin, SSH reachability, remote Git, and remote clone state, so that failures are found before spawning agents.
10. As a developer, I want `devcloud run <cmd...>` to execute in the remote project directory, so that remote commands do not accidentally run in `~`.
11. As a developer, I want `devcloud ssh` to open an interactive shell in the remote project directory, so that manual inspection is one command.
12. As a developer, I want devcloud to refuse origin mismatches, so that it never silently points a local project at the wrong remote clone.
13. As a developer, I want remote project paths under a clean source layout, so that the VPS is understandable without devcloud.
14. As a developer, I want no live sync in v1, so that there are no Mutagen conflicts, hidden file flows, or local/remote divergence surprises.
15. As a developer, I want devme remote sync to be removed from the main workflow, so that I do not have two different systems claiming to own remote development.
16. As a developer, I want `devme` to attach the TUI without waking services, so that I can inspect the project without starting a stack.
17. As a developer, I want `devme up -d` to wake a sleeping stack, so that the command behaves like Docker Compose.
18. As a developer, I want `devme down` to sleep the stack, so that stopping services is obvious and explicit.
19. As a developer, I want `devme status` to remain read-only, so that agent status checks do not accidentally start services.
20. As a developer, I want `devme logs <service>` to wake the stack when logs require a running daemon, so that agents can ask for logs without knowing if services are asleep.
21. As a developer, I want `devme url <service>` to wake the stack when the service is asleep, so that the returned URL is meaningful.
22. As a developer, I want devme to tell the caller when it is waking services, so that agents can report "starting" instead of appearing stuck.
23. As a developer, I want wake commands to show startup estimates, so that I know whether to wait 5 seconds or 60 seconds.
24. As a developer, I want devme to record per-service startup timing, so that estimates improve over time.
25. As a developer, I want devme to sleep idle services after 30 minutes, so that unused stacks do not consume local resources indefinitely.
26. As a developer, I want idle sleep to be internal, so that I do not need to reason about leases.
27. As a developer, I want status checks not to extend idle time, so that monitors do not keep stacks alive forever.
28. As a developer, I want meaningful activity such as logs, URLs, service controls, and active TUI interaction to extend idle time, so that stacks stay awake while actually being used.
29. As an agent, I want the devme skill to describe the new wake/sleep behavior, so that I can use `devme` correctly without guessing.
30. As an agent, I want the devcloud command surface to be small and stable, so that I can compose it with Herdr or Codex from scripts.
31. As a user, I want a fish `dev` command to compose devcloud and Herdr, so that my daily workflow is `cd project && dev`.
32. As a user, I want Herdr session naming to come from devcloud name, so that all interfaces agree on the same session label.
33. As a user, I want Codex auth and sessions to remain owned by Codex, so that devme does not sync or mutate Codex internals.
34. As a user, I want Herdr sessions to remain owned by Herdr, so that devme does not become a terminal multiplexer.
35. As a future Bender/Hermes caller, I want to call devcloud and devme separately, so that voice-driven development can launch remote work without depending on devme remote sync.

## Implementation Decisions

- Build a new Rust CLI binary named `devcloud`.
- Keep devcloud in the devme workspace for v1 unless the implementation discovers a strong reason for a separate repository.
- devcloud reads a small user config with `host`, `root`, and optionally `worktrees`.
- devcloud requires the current directory to be inside a Git repository with an `origin` remote.
- devcloud parses common SSH and HTTPS Git remote URLs and derives provider host, owner or namespace, and repo name.
- Treat GitHub, GitLab, and compatible Git hosts uniformly for display identity as `owner/repo`.
- Use the repo basename as `devcloud name` for v1.
- Use the provider host and owner/repo path for canonical VPS clone layout.
- Use Git as the v1 sync boundary. Do not introduce mirrors, Mutagen, devsync, or continuous file synchronization in this PRD.
- `devcloud path` ensures or reports the remote project path as a machine-readable single line.
- `devcloud run <cmd...>` runs over SSH in the resolved remote project directory.
- `devcloud ssh` opens an interactive shell in the resolved remote project directory.
- `devcloud status` is read-only and human-oriented.
- `devcloud doctor` is diagnostic and should fail with actionable messages for missing origin, unreachable SSH, missing remote Git, origin mismatch, or missing clone.
- Remote clone convergence should clone when absent and verify origin when present.
- Remote clone convergence may fetch remote state, but should not mutate local working trees or create branches unless explicitly asked in a later issue.
- devcloud must centralize shell quoting and remote command construction in a small testable module.
- devme removes the public remote-primary surface from its main contract: remote config, transparent proxying, Mutagen sync orchestration, Herdr attach presets, wake hooks, and remote URL rewriting are no longer devme responsibilities.
- Old remote config should fail or warn with a migration hint instead of silently continuing remote-primary behavior.
- The old cloud relay ADR should be superseded by a new decision record documenting the split.
- devme keeps its existing Stack, Step, Service, Daemon, Client, and TUI domain model.
- Running bare `devme` should attach a TUI/dashboard without implicitly starting services.
- `devme up -d` is the explicit stack wake command.
- `devme down` remains the explicit stack sleep command.
- `devme status` stays read-only and does not wake services.
- Service-targeting commands that require live runtime state should wake the stack if it is asleep.
- Waking should produce a clear user-visible notice and structured state when JSON mode exists.
- Idle sleep is tracked inside the daemon or adjacent runtime state, not through a user-facing lease API.
- Activity that extends idle should be explicit and intentional: service control, logs, URLs, doctor, and meaningful TUI interaction.
- Passive status snapshots should not extend idle.
- Startup timing telemetry should be per service and keyed by service identity plus effective config hash where practical.
- Startup timing should record milestones such as spawn start, first log line, port open, health ready, and ready state.
- Startup estimates should be conservative when no history exists and should improve from observed timings over time.
- Update the embedded devme skill whenever CLI behavior changes.

## Testing Decisions

- Tests should assert external behavior and stable contracts, not private implementation details.
- devcloud Git origin parsing should be covered by pure unit tests with SSH, HTTPS, scp-like SSH, GitHub, GitLab, nested GitLab groups, and invalid remotes.
- devcloud path/name/status should be tested with temporary Git repos and fake config.
- devcloud remote command construction should be tested through a fake SSH runner or command builder, not by requiring a real VPS.
- devcloud clone convergence should be tested with local bare repositories where possible, and with command-runner fixtures for SSH behavior.
- devme remote removal should have CLI parser tests and config tests that verify old remote keys no longer drive default behavior.
- devme wake/sleep semantics should have CLI integration tests around `devme`, `up -d`, `down`, `status`, `logs`, `url`, and `doctor`.
- Idle sleep should be tested with a fake clock or controllable timer so tests do not wait 30 real minutes.
- Startup telemetry should be tested with deterministic service fixtures that delay first log, port readiness, and health readiness.
- The TUI should have render/state tests for asleep, waking, running, and idle-countdown states.
- The embedded devme skill should have snapshot or text tests ensuring the command table and gotchas mention the new behavior and no longer advertise `devme remote`.

## Out of Scope

- No live file sync in v1.
- No `devsync` binary in this PRD.
- No Mutagen integration in the new devcloud flow.
- No automatic local-to-remote takeover when a laptop sleeps.
- No syncing Codex, Claude, or Herdr internal session state.
- No hosted relay or dashboard.
- No Bender/Hermes agent launcher implementation inside devme.
- No dotfiles or fish function implementation inside this repo, except documentation of the expected command composition.
- No remote worktree orchestration beyond canonical clone convergence.
- No migration of existing user machines beyond warnings and documented cleanup.

## Further Notes

The old remote-primary design was coherent for a different goal: continuous live sync plus automatic handoff. The new design favors explicit, composable boundaries. devme should be a deep module for local runtime supervision. devcloud should be a deep module for remote project resolution and SSH execution. Herdr, Codex, and shell functions compose those modules without being embedded inside either one.

This PRD intentionally creates a narrow v1. Git-based remote work is enough to support reliable VPS development and agent session launching. If live sync becomes necessary later, it should be introduced as a separate devsync/devcloud feature with its own conflict model and not re-coupled to devme stack supervision.
