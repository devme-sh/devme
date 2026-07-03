---
name: devme
description: Manage dev environments with devme. Use when services fail, won't start, crash-loop, show errors, databases are down, Docker isn't running, or user asks "what's wrong", "fix the environment", "check status", "restart", "logs", or mentions devme. Also use for `/devme setup` to generate a devme.toml for a new project.
arguments: [action]
allowed-tools: Bash(devme *) Bash(docker *) Bash(lsof *) Bash(ps *) Bash(find *) Bash(cat *) Bash(ls *) Read Write
---

## devme: $action

Route based on the requested action. If no action is given, default to diagnostics.

---

### If action is "setup" — generate devme.toml

Analyze the project in the current directory and generate a `devme.toml` config file.

**Step 1: Detect project structure.** Look at:
- `package.json` (Node/Bun — check scripts.dev for the dev command, check for drizzle/prisma)
- `Cargo.toml` (Rust)
- `pyproject.toml` / `requirements.txt` (Python — check for django, flask, fastapi)
- `go.mod` (Go)
- `docker-compose.yml` / `compose.yml` (existing Docker services to model)
- `Dockerfile` (containerized app)
- `.env` / `.env.example` (environment variables to declare)
- Database references in config files (postgres, mysql, redis, mongo)

**Step 2: Generate `devme.toml`.** Follow this schema:

```toml
schema_version = 1

# Env vars — declare expected variables with defaults, choices, or generators.
# devme prompts for missing values on first run and writes them to .env.local.
[env.DATABASE_URL]
required = true
default = "postgresql://user:pass@localhost:5432/mydb"
help = "Connection string for the dev database"

[env.SECRET_KEY]
generate = "openssl rand -hex 32"
help = "Signing key. Auto-generated on first run."

[env.REGION]
choices = ["us-east-1", "eu-west-1"]
default = "eu-west-1"

# Steps — prerequisites checked before services start.
# check: command that returns 0 on success.
# provision: command to auto-fix when check fails.
# trust = "auto": run provision without prompting.
[step.bun]
check = "command -v bun"
provision = "curl -fsSL https://bun.sh/install | bash"
description = "Bun runtime"

[step.deps]
check = "test -d node_modules"
provision = "bun install"
trust = "auto"
depends_on = ["bun"]
description = "Install dependencies"

# Services — long-running processes. Use {port} for slot-aware port allocation.
[service.postgres]
cmd = "docker rm -f myapp-pg 2>/dev/null; docker run --rm --name myapp-pg -e POSTGRES_USER=dev -e POSTGRES_PASSWORD=dev -e POSTGRES_DB=mydb -p {port}:5432 postgres:17-alpine"
port = { base = 5432, slot_offset = 10 }

[service.web]
cmd = "bun run dev"
port = { base = 3000, slot_offset = 10 }
depends_on = ["deps"]
```

**Rules:**
- Always start with `schema_version = 1`
- Use `bun` for JS/TS projects (not npm/node)
- Docker services: always prefix cmd with `docker rm -f <name> 2>/dev/null;` to handle stale containers
- Docker services: always use `--rm --name <project>-<service>`
- Use `{port}` interpolation with `port = { base = <default>, slot_offset = 10 }`
- Steps that install deps should use `trust = "auto"` and depend on the runtime step
- Database migration steps should depend on both `deps` and the database service
- Scan `.env.example` or `.env` for variables to declare in `[env.*]`
- Add `help` to env vars so the user knows where to find the value
- Use `generate` for secrets that can be auto-created
- Use `choices` for values with a known set of options

**Step 3:** Write the file and explain what was generated.

---

### If action is "doctor" or empty — diagnose and fix

**Step 1:** Run `devme doctor --tail 30`. Parse the JSON — it has every service's state, pid, port, restart count, and recent log lines.

**Step 2:** Focus on `Failed` or `CrashLoop` services. Read their `logs` array for errors.

**Step 3:** Fix the root cause:
- **Container name conflict** ("already in use"): `docker rm -f <name>`, then `devme restart <service>`
- **Port conflict** ("address already in use"): `lsof -ti :<port> | xargs kill -9`, then `devme restart <service>`
- **Docker not running**: `devme config set docker.daemon orbstack`, then `devme up -d`
- **Step failed**: read the step's logs, fix the issue, `devme restart <dependent>`
- **Crash loop**: check logs for root error, fix cause, `devme restart <service>`

**Step 4:** Run `devme doctor --tail 10` to confirm everything is healthy.

---

### If action is "logs" — read service logs

Run `devme logs <service> --tail 100` for the service the user asks about. If they don't specify, run `devme doctor --tail 20` to see all services and their recent output.

---

### CLI reference

| Command | Purpose |
|---------|---------|
| `devme doctor --tail N` | JSON diagnostic: states + last N log lines per service |
| `devme status` | One-line-per-service status |
| `devme logs <svc> --tail N` | Last N lines of a service |
| `devme restart <svc>` | Restart a service |
| `devme start <svc>` | Start a stopped service |
| `devme stop <svc>` | Stop a service |
| `devme up -d` | Start everything detached |
| `devme down` | Stop everything |
| `devme config` | Show global config |
| `devme config set <key> <val>` | Set a config value |

### Gotchas

- `devme doctor` returns JSON — summarize for the user, don't dump raw.
- Services have dependency ordering. Restarting a database may cascade.
- Step states (`Passed`/`Failed`) gate dependent services.
- Docker `--name` containers need `docker rm -f` cleanup on restart.
- If no daemon: `devme doctor` returns `status: "no_daemon"` — tell user to run `devme up -d`.
