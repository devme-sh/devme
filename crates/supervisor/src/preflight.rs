//! Pre-daemon step checks (ADR-0014 companion).
//!
//! Runs `check` commands for steps that don't depend on services,
//! renders results in Clack-style, and offers to provision failures
//! before the daemon starts. Steps that depend on services are left
//! to the daemon's executor.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Command;

use devme_config::{Provision, Stack};
use devme_core::Trust;

// Shared Clack-style constants
const S_BAR: &str = "│";
const S_BAR_END: &str = "└";
const S_STEP_ACTIVE: &str = "◆";
const S_STEP_SUBMIT: &str = "◇";
const C_RESET: &str = "\x1b[0m";
const C_DIM: &str = "\x1b[2m";
const C_BOLD: &str = "\x1b[1m";
const C_CYAN: &str = "\x1b[36m";
const C_GREEN: &str = "\x1b[32m";
const C_YELLOW: &str = "\x1b[33m";
const C_RED: &str = "\x1b[31m";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepResult {
    Passed,
    Provisioned,
    Failed,
    Skipped,
    /// `trust = "manual"`: the suggested command was displayed but devme
    /// deliberately did not run it — the user must run it themselves.
    Manual,
}

pub struct PreflightResult {
    pub results: Vec<(String, StepResult)>,
}

/// Identify steps that can run pre-daemon: those whose entire transitive
/// dependency chain contains only other steps (no services).
fn preflight_steps(stack: &Stack) -> Vec<String> {
    let service_names: HashSet<&str> = stack.service.keys().map(String::as_str).collect();

    // A step is preflight-eligible if none of its transitive deps are services
    let mut eligible: HashSet<String> = HashSet::new();
    let mut ineligible: HashSet<String> = HashSet::new();

    fn check_eligible(
        name: &str,
        stack: &Stack,
        service_names: &HashSet<&str>,
        eligible: &mut HashSet<String>,
        ineligible: &mut HashSet<String>,
    ) -> bool {
        if eligible.contains(name) {
            return true;
        }
        if ineligible.contains(name) || service_names.contains(name) {
            return false;
        }
        let step = match stack.step.get(name) {
            Some(s) => s,
            None => return false,
        };
        for dep in &step.depends_on {
            if service_names.contains(dep.name.as_str()) {
                ineligible.insert(name.to_string());
                return false;
            }
            if !check_eligible(&dep.name, stack, service_names, eligible, ineligible) {
                ineligible.insert(name.to_string());
                return false;
            }
        }
        eligible.insert(name.to_string());
        true
    }

    for name in stack.step.keys() {
        check_eligible(name, stack, &service_names, &mut eligible, &mut ineligible);
    }

    // Return in declaration order
    stack
        .step
        .keys()
        .filter(|name| eligible.contains(name.as_str()))
        .cloned()
        .collect()
}

