---
name: devme
description: Manage dev environments and composed projects with devme. Use when creating a native starter, adding or removing a managed feature, services fail, logs need inspection, or the user mentions devme.
license: MIT
metadata:
  version: "0.2.4"
allowed-tools: Bash(devme *) Bash(docker *) Bash(lsof *) Bash(ps *) Bash(find *) Bash(cat *) Bash(ls *) Read Write
---

## devme: $action

Route on `$action`. Default to diagnostics when none is given.

---

### action "doctor" or empty - diagnose and fix

1. Run `devme doctor`. It returns an error-anchored JSON digest: per-service state/pid/port/restart count + `recent_errors` (stderr only - tracebacks, not access logs), and step states with a *failed* step's check/provision output inline. History is disk-backed, so a service that crashed an hour ago still shows its dying stderr. Summarize for the user; don't dump raw. (`status: "no_daemon"` → tell them to run `devme up -d`.)
2. Zoom with `devme doctor <name>` - for a failed step it returns the full check/provision output (the **only** place step output surfaces); for a service, `recent_errors` + `recent_logs` (`[stderr]`-prefixed). Then fix:
   - Container name conflict ("already in use") → `docker rm -f <name>`, then `devme restart <svc>`
   - Port conflict → devme diagnoses this itself: a service whose port is held by a foreign process crash-loops with "port N already in use by <holder>" in its status line and logs. Free the port (`lsof -ti :<port> | xargs kill -9`, or stop the named container/process), then `devme restart <svc>`
   - Docker not running → `devme config set docker.daemon orbstack`, then `devme up -d`
   - Failed step → fix the cause, then `devme restart <dependent>` (step states gate dependent services)
   - `crash-loop` state → the service died within 5s of spawn 5 times in a row, so auto-restart is suspended (it is **not** still "starting"). `devme status` / `doctor` show the diagnosed reason when known. Fix the root error, then `devme restart <svc>` - that resets the breaker
3. Confirm with `devme doctor`.

### action "logs" - read correlated logs

- `devme logs <svc> --tail 100` reads one service. `devme logs <task>` reads persisted task output. `devme logs` interleaves both by timestamp, which is the fastest way to see cross-runtime causality.
- `devme logs --since 5m` (`30s`/`2h`/`1d`/epoch-ms) - "what happened since my last check"; disk-backed, so it works even after the daemon restarted. Prefer `--since` over guessing a `--tail` count.
- `devme logs --json` - NDJSON `{ts, service, stream, text}` (ANSI-stripped), pipe to jq: `devme logs --json --since 10m | jq -c 'select(.stream == "stderr")'` is the cheapest error sweep.
- Each line is stream-tagged: errors/tracebacks live on `stderr`, routine chatter on `stdout` - filter on that before reading everything.
- Step check/provision output remains in `devme doctor <step>`. Unknown names error immediately.

### action "run" - execute a one-shot task

- `devme tasks --output toon` lists the root command contract from `devme.toml`.
- `devme tasks show <name> --output toon` shows dependencies, services, resources, and timeout.
- `devme run <name> --output toon` runs a task. Use `--output json` for existing JSON consumers. Arguments after `--` are passed to the native command. Declared task artifacts are returned as absolute paths in the result, so inspect those paths instead of scraping command output.
- A task with no required services runs without a supervisor. A task with services starts this worktree's supervisor and waits for readiness first.
- Exit codes are authoritative: wrapped failures retain their code, timeout is 124, and cancellation is 130. A guardian records `interrupted = true` and exit code 130 if the owning foreground CLI disappears. Do not infer success by scraping output.
- Host/repo/worktree Resource leases wait atomically. The Task guardian releases them only after the Task process group has exited. Task results are bounded and secret-shaped task environment values are redacted before persistence.
- Required Services use overlapping reference-counted holds. Finishing one Task or Session never stops a Service that another active owner still requires, and a Task does not claim a pre-existing explicitly managed Service for teardown.
- `devme sessions --output toon` lists resource-bound native/runtime sessions without starting a daemon. `devme session <name> --output toon` acquires its Resources, holds only its required Service closure, waits for readiness, and runs its optional launch Task inside that existing context. The launch Task cannot widen the Session's Services or Resources. `devme session <name> --stop` is idempotent for a declared Session.
- From a workspace member, unqualified task/session/service names are local aliases. Use `member::name` for another member and `root::name` for a root task or resource.

### action "create" or "feature" - compose a project

