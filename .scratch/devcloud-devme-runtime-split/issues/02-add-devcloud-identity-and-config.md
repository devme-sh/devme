Status: ready-for-agent
Type: AFK

# Add devcloud Identity And Config

## Parent

PRD: devcloud Split And devme On-Demand Runtime

## What to build

Add the initial devcloud CLI surface for resolving the current Git repository into a clean project identity and configured remote host/root. This slice should make `devcloud name`, `devcloud path`, `devcloud status`, and `devcloud doctor` work locally without yet running arbitrary remote commands.

## Acceptance criteria

- [ ] `devcloud name` prints the repo basename as a single machine-readable line.
- [ ] `devcloud path` prints the canonical remote path derived from configured host/root and Git origin.
- [ ] `devcloud status` shows local repo identity, origin, host, and remote path in human-readable form.
- [ ] `devcloud doctor` fails clearly when the current directory is not a Git repo or has no origin.
- [ ] Git origin parsing supports common SSH and HTTPS forms for GitHub/GitLab-compatible remotes.
- [ ] Unit tests cover origin parsing, name derivation, and path derivation.

## Blocked by

- 01-record-remote-split-decision
