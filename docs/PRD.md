# PRD — devstack v1

**Status**: Draft — to be moved to Linear
**Last updated**: 2026-05-23

---

## Problem Statement

Modern dev workflows run multiple services side by side — a backend, a frontend, a database, often a proxy or two — and developers increasingly work across multiple git worktrees of the same repo simultaneously (one for the current PR, one for code review, one for an experiment). The existing tools in this space (foreman, mprocs, process-compose, hand-rolled `tmux` setups) were designed for the single-worktree case and have three failure modes:

1. **Setup is the user's problem.** Fresh clones need a README walkthrough — install Postgres, generate a `.env`, run a migration. This burden is duplicated across every contributor, every onboarding, every machine refresh.
2. **Multi-worktree workflows are unsupported.** Running two worktrees means two competing servers fighting for port 8080. The user manually offsets ports, manages two terminal sessions, and remembers which is which.
3. **AI coding agents can't drive these tools.** None of them expose structured JSON, semantic exit codes, or stable contracts. An agent watching a service crash has no machine-readable way to diagnose it.

A developer who clones a multi-service repo today still needs to read a README, run setup commands manually, juggle terminal windows, and remember port mappings. We can do better.

## Solution

A single CLI + TUI tool, **devstack** (working name), that:

- Detects what a repo needs and walks the user from missing prerequisites to a running stack with one command (`devstack`).
- Treats every git worktree as a first-class **Stack instance** with stable port allocations so multiple worktrees coexist.
- Exposes a JSON-everywhere CLI surface designed to be driven by AI coding agents as well as humans.
- Ships a lazygit-quality TUI with a fixed three-pane layout (instances sidebar, per-service tabs, live log viewport) and a Catppuccin-aligned default theme.
- Handles shared infrastructure (`repo`-scoped services like a cloud-sql-proxy) via a per-repo coordinator daemon, with crash-isolated lifecycles per worktree.
- Provides a first-run wizard that feels like fly.io's `fly launch`: detect → one big question → review-before-commit → drop into the working TUI as the success state.

The result: typing `devstack` in any configured repo gets the developer from zero to a running, supervised stack with no README required.

## User Stories

### Core developer experience

1. As a developer, I want a single `devstack` command in any configured repo, so that I don't need to read a README to get the dev stack running.
2. As a developer, I want devstack to detect what services my repo needs and propose sensible defaults, so that I don't have to configure each one by hand.
3. As a developer, I want to run multiple worktrees of the same repo without port collisions, so that I can review one PR while continuing work on another.
4. As a developer, I want each worktree to keep its slot stable across restarts, so that `localhost:5183` always points to the same worktree.
5. As a developer, I want shared infrastructure (cloud-sql-proxy, docker daemon) to run once per repo rather than once per worktree, so that I'm not duplicating connections or fighting for ports.
6. As a developer, I want services to be cleanly killed when I close the TUI in foreground mode, so that I don't accumulate orphan processes.
7. As a developer, I want a detached mode (`devstack up`) where services survive me closing the TUI, so that I can leave my dev stack running in the background.
8. As a developer, I want to reattach to a running detached stack from any terminal (`devstack attach`), so that I can come and go without losing state.

### Setup, prerequisites, and consent

9. As a developer, I want missing prerequisites (gcloud, docker, the right Rust toolchain) to be detected automatically, so that fresh-clone setup is one command.
10. As a developer, I want devstack to show me the exact command it's about to run before installing anything, so that I'm never surprised by what landed on my machine.
11. As a developer, I want safe operations (`mkdir`, `touch`, generating local files) to run without prompting, so that I'm not asked about every trivial action.
12. As a developer, I want a `--yes` flag, so that CI and scripted runs can bypass all prompts.
13. As a developer, I want to mark a prerequisite as installed when devstack's check is wrong (because I have it via nix / asdf / a custom path), so that I'm not blocked by a false-negative detection.
14. As a developer, I want the failure overlay to give me four clear actions (Enter to install / r to retry / s to skip-once / i to mark-installed), so that I can choose the right escape hatch for each situation.
15. As a developer, I want overrides to be visible in the TUI and listable via `devstack overrides list`, so that I can audit which checks I've bypassed.
16. As a developer, I want overrides to be reversible (`devstack overrides clear <step>` or `devstack health --recheck`), so that I'm never stuck with a bad assertion.

### Service lifecycle and failure handling