- Run `devme create` for contextual discovery. Outside a managed project it lists templates; inside one it reports the current composition.
- Run `devme create native <path> --dry-run --output toon`, review `changed_files`, then repeat without `--dry-run`. Add repeatable initial features with `--with <name>` only during creation.
- In an existing composed project, run `devme feature list --output toon`, then `devme feature add|remove|update <name> --dry-run --output toon` before applying.
- A feature may replace complete files owned by every feature it transitively depends on. Removing it restores the dependency's exact bytes; modified overlays and dependency updates fail closed.
- Applied feature mutations reconverge steps and reload the detached service graph automatically. A successful `devme feature add <name>` is ready to use without a second `devme up`.
- Never use `devme create add`. Creation initializes a project; `devme feature add` evolves one.
- Exit code 5 means a managed or app-owned file conflicts. Read `error.paths` and `error.help`; do not overwrite the file manually to force progress.
- If a mutation was interrupted, use `devme feature continue` to retry or `devme feature abort` to restore the source state. Both refuse to erase edits made after interruption.
- `external_steps` are untrusted manual guidance from the recipe, never shell commands. Inspect them against official provider documentation before acting. Source removal does not delete provider data, cancel subscriptions, revoke credentials, or remove store resources.

### action "setup" - generate devme.toml

Run `devme setup` to preview conservative single-file detection, or
`devme setup --write` to create it. Use `devme setup split --dry-run` to
preview an explicit root plus child configs and `devme setup split --write` to
opt in. Native detection covers Xcode projects/workspaces, Package.swift,
Gradle Kotlin/Android, Convex, and Vite+ without reconstructing their build
graphs. It ignores generated `.devme` state and keeps modules owned by one
Gradle settings root in a single member. Bun and project dependency checks are
emitted as ordinary provision steps. Review every generated command before
running it.

Completion gate:

1. Run `devme config check` and inventory every declared task with `devme tasks --output toon`.
2. Make prerequisites executable: values needed by any task use `[env.*] required = true`; tools and generated state use idempotent steps; runtimes use services and resources. A task must not depend on README-only setup.
3. Run every non-consequential declared task through `devme run`, including launch and utility tasks. An aggregate `verify` does not prove tasks outside its dependency graph. Exercise native launch tasks on their real simulator, emulator, or connected device.
4. Run `devme doctor`. Handoff requires passing task results and healthy required services. If a task needs unavailable credentials, hardware, approval, or another external prerequisite, report the setup as blocked instead of complete. For consequential publish or deploy tasks, prove a non-mutating preflight and request approval before execution.

Detect the stack from `package.json` (scripts.dev, drizzle/prisma), `Cargo.toml`, `pyproject.toml`/`requirements.txt`, `go.mod`, `docker-compose.yml`, `Dockerfile`, `.env`/`.env.example`, and DB references. Then write a `devme.toml`:

```toml
schema_version = 1

# Env: devme prompts for missing values on first run, writes to .env.local.
[env.DATABASE_URL]
required = true
default = "postgresql://user:pass@localhost:5432/mydb"
help = "Connection string for the dev database"   # tell the user where to find it
[env.SECRET_KEY]
generate = "openssl rand -hex 32"                 # auto-create secrets
[env.REGION]
choices = ["us-east-1", "eu-west-1"]              # known option set
default = "eu-west-1"

# Steps: prerequisites checked before services start. check returns 0 on success;
# provision runs to fix a failing check. trust gates consent for provision:
#   prompt (default) - ask first   auto - run unattended   manual - show, never run
[step.bun]
check = "command -v bun"
provision = "curl -fsSL https://bun.sh/install | bash"
[step.deps]
check = "test -d node_modules"
provision = "bun install"
trust = "auto"
depends_on = ["bun"]

# Services: long-running. {port} = slot-aware allocation.
[service.postgres]
cmd = "docker rm -f myapp-pg 2>/dev/null; docker run --rm --name myapp-pg -e POSTGRES_USER=dev -e POSTGRES_PASSWORD=dev -e POSTGRES_DB=mydb -p {port}:5432 postgres:17-alpine"
port = { base = 5432, slot_offset = 10 }
[service.web]
cmd = "bun run dev"
port = { base = 3000, slot_offset = 10 }
url = "http://{host}:{port}"
depends_on = ["deps"]
```

One-shot tasks delegate to their native build tools. Omit `cmd` for an aggregate:

