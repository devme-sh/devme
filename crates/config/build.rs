use std::env;
use std::fs;
use std::path::PathBuf;

const SKILL_PATH: &str = "skill/SKILL.md";
const REPOSITORY_SKILL_PATH: &str = "../../.agents/skills/devme/SKILL.md";
const GUIDANCE_HEADING: &str = "## Live agent guidance";

fn main() {
    println!("cargo:rerun-if-changed={SKILL_PATH}");
    println!("cargo:rerun-if-changed={REPOSITORY_SKILL_PATH}");

    let skill = fs::read_to_string(SKILL_PATH)
        .unwrap_or_else(|error| panic!("failed to read {SKILL_PATH}: {error}"));
    check_repository_skill_mirror(&skill);
    let guidance = extract_live_guidance(&skill)
        .unwrap_or_else(|| panic!("{SKILL_PATH} must contain one `{GUIDANCE_HEADING}` section"));

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"))
        .join("agent-guidance.md");
    fs::write(&output, guidance)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", output.display()));
}

fn check_repository_skill_mirror(skill: &str) {
    let Ok(repository_skill) = fs::read_to_string(REPOSITORY_SKILL_PATH) else {
        // Published crate sources do not include the repository-level mirror.
        return;
    };
    if repository_skill != skill {
        panic!("repository Devme skill is stale; run scripts/sync-devme-skill.sh");
    }
}

fn extract_live_guidance(skill: &str) -> Option<&str> {
    let marker = format!("\n{GUIDANCE_HEADING}\n");
    let (before, guidance) = skill.split_once(&marker)?;
    if guidance.contains(&marker) {
        return None;
    }
    if before.is_empty() || guidance.trim().is_empty() {
        return None;
    }
    Some(guidance)
}
