# ADR-0023: Live agent-friendly environment setup

**Status**: Accepted
**Date**: 2026-07-17

## Context

Declarative environment setup originally blocked on terminal input. A coding
agent could discover a provider URL in prose and edit the env file, but the
human prompt would remain blocked and could not observe that progress. Secret
values were also indistinguishable from ordinary text inputs.

Projects such as Sambu need several Google OAuth values from the same provider
console. The useful shared contract is small: describe where a value comes
from, report whether it is present, and accept it without exposing secrets.

## Decision

`[env.*]` accepts two optional fields:

- `setup_url` is a browser destination where the value can be obtained.
- `secret = true` masks interactive input, omits the value from structured
  output, and requires CLI submission through stdin.
- A secret cannot declare `choices`, because choices are intentionally exposed
  as setup metadata and rendered by selectors.

The configured env file remains the source of truth. Interactive Devme watches
it while presenting missing values and advances automatically when another
process supplies the current value. Values accepted in the human wizard are
persisted immediately instead of waiting for the whole form to finish.

The wizard tells the human that a coding agent can help and offers a shortcut
to copy a ready-to-paste agent prompt. The prompt identifies the project,
names `devme setup status --output toon` as the live redacted context surface,
explains safe `setup set` usage, and reserves authentication and approval for
the human.

For variables with `setup_url`, the human wizard offers `Open browser`.
Browser automation stays outside Devme. An agent reads the same URL through
`devme setup status --output toon`, uses its available browser adapter, and
submits results with `devme setup set`.

The text prompt is visible and active as soon as a field appears. Printable
text and paste begin value entry immediately. Tab copies the agent prompt and
Shift+Tab opens a setup URL, so valid input characters are never consumed as
wizard commands.

`setup status` and `setup set` support human, TOON, and JSON output. A
non-interactive invocation defaults to TOON; `--json` remains the stable JSON
compatibility alias.

`devme setup set <name> --value <value>` accepts non-secret values.
`devme setup set <name>` reads stdin and is required for secrets. It locks the
configured env file, refuses to overwrite an existing non-empty value, and
returns the refreshed redacted setup snapshot.

Submitting the same value again is a successful no-op so interrupted agents
can retry safely. Submitting a different value remains a conflict and never
overwrites the configured value.

There is no normal recheck action. The wizard and `setup status` derive their
state from the env file on every observation. Explicit retry is reserved for a
future check execution failure, not ordinary provider setup.

## Consequences

- Humans and agents can complete the same setup without driving each other's
  presentation.
- A human terminal visibly advances when an agent supplies a value.
- Setup survives interruption because completion is derived from the env file.
- Secrets never appear in setup JSON or process arguments.
- Provider-specific browser logic does not enter Devme's interface.
- Existing projects keep `.env.local` by default and may select a narrower file
  such as Sambu's `.env.auth.local` through `[stack].env_file`.

## Alternatives considered

**Persisted wizard session with explicit complete and recheck actions.** This
duplicates state already represented by the env file and creates conflicting
sources of truth. Rejected.

**Browser automation inside Devme.** This would require provider-specific
authentication and browser adapters. Agents already own that capability.
Rejected.

**Use `.env` for every project.** Generic `.env` files are more likely to be
committed or loaded by unrelated tools. Devme keeps `.env.local` as the default
and honors explicit narrower files. Rejected.
