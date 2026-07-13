//! Runtime entry point shared by the standalone supervisor binary and the
//! hidden, version-locked launcher in the `devme` CLI.

use devme_config::ResolvedWorkspace;
use devme_core::InstanceInfo;
use devme_slot_allocator::SlotAllocator;

use crate::daemon::DaemonServer;

pub fn run() -> anyhow::Result<()> {
    let invocation = std::env::current_dir()?;
    let resolved = ResolvedWorkspace::resolve(&invocation)
        .map_err(|error| anyhow::anyhow!("resolving workspace: {error}"))?;
    let cwd = resolved.root().to_path_buf();
    let mut stack = resolved.into_stack();

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

    let has_external = stack.service.values().any(|service| service.external);
    let cwd_for_shared = cwd.clone();

    devme_config::validate(&stack).map_err(|errors| {
        let joined = errors
            .iter()
            .map(|error| format!("  - {error}"))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::anyhow!("config invalid:\n{joined}")
    })?;

    let sock_path = devme_config::paths::supervisor_socket(&cwd)?;
    let instance_id = devme_config::paths::instance_id(&cwd);
    let registry = devme_config::paths::slot_registry()?;
    let allocator = SlotAllocator::open(&registry);
    let slot = allocator
        .claim(&instance_id)
        .map_err(|error| anyhow::anyhow!("claiming port slot: {error}"))?;

    eprintln!(
        "devme-supervisor: slot {slot} • listening on {}",
        sock_path.display()
    );

    let canonical_cwd = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let label = git_branch_name(&canonical_cwd).unwrap_or_else(|| {
        canonical_cwd
            .file_name()
            .and_then(|name| name.to_str())
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
        if has_external
            && let Err(error) = crate::spawn::ensure_shared_daemon(&cwd_for_shared).await
        {
            eprintln!("devme-supervisor: shared supervisor not started: {error}");
        }
        let server = DaemonServer::bind_with_instance(&sock_path, stack, slot, instance)?;
        server.serve().await
    });

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
