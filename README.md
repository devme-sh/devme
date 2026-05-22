# devstack

> Working name. May change before v1.0 — see `docs/adr/` for current decisions.

A dev-stack supervisor for projects with multiple services and multiple git worktrees. Spawns, monitors, restarts, and tails logs from your backend, frontend, database, proxy, and any other services declared in `devstack.toml`. Each git worktree gets its own coexisting stack instance with non-colliding port allocations.

Designed to be agent-friendly: every command supports `--json`, exit codes are semantic, and an `agent-context` subcommand emits a machine-readable manifest of the CLI surface.

## Status

Early development. No public release yet. Design captured in [`CONTEXT.md`](./CONTEXT.md) and [`docs/adr/`](./docs/adr/).

## Project structure

```
crates/
  core/             types shared by every crate
  config/           devstack.toml parsing + validation
  supervisor/       per-instance daemon binary
  shared-supervisor/  per-repo shared-services daemon binary
  client/           IPC client library (used by tui + cli)
  tui/              ratatui TUI binary
  cli/              command-line surface
docs/
  adr/              architectural decisions (numbered, append-only)
CONTEXT.md          domain glossary
```

## Development

Requires Rust 1.89+ (for stdlib `File::lock`).

```
cargo build
cargo nextest run
cargo clippy --all-targets
```

Eventually devstack will manage its own dev workflow (test watch, lint watch, docs serve) once the supervisor is functional — see "Dogfooding" in `CONTEXT.md`.

## Future work

- Disk-based check caching with TTL invalidation (v1 ships with in-memory dedup only)
- Self-update via `devstack upgrade` (v1 ships with curl-pipe-sh re-run as the update path)
- Command palette (`Ctrl+P`) — deferred to v1.1 unless the command surface grows
- MCP server endpoint for dynamic discovery — deferred indefinitely (skill + CLI is the canonical agent surface)
- Windows native support (WSL is the v1 path)

## License

TBD before public release.
