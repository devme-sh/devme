# ADR-0002: Prompt-by-default consent for mutating provisions

**Status**: Accepted
**Date**: 2026-05-23

## Context

When a Step's `check` fails, devme can run its `provision` command to satisfy it. Provisions can be anything from `mkdir tmp` to `brew install --cask google-cloud-sdk` to `gcloud auth login`. The consent model — when do we run vs ask vs only display — determines whether devme feels magical or terrifying. Silent auto-installation of 500 MB packages without asking erodes trust permanently after one bad incident; requiring confirmation for every `mkdir` is dialog fatigue.

## Decision

Each Step declares a `trust` level: `auto`, `prompt` (default), or `manual`. The TUI's failure overlay shows the proposed provision command and waits for `Enter` (run), `r` (retry check), `s` (skip this run), `i` (mark as installed), or `q` (cancel). A global `--yes` flag promotes every `prompt` step to `auto` for one invocation (for CI and "I know what I want" mode).

## Consequences

- Users always see the literal command devme is about to run before it runs. No hidden side effects.
- CI works via `--yes` without any per-step config changes.
- Config authors carry a small annotation burden: deciding `trust` per Step. Sensible defaults built into devme help (anything invoking `brew`/`apt`/`pip`/`npm`/`cargo install` defaults to `prompt`; `mkdir`/`touch`/file-generation defaults to `auto`).
- Reversing this decision later would break user expectations — users will rely on "devme always asks before installing things." Treat this as load-bearing.

## Alternatives considered

**Auto-provision silently.** Fastest UX. Maximally dangerous — auto-installing packages without asking is the kind of thing that goes viral on Hacker News when one user has a bad day.

**Manual mode only (print "run X" and exit).** Bulletproof but undermines the "no README" promise. Out.

**Per-invocation flags only, no per-step `trust`.** Less ergonomic for the common case ("auto for safe steps, prompt for risky ones is exactly what I want, but every step asks me with no flag"). Per-step `trust` is the right granularity.