17. As a developer, I want services to auto-restart on failure with exponential backoff (1s → 32s), so that transient crashes don't require manual intervention.
18. As a developer, I want a crash-loop guard (5 restarts in 60s → give up), so that a wedged service doesn't burn my CPU forever.
19. As a developer, I want to declare some dependencies as optional with a `?` suffix (e.g. `depends_on = ["db", "proxy?"]`), so that my backend can start even when the proxy is unavailable.
20. As a developer, I want to force-start a service whose required dependency is down (`devstack start backend --skip-deps` or `f` in the TUI), so that I can develop offline when needed.
21. As a developer, I want force-started services to display "started without proxy" status, so that I never silently think everything is fine.
22. As a developer, I want to declare a service as `external = true`, so that devstack just monitors it instead of trying to manage its lifecycle (useful for `brew services` Postgres, host docker, etc.).
23. As a developer, I want a service that crashes within 10s of starting up to trigger a recheck of its upstream prerequisites, so that I catch the "you just uninstalled gcloud" case.

### TUI experience

24. As a developer, I want to see live logs from every service in one TUI, so that I can debug without juggling terminal windows.
25. As a developer, I want a sidebar listing all my Stack instances, so that I can switch between worktrees without remembering paths.
26. As a developer, I want the focused service's tab to highlight its state with color (green=running, red=failed, yellow=starting, blue=waiting), so that health is visible at a glance.
27. As a developer, I want a status bar at the bottom showing my current focus, key hints, and global health summary, so that I always know what's happening and what I can do next.
28. As a developer, I want both Vim-style (hjkl) and arrow-key bindings, so that I'm not forced into one navigation style.
29. As a developer, I want `?` to open a help overlay listing every keybinding from my current focus, so that discovery doesn't require reading docs.
30. As a developer, I want to restart (`r`), stop (`s`), start (`S`), and force-start (`f`) services from the TUI, so that I don't need to remember CLI command names.
31. As a developer, I want mouse support for click-to-focus and scroll, so that the TUI works for users who prefer the mouse.
32. As a developer with a small terminal, I want the TUI to degrade gracefully (collapse sidebar, hide tabs), so that the tool stays usable at 60 cols and below.
33. As a developer with a colorblind preference, I want non-color status indicators (✓/⚠/✗ glyphs), so that I don't depend on hue alone to read service state.
34. As a developer, I want a `mocha` (dark), `latte` (light), and `mono` (no color) theme built in, so that the TUI matches my terminal preferences.

### CLI surface

35. As a developer at the command line, I want `devstack status` to show a table of services across all worktrees, so that I can see the big picture without opening the TUI.
36. As a developer at the command line, I want `devstack logs <service>` to print the last 200 lines, so that I can pipe them to grep or save them to a file.
37. As a developer at the command line, I want `devstack logs --follow`, `--since 5m`, `--lines 1000`, `--level error`, and `--grep <pattern>`, so that log inspection is composable.
38. As a developer at the command line, I want `devstack errors` to give me a rich debugging packet per failure, so that I can diagnose without scrolling through hundreds of log lines.
39. As a developer at the command line, I want `devstack ports` to show port allocations across instances, so that I can debug "what's using port 5183."
40. As a developer at the command line, I want `devstack env <service>` to show resolved env vars, so that I can debug "why isn't DATABASE_URL set."
41. As a developer at the command line, I want `devstack health` to run every check and report pass/fail, so that I can prove my setup is correct.
42. As a developer at the command line, I want `devstack instances` to list every live Stack instance on the machine, so that I can see what's running across all my repos.
43. As a developer at the command line, I want every command to default to the current worktree's instance, so that I rarely need `--instance <id>`.
44. As a developer at the command line, I want every command to support `--json`, so that I can pipe structured output into other tools.
45. As a developer at the command line, I want semantic exit codes documented in `devstack errors --list`, so that my scripts branch correctly on the kind of failure.
46. As a developer at the command line, I want shell completions for bash/zsh/fish/nushell via `devstack completions <shell>`, so that I can discover the surface by Tab-completing.

### AI coding agents

