# Changelog

All notable changes to devme are recorded here. The format is loosely based on
[Keep a Changelog](https://keepachangelog.com/), and the project follows
semantic versioning. Each released version has its own section so the entries
stay structured enough to render elsewhere later (e.g. a release-notes panel in
the TUI).

## [Unreleased]

### Logs: agent-first overhaul (in progress)

Reworking how logs are captured and queried, with the primary reader being a
coding agent that pays context tokens per line — so the north star is
signal-to-token and deterministic "find the right slice" queries.

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
