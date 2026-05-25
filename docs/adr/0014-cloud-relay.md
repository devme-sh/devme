# ADR-0014: Cloud relay — continuous sync and automatic session handoff

**Status**: Accepted
**Date**: 2026-05-25

## Context

devme supervises local dev stacks. Users working with AI coding agents (Claude Code) want the agent to keep working when the laptop goes to sleep. The agent session and dev environment should transfer seamlessly to a remote host without any explicit handoff command — just close the laptop and the agent keeps going.

Omnara (YC S25) solves this with a hosted relay service. We want the same seamless experience but self-hosted first, with the option to offer a paid devme-hosted offering later using the same protocol.

Key experimental finding: Claude Code JSONL session files are append-only, tree-structured (uuid/parentUuid), and fully portable across machines. User messages, assistant responses, and full turns can be appended to a JSONL and `claude --resume` loads them as native conversation history. Live injection into a running instance does NOT work (state is in memory, JSONL is only read on startup) — we handle this by killing and re-resuming the process.

## Decision

### Architecture

Cloud orchestration lives in a separate `devme-cloud` crate in the workspace, shipping as `devme cloud *` subcommands under the same binary. Core devme stays focused on stack supervision. Lifecycle hooks in devme bridge the two — no formal plugin system. The protocol between local and remote is provider-agnostic so a future paid devme-hosted offering can use the same relay.

### Sync

Mutagen provides continuous bidirectional worktree sync (`two-way-safe` mode — flags conflicts, doesn't silently overwrite). Sub-second latency, auto-stages its agent binary on the remote. devme generates a `.mutagen.yml` from `.gitignore` at sync creation time. Claude Code JSONL files sync alongside the worktree. All files sync including `.env` — security hardening is future work.

Sync starts automatically with `devme up` or the TUI when `[cloud]` is configured in `~/.config/devme/global.toml`. All worktrees with active `devme up` sessions sync to the VPS automatically. Each worktree gets its own subdirectory under the configured remote base path.

### Session transfer

One session, transferred — same session ID, same JSONL, same conversation tree on both machines. The path-hash mismatch between machines (`~/.claude/projects/<hash>/` is derived from the absolute project path) is computed at `devme cloud init` time for both ends. The JSONL is the transfer protocol — no custom format, no lossy export.

### Process lifecycle

`devme claude` wraps Claude Code inside a managed tmux session, giving devme programmatic control to kill and restart the process (forcing a JSONL re-read). The tmux pane is always "where the action is":

- **Working locally**: tmux pane runs local Claude Code.
- **Laptop closed, VPS takes over**: VPS runs `claude --resume` in its own tmux with `/remote-control` active.
- **Laptop opens, agent still working**: tmux pane SSH-attaches to the VPS tmux — user sees live Claude Code, can type to steer the agent.
- **Agent finishes (or user pulls back)**: devme detaches from remote, syncs JSONL, starts local `claude --resume`.
- **Laptop opens, agent already done**: devme syncs JSONL, starts local `claude --resume` immediately.

### Takeover

A lightweight devme watchdog runs on the VPS (warm standby — stack is NOT running, only watchdog + mutagen agent consume resources). It maintains a WebSocket heartbeat with the local machine.

On heartbeat loss: 2-minute delay, then takeover — but only when the Claude session was idle (last JSONL entry is assistant response or turn_duration, not mid-tool-use). If the session was mid-turn, local Claude resumes that turn on wake.

On takeover: watchdog runs `devme up` (10-30s for services), then `claude --resume`. If the session was mid-work, devme injects a "continue" prompt. If truly idle, the session sits ready — controllable from the Claude app via the built-in remote control bridge.

### Pull-back

Two methods:
- **Tmux keybinding** (primary): e.g. `Ctrl+B p` while attached to the remote session. devme stops the VPS agent, syncs JSONL, swaps the pane back to local `claude --resume`.
- **CLI** (fallback): `devme cloud pull` from another terminal. If multiple cloud sessions exist, shows an interactive picker.

### Notifications

Via the Claude app's existing remote control — no custom notification system for v1. No web dashboard.

### Configuration

Global only, not per-repo. Stored in `~/.config/devme/global.toml`:

```toml
[cloud]
host = "vps"                           # SSH alias or Tailscale hostname
base_path = "/home/henrik/devme"       # all worktrees land under here
takeover_delay_secs = 120
```

One-time setup via `devme cloud init`: configure host connection, set remote base path, install devme + Claude Code on VPS (user must SSH in for interactive `claude login`), set up mutagen, compute JSONL path mapping.

## Consequences

### Positive

- Close your laptop and the agent keeps working. Open it and you see what it did. Zero explicit handoff commands.
- Self-hosted — code and conversations never leave your infrastructure.
- Environment parity via `devme.toml` — the VPS runs the same stack as local, guaranteed.
- No custom session format — Claude Code's own JSONL is the transfer protocol.
- Claude app remote control provides phone access for free — no dashboard to build.
- Warm standby means near-zero VPS resource cost when working locally.
- Provider-agnostic protocol enables a future paid hosted offering.

### Negative

- Depends on mutagen for file sync — a third-party dependency (MIT, backed by Docker).
- Depends on Claude Code's JSONL format remaining stable — undocumented internal format that could change.
- `devme claude` adds a tmux layer between the user and Claude Code — slight indirection.
- No web dashboard in v1 — monitoring is tmux + Claude app only.
- Security is trust-based for now — `.env` files sync unencrypted to VPS.
- Warm standby watchdog is a new persistent process on the VPS.

### New crate

`devme-cloud` joins the workspace. Imports `devme-client` and `devme-config`. Ships as part of the `devme` binary. ~estimated 2-3k lines for v1 (watchdog, sync management, tmux orchestration, init flow).

## Alternatives considered

**Explicit `/handoff` command.** User types a command to transfer the session. Simpler to build but breaks the "just close your laptop" experience. Rejected — the whole point is zero-friction.

**Omnara (hosted relay service).** Solves the same problem but routes all code and conversations through a third-party server. No E2E encryption. $9-20/month. Rejected for privacy and cost — self-hosted is the right default, with a hosted offering as a future option.

**Always-remote agent (SSH + tmux only).** Agent always runs on VPS, local terminal is just an SSH client. Zero sync needed. Rejected — worse daily experience (SSH latency for everything), and doesn't give you local IDE integration.

**Custom session format instead of JSONL.** Build our own conversation transfer protocol. Rejected — the JSONL already works, is complete, and avoids maintaining a parallel format.

**Syncthing instead of Mutagen.** Peer-to-peer, excellent offline recovery. Rejected — heavier resource footprint, 2-10s latency (vs sub-second), conflict files pollute the working directory, not purpose-built for dev workflows.

**No web dashboard.** Accepted for v1 — Claude app remote control + tmux attach covers monitoring and control. Dashboard is a future enhancement if the tmux experience proves insufficient.
