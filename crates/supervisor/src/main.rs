//! Per-instance supervisor daemon binary. Loads `devme.toml` from the
//! current working directory, validates it, binds the Unix socket, and
//! runs the serve loop until shutdown.
//!
//! See `docs/adr/0003-daemon-per-instance-lifecycle.md`.

use devme_config::Stack;
use devme_core::InstanceInfo;
use devme_slot_allocator::SlotAllocator;
use devme_supervisor::daemon::DaemonServer;

fn main() {
    if let Err(e) = real_main() {
        eprintln!("devme-supervisor: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config_path = cwd.join("devme.toml");

    let toml = std::fs::read_to_string(&config_path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", config_path.display()))?;
    let mut stack = Stack::parse(&toml).map_err(|e| anyhow::anyhow!("parsing config: {e}"))?;

    // Repo-scoped services are owned by the shared supervisor (ADR-0007).
    // Convert them to external services with a health check so the
    // dependency graph stays intact — dependents like db_migrate can still
    // wait for postgres to be healthy without the instance supervisor
    // trying to spawn it.
    for svc in stack.service.values_mut() {
        if svc.scope == devme_core::Scope::Repo {
            svc.external = true;
            if svc.health.is_none()
                && let Some(port) = svc.port
            {
                let resolved = port.resolve(0);
                svc.health = Some(devme_core::HealthCheck::Tcp {
                    tcp: format!("localhost:{resolved}"),
                });
            }
        }
    }

    // Whether this stack has any repo-scoped (now `external`) services. If so,
    // their lifecycle is owned by the shared supervisor — which this daemon
    // must make sure is running, otherwise nothing ever spawns proxy/postgres
    // and every dependent here waits forever. The TUI / `devme up` also try to
    // ensure it, but doing it here too makes the daemon self-healing: it's the
    // one process guaranteed to be alive whenever this stack's services run, so
    // a fire-and-forget front-end attempt that raced or failed silently can't
    // leave the stack wedged. See ADR-0007.
    let has_external = stack.service.values().any(|s| s.external);
    let cwd_for_shared = cwd.clone();

    devme_config::validate(&stack).map_err(|errors| {
        let joined = errors
            .iter()
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::anyhow!("config invalid:\n{joined}")
    })?;

    let sock_path = devme_config::paths::supervisor_socket(&cwd)?;
    let instance_id = devme_config::paths::instance_id(&cwd);

    // Claim our port slot before binding so the IPC handshake never sees a
    // partially-configured daemon. Held for the daemon's lifetime; released
    // on Drop via the explicit `release` call below.
    let registry = devme_config::paths::slot_registry()?;
    let allocator = SlotAllocator::open(&registry);
    let slot = allocator
        .claim(&instance_id)
        .map_err(|e| anyhow::anyhow!("claiming port slot: {e}"))?;

    eprintln!(
        "devme-supervisor: slot {slot} • listening on {}",
        sock_path.display()
    );

    // Identity the daemon advertises over the wire. Label is the worktree
    // basename so the TUI sidebar reads naturally (`smoke`, `web-app`, …);
    // cwd is canonicalised so two paths that resolve to the same directory
    // present the same identity. `id` is the path hash already used for
    // socket naming.
    let canonical_cwd = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let label = git_branch_name(&canonical_cwd).unwrap_or_else(|| {
        canonical_cwd
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("devme")
            .to_string()
    });
    let instance = InstanceInfo {
        id: instance_id.clone(),
        label,
        cwd: canonical_cwd.display().to_string(),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async move {
        // Make sure the shared supervisor (owner of repo-scoped services) is up
        // before we start health-probing its services. Non-fatal: a transient
        // failure shouldn't stop this daemon from coming up, and the probe loop
        // will pick the services up once the shared daemon is healthy.
        if has_external
            && let Err(e) = devme_supervisor::spawn::ensure_shared_daemon(&cwd_for_shared).await
        {
            eprintln!("devme-supervisor: shared supervisor not started: {e}");
        }
        // bind must run inside the tokio runtime — UnixListener registers
        // with the reactor at creation time.
        let server = DaemonServer::bind_with_instance(&sock_path, stack, slot, instance)?;
        server.serve().await
    });

    // Release the slot whether the loop exited cleanly or errored out, so a
    // crashed daemon doesn't hog a slot that the next process needs.
    let _ = allocator.release(&instance_id);

    result?;
    Ok(())
}

fn git_branch_name(cwd: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8(out.stdout).ok()?;
    let trimmed = branch.trim();
    if trimmed.is_empty() || trimmed == "HEAD" {
        return None;
    }
    Some(trimmed.to_string())
}
