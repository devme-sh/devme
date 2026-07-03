# Issue tracker: Linear

Issues and PRDs for this repo live in Linear for the `devme-sh` workflow. Use the Linear app/plugin tools for issue operations whenever they are available.

## Conventions

- **Create an issue**: use the Linear issue creation tool, setting the team/project to the `devme-sh` workflow.
- **Read an issue**: fetch the Linear issue by identifier or URL, including description, comments, labels, status, priority, assignee, project, and relations.
- **List issues**: use Linear issue filters for team/project, state, label, assignee, priority, and search query.
- **Comment on an issue**: add a Linear comment to the issue.
- **Apply / remove labels**: update the Linear issue labels using the vocabulary in `docs/agents/triage-labels.md`.
- **Close or decline**: move the Linear issue to the appropriate terminal status, and add a short comment when the decision is not obvious from the issue history.

If a Linear tool requires an exact team ID and `devme-sh` is not resolvable in the connected workspace, list Linear teams/projects first and confirm the target before creating or updating issues.

## When a skill says "publish to the issue tracker"

Create a Linear issue in the `devme-sh` workflow.

## When a skill says "fetch the relevant ticket"

Read the referenced Linear issue by URL or issue identifier.
