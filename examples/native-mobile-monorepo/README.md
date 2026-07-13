# Native-mobile workspace example

This example keeps one Devme orchestration interface while organizing the
configuration beside three independently authoritative toolchains:

```text
devme.toml                  workspace membership, logs, resources, aggregate task
backend/devme.toml          Bun, Vite+, Convex, and backend readiness
apps/ios/devme.toml         xcodebuild and simulator logs
apps/android/devme.toml     Gradle and adb logcat
```

The files are intentionally configuration-only. They are a reusable reference,
not an application and not a replacement build graph.

From the root:

```sh
devme config check
devme run backend::test
devme run ios::test
devme run android::test
devme run check
```

From a member, use its local alias:

```sh
cd apps/ios
devme run test
```

The iOS `dev` session acquires an iOS runtime and signing lease, starts the
simulator log sidecar, converges `backend::api` and Convex through the sidecar's
dependency closure, then runs the optional `launch` task. It does not start
Android. The Convex probe calls its function-spec command so process startup or
an open port alone cannot unblock the app. Session-scoped sidecars stop before
their resource leases are released, with a short linger for reconnecting
clients.

The root owns host-scoped runtime and signing leases. Each tool writes generated
state under `{worktree}/.devme` with a worktree slot or allocated runtime slot
in the path. Define `CONVEX_URL`, `SIMULATOR_UDID`, and `ANDROID_SERIAL` through
the normal project environment before running the corresponding real services.
Projects can replace the direct simctl or adb commands with wrappers that map a
generic allocated slot to their chosen device identity.

Simulator and adb streams are ordinary Devme services. Their records share the
root retention and redaction policy with backend and task records, so agents can
inspect one timestamp-ordered history without mobile-specific Devme code.
