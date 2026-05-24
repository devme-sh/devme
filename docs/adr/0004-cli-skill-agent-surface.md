# ADR-0004: CLI + Claude Code skill as the agent surface (no MCP server)

**Status**: Accepted
**Date**: 2026-05-23

## Context

devme is designed to be driven by AI coding agents (Claude Code, etc.) as well as humans. There are three plausible agent surfaces: a CLI with `--json` output, an MCP server, or both. Each has costs: a CLI is universal and easy to ship; an MCP server adds a runtime dependency and a third interface to maintain.

We also have to decide how agents discover the CLI's capabilities: do they read `--help` outputs, parse a manifest, or rely on a separately-distributed skill that codifies the patterns?

## Decision

Ship a single agent surface: a CLI with `--json` everywhere, semantic exit codes, idempotent mutations, and a `devme agent-context` subcommand that emits a machine-readable manifest of every command, flag, exit code, and JSON schema. Pair the CLI with a Claude Code skill that codifies the high-level workflows agents need ("read errors and diagnose," "restart this service and verify it's healthy," "tail logs since the last deploy"). No MCP server.

## Consequences

- One canonical surface to design, document, test, and version.
- The Claude Code skill ships separately and can evolve independently. Other ecosystems can author their own skills/wrappers using the same CLI.
- Agents that don't run a skill still get a useful CLI; the skill is value-add, not required.
- `devme errors` is structured rich enough (see ADR-0005's debugging packet model) that an agent can usually diagnose without needing further tool calls.
- We commit to keeping the CLI surface stable. Breaking changes go through the same versioning discipline as a public library API.

## Alternatives considered

**MCP server only.** Smaller surface, dynamic discovery. But requires MCP runtime dependencies everywhere devme runs, and forecloses non-MCP agents. Out.

**MCP + CLI.** Two surfaces means twice the testing and twice the chance of drift. The MCP server adds value only if we can't do dynamic discovery another way — but `devme agent-context` covers that need at zero extra runtime cost. Out for v1.

**CLI only, no skill.** Possible. The skill exists because the *patterns* for using the CLI (which subcommand to call in which situation) are themselves valuable knowledge that's hard for an agent to derive from `--help`.
