# devstack — Domain Context

devstack supervises multi-service dev environments. One running copy supervises one git worktree (or one non-git project). A user with multiple worktrees of the same repo runs multiple coexisting devstack instances on the same machine without port collisions or config conflicts. The CLI is designed to be driven by AI coding agents as well as humans.

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

### Scope

A property of every [[Step]] and [[Service]]:

- `instance` (default) — One copy per [[Stack instance]]. Backend, frontend, db.
- `repo` — One copy per repo, shared across all [[Stack instance]]s of that repo. Cloud SQL proxy.

A third scope, machine-wide, is declared only in the [[User global config]] — never in the [[Repo config]].

### Repo config

`devstack.toml` at the root of a repo. Branch-local (checked into git). Declares the [[Stack]] — every [[Step]] and [[Service]] for that repo. May reference machine-level dependencies via abstract checks (e.g. `docker info`) without naming how they are provided.

### User global config

`~/.config/devstack/global.toml`. User-level. Declares machine-wide [[Step]]s and [[Service]]s and tool preferences. Resolves abstract dependencies declared in [[Repo config]] — for example, the user picks OrbStack as their Docker provider here.

### Trust level

Per-[[Step]] consent policy for running its `provision` command:

- `auto` — Run without asking. Safe operations only (mkdir, touch, generating local files).
- `prompt` (default) — Ask before running. Anything that mutates the system, installs packages, or hits the network.
- `manual` — Never auto-run. Display the suggested command; let the user execute it.

The global `--yes` flag promotes every `prompt` step to `auto` for a single invocation.

### Override

A user-asserted bypass of a [[Step]]'s `check`. Stored in `.devstack/overrides.toml`. Visible in TUI and `devstack overrides list`. Created via the failure overlay's `i` action. Cleared per-step or wholesale via `devstack health --recheck`. Used when the check is wrong (the dep is satisfied via a path our check can't see) or when the user has chosen to assert satisfaction manually.

### Optional dependency

A [[Service]]'s `depends_on` edge marked with `?` (e.g. `depends_on = ["db", "proxy?"]`) or `required = false`. The dependent service starts even if the dep is down or failing. Used when the service has a graceful degraded mode.

### Forced start

Runtime override of a `required = true` dep. The service runs even though the dep is in wait state. Status reflects which deps were skipped (e.g. `running (started without proxy)`). Never persists; per-invocation only.

### External service

A [[Service]] with `external = true`. devstack never manages its lifecycle, only health-checks it (required `health` field) and optionally tails its log file (optional `log_tail` path). Status surfaces as `external (healthy)` or `external (unreachable)`. Used for infra the user manages outside devstack (system postgres, brew-services nginx).

### Daemon

The supervisor process. Two variants:

- **Instance daemon** — One per [[Stack instance]] (one per worktree). Owns the instance's `instance`-scoped services. Listens on `~/.local/share/devstack/instances/<id>.sock`. Ref-counts clients (TUI windows, CLI commands, agent processes). Shuts down when ref count hits zero, unless started in detached mode via `devstack up`.
- **Shared-services daemon** — One per repo. Spawned on demand by the first [[Instance daemon]] that needs a `repo`-scoped service. Listens on `~/.local/share/devstack/repos/<repo-hash>/shared.sock`. Owns all `repo`-scoped services across all instances of that repo. Exits when no instance daemons are attached.

### Client

Any connection to a [[Daemon]] — the TUI, a CLI subcommand, or an agent process. Clients connect over Unix sockets, subscribe to log streams and status updates, and send control messages (start, stop, restart).

### Slot

A small integer (0..9 by default) assigned to a [[Stack instance]] at startup. Used to offset port allocations so multiple worktrees can run their stacks on the same machine without colliding. Frontend port = `5173 + slot * 10`, backend port = `8080 + slot * 10`, etc. Slot 0 keeps the natural defaults. Slots are stable per instance ID across daemon restarts.

### Instance ID

A hash of the canonical absolute worktree path. Stable: renaming the worktree directory changes the ID, switching branches in a worktree does not. Used as the primary key for slot allocation and socket file naming.

### Wizard

A custom interactive script in `.devstack/` that handles complex [[Step]] provisioning beyond a single shell command. Multi-field forms, choice lists with dynamic options, waiting for an external interactive process to complete. Speaks the [[Wizard protocol]] over stdin/stdout.

### Wizard protocol

JSON-lines over stdin/stdout. The wizard writes events to stdout (`ask`, `progress`, `log`, `set_var`, `done`) and reads user responses from stdin. Language-agnostic — any executable that can do JSON works. devstack ships a thin Bun SDK at `@devstack/wizard-sdk` as a convenience wrapper.

### Service config hash

A hash over a [[Service]]'s effective config (command, env, port). Used to detect when a running `repo`-scoped service is stale relative to what a newly-starting instance expects. Mismatch → the new instance's TUI flags the service as `⚠ stale config` and offers a one-key "restart with new config" action.

### Failure overlay

The TUI modal shown when a [[Step]]'s check fails. Actions: `Enter` (install — run the provision), `r` (retry check), `s` (skip just this run), `i` (mark as installed, create [[Override]]), `q`/`Esc` (cancel).

### Supervisor tab

The first tab inside every [[Stack instance]]'s pane. Synthetic — not a real [[Service]]. Shows the graph traversal status, every [[Step]]'s state, output from the daemon itself, and shared-service status. The "what's happening at the meta level for this instance" view.
