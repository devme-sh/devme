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

## Testing the TUI

Unit render tests (ratatui `TestBackend`) cover frame *shape*; they don't
exercise the event loop or input. For end-to-end TUI behavior (keybindings,
modals, scroll), drive it headlessly in **tmux** and assert on the captured
grid — `tmux new-session -d -x W -y H '…devme'`, `send-keys`, `capture-pane -p`,
`grep -F`. The `verify-tui` skill documents the pattern and its pitfalls
(startup races, fixed width, daemon cleanup on EXIT).

- `scripts/tui-smoke.sh` — general TUI smoke (sidebar, pause/scroll, quit).
- `scripts/skill-modal-smoke.sh` — the `devme skill` startup modals
  (install / update / silent auto-update) under an isolated `HOME`.

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

The `devme` agent skill is a `SKILL.md` that teaches coding agents how to drive
the CLI. **The canonical copy lives in this repo at
`crates/config/skill/SKILL.md` and is embedded into the binary at build time**
(`include_str!` in `crates/config/src/skill.rs`), so `devme skill install`
always writes the version matching the running binary — no skill-vs-binary
drift. Users install it either way:

- `devme skill install` (`--global`) — the embedded copy, version-locked to
  their binary, offline, no Node.
- `npx skills add devme-sh/skills` — the published mirror (Vercel skills CLI,
  `github.com/vercel-labs/skills`).

The sibling repo `devme-sh/skills` (local checkout `../skills`,
`skills/devme/SKILL.md`) is a **CI-generated mirror** of the canonical file —
do not edit it by hand; it is overwritten from `crates/config/skill/SKILL.md`
on release.

**Whenever you change CLI mechanics — add/rename/remove a command or flag, or
change a command's output shape — update `crates/config/skill/SKILL.md` in the
same change** (the CLI reference table, the action sections, and the gotchas).
The embedded skill is the executable contract agents rely on; letting it drift
from the binary breaks them silently. The mirror in `../skills` syncs
automatically, so don't touch it directly.
