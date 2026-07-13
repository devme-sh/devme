# devme examples

Sandbox stacks for local testing. Each subdirectory is its own stack
with a `devme.toml`. Run `devme` (TUI) or `devme up` (foreground)
inside any of them.

| Path                       | What it exercises                                                                   |
| -------------------------- | ----------------------------------------------------------------------------------- |
| `smoke/`                   | Three services covering the happy path, on-failure restart, and never-restart.      |
| `web-app/`                 | Realistic six-node graph: db, cache, api, worker, web, plus a one-shot `migrations` step. Shows port-slot interpolation and optional deps. |
| `shared/frontend/`, `shared/backend/` | Two stacks that share a `scope = "repo"` cache service. Demonstrates the shared-supervisor coordination. |
| `interp-envfile/`          | Cross-service port interpolation (`frontend` env references `{port.backend}`), a `scope = "repo"` fixed-port `proxy`, and `env_file = ".env"`. |
| `native-mobile-monorepo/`  | Explicit root + backend + iOS + Android configs, cross-member readiness, isolated writable state, generic device-log services, and host-scoped runtime leases. |

Most examples use portable shell loops. The native-mobile example intentionally
delegates to Bun, xcodebuild, Gradle, and adb; the CLI test suite has a matching
portable executable fixture for environments without those toolchains.

The native-mobile example also demonstrates optional workspace composition.
Its root `devme.toml` explicitly lists three one-level members. From the root,
names such as `ios::test` address the flattened workspace graph. From
`apps/ios`, the local alias `devme run test` resolves to that same task and can
start the declared `backend::api` dependency. There is still one supervisor,
one worktree slot, and one correlated history for the whole workspace.
