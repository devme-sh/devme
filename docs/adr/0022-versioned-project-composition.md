# ADR-0022: Versioned project composition

**Status**: Accepted
**Date**: 2026-07-15

## Context

Devme already makes an existing repository discoverable and runnable. The native starter also needs a safe way to create a thin project and add optional capabilities later. Git history is not a suitable public module interface because generated repositories do not preserve a useful relationship to the template's commits, and source rollback cannot reverse provider data, subscriptions, store identifiers, or credentials.

## Decision

Devme bundles a generic Project composer and keeps recipes independently versioned.

- `devme create native <path>` creates a project.
- `devme create native <path> --with <feature>` composes repeatable initial features.
- `devme feature add|remove|update|list` manages an existing composed project.
- `devme feature continue|abort` recovers an interrupted mutation.
- `devme create add` is invalid and points to `devme feature add`.

The stable `native` recipe locator follows the recipe repository's `main` branch. The recipe pins its base to a full Git commit ID, and each generated project records the recipe name, version, recipe SHA-256 digest, feature versions, and managed file digests in `.devme/composition.lock`. The recipe digest covers the manifest and every local payload file, including executable mode. A changed digest under the same recipe version fails closed. Devme releases and recipe releases are therefore independent.

Every mutation plans all affected files before writing. A recipe can replace a file only when the composition lock owns the current digest. Existing untracked files and modified managed files produce a structured conflict with exit code 5 and exact recovery guidance. A feature may declare complete payload files as `generated_files`; drift on those paths may be replaced only when the path is new or already recipe-managed, and every such replacement is reported as `regenerated_files`. This never adopts an app-owned file. Recipe paths must be safe and relative, source and target symlinks are rejected, case-colliding paths and composer authority paths are reserved, executable modes are retained, and recipes never execute arbitrary setup commands.

The first recipe format supports complete managed files and generated-file boundaries. It deliberately does not offer textual search-and-replace. A feature that cannot own a complete file must wait for a syntax-aware adapter for that file type or move the seam behind a generated file.

Before a feature mutation, Devme records the touched file states, composition lock, and feature backups in an Operation journal. Continue first restores the recorded pre-operation state and then reruns the operation. Abort restores that state. Either command refuses to proceed if a touched app file changed after interruption.

Feature removal restores source files but does not claim to undo external effects. Recipe `external_steps` and `remove_external_steps` are reported as typed, untrusted manual guidance and recorded separately. They are never commands. Provider accounts, remote data, store resources, active subscriptions, and credentials require explicit lifecycle instructions owned by the feature recipe.

## Consequences

- The CLI surface stays small: create is project initialization, feature is project evolution.
- Agents receive strict TOON or JSON reports, semantic exit codes, changed paths, external steps, and next commands.
- Template maintainers can publish recipe changes without releasing a new Devme binary.
- Auth and billing must be extracted into honest independent recipes before they are advertised as optional features.
- The initial native recipe publishes the verified thin core only. The integrated auth and Stripe app remains a reference until those boundaries are extracted.
