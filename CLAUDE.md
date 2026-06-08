# devme

Rust CLI tool — "the executable README". Monorepo with workspace crates.

## Architecture

- `crates/cli` — main `devme` binary, entry point
- `crates/supervisor` — `devme-supervisor` daemon, manages processes
- `crates/tui` — `devme-tui` terminal UI
- `crates/shared-supervisor` — `devme-shared-supervisor` for repo-scoped services
- `crates/config` — devme.toml parsing (Stack, EnvVar, etc.)
- `crates/executor` — step/service execution
- `crates/client` — IPC client to supervisor
- `crates/ipc` — IPC protocol
- `crates/core` — shared types
- `crates/slot-allocator` — port slot management

## Building

```sh
cargo build --release        # all binaries
cargo install --path crates/cli  # install devme to ~/.cargo/bin
```

## Releasing

Use `/release` to create a new release. This bumps the version, tags, and pushes.
The CI workflow builds for 3 targets (linux x86_64, linux aarch64, macOS aarch64)
and auto-updates the Homebrew formula.

macOS Intel is not built separately — Rosetta 2 runs the ARM binary.

## Distribution

- **curl**: `curl -fsSL https://devme.sh/install | sh` (proxied via web app)
- **brew**: `brew install devme-sh/tap/devme`
- Source: `install.sh` in repo root
- Formula: `devme-sh/homebrew-tap`
- CI secret: `HOMEBREW_TAP_TOKEN` (fine-grained PAT scoped to homebrew-tap repo)

## Agent skill

The `devme` agent skill lives in the sibling repo `devme-sh/skills` (local
checkout: `../skills`, installed by users via `npx skills add devme-sh/skills`;
Vercel skills CLI — `github.com/vercel-labs/skills`). Its `SKILL.md` documents
the CLI surface (commands, flags, output) that agents drive.

**Whenever you change CLI mechanics — add/rename/remove a command or flag, or
change a command's output shape — update `../skills/skills/devme/SKILL.md` in
the same change** (the CLI reference table, the action sections, and the
gotchas). The skill is the executable contract agents rely on; letting it drift
from the binary breaks them silently.
