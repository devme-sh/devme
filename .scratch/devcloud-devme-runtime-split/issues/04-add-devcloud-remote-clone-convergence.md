Status: ready-for-agent
Type: AFK

# Add devcloud Remote Clone Convergence

## Parent

PRD: devcloud Split And devme On-Demand Runtime

## What to build

Make devcloud ensure that the canonical remote project directory exists and points at the same Git origin as the local project. When absent, devcloud should clone. When present, it should verify the origin and refuse mismatches.

## Acceptance criteria

- [ ] devcloud creates the remote project directory by cloning the local origin when it is absent.
- [ ] devcloud verifies the remote clone origin when the directory already exists.
- [ ] Origin mismatches fail closed with a message naming the expected and actual remotes.
- [ ] Remote clone convergence is used by `path`, `run`, and `ssh` where needed, or exposed through a clearly documented ensure step if the implementation chooses not to make `path` mutating.
- [ ] Tests use local Git fixtures or a fake command runner to cover absent clone, matching clone, and mismatched clone behavior.

## Blocked by

- 03-add-devcloud-ssh-run-and-shell
