# ADR-0020: Explicit one-level workspace composition

**Status**: Accepted
**Date**: 2026-07-13

## Context

A native-mobile monorepo can contain a backend, an iOS app, and an Android app
with enough orchestration configuration that one root file becomes difficult
to navigate. Developers and coding agents also work from member directories and
expect `devme` there to focus the local app without losing declared backend
dependencies, worktree isolation, shared logs, or scarce-resource safety.

Independent nested stacks would fragment the runtime. They could allocate
conflicting ports and devices, start duplicate backends, and write histories
that cannot be correlated. Recursive discovery would also make an executable
configuration change when an unrelated file appears.

## Decision

The workspace root may explicitly list optional child configs:

```toml
[workspace.members]
backend = "backend"
ios = "apps/ios"
android = "apps/android"
```

Membership is one level deep and opt-in. Every member path is relative to the
root, must remain inside it, and must contain `devme.toml`. A child cannot
declare another workspace. Devme does not recursively discover configs.

The resolver flattens the root and its children into one graph before normal
validation. Root nodes keep their existing names. Child nodes receive stable
`member::node` names. Within a child, an unqualified dependency means the local
member, a qualified name addresses another member, and `root::name` addresses
a root node. Names containing `::` are reserved in source configs.

Commands, working directories, log-tail paths, setup checks, and provisioning
paths remain relative to the config that declared them. The resolver rebases
them to the workspace root. Workspace-wide persistence and redaction policy is
declared at the root.

An invocation directory establishes focus, not a second runtime. At the root,
qualified names are available and a bare interactive `devme` targets the whole
workspace. Within a member, local CLI names resolve to that member and a bare
interactive invocation targets its services. Normal dependency closure can
still start qualified backend or infrastructure services. Non-interactive bare
`devme` remains a read-only, directory-scoped agent context.

A focused member can also declare a narrow `[session]` composition over its
existing services, root or local resources, and an optional launch task. For
example, an iOS session can hold a simulator lease while its backend closure,
simulator-log sidecar, and launch command run. Session declarations are
namespaced and resolved by the same local and cross-member rules; they do not
introduce a second dependency graph.

There is one supervisor socket, worktree slot, state domain, service graph,
resource namespace, and correlated log history per worktree. A member config is
an organizational boundary only. Xcode, Gradle, Convex, Vite+, Bun, and other
native tools remain authoritative for their own build graphs.

## Consequences

Teams can keep related commands beside each app and run concise local commands
without giving up deterministic root orchestration. Cross-member dependencies
are visible in configuration and targeted startup does not launch unrelated
apps.

Moving a declaration between root and child changes its relative path and name,
so automatic splitting would be lossy. `devme setup split --dry-run` previews
the complete file plan and `devme setup split --write` explicitly applies it.
Single-file setup output remains the conservative default.

Unlisted nested configs and recursive workspaces are errors rather than hidden
standalone runtimes. Devme gains no package discovery, package dependency
graph, build cache, or native build semantics.

This decision resolves the nested-config question left open by ADR-0013 while
preserving its principle that Devme orchestrates explicit services and delegates
build semantics to authoritative project tools.
