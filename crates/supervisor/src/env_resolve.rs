//! Declarative env-var resolution (ADR-0014).
//!
//! Before the executor starts step checks, this module reads `.env.local`,
//! diffs it against the `[env.*]` declarations in `devme.toml`, and prompts
//! the user for any missing values. New vars added by teammates are
//! automatically prompted on the next `devme` run.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use devme_config::EnvVar;
use devme_ui::{Item, Section, Style};

#[derive(Debug)]
pub struct EnvResolution {
    pub existing: HashMap<String, String>,
    pub resolved: Vec<(String, String)>,
    pub skipped: Vec<String>,
}

pub struct ParsedEnvFile {
    pub vars: HashMap<String, String>,
    pub skipped: HashSet<String>,
}

pub fn parse_env_file(path: &Path) -> ParsedEnvFile {
    let mut vars = HashMap::new();
    let mut skipped = HashSet::new();
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return ParsedEnvFile { vars, skipped },
    };
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            let rest = rest.trim();
            if let Some((key, _)) = rest.split_once('=') {
                let key = key.trim();
                if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    skipped.insert(key.to_string());
                }
            }
            continue;
        }
        if let Some((key, raw_value)) = trimmed.split_once('=') {
            let key = key.trim();
            let value = unquote(raw_value.trim());
            if !key.is_empty() {
                vars.insert(key.to_string(), value);
            }
        }
    }
    ParsedEnvFile { vars, skipped }
}

