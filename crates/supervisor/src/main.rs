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

    let toml = std::fs::read_to_string(&config_path).map_err(|e| {
        anyhow::anyhow!("reading {}: {e}", config_path.display())
    })?;
    let stack = Stack::parse(&toml).map_err(|e| anyhow::anyhow!("parsing config: {e}"))?;
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
    let label = canonical_cwd
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("devme")
        .to_string();
    let instance = InstanceInfo {
        id: instance_id.clone(),
        label,
        cwd: canonical_cwd.display().to_string(),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async move {
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