fn run_check(cmd: &str, cwd: &Path) -> bool {
    Command::new("sh")
        .args(["-c", cmd])
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_provision(cmd: &str, cwd: &Path) -> bool {
    Command::new("sh")
        .args(["-c", cmd])
        .current_dir(cwd)
        // stdin is intentionally left inherited so interactive installers
        // (rustup's menu, `gh auth login`, any `read` prompt) can talk to the
        // user's real terminal. The preflight runs before the TUI takes the
        // screen, so the terminal is still in cooked mode here.
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a provision, re-check, and render the outcome line. Shared by the
/// `auto` path (runs unconditionally) and the `prompt` path (runs after the
/// user hits Enter). Returns the resulting [`StepResult`].
fn provision_and_report<W: Write>(
    check: &str,
    cmd: &str,
    cwd: &Path,
    output: &mut W,
) -> Result<StepResult, std::io::Error> {
    writeln!(
        output,
        "  {C_DIM}{S_BAR}{C_RESET}    {C_DIM}running...{C_RESET}"
    )?;
    if run_provision(cmd, cwd) && run_check(check, cwd) {
        writeln!(
            output,
            "  {C_DIM}{S_BAR}{C_RESET}    {C_GREEN}{S_STEP_SUBMIT} Installed{C_RESET}"
        )?;
        Ok(StepResult::Provisioned)
    } else {
        writeln!(
            output,
            "  {C_DIM}{S_BAR}{C_RESET}    {C_RED}▲ Failed to install{C_RESET}"
        )?;
        Ok(StepResult::Failed)
    }
}

/// True if every preflight step's `check` command passes. Runs silently.
pub fn all_checks_pass(stack: &Stack, cwd: &Path) -> bool {
    let steps = preflight_steps(stack);
    steps.iter().all(|name| {
        let step = &stack.step[name];
        run_check(&step.check, cwd)
    })
}

/// Run preflight step checks and render results.
/// Returns the check results so the caller can decide whether to proceed.
///
/// `assume_yes` (the `--yes` flag) promotes every `trust = "prompt"` step to
/// `auto` for this run — fixes run without asking. `manual` steps are never
/// run regardless (ADR-0002).
pub fn run_preflight<R: BufRead, W: Write>(
    stack: &Stack,
    cwd: &Path,
    input: &mut R,
    output: &mut W,
    interactive: bool,
    assume_yes: bool,
) -> Result<PreflightResult, std::io::Error> {
    let steps = preflight_steps(stack);
    if steps.is_empty() {
        return Ok(PreflightResult { results: vec![] });
    }

    let mut results = Vec::new();

    // First pass: run all checks
    let mut check_results: Vec<(String, bool)> = Vec::new();
    for name in &steps {
        let step = &stack.step[name];
        let passed = run_check(&step.check, cwd);
        check_results.push((name.clone(), passed));
    }

    let all_passed = check_results.iter().all(|(_, p)| *p);
    let _any_failed = check_results.iter().any(|(_, p)| !*p);

    // Render header
    writeln!(output)?;
    writeln!(
        output,
        "  {C_CYAN}{S_STEP_ACTIVE}{C_RESET}  {C_BOLD}Check dependencies{C_RESET}"
    )?;
    writeln!(output, "  {C_DIM}{S_BAR}{C_RESET}")?;

    if all_passed {
        // Everything is good — compact display
        for (name, _) in &check_results {
            let step = &stack.step[name];
            let label = step.description.as_deref().unwrap_or(name);
            writeln!(
                output,
                "  {C_DIM}{S_BAR}{C_RESET}  {C_GREEN}{S_STEP_SUBMIT}{C_RESET} {label}"
            )?;
        }
        writeln!(output, "  {C_DIM}{S_BAR}{C_RESET}")?;
        writeln!(
            output,
            "  {S_BAR_END}  {C_GREEN}All dependencies satisfied{C_RESET}"
        )?;
        writeln!(output)?;

        for (name, _) in check_results {
            results.push((name, StepResult::Passed));
        }
        return Ok(PreflightResult { results });
    }

    // Some failed — show each with status and offer to provision
    for (name, passed) in &check_results {
        let step = &stack.step[name];
        let label = step.description.as_deref().unwrap_or(name.as_str());

        if *passed {
            writeln!(
                output,
                "  {C_DIM}{S_BAR}{C_RESET}  {C_GREEN}{S_STEP_SUBMIT}{C_RESET} {label}"
            )?;
            results.push((name.clone(), StepResult::Passed));
        } else {
            writeln!(
                output,
                "  {C_DIM}{S_BAR}{C_RESET}  {C_YELLOW}▲{C_RESET} {C_BOLD}{label}{C_RESET}  {C_DIM}not found{C_RESET}"
            )?;

            // `--yes` promotes a `prompt` step to `auto`; `manual` is never
            // promoted and `auto` is already unattended.
            let effective_trust = if assume_yes && step.trust == Trust::Prompt {
                Trust::Auto
            } else {
                step.trust
            };

            match &step.provision {
                Some(Provision::Shell(cmd)) => {
                    writeln!(
                        output,
                        "  {C_DIM}{S_BAR}{C_RESET}    {C_DIM}fix: {cmd}{C_RESET}"
                    )?;
                    match effective_trust {
                        // Never auto-run: surface the command for the user.
                        Trust::Manual => {
                            writeln!(
                                output,
                                "  {C_DIM}{S_BAR}{C_RESET}    {C_DIM}run this yourself{C_RESET}"
                            )?;
                            results.push((name.clone(), StepResult::Manual));
                        }
                        // Safe by declaration (or `--yes`): run without asking.
                        Trust::Auto => {
                            let r = provision_and_report(&step.check, cmd, cwd, output)?;
                            results.push((name.clone(), r));
                        }
                        // Ask first — but only when we have a terminal.
                        Trust::Prompt => {
                            if !interactive {
                                results.push((name.clone(), StepResult::Failed));
                                continue;
                            }
                            write!(
                                output,
                                "  {C_DIM}{S_BAR}{C_RESET}    {C_DIM}Run fix? Enter to run, s to skip ›{C_RESET} "
                            )?;
                            output.flush()?;

                            let mut line = String::new();
                            match input.read_line(&mut line) {
                                Ok(0) => {
                                    writeln!(output)?;
                                    results.push((name.clone(), StepResult::Skipped));
                                    continue;
                                }
                                Ok(_) => {}
                                Err(_) => {
                                    results.push((name.clone(), StepResult::Skipped));
                                    continue;
                                }
                            }

                            let trimmed = line.trim();
                            if trimmed == "s" || trimmed == "S" || trimmed == "skip" {
                                writeln!(
                                    output,
                                    "  {C_DIM}{S_BAR}{C_RESET}    {C_DIM}{S_STEP_SUBMIT} Skipped{C_RESET}"
                                )?;
                                results.push((name.clone(), StepResult::Skipped));
                            } else {
                                let r = provision_and_report(&step.check, cmd, cwd, output)?;
                                results.push((name.clone(), r));
                            }
                        }
                    }
                }
                Some(Provision::Wizard { wizard }) => {
                    writeln!(
                        output,
                        "  {C_DIM}{S_BAR}{C_RESET}    {C_DIM}requires wizard: {wizard}{C_RESET}"
                    )?;
                    results.push((name.clone(), StepResult::Failed));
                }
                None => {
                    // No provision — just report the failure
                    results.push((name.clone(), StepResult::Failed));
                }
            }
        }
    }

    // Outro
    writeln!(output, "  {C_DIM}{S_BAR}{C_RESET}")?;
    let failed_count = results
        .iter()
        .filter(|(_, r)| *r == StepResult::Failed)
        .count();
    if failed_count > 0 {
        writeln!(
            output,
            "  {S_BAR_END}  {C_YELLOW}{failed_count} dependency check{} failed{C_RESET}",
            if failed_count == 1 { "" } else { "s" }
        )?;
    } else {
        writeln!(
            output,
            "  {S_BAR_END}  {C_GREEN}All dependencies resolved{C_RESET}"
        )?;
    }
    writeln!(output)?;

    Ok(PreflightResult { results })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse_stack(toml: &str) -> Stack {
        Stack::parse(toml).expect("parse")
    }

    #[test]
    fn all_passing_shows_compact() {
        let stack = parse_stack(
            r#"
schema_version = 1

[step.shell]
check = "true"
description = "Shell available"
"#,
        );

        let mut input = Cursor::new(b"");
        let mut output = Vec::new();
        let dir = std::env::temp_dir();

        let result = run_preflight(&stack, &dir, &mut input, &mut output, false, false).unwrap();
        assert_eq!(result.results[0].1, StepResult::Passed);

        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("All dependencies satisfied"));
    }

    #[test]
    fn step_depending_on_service_is_excluded() {
        let stack = parse_stack(
            r#"
schema_version = 1

[step.tool]
check = "true"
description = "Tool check"

[step.migrate]
check = "false"
provision = "echo migrate"
depends_on = ["db"]

[service.db]
cmd = "echo db"
"#,
        );

        let steps = preflight_steps(&stack);
        assert_eq!(steps, vec!["tool"]);
        assert!(!steps.contains(&"migrate".to_string()));
    }

    #[test]
    fn failing_step_detected() {
        let stack = parse_stack(
            r#"
schema_version = 1

[step.missing]
check = "false"
provision = "echo install"
description = "Missing tool"
"#,
        );

        let mut input = Cursor::new(b"s\n");
        let mut output = Vec::new();
        let dir = std::env::temp_dir();

        let result = run_preflight(&stack, &dir, &mut input, &mut output, true, false).unwrap();
        assert_eq!(result.results[0].1, StepResult::Skipped);
    }

    /// A throwaway directory used as `cwd` so provisions can `touch` a marker
    /// relative to it without env vars or cross-test races. Removed on drop.
    struct TempCwd(std::path::PathBuf);
    impl TempCwd {
        fn new(tag: &str) -> Self {
            // Unique per (process, tag, address-of-local) to avoid collisions
            // across parallel tests without needing a clock or RNG.
            let mut p = std::env::temp_dir();
            let probe = 0u8;
            p.push(format!(
                "devme-preflight-{}-{tag}-{:p}",
                std::process::id(),
                &probe
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempCwd(p)
        }
    }
    impl Drop for TempCwd {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn auto_trust_runs_without_prompt() {
        // `trust = "auto"` runs the provision with no input available.
        let stack = parse_stack(
            r#"
schema_version = 1

[step.mk]
check = "test -f marker"
provision = "touch marker"
trust = "auto"
"#,
        );
        let cwd = TempCwd::new("auto");
        let mut input = Cursor::new(b""); // no input — proves no prompt
        let mut output = Vec::new();
        let result = run_preflight(&stack, &cwd.0, &mut input, &mut output, false, false).unwrap();

        assert_eq!(result.results[0].1, StepResult::Provisioned);
    }

    #[test]
    fn manual_trust_never_runs() {
        // `trust = "manual"` shows the command but does not run it, even
        // though stdin would accept an Enter.
        let stack = parse_stack(
            r#"
schema_version = 1

[step.priv]
check = "false"
provision = "echo would-run"
trust = "manual"
description = "Privileged step"
"#,
        );
        let dir = std::env::temp_dir();
        let mut input = Cursor::new(b"\n");
        let mut output = Vec::new();
        let result = run_preflight(&stack, &dir, &mut input, &mut output, true, false).unwrap();

        assert_eq!(result.results[0].1, StepResult::Manual);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("run this yourself"));
    }

    #[test]
    fn yes_flag_promotes_prompt_to_auto() {
        // With `assume_yes`, a `prompt` step runs without consuming input.
        let stack = parse_stack(
            r#"
schema_version = 1

[step.mk]
check = "test -f marker"
provision = "touch marker"
"#,
        );
        let cwd = TempCwd::new("yes");
        let mut input = Cursor::new(b""); // no Enter available
        let mut output = Vec::new();
        let result = run_preflight(&stack, &cwd.0, &mut input, &mut output, false, true).unwrap();

        assert_eq!(result.results[0].1, StepResult::Provisioned);
    }
}
