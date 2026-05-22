# ADR-0008: CLI & TUI conventions (clig.dev + agent-native principles)

**Status**: Accepted
**Date**: 2026-05-23

## Context

devstack has both a CLI surface (used by humans and agents) and a TUI surface (used by humans). The 2025-2026 consensus on CLI design has consolidated around "structured, deterministic, machine-stable interfaces that AI agents can drive" — `--json` everywhere, semantic exit codes, non-interactive defaults, errors that enumerate valid values. We commit to following those principles rather than re-deriving them.

## Decision

### CLI

- **`--json` boolean on every data-returning command.** JSON envelope `{ "schema_version": 1, ... }`. JSONL for streaming.
- **Stdout = data contract; stderr = everything else** (progress, warnings, spinners, headers).
- **Exit codes.** `0` success, `1` general, `2` usage (bad args), `3` not found, `4` permission, `5` conflict. Documented in `devstack errors --list` and unit-tested.
- **Flag conventions.** `--exclude <pattern>` (repeatable, glob) for item filtering. `--skip-<behaviour>` for skipping behaviors. `--no-input` to disable prompts (auto-implied when stdin isn't a TTY). `--yes`/`-y` to bypass confirmations.
- **Color.** Respect `NO_COLOR` (precedence) and `FORCE_COLOR`/`CLICOLOR_FORCE`. TTY-aware by default.
- **Logging.** `-v`/`-q` for ergonomics; `--log-level=trace|debug|info|warn|error` for agents.
- **Subcommands.** Flat top-level verbs while small; promote to noun-verb only when 3+ verbs accumulate on the same noun.
- **Completions.** `devstack completions {bash,zsh,fish,nu}` via `clap_complete`. No man pages.
- **Idempotent mutations.** `devstack restart backend` succeeds whether running or not. `--dry-run` returns a structured JSON diff, not prose.
- **`devstack agent-context`.** Machine-readable manifest of every command, flag, exit code, and JSON schema.

### TUI

- **Layout.** Lazygit-style fixed three-pane: sidebar (instances, 20 cols) + top tabs (services within focused instance, with `supervisor` synthetic tab always first) + main viewport (live logs) + bottom status bar (always visible, three regions: focus breadcrumb, key hints, global health summary).
- **Bindings.** Modeless. Dual hjkl + arrows + Tab cycles panes. `1-9` jumps to pane index. `Enter` drills in; `Esc` dismisses overlays.
- **Actions.** `r` restart, `s` stop, `S` start, `f` toggle log-follow, `/` search, `n`/`N` next/prev hit, `Ctrl+L` clear viewport, `q` quit (with confirmation if foreground and services running).
- **Discovery.** `?` opens help overlay (every binding from current focus). `Ctrl+P` command palette deferred to v1.1.
- **Mouse.** Click-to-focus, scroll. Not load-bearing — every action reachable via keyboard.
- **Color.** True-color preferred; degrade to 256-color via `supports-color`. Catppuccin-aligned default palette. Built-in themes: `mocha` (dark, default), `latte` (light), `mono` (no color). Respect `NO_COLOR`; provide non-color status glyphs (`✓`/`⚠`/`✗`) so colorblind users see state via shape.

## Consequences

- One umbrella convention document instead of re-justifying each choice in every PR.
- Agents have a stable contract — schema-versioned JSON, semantic exit codes, idempotent mutations.
- The `agent-context` command is the canonical machine-readable description of the CLI; consumers don't have to scrape `--help` output.
- We commit to schema stability. Breaking changes go through a major version bump.

## Alternatives considered

**Roll our own conventions.** Reinvents the wheel and produces inconsistencies with the rest of the modern Rust CLI ecosystem (uv, ruff, mise, just). Out.

**MCP-style dynamic discovery.** Covered in ADR-0004. The `agent-context` static manifest covers the same need with zero extra runtime.

**Vim-style modal TUI.** Useful for editor-like tools, friction for supervisors where you're mostly switching panes and reading. Out.