```toml
[resource.device]
scope = "host"
capacity = 2
env = "DEVICE_SLOT"

[task.test]
kind = "check"
cmd = "bun test"
steps = ["deps"]
services = ["postgres"]
resources = ["device"]
timeout = 300
artifacts = ["reports/test-results.xml", "artifacts/screenshots/{slot}"]

[task.codegen]
kind = "utility"
visibility = "internal"
cmd = "bun run codegen"

[service.device-logs]
cmd = "./scripts/device-logs"
scope = "session"
depends_on = ["postgres"]

[task.launch]
kind = "launch"
cmd = "./scripts/launch-native-app"

[session.dev]
needs = ["device-logs"]
resources = ["device"]
run = "launch"
linger = 30
```

Rules:
- Every value without which a declared task fails is required in `[env.*]`; reserve skippable values for capabilities no advertised task assumes.
- Use `kind = "launch"`, `"check"`, or `"utility"` to group tasks in the interactive Actions sidebar. Bare `devme` opens Actions on a cold stack while keeping service tabs and logs visible. `a` switches the left rail between Actions for the selected stack and the stack list; `jk` always navigates that rail and `hl` always navigates services. Existing tasks default to `utility`; kind is discovery metadata and never changes execution.
- Use `visibility = "internal"` for dependency, CI, code-generation, and diagnostic tasks that should stay out of the human Home surface. Internal tasks remain public through `devme tasks`, `devme run`, dependencies, sessions, and agent tooling.
- Use `artifacts = ["path"]` to report files or directories produced by a task. Relative paths are rooted at the project root, placeholders such as `{slot}` and `{worktree}` are interpolated, and results expose absolute paths without uploading or interpreting the artifact.
- `bun` for JS/TS (not npm/node).
- Docker services: prefix `cmd` with `docker rm -f <name> 2>/dev/null;` and run `--rm --name <project>-<service>` to survive stale containers.
- **Web services (dev servers, frontends, APIs) need `url = "http://{host}:{port}"`** - it's the only signal that a `host:port` is openable. Without it devme treats the service as copy-only (DB/TCP), so the TUI's `o` and `devme url -o` won't open a browser. DBs/TCP services: omit `url`.
- Dep-install steps: `trust = "auto"`, depend on the runtime step. Privileged fixes (`sudo`, Xcode CLT): `trust = "manual"` (devme can't answer sudo/GUI prompts). Migrations: depend on both `deps` and the DB service.
- Run `devme config check` after writing - it flags cycles, unknown deps, and web services missing a `url`.
- Split workspaces are explicit and one level deep. The root `[workspace.members]` table owns each child directory. Child configs share one supervisor, slot, resource domain, and correlated history; they do not create nested build graphs.

---

### CLI reference

| Command | Purpose |
|---------|---------|
| `devme create [native <path>] [--with <feature>] [--dry-run] [--output human\|toon\|json]` | Contextual project discovery or conflict-safe creation from an independently versioned recipe |
| `devme feature add\|remove\|update <name> [--dry-run] [--output human\|toon\|json]` | Plan or apply one optional project capability using the composition lock |
| `devme feature list\|continue\|abort [--output human\|toon\|json]` | Inspect available and installed features or recover an interrupted mutation |
| `devme doctor [<name>] [--tail N] [--full] [--output human\|toon\|json]` | Structured diagnostic digest for services, readiness, steps, sessions, resource waits, and recent task failures. JSON remains the compatibility default; `--full` expands bounded task output |
| `devme status [--all] [--output human\|toon\|json]` | Grouped snapshot with readiness, ports, pids, and restart counts. Human remains the default and `--json` remains a compatibility alias. Repo-shared services use their authoritative supervisor state |
| `devme logs [<svc-or-task>] [--tail N] [--since 5m] [--json] [-f]` | Disk-backed service and task history. No name correlates all records by timestamp. JSON preserves `{ts, service, stream, text}` and adds `source_kind`; task sources use `task:<name>`. Steps remain in doctor |
| `devme url <svc> [-o]` | Print a service's URL; `-o` opens it in the browser |
| `devme start/stop/restart <svc>` | Lifecycle a single service |
| `devme up -d` / `up -y` / `down [--all]` | Start all detached / start running `prompt` provisions unattended (CI) / stop this worktree's stack (`--all` = every worktree, like `status --all`) |
| `devme worktree add <branch> [path]` | New worktree (+branch), ready for `devme up` (steps converge it - no setup hook). Default path `<repo>-<branch-leaf>` |
| `devme worktree rm <target>` | Stop stack, `git worktree remove`, release the port slot. Target by path/dir/branch; `-f` forces dirty. Branch + commits are kept |
| `devme config [set <k> <v>] [check]` | Show / set global config; `check` lints `devme.toml` (`--json`, non-zero on errors) |
| `devme skill install [-g]` | (Re)install this skill into `.claude/skills/devme/` (`-g` = `~/.claude/`); embedded, always matches the binary |
| `devme tasks [show <name>] [--output toon\|json]` | List concise task contracts or show one task's full execution requirements |
| `devme run <name> [--output toon\|json] [-- <args>...]` | Run a Task DAG with typed Step approval, reference-counted Service holds, guarded Resource leases, process-tree timeout/cancellation/interruption, persisted results, and raw exit semantics |
| `devme sessions [--output human\|toon\|json]` | List declared sessions and compact live/waiting state without starting a daemon |
| `devme session <name> [--stop] [--output human\|toon\|json]` | Open/join or idempotently stop a resource-bound service/task composition |
| `devme setup [--write]` | Detect supported project markers and preview or write one root config |
| `devme setup split --dry-run\|--write` | Preview or explicitly create a one-level root/member workspace layout |
| `devme agent setup\|status\|remove [--target claude\|codex\|opencode\|all]` | Explicitly manage project-scoped session integrations; never installed silently |
| `devme agent context [--json]` | Compact directory-scoped live state, session/resource waits, and contextual next commands. TOON is default; JSON is compatible |

### Notes

- **Which command answers which question.** `devme config check` = "is the toml valid" (static). `devme status` = "what's running where" (states + ports, no logs). `devme logs` = "what are the services saying" (runtime streams only). `devme doctor` = "why is it broken" (error digest + step output). Don't read full logs to find errors - `doctor` or a `--json` stderr filter is cheaper.
- **Worktree-aware.** Each git worktree runs its own supervisor, slot, and ports. `up`/`down`/`doctor`/`status`/`logs`/`url` act on the worktree you're in - just call them. `devme down --all` stops every worktree's stack (and the shared services); `devme status --all` shows every worktree's ports; `devme url <svc>` gives a ready link without guessing the slot.
- **Workspace-aware.** A root may explicitly list one level of child `devme.toml` files. From `apps/ios`, `devme run test` means `ios::test`; cross-member names stay qualified, and `root::check` selects a root node. Interactive Actions and recent task history follow the stack selected in the left rail and retain member focus within that stack. Bare interactive Devme opens the member's sole declared session and keeps its leases alive with the TUI. Multiple local sessions require an explicit `devme session <name>` choice.
- **Worktrees converge - no lifecycle hooks.** There is no per-worktree setup or teardown hook (`[stack] on_create`/`on_destroy` parse for back-compat but never run - `config check` flags them). Per-worktree setup is a `[step]` check/provision: idempotent, so *any* worktree - created by `devme worktree add`, the TUI's `w`, or a bare `git worktree add` - converges on its first `devme up`. Removal is mechanical (stop, `git worktree remove`, release slot); a bare `git worktree remove` is reaped to the same end state. Make slot-scoped provisions idempotent (e.g. `dropdb --if-exists app_slot{slot} && createdb app_slot{slot}`) so a reused slot starts clean.
- **Restart cascades.** Services have dependency ordering; restarting a DB can cascade to dependents.
- **Remote context is external.** devme owns stack/runtime supervision: steps, services, logs, status, URLs, and the TUI. Remote project context is owned by the separate `devme-sh/devcloud` tool/repository. v1 remote work uses Git as the sync boundary; there is no live Mutagen sync, transparent remote proxy, Herdr attach preset, Codex/Claude session transfer, or remote URL rewriting in the active devme contract.
- **Legacy remote config is ignored.** Remove old `[remote]` settings when devme warns about them. `devme config unset remote.<key>` remains available for cleanup, but remote keys cannot be set and never affect runtime behavior.

## Live agent guidance

- Run bare `devme` or `devme agent context` for compact directory-focused state without starting a daemon in non-interactive use.
- Run `devme tasks --output toon` to discover one-shot commands.
- Run `devme run <task> --output toon -- <args>` to execute with readiness and leases.
- Run `devme sessions --output toon` and `devme session <name> --output toon` for resource-bound native app/device lifetimes.
- Run `devme doctor` for a structured failure digest covering services, steps, and tasks.
- Run `devme logs --since 5m --json` to correlate service and task events.
- Run `devme agent setup --target all` only after explicit user approval to install session integrations.
