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

All commands are shell loops — no real binaries needed.