47. As an AI coding agent, I want a stable JSON schema versioned with `schema_version`, so that I can parse output reliably across devstack versions.
48. As an AI coding agent, I want `devstack agent-context` to enumerate every command, flag, exit code, and JSON schema, so that I can drive devstack without scraping `--help`.
49. As an AI coding agent, I want `devstack errors --json` to return a complete debugging packet — service, kind, message, exit code, restart count, recent logs from the service AND its dependencies, env snapshot, command line, cwd, last successful start — so that I can diagnose without making additional tool calls.
50. As an AI coding agent, I want commands to be idempotent, so that `devstack restart backend` works whether the service is running or not.
51. As an AI coding agent, I want `--no-input` to disable interactive prompts (auto-implied when stdin isn't a TTY), so that I can drive devstack from a sandbox without hanging on a prompt.
52. As an AI coding agent, I want a Claude Code skill that codifies the workflows ("read errors and diagnose," "restart and verify"), so that I have high-level patterns rather than just primitives.
53. As an AI coding agent, I want `--dry-run` to return a structured JSON diff (not prose), so that I can validate intent before committing.

### First-run wizard

54. As a first-time user, I want to see what devstack detected about my project before any prompts, so that I trust the tool before it does anything.
55. As a first-time user, I want exactly one big choice ("recommended setup / customize / inspect-only"), so that I'm not overwhelmed by a questionnaire.
56. As a first-time user, I want a review-before-commit screen showing every file and service action devstack will take, so that I see the full plan before confirming.
57. As a first-time user, I want the live TUI to appear immediately after setup, so that the working product is my success message instead of a "setup complete" toast.
58. As a first-time user who Ctrl-C's mid-wizard, I want to resume where I left off on the next run, so that I don't have to repeat answers.
59. As a first-time user, I want every wizard prompt to have a CLI-flag equivalent (`devstack init --yes`), so that scripted setups are deterministic.
60. As a first-time user, I want telemetry to be off by default and only asked about after 7 days of use, so that I don't feel surveilled during onboarding.
61. As a first-time user with a custom wizard script in `.devstack/`, I want to ask the user for a value via a multi-field form, so that complex setup (env collection, GCP project selection) feels native.

### Config authoring

62. As a config author, I want to declare services with `scope = "instance"` (default) or `scope = "repo"`, so that shared infrastructure isn't duplicated across worktrees.
63. As a config author, I want abstract dependencies (like `[step.docker_running] check = "docker info"`) that the user resolves in their global config, so that my repo doesn't need to know whether they use Docker Desktop, OrbStack, or Podman.
64. As a config author, I want the `?` suffix and `required = false` to mark optional dependencies, so that graceful-degradation intent is documented in the config.
65. As a config author, I want a per-step `trust` level (`auto` / `prompt` / `manual`), so that I can mark trivial provisions as auto-runnable while keeping risky ones gated.
66. As a config author, I want `port = { base = 8080, slot_offset = 10 }` syntax, so that I can express slot-aware port allocations in one line.

### Installation

67. As a new user, I want a curl-piped install script (`curl -fsSL https://<host>/install | sh`), so that I can try devstack with one line.
68. As a macOS user, I want devstack on Homebrew (`brew install <org>/devstack/devstack`), so that I can install via my normal package manager.
69. As a Rust developer, I want `cargo install devstack`, so that I can install from source.
70. As an enterprise/firewalled user, I want pre-built binaries on GitHub Releases, so that I can audit before installing.
71. As a user updating devstack, I want to re-run the install command (no `devstack upgrade` in v1), so that the upgrade path is the same as the install path.

## Implementation Decisions

### Language, toolchain, project structure

- **Rust**, edition 2024, MSRV 1.89 (for `std::fs::File::lock`).
- **Workspace** with seven crates: `core`, `config`, `supervisor`, `shared-supervisor`, `client`, `tui`, `cli` (see the existing scaffolding).
- **`cargo-dist`** drives the release pipeline (binaries, install.sh, Homebrew formula, GitHub Actions workflow).
- **`cargo-nextest`** as the test runner.
- **`clap`** for argument parsing, **`clap_complete`** for shell completions.
- **`ratatui`** + **`crossterm`** for the TUI; **`portable-pty`** for child PTYs; **`tokio`** + **`tokio-util`** for async I/O and codec; **`process_alive`** for cross-platform PID liveness; **`serde`** + **`toml`** + **`serde_json`** for configuration and IPC; **`tracing`** for structured logs.

### Deep modules (test-driven, isolated from I/O at the edges)

- **`config`** — TOML → validated Stack. Performs glob expansion, scope inheritance, dependency-graph cycle detection, port-template resolution. Pure function: `parse(repo_toml: &str, global_toml: &str) -> Result<Stack, ConfigError>`. No file I/O inside; the caller reads the files.
- **`slot-allocator`** — File-locked claim/release on a sidecar lock + atomic-rename of the data file. Stale-PID detection via `process_alive` plus PID-start-time pair to defeat PID reuse. Exposed as `claim(instance_id, registry_path) -> Result<Slot, AllocError>` and `release(slot, registry_path)`. NFS detection at startup, refuses to coordinate on broken filesystems.
- **`graph-executor`** — Pure DAG walker driven by an `EventSink` trait. Decides "run check X next," "spawn service Y," "Y is waiting on Z." No PTYs, no sockets, no time — testable via a fake `EventSink` and a fake `Clock`. Handles the failure model from ADR-0005: halt-on-provision-failure for the affected subtree, `waiting_on_dependency` state for blocked Services, optional-dep / forced-start branches.
- **`process-supervisor`** — Given a `Command`, spawns a PTY via `portable-pty`, captures stdout/stderr line-by-line, manages the restart policy (`on-failure` / `always` / `never` + exponential backoff + crash-loop guard). Owns exactly one process's lifecycle. Tested with mock commands that exit predictably.
- **`ipc-codec`** — Length-prefixed JSON-lines codec implementing `tokio_util::codec::{Encoder, Decoder}`. Pure framing logic, tested with byte fixtures.
- **`wizard-runner`** — Spawns a wizard subprocess, exchanges JSON-lines events (`ask`, `progress`, `log`, `set_var`, `done`), mediates between the wizard and the TUI. Includes fixture replay for tests — given a transcript of events and responses, validate the runner correctly serializes them.

### Medium-deep modules (integration-tested)

- **`daemon-server`** — Listens on a Unix socket, ref-counts clients, routes log streams and status updates to subscribers, handles graceful and abrupt client disconnects. One implementation shared between `supervisor` (instance daemon) and `shared-supervisor` (per-repo daemon) via configuration parameters.
- **`client`** — Connection management, subscription, and a typed API for the IPC protocol. Used by `tui` and `cli`. Tested via an in-memory transport.
- **`tui-state-model`** — Pure state machine: events from `client` go in, render directives come out. Decoupled from `ratatui`'s frame rendering so the state logic is testable without a terminal.

### Shallow modules

- **`cli`** — clap subcommands, output formatting (human table vs `--json`), exit codes. Glue.
- **`tui-render`** — `ratatui` frame construction from the state model. Snapshot-tested.

### Protocols and on-disk formats

- **IPC protocol** — Length-prefixed JSON-lines envelope (`{ "schema_version": 1, "kind": "...", ... }`). Messages: `Subscribe`, `Unsubscribe`, `LogChunk`, `StatusUpdate`, `Restart`, `Stop`, `Start`, `RecheckHealth`, `Shutdown`. Stable wire schema; breaking changes go through `schema_version` bumps.
- **Wizard protocol** — Same JSON-lines envelope. Wizard events: `ask` (with subtypes `text`, `password`, `choice`, `multi_choice`, `confirm`, `form`), `progress` (`start`/`update`/`end`), `log` (`info`/`warn`/`error`), `set_var`, `done`. Wizard reads responses from stdin: `{ "value": ... }`.
- **`devstack.toml`** — Repo config. Branch-local, checked into git. Declares Steps and Services with `scope` (`instance` | `repo`), `trust`, `depends_on`, `port`, `health`, `external`, etc.
- **`~/.config/devstack/global.toml`** — User global config. Declares machine-wide Steps and Services.
- **`.devstack/overrides.toml`** — Persisted mark-as-installed overrides.
- **`.devstack/.first-run.json`** — Resumable wizard state; gitignored.
- **`~/.local/share/devstack/instances/<id>.sock`** — Instance daemon socket.
- **`~/.local/share/devstack/repos/<repo-hash>/shared.sock`** — Per-repo shared daemon socket.
- **`~/.local/share/devstack/repos/<repo-hash>/slots.toml`** + **`.lock`** — Slot allocation registry.

### Instance identity and slot allocation (ADR-0006)

- **Instance ID** — Hash of canonical absolute worktree path. Stable across branch switches; changes only when the worktree directory is renamed.
- **Slot allocation** — Stdlib `std::fs::File::lock` exclusive on the `.lock` sidecar; read-modify-write of `slots.toml` via tempfile + atomic rename. Records `(slot, instance_id, pid, start_time_epoch, claimed_at)`. Stale entries reclaimed when PID is dead or start-time differs from the live process's.
- **Slot cap** — Configurable, default 10. Refuses to start beyond 10 with a clear error.

### Lifecycle (ADRs 0003, 0007)

- **`devstack`** in a worktree → spawn instance daemon if none, attach TUI as client. On TUI exit, ref count drops; daemon shuts down services and exits.
- **`devstack up`** → spawn instance daemon with sticky flag; services survive client disconnect until `devstack down`.
- **`devstack attach`** → connect TUI to an existing instance daemon. Does not change the sticky flag.
- **`devstack down`** → graceful shutdown signal to the daemon.
- **First instance daemon needing a `repo`-scoped service** → spawn `shared-supervisor` (per-repo). Subsequent instances attach as clients of the shared daemon. Last instance disconnects → shared daemon shuts down.
- **Hard kill of any daemon** → other daemons detect dropped sockets; one elects itself (lowest slot) and respawns the shared daemon if needed.

### Failure model (ADR-0005)

- **Failed `check`** → failure overlay with four actions: `Enter` (install), `r` (retry), `s` (skip-once), `i` (mark-installed → persisted Override).
- **Failed `provision`** → halt the affected subtree (not the whole graph). Inline retry/skip/abort options.
- **Service crash** → restart policy (default `on-failure`), exponential backoff (1s → 32s), crash-loop guard (5 restarts in 60s → mark `crashed_too_often`, stop auto-restarting).
- **Service crashes within 10s of startup** → recheck upstream `check`s; if any now fail, the cache (in-memory dedup) is invalidated and the chain reports as `dependency_check_stale_invalidated` to the agent.

### CLI conventions (ADR-0008)

- Every data-returning command supports `--json` with `{ "schema_version": 1, ... }`.
- Stdout = data contract; stderr = progress, warnings, spinners.
- Exit codes: 0 success, 1 general, 2 usage, 3 not found, 4 permission, 5 conflict.
- `--exclude <pattern>` (repeatable, glob) for item filtering; `--skip-<behavior>` for behavior skipping; `--no-input` for non-interactive; `--yes` to bypass confirmations.
- Respect `NO_COLOR` and `FORCE_COLOR`.
- Flat top-level verbs while small; promote to noun-verb only when 3+ verbs accumulate on the same noun.
- `devstack agent-context` emits a machine-readable manifest of every command, flag, exit code, and JSON schema.

### TUI (ADR-0010)

- Fixed three-pane layout (sidebar + top tabs + main viewport) + bottom status bar.
- Synthetic `supervisor` tab is always first within each instance, showing graph status, shared service health, and setup output.
- Dual hjkl + arrow + Tab navigation. `1-9` jumps to pane index. `?` opens help overlay. `Ctrl+B` toggles sidebar.
- Catppuccin-aligned palette. Three themes: `mocha` (default dark), `latte` (light), `mono` (no color).
- Mouse support for click-to-focus and scroll; not load-bearing.
- Resize behavior: full layout ≥100 cols; abbreviated sidebar 60-99 cols; sidebar hidden 40-59 cols; "too small" message <40 cols.

### First-run wizard (ADR-0011)

- Triggered by absence of `devstack.toml` in the repo.
- Five screens: detection banner (auto-advance) → one big question (recommended/customize/inspect) → optional batched form → review-before-commit → drop into live TUI.
- Resumable state on Ctrl-C via `.devstack/.first-run.json`.
- Every prompt has a CLI flag equivalent (`devstack init --yes --services=backend,frontend,db`).
- Telemetry off by default; deferred prompt after 7 days of use.

### What we cache (revised down from earlier discussion)

- **In-memory dedup within a run.** Each Step's check runs at most once per `devstack` launch.
- **No disk cache, no TTL, no invalidation.** Every fresh launch re-runs every check. Disk caching deferred to a post-v1 optimization once we have real data on which checks are slow enough to matter.

## Testing Decisions

### What makes a good test

- **Test external behavior, not implementation details.** A test for the graph executor should assert "given this Stack, the executor produces this sequence of events," not "the executor calls `internal_function_x` with arguments y."
- **Use real filesystem and real sockets where possible.** Slot allocation tests use a temp directory; daemon tests bind real Unix sockets. Mocking the filesystem or socket layer hides exactly the bugs those layers cause.
- **Mock at the boundaries only.** `process-supervisor` tests mock the `Command` to use a deterministic helper binary that exits with a controlled code after a controlled delay. The PTY plumbing itself is real.
- **Snapshot tests for TUI rendering.** Use `insta` or equivalent. The state model is tested with assertions; the render layer is tested with snapshots so visual regressions surface immediately.
- **Property-based tests for protocol round-trips.** Use `proptest` to assert `decode(encode(message)) == message` for every IPC message variant.

### Modules to TDD

1. **`config`** — parse + validate. Parsing happy paths, cycle detection, scope inheritance, invalid TOML, abstract-dep resolution against a fake global config.
2. **`slot-allocator`** — concurrent claim contention (use `tokio::spawn` to race claims), stale-PID reclamation, PID-reuse defense (mock `process_alive`), NFS detection.
3. **`graph-executor`** — straight-line dependencies, optional deps, forced start, crash-loop guard trip, halt-on-provision-failure subtree scoping, recheck-on-downstream-crash.
4. **`process-supervisor`** — restart policies, exponential backoff timing (with fake clock), crash-loop guard threshold, clean shutdown on stop signal.
5. **`ipc-codec`** — round-trip every message variant; fragmented reads; partial writes; oversized payload rejection.
6. **`wizard-runner`** — fixture wizards in `tests/fixtures/wizards/` (small Bun and shell scripts emitting known event streams); verify the runner correctly mediates each primitive (text, choice, form, progress, done).

### Modules with integration tests

7. **`daemon-server` + `client`** — end-to-end "spawn daemon, connect client, subscribe, see log stream" tests with real sockets in temp dirs.
8. **`shared-supervisor` lifecycle** — first instance spawns it, second attaches, last disconnects → shared exits.
9. **Multi-worktree slot allocation race** — spawn 10 daemons concurrently against a shared registry; assert no two get the same slot, all settle on distinct slots.
10. **First-run wizard end-to-end** — run `devstack init --json` non-interactively with various flag combinations; assert byte-identical `devstack.toml` output for identical inputs.

### Modules with minimal coverage (smoke-test only)

- **`cli`** — clap parsing tests for each subcommand; ensure every command supports `--json` and `--help`.
- **`tui-render`** — one snapshot per major screen (empty state, single instance, multiple instances with mixed health).

### Prior art

The Rust ecosystem has well-trodden patterns for each test category:

- Cargo's own test suite for file-locking concurrency.
- `tokio`'s test utilities (`tokio::test`, `tokio::time::pause`) for time-dependent tests.
- `assert_cmd` for end-to-end CLI assertion.
- `insta` for snapshot tests.
- `proptest` for property tests.
- `tempfile` for isolated filesystem state.

## Out of Scope (v1)

- **Disk-based check caching.** Only in-memory dedup within a single run.
- **`devstack upgrade` self-update.** Users re-run the install command to upgrade.
- **`Ctrl+P` command palette.** Deferred to v1.1 unless trivial to add.
- **MCP server endpoint.** The CLI + skill is the canonical agent surface; an MCP wrapper can come later if there's demand.
- **Native Windows support.** WSL is the v1 path; tokio + ratatui work on Windows but the supervisor model (PTYs, Unix sockets) needs nontrivial work to be cross-platform.
- **Web-based onboarding fallback.** The terminal wizard covers the v1 surface; a web fallback (à la `fly launch`) is a v1.1 candidate if the form grows.
- **Mandatory telemetry.** Off by default; deferred consent after 7 days.
- **Plugin system / extensibility beyond wizard scripts.** The wizard protocol is the extension point.
- **AUR, nixpkgs, apt, yum packaging.** Community-maintained if they appear; not officially shipped.

## Further Notes

- **Working name.** "devstack" is provisional. The npm package and `.dev`/`.io`/`.sh` domains are taken, and OpenStack devstack collides on SEO. A public naming decision is required before v1.0; renaming an internal-only repo is cheap.
- **Dogfooding.** Once `supervisor`, `cli`, and basic `tui` are functional, devstack should manage its own dev workflow (test watch, lint watch, mdbook docs) as a forcing function. CI assertion: fresh clone → `cargo install --path .` → `devstack up` → all services healthy within 30s → `devstack down` → no orphans.
- **Reference implementations to study.** lazygit (TUI layout discipline), `fly launch` + Clack (first-run wizard UX), Cargo's `src/cargo/util/flock.rs` (file locking), uv / ruff / mise (modern Rust CLI conventions), atuin (curl-piped install + reattach-friendly).
- **Naming-related findings to keep in mind.** The worktree-aware tooling space exploded in 2025–2026 — baton, grove, tend, marshal, wisp, muster, steward, tether are all already taken by direct or adjacent competitors. Differentiation will be on execution (agent-friendly CLI, wizard quality, `repo`-scope coordination), not the worktree concept alone.
- **Domain glossary.** See `CONTEXT.md` at the repo root.
- **Architectural decisions.** See `docs/adr/` (ADR-0001 through ADR-0011).
