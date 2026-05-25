# ADR-0015: Skill install hint with exponential backoff

**Status**: Accepted
**Date**: 2026-05-26

## Context

devme ships an agent skill (`devme-sh/skills`) that works with Claude Code, Cursor, Codex, and 50+ other AI coding agents. The skill lets agents generate devme.toml configs, diagnose failing services, and read logs. Most users won't know it exists unless we tell them.

The question is how and how often to surface this. Research on CLI hint frequency (git, npm, Docker, rustup, update-notifier) and UX nudge studies (Digia 2024, Plotline, Chameleon 2026, NCSU digital nudge research) converges on several findings:

- **Once is not enough.** A single exposure often misses the user when they're not in the right context. NCSU research found single-exposure nudges underperform for developer tool adoption.
- **Every time is too much.** npm's `funding` message and Docker's `What's Next?` hints are widely complained about. 46% of users opt out after just 2-5 messages per week (Plotline).
- **Contextual beats periodic.** Git's `hint:` model (show when relevant to the command's state) is tolerated far better than Docker's (show on every command). Developers accept help in the moment but resent interruptions disconnected from their task.
- **Exponential backoff hits the sweet spot.** The `update-notifier` npm package (used by hundreds of CLIs) defaults to daily checks. rustup settled on daily after community discussion. Both avoid showing on every invocation.
- **Banner blindness is fast.** Nielsen Norman Group research shows users develop blindness to fixed-position repeated notifications within weeks. Anything that looks the same and appears in the same spot every time becomes invisible.
- **An escape hatch is critical.** Firefox update nags and Chrome DevTools "What's New" are cited as frustrating because they lack easy permanent dismissal.

## Decision

### CLI hint (implemented)

Show a dim `hint:` line after `devme up -d` and TUI exit. Use exponential backoff: 4 exposures max, spaced at 0 days, 3 days, 2 weeks, 6 weeks, then stop permanently. The first exposure also shows the suppress command.

State is tracked in `~/.config/devme/skills-hint-state` (show count + last-shown timestamp). Suppressed permanently via `devme config set hints.skills false`. Respects `--quiet`.

Format: `hint: devme has an AI coding skill. Run: npx skills add devme-sh/skills`

This mirrors git's `hint:` style: dim, single-line, non-blocking, at the end of output.

### TUI hint (implemented, simple version)

The footer's centre region normally shows keybinding hints. When the skill hint is eligible (same backoff logic as CLI), the centre region alternates: it shows the skill hint for 8 seconds, then switches back to keybindings for 30 seconds, then shows the hint again. This continues until the session ends.

This avoids banner blindness (intermittent, shares space with useful content) while keeping the hint visible during longer TUI sessions where the user might act on it. Dismissing via `devme config set hints.skills false` suppresses both CLI and TUI hints.

The TUI hint does NOT increment the show counter or update the timestamp. Only the CLI hint path does. This prevents the TUI from burning through all 4 exposures during a single long session.

## Consequences

- Users see the skill hint at most 4 times over ~2 months in CLI output, then never again.
- The TUI hint is ambient and non-blocking. It can't be dismissed with a keypress (that would require adding a keybinding), but it only appears when the CLI backoff schedule allows it.
- The `hints.skills` config key is the single control point for both surfaces.
- Future hints (new features, breaking changes, deprecations) can reuse the same backoff infrastructure with different state files and config keys.

## Alternatives considered

**Show once, never again.** Simple but research says single-exposure underperforms. Users may not be in the right mindset, or may dismiss without reading. Out.

**Show on every invocation (Docker model).** Guaranteed visibility but guaranteed annoyance. npm and Docker are cautionary tales. Out.

**Fixed daily/weekly interval.** Better than every-time, but still not contextual. A user who runs devme 20 times on Monday and 0 times the rest of the week gets one hint at the wrong time. The backoff model adjusts to natural usage patterns. Out.

**Modal/popup in the TUI.** Maximum attention but interrupts flow. Research specifically flags modals as a developer anti-pattern for non-critical notifications. Out.

**No hint at all, rely on docs/homepage.** The skill is a discovery problem, not a documentation problem. People who don't know it exists won't search for it. Out.
