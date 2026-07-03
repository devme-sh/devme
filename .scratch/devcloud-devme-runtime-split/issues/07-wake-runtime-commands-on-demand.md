Status: ready-for-agent
Type: AFK

# Wake Runtime Commands On Demand

## Parent

PRD: devcloud Split And devme On-Demand Runtime

## What to build

Make runtime commands wake the stack when the stack is asleep and the command needs live runtime state. Read-only status should remain passive. Commands that wake should say so clearly and then continue with the requested operation.

## Acceptance criteria

- [ ] `devme status` does not wake an asleep stack.
- [ ] `devme logs <service>` wakes an asleep stack when needed and then returns logs or follows them.
- [ ] `devme url <service>` wakes an asleep stack when needed and returns a usable URL after the service is ready enough.
- [ ] `devme doctor` wakes only when the requested diagnosis requires live runtime state.
- [ ] `devme start`, `stop`, and `restart` have clear behavior against asleep stacks.
- [ ] Human output tells the caller that the stack is starting.
- [ ] JSON output, where available, exposes a structured waking/starting state.
- [ ] Tests cover status-passive behavior and wake-on-logs/url behavior.

## Blocked by

- 06-make-devme-attach-asleep-by-default
