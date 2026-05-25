# ADR-0014: Declarative environment variable management

**Status**: Accepted
**Date**: 2026-05-25

## Context

Every multi-service project has environment variables that differ per developer: database URLs, API keys, auth secrets. The current state of the art is a `.env.example` file that developers copy and fill in manually, often guided by a README section that drifts from reality.

devme's "no README" goal demands that env var setup is interactive, contextual, and incremental. When a developer runs `devme` and an env var is missing, devme should prompt for it with help text explaining where to find the value. When a teammate adds a new `[env.STRIPE_KEY]` to `devme.toml` and pushes, every other developer gets prompted for just that one var on their next `devme` run.

## Decision

Add a top-level `[env.*]` table to `devme.toml`. Each key declares an expected environment variable with optional metadata:

```toml
[env.DATABASE_URL]
required = true
default = "postgresql://devme:devme@localhost:5432/devme_web"
help = "Connection string. Default matches the docker container devme starts."

[env.BETTER_AUTH_SECRET]
generate = "npx -y @better-auth/cli secret"
help = "Auth signing key. Auto-generated on first run."

[env.VITE_POSTHOG_KEY]
help = "Find it at: app.posthog.com → your project → Settings → Project API Key"

[env.VITE_POSTHOG_HOST]
choices = ["https://us.i.posthog.com", "https://eu.i.posthog.com"]
default = "https://us.i.posthog.com"
help = "PostHog ingestion endpoint."
```

### Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `required` | bool | `false` | If true, devme blocks startup until set. If false, skippable. |
| `default` | string | none | Pre-filled value. Supports `{service.port}` interpolation. |
| `help` | string | none | Shown below the prompt. Explains where to find the value. |
| `generate` | string | none | Shell command whose stdout becomes the value. Runs automatically if the var is missing and the user doesn't provide input. |
| `choices` | string[] | none | Renders a selector instead of free-text input. |

### Resolution flow

Runs before step checks, after config parsing:

1. Parse `[env.*]` declarations from `devme.toml`
2. Read existing `.env.local` (or `.env` — configurable via `env_file` top-level key)
3. Diff: find keys declared in toml but missing from the file
4. For each missing key (in declaration order):
   - If `generate` is set and no `choices`/`required`: run the command, use stdout, log the result
   - If `choices` is set: show a selector with the options
   - Otherwise: show a text prompt with `default` pre-filled and `help` below
   - If not `required`: allow skipping (Enter with no input)
5. Append new values to `.env.local`
6. Load the full `.env.local` into the process environment for all subsequent steps and services

### Interaction with services

Services' `env = { ... }` table in `devme.toml` provides hardcoded overrides. The `[env.*]` system manages the `.env.local` file which the application framework (dotenv, Vite, etc.) loads at runtime. devme does not inject `[env.*]` values into service processes — it manages the file that the app reads.

## Consequences

### Positive

- New env vars are automatically prompted when teammates pull — no Slack message or README update needed.
- Help text is colocated with the declaration, not in a separate doc.
- `generate` eliminates the "copy this command" step for secrets.
- `choices` prevents typos for enum-like values (regions, hosts, tiers).
- The `.env.local` file remains the app's source of truth — devme writes it but doesn't own the format.

### Negative

- `generate` runs arbitrary shell commands — same trust model as `provision`, gated by consent.
- Reading/writing `.env.local` requires a simple parser that handles quoting and comments without losing formatting.

## Alternatives considered

### Custom wizard scripts per-project

The wizard protocol (ADR-0011) supports arbitrary interactive provisioning scripts. Using it for env vars works but requires every project to write a bespoke TypeScript/Python script that reimplements the same pattern: read file, diff keys, prompt, write file. The declarative approach handles the common case; wizard scripts remain available for exotic provisioning needs.

### Inject env vars into service processes directly

Rejected. The application framework (Vite, dotenv, Django) already loads `.env.local`. Injecting via devme would create a second source of truth and break `npm run dev` outside of devme.
