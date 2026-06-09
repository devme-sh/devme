# Changelog

All notable changes to devme are recorded here. The format is loosely based on
[Keep a Changelog](https://keepachangelog.com/), and the project follows
semantic versioning. Each released version has its own section so the entries
stay structured enough to render elsewhere later (e.g. a release-notes panel in
the TUI).

## [0.2.0] — 2026-06-10

### Logs: agent-first overhaul

Reworks how logs are captured and queried, with the primary reader being a
coding agent that pays context tokens per line — so the north star is
signal-to-token and deterministic "find the right slice" queries. The
partition: `config check` = is the file valid, `status` = what's running
where, `logs` = what the services are saying, `doctor` = why it's broken.

#### Added
- **Dual-PTY capture.** Each service now runs under *two* PTYs — one for stdout,
  one for stderr — so the supervisor can tell the streams apart (errors and
  tracebacks almost always go to stderr) while both file descriptors still see a
  real terminal, so color and progress output behave exactly as in a shell.
  Every log line is tagged with its stream (`stdout`/`stderr`).
- **Disk-spilled log history.** Each daemon (instance and shared) now tees every
  line to an append-only JSON-lines file beside its socket, so history survives
  ring eviction *and* a daemon restart. Bounded, not unbounded: one active file
  plus one rotated file per service, size-capped (~16 MiB/service), so a
  crash-looping service can never fill the disk. This is what makes `--since`
  reliable rather than best-effort. Reads can flag when older history rotated
  away, so a clipped window is never mistaken for the whole story.
- **`devme logs` is now the agent workhorse.**
  - `devme logs` with no service interleaves *all* services into one
    timestamp-ordered stream — cross-service causality ("api 500s right after
    postgres restarted") in a single query.
  - `--since 30s|5m|2h|1d|<epoch-ms>` time-anchors the window: "what happened
    since my last check" instead of guessing a `--tail` count.
  - `--json` emits NDJSON (`{ts, service, stream, text}`, ANSI-stripped, one
    object per line) so output pipes straight to `jq` — e.g.
    `devme logs --json | jq 'select(.stream == "stderr")'` for errors only.
  - Queries are served from the disk history tier, so they reach past ring
    eviction and daemon restarts.
  - One-shot queries are deterministic: the daemon marks end-of-replay
    (`LogEnd`) and doesn't subscribe non-follow clients, so a `--tail N`
    window always contains exactly the last N lines — a freshly-emitted live
    line can't race into it. Also drops ~120 ms of idle-timeout latency.
  - A rotation warning (stderr, never stdout) fires only when requested
    history actually rotated away — not when you asked for a `--tail` clip.

- **`devme doctor` reframed as the error digest.** The no-arg report now
  anchors on *errors* instead of dumping every service's recent chatter:
  per-service `recent_errors` (stderr only — tracebacks, not access logs),
  failed/crash-looped states up front, and a failed step's check/provision
  output inline. Healthy steps stay one line. Replays come from the disk
  history tier, so the diagnosis survives ring eviction and daemon restarts —
  a service that crashed an hour ago still has its dying stderr here.
- **`devme doctor <name>` zooms into one node.** For a step: its full
  check/provision output (the only place step output surfaces — `logs` is
  services-only). For a service: state, pid, port, restart count,
  `recent_errors` (stderr) and `recent_logs` (both streams, `[stderr]`
  prefixed). Unknown names error immediately.

#### Fixed
- **`devme logs <name>` no longer renders the "Check dependencies" provisioning
  tree.** Log queries now connect without running the preflight, so the
  dependency-check UI can't leak into the logs channel.
- **`devme logs <step>` redirects instead of hanging.** Steps have
  check/provision *output*, not a runtime stream; asking for a step's logs now
  errors with a pointer to `devme doctor <step>`, and an unknown name errors
  immediately instead of waiting for logs that will never come. Step lines are
  filtered out of the all-services interleave too, even when a step re-runs
  mid-`--follow`.
- **One-shot queries can't tear down shared services.** A plain `devme logs` /
  `status` against a detached stack no longer bumps the shared supervisor's
  subscriber refcount, so its disconnect can't arm the 30-second idle teardown.

#### Changed
- **`devme status` rows are annotated.** Steps and services show their
  `description` from devme.toml; unevaluated steps say ``runs on `devme up```
  instead of an unexplained "pending"; the all-stopped summary names the next
  move (`devme up -d`).
- New `scripts/logs-smoke.sh` — 19 end-to-end assertions over the whole log
  surface, in an isolated fixture.
