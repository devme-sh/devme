Status: ready-for-agent
Type: AFK

# Record The Remote Split Decision

## Parent

PRD: devcloud Split And devme On-Demand Runtime

## What to build

Record the new architectural decision that devme no longer owns remote-primary sync, Herdr attach, or agent session orchestration. The decision should supersede the old cloud relay direction without deleting useful historical context. The public documentation and embedded agent skill should describe devme as the stack/runtime supervisor and devcloud as the remote project context adapter.

## Acceptance criteria

- [ ] A new decision record documents the devme/devcloud split and explicitly supersedes the old cloud relay direction.
- [ ] The glossary or domain docs describe devcloud, remote project context, and on-demand runtime without treating them as devme stack concepts.
- [ ] The embedded devme skill no longer tells agents to use `devme remote` for remote-primary work.
- [ ] The docs explain that v1 remote work uses Git, not live sync.
- [ ] The old remote documentation has a clear migration note instead of silently conflicting with the new design.

## Blocked by

None - can start immediately
