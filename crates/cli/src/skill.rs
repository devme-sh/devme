//! `devme skill …` — user-facing output over the shared skill logic in
//! [`devme_config::skill`]. The embedding, hashing, install/state, and
//! auto-update all live in the config crate (so the TUI can share them); this
//! module only decides what to print.

use std::path::Path;

use devme_config::GlobalConfig;
use devme_config::skill::{self, InstallOutcome, RemoveOutcome};

/// `devme skill install [--global] [--force]`.
pub fn install(global: bool, force: bool, json: bool) -> anyhow::Result<()> {
    let (path, outcome) = skill::install(global, force).map_err(|e| anyhow::anyhow!("{e}"))?;
    let verb = match outcome {
        InstallOutcome::AlreadyCurrent => "up-to-date",
        InstallOutcome::Installed => "installed",
        InstallOutcome::Updated => "updated",
    };
    emit(json, verb, &path.to_string_lossy(), &skill::embedded_version());
    Ok(())
}

/// `devme skill uninstall [--global]`.
pub fn uninstall(global: bool, json: bool) -> anyhow::Result<()> {
    let (path, outcome) = skill::uninstall(global).map_err(|e| anyhow::anyhow!("{e}"))?;
    let verb = match outcome {
        RemoveOutcome::Absent => "absent",
        RemoveOutcome::Removed => "removed",
    };
    emit(json, verb, &path.to_string_lossy(), &skill::embedded_version());
    Ok(())
}

/// `devme skill status` — every place the skill lives and whether it's current.
pub fn status(json: bool) -> anyhow::Result<()> {
    let cfg = GlobalConfig::load();

    // Candidate locations: the two default scopes plus anything we recorded.
    let mut paths: Vec<String> = Vec::new();
    for p in [skill::skill_file(false), skill::skill_file(true)].into_iter().flatten() {
        paths.push(p.to_string_lossy().to_string());
    }
    for k in cfg.skill_installs().keys() {
        if !paths.contains(k) {
            paths.push(k.clone());
        }
    }

    let rows: Vec<(String, skill::InstallStatus)> = paths
        .iter()
        .map(|p| (p.clone(), skill::status_at(Path::new(p), cfg.skill_installs().get(p))))
        .collect();

    if json {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|(p, s)| serde_json::json!({ "path": p, "status": s.label() }))
            .collect();
        devme_ui::json(&serde_json::json!({
            "version": skill::embedded_version(),
            "installs": arr,
        }));
        return Ok(());
    }

    println!("devme skill {} (embedded in this binary)", skill::embedded_version());
    let mut any = false;
    for (p, s) in &rows {
        if *s == skill::InstallStatus::Missing {
            continue;
        }
        any = true;
        println!("  {:<11} {p}", s.label());
    }
    if !any {
        println!("  not installed");
        devme_ui::hint("devme skill install (or --global)");
    }
    Ok(())
}

fn emit(json: bool, verb: &str, path: &str, version: &str) {
    if json {
        devme_ui::json(&serde_json::json!({
            "action": verb,
            "path": path,
            "version": version,
        }));
    } else {
        // Mutation narration, not data — `devme skill: installed …` on
        // stderr, like every other one-liner (ADR-0017).
        devme_ui::scoped("skill").success(format!("{verb}: {path} (v{version})"));
    }
}
