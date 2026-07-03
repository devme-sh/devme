Status: ready-for-agent
Type: AFK

# Add Startup Timing Estimates

## Parent

PRD: devcloud Split And devme On-Demand Runtime

## What to build

Record per-service startup timings and use them to estimate wake time when services are starting from sleep. Estimates should improve as devme observes real starts and should stay conservative when no history exists.

## Acceptance criteria

- [ ] devme records startup timing milestones per service.
- [ ] Timings include at least process spawn, first log line, port open when relevant, health ready when relevant, and ready/running state.
- [ ] Timing history survives daemon restarts when practical.
- [ ] Waking output can show an estimated wait time per service or for the stack.
- [ ] No-history estimates are conservative and do not imply precision.
- [ ] Tests cover timing collection with deterministic service fixtures.
- [ ] Tests cover estimate rendering with no history, one sample, and multiple samples.

## Blocked by

- 07-wake-runtime-commands-on-demand
