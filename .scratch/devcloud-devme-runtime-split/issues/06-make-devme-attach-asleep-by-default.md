Status: ready-for-agent
Type: AFK

# Make devme Attach Asleep By Default

## Parent

PRD: devcloud Split And devme On-Demand Runtime

## What to build

Change bare `devme` so it attaches a TUI/dashboard without waking the stack. The UI should make the asleep state visible and provide explicit wake controls, while `devme up -d` remains the command that starts services.

## Acceptance criteria

- [ ] Running bare `devme` does not start services by itself.
- [ ] The TUI can show an asleep stack state with service names and configured metadata.
- [ ] The TUI provides an explicit wake/start action that maps to the same behavior as `devme up -d`.
- [ ] `devme up -d` starts or wakes services as before, with clear output.
- [ ] `devme down` sleeps/stops services as before.
- [ ] Tests cover bare attach, explicit wake, and explicit sleep behavior.

## Blocked by

- 05-remove-devme-remote-primary-surface
