Status: ready-for-agent
Type: AFK

# Add Internal Idle Sleep

## Parent

PRD: devcloud Split And devme On-Demand Runtime

## What to build

Add an internal activity tracker that sleeps services after a default idle window, initially 30 minutes. Do not expose a public lease CLI in v1. Activity should be based on meaningful runtime use, not passive polling.

## Acceptance criteria

- [ ] Running services sleep after the configured idle window when no meaningful activity occurs.
- [ ] The default idle window is 30 minutes.
- [ ] Status checks do not extend the idle deadline.
- [ ] Runtime commands and meaningful TUI actions extend the idle deadline.
- [ ] Sleep uses the same graceful stop path as explicit down where practical.
- [ ] The TUI and CLI can show the idle deadline or asleep reason without noisy output.
- [ ] Tests use a fake clock or controllable timer; they do not wait in real time.

## Blocked by

- 07-wake-runtime-commands-on-demand
