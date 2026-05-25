<p align="center">
  <img src="https://devme.sh/logo.png" alt="devme" width="200">
</p>

<h3 align="center">Your dev stack, supervised.</h3>

<p align="center">
  Multi-service dev environments that just work — across worktrees, without Docker.
</p>

<p align="center">
  <a href="https://devme.sh">Website</a> · <a href="#quick-start">Quick Start</a> · <a href="./docs/adr/">Architecture Decisions</a>
</p>

---

devme spawns, monitors, restarts, and tails logs from every service in your project — backend, frontend, database, proxy — declared in a single `devme.toml`. Each git worktree gets its own coexisting stack with automatically non-colliding ports.

<!-- TODO: Replace with VHS-generated GIF of `devme up` starting services + TUI -->
<!-- ![devme TUI demo](assets/demo.gif) -->

## Why devme?

Running a modern project means juggling 3–8 services. Open five terminals, remember the right order, hope nothing grabs the wrong port. Now multiply that by worktrees.

devme fixes this:

- **One command** — `devme up` starts everything in dependency order with health checks
- **Worktree-aware** — Slot-based port allocation means `main` and `feature-branch` run side by side without collisions
- **TUI dashboard** — Real-time status, logs, and controls for every service
- **Agent-friendly** — Every command supports `--json`, exit codes are semantic, `devme agent-context` emits a machine-readable manifest
- **No Docker required** — Supervises native processes directly
- **Smart provisioning** — Steps with trust levels (`auto`/`prompt`/`manual`) handle setup dependencies safely

## Quick Start

```bash
cd my-project
devme init          # generates devme.toml from your project
devme up            # starts everything
```

## Configuration

```toml
# devme.toml
[services.backend]
command = "cargo watch -x run"
port = 8080
health = "http://localhost:{{port}}/health"

[services.frontend]
command = "npm run dev"
port = 5173
depends_on = ["backend"]

[services.db]
command = "docker compose up postgres"
port = 5432
health = { command = "pg_isready -p {{port}}" }
```

Ports automatically offset per worktree slot — slot 0 keeps defaults, slot 1 gets `+10`, etc.

## How It Compares

| | devme | docker-compose | process-compose | Procfile (foreman) |
|---|---|---|---|---|
| Worktree-aware ports | Yes | No | No | No |
| Dependency graph | Yes | Yes | Yes | No |
| Health checks | Yes | Yes | Yes | No |
| TUI | Yes | No | Yes | No |
| Agent/AI interface | Yes | No | No | No |
| Requires Docker | No | Yes | No | No |
| Setup step provisioning | Yes | No | No | No |

## Architecture

```
crates/
  core/              Shared types
  config/            devme.toml parsing + validation
  slot-allocator/    Port offset allocation
  executor/          Process spawning and lifecycle
  ipc/               Unix socket protocol
  supervisor/        Per-worktree daemon
  shared-supervisor/ Per-repo shared-services daemon
  client/            IPC client library
  tui/               Ratatui terminal UI
  cli/               CLI surface (clap)
```

Two-tier daemon architecture: an **instance daemon** per worktree manages instance-scoped services, while a **shared-services daemon** per repo handles services shared across worktrees (e.g., a cloud SQL proxy).

## Development

Requires Rust 1.89+ (for stdlib `File::lock`).

```bash
cargo build
cargo nextest run
cargo clippy --all-targets
```

<details>
<summary>Design documentation</summary>

- [`CONTEXT.md`](./CONTEXT.md) — Domain glossary and invariants
- [`docs/adr/`](./docs/adr/) — Architectural decisions (numbered, append-only)

</details>

## Status

Early development — not yet published. The design is captured and implementation is progressing through the crate structure above. Contributions welcome once the core stabilizes.

## License

TBD before public release.
