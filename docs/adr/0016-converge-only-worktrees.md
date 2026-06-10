# ADR-0016: Converge-only worktrees — no lifecycle hooks

**Status**: Accepted
**Date**: 2026-06-10

## Context

devme originally offered two per-worktree lifecycle hooks in `[stack]`:
`on_create` (run once after a worktree was created, gated by a
`.devme-initialized` marker) and `on_destroy` (run after `devme worktree rm`,
interpolated with `{slot}`/`{worktree}`/`{branch}`, intended for things like
`dropdb app_slot3`).

Hooks have an event problem: they only fire when devme *witnesses* the event.
A bare `git worktree add` (or one made by herdr, an agent, or an IDE) never
ran `on_create` until the TUI happened to autospawn it; a bare
`git worktree remove` never ran `on_destroy` at all, silently leaking the
resources the hook was supposed to reclaim. They also duplicate a mechanism
the config already has: `[step]` check + provision is idempotent and
reality-based — it re-runs when the artifact it checks for is missing, which
is strictly stronger than a marker file recording that a command once ran.
Two ways to express setup is one too many for a tool whose pitch is a simple
mental model.

The teardown side has a structural escape hatch: per-worktree resources are
keyed by `{slot}`, and slots come from a small bounded pool. An orphaned
`app_slot3` isn't garbage that grows without bound — it's reclaimed the next
time any worktree lands on slot 3, provided the provision step is idempotent
(`dropdb --if-exists … && createdb …`).

## Decision

Remove hook *execution* entirely; keep the config fields parsed (StackMeta is
`deny_unknown_fields`, so deleting them would hard-break old configs) and flag
them with a `devme config check` lint carrying a migration hint.

The worktree model becomes converge-only:

- **Setup** is `[step]` check/provision. Any worktree — created by
  `devme worktree add`, the TUI's `w` prompt, or a bare `git worktree add` —
  converges on its first `devme up`. No creation event is needed.
- **Removal** is mechanical: stop the instance supervisor, `git worktree
  remove`, release the slot claim. The TUI's autospawner also *reaps*
  worktrees whose directory vanished outside devme (watching
  `<git-common-dir>/worktrees/` for Remove events), so every removal path
  converges to the same end state.
- **Slot hygiene** is the provision step's job: slot-scoped provisions must be
  idempotent so a reused slot starts clean.

`devme worktree add`/`rm` stay, as pure mechanics with better ergonomics than
raw git (slot release, daemon stop, target by branch/dir/path), alongside the
TUI's `w`/`x` (the latter always behind a confirmation modal, with
merged-status and PR context).

## Consequences

- One mental model: the stack converges from `devme.toml`; nothing depends on
  devme having observed a lifecycle event. Worktrees created or removed by any
  tool behave identically.
- `.devme-initialized` markers are gone; nothing re-runs "once" — steps re-run
  exactly when their check fails, which is what users actually want.
- Teardown is eventually-consistent, not immediate: an orphaned slot resource
  (a cloned DB) lives until its slot is reused. With a bounded slot pool this
  is bounded garbage; we accept it for the simplicity.
- A truly external resource that is *not* slot-keyed (e.g. a cloud resource
  per branch) has no devme-native teardown anymore. Users who need that run
  it themselves; we judged the niche too small to keep a whole hook mechanism
  for.
- Old configs keep parsing; the lint (not a hard error) tells authors the hook
  no longer runs, instead of silently changing behavior with no signal.

## Alternatives considered

- **Keep `on_create`, drop only `on_destroy`.** Creation is the better-served
  side already — steps cover it fully — so keeping the weaker duplicate while
  dropping the harder one inverts the value. Rejected to get to one model.
- **A per-repo daemon watching for worktree removals to fire `on_destroy`
  reliably.** Solves the missed-event problem at the cost of an always-on
  process per repo, ordering races (the hook needs `{slot}` resolved *before*
  the directory is gone), and still no coverage when the machine was off.
  Rejected as heavy machinery for bounded garbage.
- **Hard-error on hook fields (remove from schema).** Breaks every existing
  config at parse time for what is now a no-op field. A lint communicates the
  same thing without the breakage.
- **Run `on_create` lazily on first `devme up` instead of at creation.** This
  is just a worse spelling of a provision step (marker-gated instead of
  reality-checked). Rejected as redundant.
