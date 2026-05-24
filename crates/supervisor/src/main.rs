//! Per-instance supervisor daemon binary. Loads `devme.toml` from the
//! current working directory, validates it, binds the Unix socket, and
//! runs the serve loop until shutdown.
//!
//! See `docs/adr/0003-daemon-per-instance-lifecycle.md`.

use devme_config::Stack;
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
    eprintln!("devme-supervisor: listening on {}", sock_path.display());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        // bind must run inside the tokio runtime — UnixListener registers
        // with the reactor at creation time.
        let server = DaemonServer::bind_with_stack(&sock_path, stack)?;
        server.serve().await
    })?;
    Ok(())
}

