Status: ready-for-agent
Type: AFK

# Document dev Command Composition

## Parent

PRD: devcloud Split And devme On-Demand Runtime

## What to build

Document the intended shell-level composition where a user can run one `dev` command inside a project to use devcloud values when starting Herdr or Codex remotely. This issue should not implement dotfiles in this repo; it should document the contract that dotfiles can consume.

## Acceptance criteria

- [ ] Documentation shows how `devcloud name` and `devcloud path` compose into a Herdr session command.
- [ ] Documentation states that Herdr owns sessions and Codex owns Codex auth/threads.
- [ ] Documentation states that devme does not launch Herdr or Codex directly.
- [ ] Documentation includes the expected daily workflow: enter project directory, run `dev`, attach to remote session, wake services only when needed.
- [ ] The embedded skill or docs mention that agents should use devcloud for remote project context and devme for services.

## Blocked by

- 02-add-devcloud-identity-and-config
- 05-remove-devme-remote-primary-surface
