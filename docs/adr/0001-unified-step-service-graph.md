# ADR-0001: Unified Step + Service graph model

**Status**: Accepted
**Date**: 2026-05-23

## Context

devstack needs to handle both setup work (installing gcloud, generating `.env`, logging into GCP) and runtime work (running backend, frontend, database). Most tools in the space (foreman, mprocs, process-compose) only model the runtime side; the user is expected to read a README and run setup commands manually. Our "no README" goal requires that typing `devstack` in a fresh-clone repo handles the entire path from missing prerequisites to running stack.

We could model setup and runtime as two phases (separate config sections, separate execution paths), as one graph with two node kinds (Step + Service), or with a single uniform node type that has different attributes.

## Decision

Use a single DAG containing two node kinds: **Step** (oneshot, with a `check` command that determines satisfaction) and **Service** (long-running, kept alive). Both can declare `depends_on` edges to either kind. On launch, devstack walks the graph; Steps run their `check` first and only execute their `provision` if the check fails; Services start once their dependencies are satisfied.

## Consequences

- One execution model to implement, one mental model for the config author.
- "Setup" and "runtime" become emergent rather than declared — a Step is anything with a `check`, a Service is anything that stays alive. No phase boundary to manage.
- Future node kinds (e.g., periodic Steps, finalize-on-shutdown Services) can be added without restructuring the graph.
- Slightly more verbose config than a "just list your services" model, because every prerequisite needs a `check` + `provision`. Trivial cases (`command -v X`) are still one-liners.

## Alternatives considered

**Two-phase model (`devstack setup` then `devstack`).** Cleaner separation but forces a mode switch on the user. Breaks the "type `devstack` and it works" promise. Out.

**Implicit setup via separate config sections (`[setup.X]` + `[service.X]`).** Conceptually splits the two but couples their execution. Cannot model "this setup step depends on a running service" or "this service depends on a one-shot warm-up." Out.

**No setup at all; rely on README.** Disqualified by product goal.
