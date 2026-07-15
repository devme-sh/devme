# devme — Domain Context

devme supervises multi-service dev environments. One running copy supervises one git worktree (or one non-git project). A user with multiple worktrees of the same repo runs multiple coexisting devme instances on the same machine without port collisions or config conflicts. The CLI is designed to be driven by AI coding agents as well as humans.

This document is the glossary. Implementation lives in code; design decisions live in `docs/adr/`.

## Core terms

### Stack instance

One running copy of a Stack. Each git worktree maps to one Stack instance; non-git projects get a single default instance. The TUI's sidebar enumerates Stack instances.

### Stack

The configured set of [[Step]]s and [[Service]]s that defines what an [[Stack instance]] should run. Declared in the [[Repo config]] file at the root of the repo.

### Step

A oneshot node in the [[Stack]] graph. Considered satisfied when its `check` command exits 0. Used for setup work: installing tooling, generating local files, fetching credentials. Each Step declares a `check` (read-only) and a `provision` (the action to satisfy the check if it fails).

### Service

A long-running node in the [[Stack]] graph. Spawned and kept alive by a [[Daemon]]; the supervisor manages its lifecycle (start, stop, restart, crash recovery). Examples: backend HTTP server, frontend dev server, local database.

### Task

A one-shot command in a [[Stack]] that delegates work to an authoritative project tool. A Task may depend on other Tasks, require [[Step]]s and [[Service]]s, and acquire [[Resource]]s. Dependency Tasks execute in declaration order, and aggregate Tasks group dependencies without declaring their own command.

### Task kind

Semantic discovery metadata for a one-shot task: `launch`, `check`, or `utility`. It controls grouping on the interactive [[Home screen]] but never changes execution. Existing tasks default to `utility`.

### Home screen

The first interactive TUI view for bare `devme`. It groups curated one-shot actions into Run, Check, and Utilities, shows service health and recent results, and delegates execution to the same task runner used by `devme run`. It reports completed launches as historical results, not observable runtime state.

### Scope

A property of every [[Step]] and [[Service]]:

- `instance` (default) — One copy per [[Stack instance]]. Backend, frontend, db.
- `repo` — One copy per repo, shared across all [[Stack instance]]s of that repo. Cloud SQL proxy.

A third scope, machine-wide, is declared only in the [[User global config]] — never in the [[Repo config]].

### Repo config

`devme.toml` at the root of a repo. Branch-local (checked into git). Declares the [[Stack]] — every [[Step]] and [[Service]] for that repo. May reference machine-level dependencies via abstract checks (e.g. `docker info`) without naming how they are provided.

### User global config

`~/.config/devme/global.toml`. User-level. Declares machine-wide [[Step]]s and [[Service]]s and tool preferences. Resolves abstract dependencies declared in [[Repo config]] — for example, the user picks OrbStack as their Docker provider here.

### Trust level

Per-[[Step]] consent policy for running its `provision` command:

- `auto` — Run without asking. Safe operations only (mkdir, touch, generating local files).
- `prompt` (default) — Ask before running. Anything that mutates the system, installs packages, or hits the network.
- `manual` — Never auto-run. Display the suggested command; let the user execute it.

The global `--yes` flag promotes every `prompt` step to `auto` for a single invocation.

### Override