fn unquote(s: &str) -> String {
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

pub fn append_to_env_file(
    path: &Path,
    vars: &[(String, String)],
    skipped: &[String],
) -> std::io::Result<()> {
    if vars.is_empty() && skipped.is_empty() {
        return Ok(());
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;

    if let Ok(existing) = std::fs::read_to_string(path)
        && !existing.is_empty()
        && !existing.ends_with('\n')
    {
        writeln!(file)?;
    }

    for (key, value) in vars {
        if value.contains(' ') || value.contains('"') || value.contains('#') {
            writeln!(file, "{key}=\"{}\"", value.replace('"', "\\\""))?;
        } else {
            writeln!(file, "{key}={value}")?;
        }
    }

    for key in skipped {
        writeln!(file, "# {key}=")?;
    }

    Ok(())
}

fn run_generate(cmd: &str, cwd: &Path) -> Result<String, String> {
    let output = Command::new("sh")
        .args(["-c", cmd])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to run generate command: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "generate command exited with {}: {stderr}",
            output.status
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn read_line_safe<R: BufRead>(input: &mut R) -> Result<Option<String>, std::io::Error> {
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(line)),
        Err(e) => Err(e),
    }
}

/// Resolve missing env vars with Clack-style interactive prompts.
pub fn resolve_env_vars<R: BufRead, W: Write>(
    declared: &[(String, EnvVar)],
    env_file: &Path,
    cwd: &Path,
    input: &mut R,
    output: &mut W,
    interactive: bool,
    style: Style,
) -> Result<EnvResolution, std::io::Error> {
    let parsed = parse_env_file(env_file);
    let existing = parsed.vars;
    let previously_skipped = parsed.skipped;
    let mut resolved = Vec::new();
    let mut skipped = Vec::new();

    let missing: Vec<(&String, &EnvVar)> = declared
        .iter()
        .filter(|(name, _)| {
            !existing.contains_key(name.as_str()) && !previously_skipped.contains(name.as_str())
        })
        .map(|(name, var)| (name, var))
        .collect();

    if missing.is_empty() {
        return Ok(EnvResolution {
            existing,
            resolved,
            skipped,
        });
    }

    // Intro
    let count_note = format!(
        "{} variable{}",
        missing.len(),
        if missing.len() == 1 { "" } else { "s" }
    );
    let mut sec = Section::begin_noted(output, style, "Configure environment", Some(&count_note))?;

    let mut first = true;
    for (name, var) in &missing {
        // `begin_noted` already opened the gutter for the first field.
        if !first {
            sec.gutter()?;
        }
        first = false;

        // --- Generate vars: prompt with Enter-to-generate ---
        if let Some(gen_cmd) = &var.generate
            && var.choices.is_empty()
        {
            if interactive {
                sec.field(name, var.help.as_deref())?;
                sec.prompt("Enter to auto-generate, or type a value ›")?;

                match read_line_safe(input)? {
                    None => {
                        sec.newline()?;
                        break;
                    }
                    Some(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            match run_generate(gen_cmd, cwd) {
                                Ok(value) => {
                                    sec.sub(Item::Ok, "Generated")?;
                                    resolved.push(((*name).clone(), value));
                                }
                                Err(e) => {
                                    sec.sub(Item::Warn, &format!("Generate failed: {e}"))?;
                                    skipped.push((*name).clone());
                                }
                            }
                        } else {
                            sec.sub(Item::Ok, "Set")?;
                            resolved.push(((*name).clone(), trimmed.to_string()));
                        }
                    }
                }
                continue;
            } else {
                // Non-interactive: auto-generate silently
                match run_generate(gen_cmd, cwd) {
                    Ok(value) => {
                        sec.item(Item::Ok, name, Some("Generated"))?;
                        resolved.push(((*name).clone(), value));
                    }
                    Err(_) => {
                        skipped.push((*name).clone());
                    }
                }
                continue;
            }
        }

        // --- Non-interactive fallback ---
        if !interactive {
            if let Some(d) = &var.default {
                sec.item(Item::Ok, name, Some(d))?;
                resolved.push(((*name).clone(), d.clone()));
            } else {
                sec.item(Item::Skip, name, Some("skipped"))?;
                skipped.push((*name).clone());
            }
            continue;
        }

        // --- Choice selector ---
        if !var.choices.is_empty() {
            sec.field(name, var.help.as_deref())?;

            let default_idx = var
                .default
                .as_ref()
                .and_then(|d| var.choices.iter().position(|c| c == d))
                .unwrap_or(0);

            if interactive {
                // Shared single-select prompt: arrow-key picker on a TTY,
                // numbered fallback when stdin is piped (CI, tests).
                let picked = crate::prompt::select_one(
                    input,
                    sec.writer(),
                    &var.choices,
                    default_idx,
                    style,
                )?;
                match picked {
                    Some(idx) => {
                        let value = var.choices[idx].clone();
                        sec.sub(Item::Ok, &value)?;
                        resolved.push(((*name).clone(), value));
                    }
                    None => {
                        sec.sub(Item::Skip, "Skipped")?;
                        skipped.push((*name).clone());
                    }
                }
            } else {
                // Non-interactive: use default
                let value = var.choices[default_idx].clone();
                sec.sub(Item::Ok, &value)?;
                resolved.push(((*name).clone(), value));
            }
            continue;
        }

        // --- Free-text prompt ---
        sec.field(name, var.help.as_deref())?;

        let prompt_hint = if let Some(d) = &var.default {
            format!("Enter for {d}, or type a value ›")
        } else if var.required {
            "required ›".to_string()
        } else {
            "Enter to skip, or type a value ›".to_string()
        };
        sec.prompt(&prompt_hint)?;

        match read_line_safe(input)? {
            None => {
                sec.newline()?;
                break;
            }
            Some(line) => {
                let trimmed = line.trim();
                let value = if trimmed.is_empty() {
                    var.default.clone().unwrap_or_default()
                } else {
                    trimmed.to_string()
                };

                if value.is_empty() {
                    if var.required {
                        sec.sub(Item::Fail, "This variable is required.")?;
                        sec.prompt("›")?;
                        match read_line_safe(input)? {
                            None => {
                                sec.newline()?;
                                break;
                            }
                            Some(retry) => {
                                let retry_val = retry.trim();
                                if retry_val.is_empty() {
                                    skipped.push((*name).clone());
                                } else {
                                    sec.sub(Item::Ok, "Set")?;
                                    resolved.push(((*name).clone(), retry_val.to_string()));
                                }
                            }
                        }
                    } else {
                        sec.sub(Item::Skip, "Skipped")?;
                        skipped.push((*name).clone());
                    }
                } else {
                    sec.sub(Item::Ok, &value)?;
                    resolved.push(((*name).clone(), value));
                }
            }
        }
    }

    // Outro
    if !resolved.is_empty() {
        sec.end(
            Item::Ok,
            &format!(
                "Wrote {} variable{} to {}",
                resolved.len(),
                if resolved.len() == 1 { "" } else { "s" },
                env_file.display()
            ),
        )?;
    } else {
        sec.end(Item::Skip, "No variables configured")?;
    }

    append_to_env_file(env_file, &resolved, &skipped)?;

    Ok(EnvResolution {
        existing,
        resolved,
        skipped,
    })
}

/// The default env file when a stack doesn't configure one.
pub const DEFAULT_ENV_FILE: &str = ".env.local";

/// Path to the env file declarative resolution reads and writes, ignoring
/// any per-stack override. Prefer [`env_file_path`] when a [`Stack`] is in
/// hand so the `[stack] env_file` setting is honoured.
pub fn default_env_file(cwd: &Path) -> PathBuf {
    cwd.join(DEFAULT_ENV_FILE)
}

