Status: ready-for-agent
Type: AFK

# Remove devme Remote-Primary Surface

## Parent

PRD: devcloud Split And devme On-Demand Runtime

## What to build

Remove or disable devme's remote-primary behavior: transparent remote proxying, Mutagen sync orchestration, remote config as an active runtime feature, Herdr attach presets, wake hooks, and remote URL rewriting. Existing users should get clear migration guidance rather than silent behavior changes.

## Acceptance criteria

- [ ] Bare `devme` no longer switches to remote behavior based on remote config.
- [ ] Daemon-facing commands are no longer transparently proxied over SSH.
- [ ] Mutagen sync commands are removed from the active public CLI or replaced by migration warnings.
- [ ] Remote config keys no longer affect stack supervision behavior.
- [ ] Existing remote config produces a clear warning or migration hint.
- [ ] CLI help, config key listing, and embedded skill docs no longer advertise `devme remote` as the remote workflow.
- [ ] Tests cover the removed default/proxy behavior and migration warning.

## Blocked by

- 01-record-remote-split-decision
