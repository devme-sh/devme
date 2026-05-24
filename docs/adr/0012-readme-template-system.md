# ADR-0012: README template system and web editor

**Status**: Deferred (v2)
**Date**: 2026-05-24

## Context

devme positions itself as the "executable README" — `devme.toml` is the source of truth for how a project is set up and run. The v1 `devme readme` command generates a Markdown README from the config, but the template format is minimal and the authoring experience is code-only.

Most READMEs contain a mix of generated content (prerequisites, install commands, service table, port mappings) and hand-written prose (project description, architecture overview, contributing guidelines). A rigid fully-generated README doesn't serve projects with rich documentation needs; a fully hand-written README drifts from reality.

We want a system where the README is a template with auto-generated component slots and free-form prose sections, authored through a visual editor rather than raw Markdown, with shareable templates across projects.

## Decision

Defer the following to v2, building on the v1 foundation of `devme readme` and `devme readme --check`:

### Template format

A Markdown-based template (`README.devme` or `.devme/readme.template`) with component blocks and prose sections:

```markdown
# {{ project.name }}

{{ project.description }}

## Getting Started

<!-- devme:component install -->
<!-- devme:component prerequisites -->

## Architecture

{{ prose }}
Hand-written content that devme preserves as-is.
{{ /prose }}

<!-- devme:component services -->
<!-- devme:component ports -->

## Contributing

<!-- devme:component contributing -->
```

Components are rendered from `devme.toml` and repo introspection (file tree, package manifests, language detection). Prose blocks are pass-through.

### Component library

Built-in components: `install`, `prerequisites`, `services`, `ports`, `environment`, `contributing`, `license`, `repo-structure`, `scripts`. Each component has sensible defaults and is configurable via parameters (e.g. `<!-- devme:component services format="table" -->`).

### Web editor

A web app (hosted at devme.sh) that:

- Connects to a GitHub repo and scans its structure
- Auto-detects relevant components
- Provides drag-and-drop section ordering
- Offers inline prose editing with live Markdown preview
- Supports themes (minimal, detailed, badge-heavy, visual)
- Publishes the template as a PR to the repo, including a GitHub Action for regeneration on push

### Template marketplace

A registry of community-contributed README templates. Templates are parameterized by project type (Rust CLI, Next.js app, Python library, monorepo) and theme. Users browse, preview, and fork templates from the web editor.

## Consequences

### Positive

- READMEs stay in sync with the actual project setup without manual maintenance.
- The web editor lowers the barrier to well-structured READMEs — authors who aren't comfortable writing Markdown from scratch get a guided experience.
- The marketplace creates a network effect: more templates attract more users, more users contribute more templates.
- CI integration (`devme readme --check`) prevents documentation drift.

### Negative

- The web editor is a separate product surface (hosting, auth, GitHub OAuth, persistence) with its own maintenance burden.
- Template format design is hard to get right — too rigid and it can't express real READMEs; too flexible and it's just Markdown with extra steps.
- The marketplace needs moderation, versioning, and quality control.

## Alternatives considered

### Ship the template system in v1

Rejected because the template format needs real-world feedback to get right. The v1 `devme readme` with simple generation gives us signal on what people actually want to customize before we commit to a template DSL.

### Use an existing README generator (readme-md-generator, etc.)

These tools generate a one-time scaffold; they don't keep the README in sync with a live config. The value of devme's approach is continuous generation from the source of truth, not a one-shot template.

### Build the editor as a CLI TUI instead of a web app

A TUI editor for rich document layout is awkward — drag-and-drop, inline preview, and theme browsing are inherently visual. The web is the right medium for this. The CLI stays focused on generation and validation.