A user-asserted bypass of a [[Step]]'s `check`. Stored in `.devme/overrides.toml`. Visible in TUI and `devme overrides list`. Created via the failure overlay's `i` action. Cleared per-step or wholesale via `devme health --recheck`. Used when the check is wrong (the dep is satisfied via a path our check can't see) or when the user has chosen to assert satisfaction manually.

### Optional dependency

A [[Service]]'s `depends_on` edge marked with `?` (e.g. `depends_on = ["db", "proxy?"]`) or `required = false`. The dependent service starts even if the dep is down or failing. Used when the service has a graceful degraded mode.

### Forced start

Runtime override of a `required = true` dep. The service runs even though the dep is in wait state. Status reflects which deps were skipped (e.g. `running (started without proxy)`). Never persists; per-invocation only.

### External service

A [[Service]] with `external = true`. devme never manages its lifecycle, only health-checks it (required `health` field) and optionally tails its log file (optional `log_tail` path). Status surfaces as `external (healthy)` or `external (unreachable)`. Used for infra the user manages outside devme (system postgres, brew-services nginx).

### Daemon

The supervisor process. Two variants:

- **Instance daemon** — One per [[Stack instance]] (one per worktree). Owns the instance's `instance`-scoped services. Listens on `~/.local/share/devme/instances/<id>.sock`. Ref-counts clients (TUI windows, CLI commands, agent processes). Shuts down when ref count hits zero, unless started in detached mode via `devme up`.
- **Shared-services daemon** — One per repo. Spawned on demand by the first [[Instance daemon]] that needs a `repo`-scoped service. Listens on `~/.local/share/devme/repos/<repo-hash>/shared.sock`. Owns all `repo`-scoped services across all instances of that repo. Exits when no instance daemons are attached.

### Client

Any connection to a [[Daemon]] — the TUI, a CLI subcommand, or an agent process. Clients connect over Unix sockets, subscribe to log streams and status updates, and send control messages (start, stop, restart).

### Slot

A small integer (0..9 by default) assigned to a [[Stack instance]] at startup. Used to offset port allocations so multiple worktrees can run their stacks on the same machine without colliding. Frontend port = `5173 + slot * 10`, backend port = `8080 + slot * 10`, etc. Slot 0 keeps the natural defaults. Slots are stable per instance ID across daemon restarts.

### Resource

A bounded pool of scarce runtime capacity allocated to a [[Task]] or [[Session]]. A Resource has host, repo, or worktree scope and exposes its allocated zero-based identifier through an optional environment variable. Examples include simulator identities, emulator identities, and signing access.

### Session

A runtime composition that holds one or more [[Resource]] allocations while its declared [[Service]] closure, sidecars, and optional launch [[Task]] use them. Multiple clients join the same Session idempotently. After the final client disconnects, an optional linger period permits reconnection before sidecars stop and Resources are released.

### Service hold

A runtime ownership claim that keeps a requested [[Service]] target and its required dependency closure active. Concurrent [[Task]]s, [[Session]]s, and explicit runtime commands may hold overlapping closures. Releasing one Service hold never stops a Service that another active hold still requires.

### Task guardian

An exact-binary helper process that owns a foreground [[Task]]'s [[Resource]] lease descriptors and an independent [[Service hold]] or [[Session]] attachment after launch. It verifies the Task process-group and CLI process identities before releasing the start gate. If the CLI disappears, the guardian terminates the full Task group, persists an interrupted result, and releases ownership only after the group exits. On clean completion, it acknowledges the runner's persisted result before either owner releases its hold.

### Instance ID

A hash of the canonical absolute worktree path. Stable: renaming the worktree directory changes the ID, switching branches in a worktree does not. Used as the primary key for slot allocation and socket file naming.

### Wizard

A custom interactive script in `.devme/` that handles complex [[Step]] provisioning beyond a single shell command. Multi-field forms, choice lists with dynamic options, waiting for an external interactive process to complete. Speaks the [[Wizard protocol]] over stdin/stdout.

### Wizard protocol

JSON-lines over stdin/stdout. The wizard writes events to stdout (`ask`, `progress`, `log`, `set_var`, `done`) and reads user responses from stdin. Language-agnostic — any executable that can do JSON works. devme ships a thin Bun SDK at `@devme/wizard-sdk` as a convenience wrapper.

### Service config hash

A hash over a [[Service]]'s effective config (command, env, port). Used to detect when a running `repo`-scoped service is stale relative to what a newly-starting instance expects. Mismatch → the new instance's TUI flags the service as `⚠ stale config` and offers a one-key "restart with new config" action.

### Failure overlay

The TUI modal shown when a [[Step]]'s check fails. Actions: `Enter` (install — run the provision), `r` (retry check), `s` (skip just this run), `i` (mark as installed, create [[Override]]), `q`/`Esc` (cancel).

### Supervisor tab

The first tab inside every [[Stack instance]]'s pane. Synthetic — not a real [[Service]]. Shows the graph traversal status, every [[Step]]'s state, output from the daemon itself, and shared-service status. The "what's happening at the meta level for this instance" view.

## Remote workflow terms

### devcloud

The remote project context adapter. devcloud is separate from devme: it resolves
the current Git repo into a canonical remote project directory, verifies or
creates the remote clone, and runs commands or shells over SSH in that directory.
It does not supervise the [[Stack]], own [[Service]] lifecycle, or manage Herdr,
Codex, or shell sessions.

### Remote project context

The Git-derived identity devcloud uses to connect a local project to a remote
clone. In v1 this starts from the local repo's `origin` remote, derives provider
host plus owner/repo, and maps that identity under the configured remote root.
Git is the source of truth for moving project state between machines.

### On-demand runtime

The devme runtime model where attaching a dashboard or checking status does not
itself imply that services must be awake. Explicit runtime commands wake or keep
the stack awake when they need live services, logs, URLs, or service control.
Passive snapshots do not count as meaningful runtime activity.

### Cloud relay

Superseded design from ADR-0014. It described continuous Mutagen sync, devme-run
remote stack proxying, watchdog takeover, and agent session handoff. Keep the
term for historical discussions only; it is not the active devme/devcloud
boundary.

### Takeover

Superseded cloud relay term for automatic transfer of an agent session from a
local machine to a remote host. Herdr and Codex own session control in the active
design; devme does not resume or transfer their sessions.

### Pull-back

Superseded cloud relay term for transferring an agent session from remote back
to local. In the active design, shell functions, Herdr, Codex, and Git compose
the workflow outside devme.