/// Path to the env file for `stack`, honouring the optional
/// `[stack] env_file` override (ADR-0014). Falls back to
/// [`DEFAULT_ENV_FILE`] (`.env.local`) when unset.
pub fn env_file_path(stack: &devme_config::Stack, cwd: &Path) -> PathBuf {
    let name = stack
        .stack
        .as_ref()
        .and_then(|m| m.env_file.as_deref())
        .unwrap_or(DEFAULT_ENV_FILE);
    cwd.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn make_env_var(
        required: bool,
        default: Option<&str>,
        help: Option<&str>,
        generate: Option<&str>,
        choices: Vec<&str>,
    ) -> EnvVar {
        EnvVar {
            required,
            default: default.map(String::from),
            help: help.map(String::from),
            generate: generate.map(String::from),
            choices: choices.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn parse_simple_env_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env.local");
        std::fs::write(&path, "DB_URL=postgres://localhost\nSECRET=abc123\n").unwrap();

        let parsed = parse_env_file(&path);
        assert_eq!(parsed.vars["DB_URL"], "postgres://localhost");
        assert_eq!(parsed.vars["SECRET"], "abc123");
    }

    #[test]
    fn parse_env_file_with_quotes_and_comments() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "# Comment\nDB_URL=\"postgres://localhost\"\nKEY='single quoted'\n\nEMPTY=\n",
        )
        .unwrap();

        let parsed = parse_env_file(&path);
        assert_eq!(parsed.vars["DB_URL"], "postgres://localhost");
        assert_eq!(parsed.vars["KEY"], "single quoted");
        assert_eq!(parsed.vars["EMPTY"], "");
    }

    #[test]
    fn missing_file_returns_empty() {
        let parsed = parse_env_file(Path::new("/nonexistent/.env"));
        assert!(parsed.vars.is_empty());
    }

    #[test]
    fn parse_env_file_detects_skipped_vars() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env.local");
        std::fs::write(&path, "ACTIVE=val\n# SKIPPED=\n").unwrap();

        let parsed = parse_env_file(&path);
        assert_eq!(parsed.vars["ACTIVE"], "val");
        assert!(parsed.skipped.contains("SKIPPED"));
        assert!(!parsed.vars.contains_key("SKIPPED"));
    }

    #[test]
    fn no_missing_vars_skips_prompting() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env.local");
        std::fs::write(&env_path, "DB_URL=x\n").unwrap();

        let declared = vec![(
            "DB_URL".into(),
            make_env_var(true, None, None, None, vec![]),
        )];

        let mut input = Cursor::new(b"");
        let mut output = Vec::new();

        let result = resolve_env_vars(
            &declared,
            &env_path,
            dir.path(),
            &mut input,
            &mut output,
            true,
            Style::PLAIN,
        )
        .unwrap();

        assert!(result.resolved.is_empty());
        assert!(result.skipped.is_empty());
        assert_eq!(result.existing["DB_URL"], "x");
    }

    #[test]
    fn prompts_for_missing_var_with_default() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env.local");
        std::fs::write(&env_path, "").unwrap();

        let declared = vec![(
            "DB_URL".into(),
            make_env_var(
                true,
                Some("postgres://localhost/dev"),
                Some("The database"),
                None,
                vec![],
            ),
        )];

        let mut input = Cursor::new(b"\n");
        let mut output = Vec::new();

        let result = resolve_env_vars(
            &declared,
            &env_path,
            dir.path(),
            &mut input,
            &mut output,
            true,
            Style::PLAIN,
        )
        .unwrap();

        assert_eq!(result.resolved.len(), 1);
        assert_eq!(
            result.resolved[0],
            ("DB_URL".into(), "postgres://localhost/dev".into())
        );

        let content = std::fs::read_to_string(&env_path).unwrap();
        assert!(content.contains("DB_URL=postgres://localhost/dev"));
    }

    #[test]
    fn generate_var_enter_triggers_generation() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env.local");
        std::fs::write(&env_path, "").unwrap();

        let declared = vec![(
            "SECRET".into(),
            make_env_var(false, None, None, Some("echo test-secret-value"), vec![]),
        )];

        // Enter triggers auto-generate
        let mut input = Cursor::new(b"\n");
        let mut output = Vec::new();

        let result = resolve_env_vars(
            &declared,
            &env_path,
            dir.path(),
            &mut input,
            &mut output,
            true,
            Style::PLAIN,
        )
        .unwrap();

        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.resolved[0].1, "test-secret-value");
    }

    #[test]
    fn generate_var_custom_value_overrides() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env.local");
        std::fs::write(&env_path, "").unwrap();

        let declared = vec![(
            "SECRET".into(),
            make_env_var(false, None, None, Some("echo generated"), vec![]),
        )];

        // User types a custom value instead of pressing Enter
        let mut input = Cursor::new(b"my-custom-secret\n");
        let mut output = Vec::new();

        let result = resolve_env_vars(
            &declared,
            &env_path,
            dir.path(),
            &mut input,
            &mut output,
            true,
            Style::PLAIN,
        )
        .unwrap();

        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.resolved[0].1, "my-custom-secret");
    }

    #[test]
    fn choice_prompt_accepts_number() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env.local");
        std::fs::write(&env_path, "").unwrap();

        let declared = vec![(
            "REGION".into(),
            make_env_var(
                false,
                Some("https://us.i.posthog.com"),
                None,
                None,
                vec!["https://us.i.posthog.com", "https://eu.i.posthog.com"],
            ),
        )];

        let mut input = Cursor::new(b"2\n");
        let mut output = Vec::new();

        let result = resolve_env_vars(
            &declared,
            &env_path,
            dir.path(),
            &mut input,
            &mut output,
            true,
            Style::PLAIN,
        )
        .unwrap();

        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.resolved[0].1, "https://eu.i.posthog.com");
    }

    #[test]
    fn optional_var_can_be_skipped() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env.local");
        std::fs::write(&env_path, "").unwrap();

        let declared = vec![(
            "OPTIONAL_KEY".into(),
            make_env_var(false, None, None, None, vec![]),
        )];

        let mut input = Cursor::new(b"\n");
        let mut output = Vec::new();

        let result = resolve_env_vars(
            &declared,
            &env_path,
            dir.path(),
            &mut input,
            &mut output,
            true,
            Style::PLAIN,
        )
        .unwrap();

        assert!(result.resolved.is_empty());
        assert_eq!(result.skipped, vec!["OPTIONAL_KEY"]);
    }

    #[test]
    fn env_file_path_defaults_to_env_local() {
        let stack = devme_config::Stack::parse("schema_version = 1\n").unwrap();
        let path = env_file_path(&stack, Path::new("/repo"));
        assert_eq!(path, Path::new("/repo/.env.local"));
    }

    #[test]
    fn env_file_path_honours_stack_override() {
        let stack =
            devme_config::Stack::parse("schema_version = 1\n\n[stack]\nenv_file = \".env\"\n")
                .unwrap();
        let path = env_file_path(&stack, Path::new("/repo"));
        assert_eq!(path, Path::new("/repo/.env"));
    }

    #[test]
    fn resolution_targets_configured_env_file() {
        // With env_file = ".env", a missing var is written to .env, not
        // .env.local.
        let dir = TempDir::new().unwrap();
        let stack =
            devme_config::Stack::parse("schema_version = 1\n\n[stack]\nenv_file = \".env\"\n")
                .unwrap();
        let env_path = env_file_path(&stack, dir.path());

        let declared = vec![(
            "API_KEY".to_string(),
            make_env_var(false, Some("from-default"), None, None, vec![]),
        )];
        let mut input = Cursor::new(b"\n");
        let mut output = Vec::new();
        let result = resolve_env_vars(
            &declared,
            &env_path,
            dir.path(),
            &mut input,
            &mut output,
            true,
            Style::PLAIN,
        )
        .unwrap();

        assert_eq!(result.resolved.len(), 1);
        let dot_env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(dot_env.contains("API_KEY=from-default"), "got: {dot_env}");
        assert!(
            !dir.path().join(".env.local").exists(),
            ".env.local should not have been written"
        );
    }

    #[test]
    fn only_prompts_for_new_vars() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env.local");
        std::fs::write(&env_path, "EXISTING=already_set\n").unwrap();

        let declared = vec![
            (
                "EXISTING".into(),
                make_env_var(true, None, None, None, vec![]),
            ),
            (
                "NEW_VAR".into(),
                make_env_var(false, Some("default_val"), None, None, vec![]),
            ),
        ];

        let mut input = Cursor::new(b"\n");
        let mut output = Vec::new();

        let result = resolve_env_vars(
            &declared,
            &env_path,
            dir.path(),
            &mut input,
            &mut output,
            true,
            Style::PLAIN,
        )
        .unwrap();

        assert_eq!(result.existing.len(), 1);
        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.resolved[0].0, "NEW_VAR");
    }
}
