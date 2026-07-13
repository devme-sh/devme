<p align="center">
  <img src="https://devme.sh/logo.png" alt="devme" width="200">
</p>

<h3 align="center">Your dev stack, supervised.</h3>

<p align="center">
  Multi-service dev environments that just work. Across worktrees, without Docker.
</p>

<p align="center">
  <a href="https://devme.sh">Website</a> · <a href="#quick-start">Quick Start</a> · <a href="./docs/adr/">Architecture Decisions</a>
</p>

---

devme spawns, monitors, restarts, and tails logs from every service in your project. Backend, frontend, database, proxy, whatever you've got. Declare them in a single `devme.toml` and each git worktree gets its own coexisting stack with non-colliding ports.

<!-- TODO: Replace with VHS-generated GIF of `devme up` starting services + TUI -->
<!-- ![devme TUI demo](assets/demo.gif) -->

## Why devme?

Running a modern project means juggling 3-8 services. Open five terminals, remember the right startup order, hope nothing grabs the wrong port. Now multiply that by worktrees.

devme fixes this. One command, `devme up`, starts everything in dependency order with health checks. Each worktree gets its own port slot, so `main` and `feature-branch` run side by side without collisions. There's a TUI dashboard for real-time status and logs. Every command supports `--json` and semantic exit codes, so AI agents can drive it too. No Docker required.

Setup steps have trust levels (`auto`, `prompt`, `manual`) so dependencies get provisioned safely.

## Quick Start

```bash
cd my-project
devme setup --write # generates devme.toml from supported project markers
devme up            # starts everything
```

## Configuration

```toml
# devme.toml
[service.backend]
cmd = "cargo watch -x run"
port = { base = 8080, slot_offset = 10 }
health = { http = "http://localhost:{port}/health" }
readiness = { interval_ms = 500, timeout_ms = 2000, retries = 60 }

[service.frontend]
cmd = "bun run dev"
port = { base = 5173, slot_offset = 10 }
depends_on = ["backend"]

[task.test]
cmd = "cargo test"
services = ["backend"]
timeout = 300
```

Ports automatically offset per worktree slot. Slot 0 keeps defaults, slot 1 gets `+10`, and so on.
One-shot tasks run through `devme run <name>` and delegate to the project's
authoritative native tools. See [`examples/native-mobile-monorepo`](./examples/native-mobile-monorepo/)
for backend, iOS, and Android orchestration with scoped runtime leases.

Coding agents can opt into compact session context with
`devme agent setup --target <claude|codex|opencode|all>`, inspect it with `devme agent status`, and
remove it with `devme agent remove`. Hooks are never installed implicitly.
The separate `devme skill install` path uses guidance embedded from the same
canonical source as `devme agent context`.

## How It Compares

| | devme | docker-compose | process-compose | Procfile (foreman) |
|---|:---:|:---:|:---:|:---:|
| Worktree-aware ports | :white_check_mark: | :x: | :x: | :x: |
| Dependency graph | :white_check_mark: | :white_check_mark: | :white_check_mark: | :x: |
| Health checks | :white_check_mark: | :white_check_mark: | :white_check_mark: | :x: |
| TUI dashboard | :white_check_mark: | :x: | :white_check_mark: | :x: |
| Agent/AI interface | :white_check_mark: | :x: | :x: | :x: |
| Docker-free | :white_check_mark: | :x: | :white_check_mark: | :white_check_mark: |
| Setup step provisioning | :white_check_mark: | :x: | :x: | :x: |

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

Two-tier daemon architecture. An instance daemon per worktree manages instance-scoped services, while a shared-services daemon per repo handles things like a cloud SQL proxy that multiple worktrees need.

## Development

Requires Rust 1.89+ (for stdlib `File::lock`).

```bash
cargo build
cargo nextest run
cargo clippy --all-targets
```

<details>
<summary>Design documentation</summary>

- [`CONTEXT.md`](./CONTEXT.md): Domain glossary and invariants
- [`docs/adr/`](./docs/adr/): Architectural decisions (numbered, append-only)

</details>

## Status

Early development, not yet published. Design is captured and implementation is progressing through the crate structure above. Contributions welcome once the core stabilizes.

## License

[MIT](./LICENSE)
