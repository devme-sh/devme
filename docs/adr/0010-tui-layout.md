# ADR-0010: TUI layout & interaction model (lazygit-derived)

**Status**: Accepted (specifics within the conventions umbrella of ADR-0008)
**Date**: 2026-05-23

## Context

The TUI's job is to show every Stack instance's services and their live logs in a way that scales from one worktree to ten, from one service per worktree to ten, on terminals from 40 cols wide to 200+. The 2025-2026 reference for TUI quality is lazygit: fixed multi-pane, never-collapsing layout, modeless dual hjkl/arrow bindings, `?` for help, status bar with context-aware hints.

This ADR records the specific layout and interaction choices (the umbrella conventions are in ADR-0008).

## Decision

### Layout

Three fixed panes plus a status bar:

```
┌────────────────┬─────────────────────────────────────────────┐
│  Instances     │  ┌ supervisor ┐ backend  frontend  db  proxy│
│                │  └────────────┘─────────────────────────────│
│ ▸ master       │                                              │
│   IWP-42       │  [live log viewport for focused service]    │
│   IWP-58       │                                              │
├────────────────┴─────────────────────────────────────────────┤
│ focus breadcrumb │ key hints │ health summary                │
└──────────────────────────────────────────────────────────────┘
```

- **Sidebar**: 20 cols wide, lists Stack instances. Hidden below 40 col terminal width; collapsed to 3-char abbreviations below 60 cols. `Ctrl+B` toggles.
- **Top tabs**: per-service within the focused instance. First tab is always the synthetic `supervisor` tab (graph status + shared services + setup output). Failed services flagged red; external services tagged `⚙`.
- **Main viewport**: live log stream for the focused service.
- **Status bar** (3 regions): left = focus breadcrumb (`instance: X · service: Y`), middle = 4-5 context-relevant key hints, right = global health summary (`<running>✓ <waiting>⚠ <failed>✗`).

### Default focus

Match cwd → instance via `git rev-parse --show-toplevel`. If no match, focus the first running instance. If none running, show empty state with command hint. Focused service defaults to the synthetic `supervisor` tab.

### Resize behavior

- Wide (≥100 cols): full layout.
- Medium (60-99 cols): sidebar collapses to 3-char labels.
- Narrow (40-59 cols): sidebar hidden, single viewport. Instance/service switching via `:` Vim-style command mode or command palette.
- Tiny (<40 cols): "terminal too small, resize to at least 40×10" message.

### Theme

Catppuccin-aligned. Three built-in themes: `mocha` (dark, default), `latte` (light), `mono` (no color). Six-color semantic palette: green (running), yellow (starting/restarting), red (failed), blue (waiting/cached), gray (stopped), magenta (external). Light/dark detection via `COLORFGBG` + terminal probe.

## Consequences

- Never-collapsing layout means no learning curve for "where did my logs go?"
- Three themes cover dark, light, and accessibility cases.
- Mouse support is genuinely optional — every action reachable via keyboard.
- The synthetic `supervisor` tab provides a "what's happening at the meta level" view that's missing from naive process-list TUIs.

## Alternatives considered

**Tile-based, user-rearrangeable panes (like tmux).** Power-user appeal, complexity nightmare for a supervisor. Out.

**Single full-screen viewport with hot-key switching.** Loses the "see everything at once" benefit. Out.

**Modal Vim-style bindings.** Useful for editor-like TUIs, friction here.
