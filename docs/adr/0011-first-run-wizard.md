# ADR-0011: First-run wizard (Clack-style with fly-launch-style review)

**Status**: Accepted
**Date**: 2026-05-23

## Context

Our "no README" goal requires that a fresh clone followed by `devme` gets a developer from zero to a running stack without reading prose. That means the first run must detect, plan, ask only what we can't infer, and drop the user into the working TUI as the success state.

Best-in-class references for 2025-2026: Clack's left-rail wizard aesthetic (used by Astro, Nuxt, T3 create scripts), `fly launch`'s "review before commit" table, `create-next-app`'s three-way "defaults / customize / reuse" fork, atuin's "Ctrl-R already works" delight moment.

## Decision

Triggered by absence of `devme.toml` in the repo. Subsequent runs skip the wizard.

1. **Screen 1 — Detection banner** (1.5s, auto-advance). List what we found: `Cargo.toml`/`package.json` → likely services; `justfile`/`Makefile` → recipe candidates; `.git` → worktree state; port availability. No prompts.

2. **Screen 2 — The single big question** (Clack-style three-way fork): "Use recommended setup" (default), "Customize", "Open the TUI in inspect mode" (attach read-only to processes already running on detected ports, no config written).

3. **Screen 3 — Customize** (only if chosen): single-screen batched form, all fields visible, Tab to move. Maximum 6 fields: services (multi-select), worktree slot, frontend port, backend port, log dir, telemetry yes/no (off by default). Inspired by `fly launch` but editable.

4. **Screen 4 — Review before commit**: a compact table of every action devme is about to take (write config, create log dir, append to `.gitignore`, start services). Exactly one `Proceed? [Y/n]`.

5. **Screen 5 — The delight moment**: clear screen, drop directly into the live TUI. Each service tile spins up with progressive glyphs (`◇ pending → ◐ starting → ● healthy`). No celebration screen. The working TUI is the success message. Footer hint appears for ~10s then fades.

Cross-cutting:

- **Ctrl-C anywhere** saves resumable state to `.devme/.first-run.json`; next run picks up.
- **Per-step errors** show inline red glyph + one-line message + actions: retry / pick another port / skip this service. Never abort the whole wizard.
- **Determinism**: every prompt has a CLI-flag equivalent. `devme init --yes` produces a byte-identical config to clicking through defaults. CI uses `--yes`.
- **Visual polish**: Clack-style vertical left rail in the theme's accent color. Single-char status glyphs (◆/◇/✓/✖). No emoji. Spinners only for >300ms operations.
- **Telemetry**: local JSONL log of step timings + outcomes. *After 7 days of use*, a one-time prompt: "Share anonymous usage to help improve devme? [y/N]". Default no.

## Consequences

- The wizard is a first-class part of the product, not an afterthought.
- One-screen forms + one final confirmation set a discipline we apply elsewhere (settings UI, instance config edits).
- The "drop into the working TUI" pattern means we never have to write "Setup complete!" text. The product is the message.
- Resumable state at every step removes the "I Ctrl-C'd and now what?" failure mode.

## Alternatives considered

**Sequential question-by-question** (one prompt per screen). Reads like a tax form past 4 prompts. Out.

**No interactive wizard; just write a default config and start.** Faster but undermines the "user knows what's about to happen" promise. Out.

**Web-based onboarding** (like `fly launch`'s "tweak in a web page" escape). Worth considering for v1.1 if the form fields outgrow what's tolerable in a terminal. For v1 the form fits.

**Mandatory telemetry / account upfront.** Erodes trust and we have no real reason to require it. Local-only metrics with deferred consent is the right tradeoff.
