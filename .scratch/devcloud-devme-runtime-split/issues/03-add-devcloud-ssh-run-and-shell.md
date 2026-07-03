Status: ready-for-agent
Type: AFK

# Add devcloud SSH Run And Shell

## Parent

PRD: devcloud Split And devme On-Demand Runtime

## What to build

Add SSH execution primitives to devcloud so users and agents can run commands or open a shell in the resolved remote project directory. This is the core composition point for Herdr, Codex, and manual VPS work.

## Acceptance criteria

- [ ] `devcloud run <cmd...>` runs the command on the configured host after changing to the resolved remote project directory.
- [ ] `devcloud ssh` opens an interactive shell on the configured host in the resolved remote project directory.
- [ ] Remote command construction uses one shared quoting path with focused tests.
- [ ] `devcloud doctor` checks SSH reachability and remote Git availability.
- [ ] Non-interactive failures return clear exit codes and actionable error messages.
- [ ] Tests cover shell quoting and command construction without requiring a real SSH host.

## Blocked by

- 02-add-devcloud-identity-and-config
