# ADR-0017: Unified output layer (`devme-ui`)

**Status**: Accepted
**Date**: 2026-06-10

## Context

ADR-0008 set the principles — stdout is the data contract, stderr is
commentary, `--json` on every data-returning command, `NO_COLOR` respected —
but nothing *enforces* them, and the implementation has drifted into five
parallel styling systems:

| Style | Where | Look |
|---|---|---|
| clack tree | `supervisor/{preflight,port_preflight,env_resolve,prompt}.rs` | `◆ │ ◇ └` headers/bars, 4 private copies of the same ANSI + glyph constants |
| compose mimicry | `down`, foreground `up` | `[+] Stopping 5/5`, ` ✔ Service web  Stopped  0.0s` |
| status table | `cli/lib.rs` (`format_status_*`) | `STEPS`/`SERVICES` grid, glyphs `● ◐ ◌ ↻ ✗ ○ ◆`, its own `ansi` module + `paint()` |
| prefix one-liners | `main.rs` (`info!`), `remote.rs` | `devme: …`, `devme remote: …`, bare prose, ad-hoc `⚠ ✔ ✗ ✓ ▲ • ↳`, `hint:` / `warning:` / `fix:` labels |
| JSON | scattered | mostly `to_string_pretty`, sometimes compact; `logs --json` is NDJSON |

Concrete drift this caused:

- **stdout/stderr discipline is violated**: `down` prints progress to stdout,
  `devme: no daemon running` goes to stdout, while the same class of message
  elsewhere goes to stderr.
- **`--no-color` doesn't reach the supervisor renderers**: the preflight tree
  prints raw ANSI regardless (it never sees the flag), while the status table
  gates correctly.
- **Glyphs disagree**: success is `✔` in four places and `✓` in one;
  "missing" is `▲` in preflight and `✗` elsewhere; hints are `↳`, `fix:`, or
  `hint:` depending on the file.
- **Quiet is inconsistent**: `main.rs` has an `info!` macro, `remote.rs`
  threads a `quiet` bool by hand, the supervisor renderers ignore it.

Every new command re-decides all of this from scratch.

## Decision

One new bottom-of-graph crate, **`crates/ui` (`devme-ui`)**, owns the entire
visual vocabulary. Everything user-facing renders through it.

### The vocabulary

- **One-liners** (stderr): `devme: <msg>`, optionally scoped
  (`devme remote: <msg>`). Levels: `info` / `success` (quiet-gated),
  `warn` / `error` (never gated). Hints are dim `  ↳ <msg>` continuation
  lines, quiet-gated.
- **Sections** (any multi-item flow): the clack tree is devme's signature
  look and becomes the *only* multi-line style — preflight, port checks, the
  env wizard, `down` progress, `remote doctor`, `validate`:

  ```
    ◆  Stopping stack
    │  ✔ web      stopped
    │  ✗ worker   kill timed out
    │    ↳ devme logs worker
    └  1 of 2 stopped
  ```

- **Glyphs**: `✔` ok · `✗` fail · `⚠` warn · `↳` hint/fix · `•` bullet ·
  `◆ ◇ │ └` structure · `● ◐ ◌ ○ ↻` service states. `✓` and `▲` are retired.
- **Streams**: stdout carries *only* the command's data (tables, URLs, JSON,
  log lines). Progress, narration, warnings, errors → stderr. Mutating
  commands narrate to stderr and rely on `status --json` as their queryable
  result; read commands own a `--json` shape.
- **JSON**: one emit helper, pretty-printed, stdout. Streaming stays NDJSON.
- **Color**: resolved once per stream (flag → `NO_COLOR` → is-a-tty for
  *that* stream — stderr styling no longer keys off stdout's tty-ness), then
  passed as a plain `bool`/`Style`. Renderers never probe the environment.

### The API

```rust
devme_ui::init(quiet, no_color);          // once, in main()
devme_ui::info("started 5 services");     // → stderr "devme: started 5 services"
devme_ui::scoped("remote").warn("…");     // → stderr "devme remote: ⚠ …"
devme_ui::hint("devme logs api");         // → stderr "  ↳ devme logs api" (dim)
devme_ui::json(&value);                   // → stdout, pretty

let st = devme_ui::Style { color };       // explicit-Write renderers (testable)
let mut sec = Section::begin(&mut w, st, "Check dependencies")?;
sec.ok("tools", None)?;
sec.warn("gcloud_adc", Some("not found"))?;
sec.hint("gcloud auth application-default login")?;
sec.line("free-form, bar-prefixed")?;     // escape hatch for prompts
sec.end_ok("All dependencies satisfied")?;
```

The supervisor renderers keep their `&mut W: Write` injection (their tests
depend on it) and gain a `Style` parameter; the global `Ui` is only for the
CLI's fire-and-forget one-liners.

## Consequences

- A new command gets the house style by *not* writing any formatting code.
- `--no-color` and `-q` behave identically everywhere, including the
  preflight tree for the first time.
- Agents get ADR-0008 for real: clean stdout, one JSON style, stable glyph
  vocabulary under `NO_COLOR`.
- The four supervisor constant blocks and `cli/lib.rs`'s private `ansi`
  module are deleted; `devme-ui` is the single source of truth.
- The TUI keeps its ratatui theme (ADR-0008's TUI section) but shares the
  state-glyph set so sidebar and `status` agree.

## Alternatives considered

**A `ui` module inside `devme-cli`.** The supervisor renders the preflight
tree and the env wizard; it can't depend on the CLI. Bottom-of-graph crate or
nothing.

**Third-party styling crates (`console`, `indicatif`, `cliclack`).** They own
the look end-to-end and fight the Write-injection testing pattern the
supervisor relies on; the vocabulary here is ~150 lines. Not worth the
dependency surface.

**Routing everything through `tracing`.** Solves levels/quiet, not layout or
glyph convergence, and turns simple eprintlns into subscriber config. Out.
