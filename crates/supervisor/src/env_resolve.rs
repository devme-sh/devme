//! Declarative env-var resolution (ADR-0014).
//!
//! Before the executor starts step checks, this module reads `.env.local`,
//! diffs it against the `[env.*]` declarations in `devme.toml`, and prompts
//! the user for any missing values. New vars added by teammates are
//! automatically prompted on the next `devme` run.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use devme_config::EnvVar;
use devme_ui::{Item, Section, Style};
use serde::Serialize;

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
    let content = std::fs::read_to_string(path).unwrap_or_default();
    parse_env_contents(&content)
}

fn parse_env_contents(content: &str) -> ParsedEnvFile {
    let mut vars = HashMap::new();
    let mut skipped = HashSet::new();
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStatus {
    Complete,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupValueState {
    Configured,
    Missing,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SetupVariable {
    pub name: String,
    pub state: SetupValueState,
    pub required: bool,
    pub secret: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_url: Option<String>,
    pub has_default: bool,
    pub can_generate: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SetupSnapshot {
    pub schema_version: u32,
    pub status: SetupStatus,
    pub env_file: PathBuf,
    pub missing_required: usize,
    pub variables: Vec<SetupVariable>,
}

pub fn setup_snapshot(declared: &[(String, EnvVar)], env_file: &Path) -> SetupSnapshot {
    let parsed = parse_env_file(env_file);
    let variables = declared
        .iter()
        .map(|(name, var)| {
            let configured = parsed
                .vars
                .get(name)
                .is_some_and(|value| !value.trim().is_empty());
            let state = if configured {
                SetupValueState::Configured
            } else if !var.required && parsed.skipped.contains(name) {
                SetupValueState::Skipped
            } else {
                SetupValueState::Missing
            };
            SetupVariable {
                name: name.clone(),
                state,
                required: var.required,
                secret: var.secret,
                help: var.help.clone(),
                setup_url: var.setup_url.clone(),
                has_default: var.default.is_some(),
                can_generate: var.generate.is_some(),
                choices: if var.secret {
                    Vec::new()
                } else {
                    var.choices.clone()
                },
            }
        })
        .collect::<Vec<_>>();
    let missing_required = variables
        .iter()
        .filter(|var| var.required && var.state != SetupValueState::Configured)
        .count();
    SetupSnapshot {
        schema_version: 1,
        status: if missing_required == 0 {
            SetupStatus::Complete
        } else {
            SetupStatus::Incomplete
        },
        env_file: env_file.to_path_buf(),
        missing_required,
        variables,
    }
}

pub fn set_env_value(
    declared: &[(String, EnvVar)],
    env_file: &Path,
    name: &str,
    value: &str,
) -> std::io::Result<SetupSnapshot> {
    if !declared
        .iter()
        .any(|(declared_name, _)| declared_name == name)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("environment variable {name} is not declared in devme.toml"),
        ));
    }
    if value.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("environment variable {name} cannot be empty"),
        ));
    }
    if value.contains(['\n', '\r', '\0']) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("environment variable {name} must be a single-line value"),
        ));
    }
    if let Some(parent) = env_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(env_file)?;
    #[cfg(unix)]
    std::fs::set_permissions(
        env_file,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    file.lock()?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    if let Some(current) = parse_env_contents(&contents)
        .vars
        .get(name)
        .filter(|current| !current.trim().is_empty())
    {
        if current == value {
            drop(file);
            return Ok(setup_snapshot(declared, env_file));
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("environment variable {name} is already configured with a different value"),
        ));
    }
    file.seek(SeekFrom::End(0))?;
    if !contents.is_empty() && !contents.ends_with('\n') {
        writeln!(file)?;
    }
    write_env_var(&mut file, name, value)?;
    file.sync_data()?;
    drop(file);
    Ok(setup_snapshot(declared, env_file))
}

enum LiveSetupInput {
    Value(String),
    Skip,
    External,
    Cancel,
}

#[derive(Clone, Copy)]
enum SetupFieldKind<'a> {
    Choice {
        values: &'a [String],
        initial: usize,
    },
    Generate {
        command: &'a str,
    },
    Text,
}

impl<'a> SetupFieldKind<'a> {
    fn from_variable(variable: &'a EnvVar) -> Self {
        // A validated config cannot combine secrets and choices. Keeping this
        // defensive guard here ensures an unvalidated caller cannot render a
        // secret choice value either.
        if !variable.secret && !variable.choices.is_empty() {
            let initial = variable
                .default
                .as_ref()
                .and_then(|default| variable.choices.iter().position(|item| item == default))
                .unwrap_or(0);
            Self::Choice {
                values: &variable.choices,
                initial,
            }
        } else if let Some(command) = variable.generate.as_deref() {
            Self::Generate { command }
        } else {
            Self::Text
        }
    }
}

struct RawModeGuard;

struct CrLfWriter<W>(W);

impl<W: Write> Write for CrLfWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        for segment in buffer.split_inclusive(|byte| *byte == b'\n') {
            if let Some(body) = segment.strip_suffix(b"\n") {
                self.0.write_all(body)?;
                self.0.write_all(b"\r\n")?;
            } else {
                self.0.write_all(segment)?;
            }
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl RawModeGuard {
    fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Resolve environment setup in a real terminal while observing the env file.
/// Another process may supply the current value through `devme setup set`; the
/// wizard notices it and advances without a manual recheck.
pub fn resolve_env_vars_live(
    declared: &[(String, EnvVar)],
    env_file: &Path,
    cwd: &Path,
    style: Style,
) -> std::io::Result<SetupSnapshot> {
    let initial = setup_snapshot(declared, env_file);
    let missing = initial
        .variables
        .iter()
        .filter(|variable| variable.state == SetupValueState::Missing)
        .count();
    if missing == 0 {
        return Ok(initial);
    }

    let _raw_mode = RawModeGuard::enter()?;
    let mut output = CrLfWriter(std::io::stderr());
    writeln!(output)?;
    writeln!(
        output,
        "  {}  {}  {}",
        style.accent(devme_ui::glyph::SECTION),
        style.bold("Configure environment"),
        style.dim(&format!(
            "{missing} variable{}",
            if missing == 1 { "" } else { "s" }
        ))
    )?;
    writeln!(output, "  {}", style.dim(devme_ui::glyph::BAR))?;
    writeln!(
        output,
        "  {}  {} ask your coding agent to finish this setup.",
        style.dim(devme_ui::glyph::BAR),
        style.accent("Agent help:")
    )?;
    writeln!(
        output,
        "  {}  It can read this wizard's live context with {}.",
        style.dim(devme_ui::glyph::BAR),
        style.accent("devme setup status")
    )?;
    writeln!(output, "  {}", style.dim(devme_ui::glyph::BAR))?;

    for (name, variable) in declared {
        let current = setup_snapshot(declared, env_file);
        let is_missing = current
            .variables
            .iter()
            .find(|candidate| candidate.name == *name)
            .is_some_and(|candidate| candidate.state == SetupValueState::Missing);
        if !is_missing {
            continue;
        }
        let field = SetupFieldKind::from_variable(variable);
        render_live_setup_field(&mut output, name, variable, field, style)?;
        match read_live_setup_input(name, variable, field, env_file, cwd, &mut output, style)? {
            LiveSetupInput::Value(value) => match set_env_value(declared, env_file, name, &value) {
                Ok(_) => writeln!(
                    output,
                    "  {}  {}  Set",
                    style.dim(devme_ui::glyph::BAR),
                    style.ok(devme_ui::glyph::DONE)
                )?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => writeln!(
                    output,
                    "  {}  {}  {name} set by another process",
                    style.dim(devme_ui::glyph::BAR),
                    style.ok(devme_ui::glyph::DONE)
                )?,
                Err(error) => return Err(error),
            },
            LiveSetupInput::Skip => {
                append_to_env_file(env_file, &[], std::slice::from_ref(name))?;
                writeln!(
                    output,
                    "  {}  {}  Skipped",
                    style.dim(devme_ui::glyph::BAR),
                    style.dim(devme_ui::glyph::DONE)
                )?;
            }
            LiveSetupInput::External => writeln!(
                output,
                "  {}  {}  {name} set by another process",
                style.dim(devme_ui::glyph::BAR),
                style.ok(devme_ui::glyph::DONE)
            )?,
            LiveSetupInput::Cancel => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "environment setup cancelled",
                ));
            }
        }
        writeln!(output, "  {}", style.dim(devme_ui::glyph::BAR))?;
    }

    let snapshot = setup_snapshot(declared, env_file);
    if snapshot.missing_required > 0 {
        let name = snapshot
            .variables
            .iter()
            .find(|variable| variable.required && variable.state != SetupValueState::Configured)
            .map(|variable| variable.name.as_str())
            .unwrap_or("unknown");
        return Err(required_env_error(name));
    }
    writeln!(
        output,
        "  {}  {}  Environment configured",
        style.dim(devme_ui::glyph::BAR_END),
        style.ok(devme_ui::glyph::DONE)
    )?;
    output.flush()?;
    Ok(snapshot)
}

fn render_live_setup_field(
    output: &mut impl Write,
    name: &str,
    variable: &EnvVar,
    field: SetupFieldKind<'_>,
    style: Style,
) -> std::io::Result<()> {
    writeln!(
        output,
        "  {}  {}",
        style.dim(devme_ui::glyph::BAR),
        style.bold(name)
    )?;
    if let Some(help) = &variable.help {
        writeln!(output, "  {}  {}", style.dim(devme_ui::glyph::BAR), help)?;
    }
    if variable.setup_url.is_some() {
        writeln!(
            output,
            "  {}  {}  {}",
            style.dim(devme_ui::glyph::BAR),
            style.accent("[Shift+Tab Open browser]"),
            style.accent("[Tab Copy URL]")
        )?;
    }
    if let SetupFieldKind::Choice { values, initial } = field {
        writeln!(
            output,
            "  {}  Selected: {}",
            style.dim(devme_ui::glyph::BAR),
            values[initial]
        )?;
        writeln!(
            output,
            "  {}  [↑/↓ Choose]  [Enter Set]",
            style.dim(devme_ui::glyph::BAR)
        )?;
        return output.flush();
    }

    let empty_action = if variable.default.is_some() {
        Some("[Enter Use default]")
    } else if matches!(field, SetupFieldKind::Generate { .. }) {
        Some("[Enter Generate]")
    } else if !variable.required {
        Some("[Enter Skip]")
    } else {
        None
    };
    if let Some(control) = empty_action {
        writeln!(output, "  {}  {}", style.dim(devme_ui::glyph::BAR), control)?;
    }
    output.flush()
}

fn read_live_setup_input(
    name: &str,
    variable: &EnvVar,
    field: SetupFieldKind<'_>,
    env_file: &Path,
    cwd: &Path,
    output: &mut impl Write,
    style: Style,
) -> std::io::Result<LiveSetupInput> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};

    let text_field = !matches!(field, SetupFieldKind::Choice { .. });
    let mut value = String::new();
    let mut choice = match field {
        SetupFieldKind::Choice { initial, .. } => initial,
        _ => 0,
    };
    if text_field {
        render_live_value(output, &value, variable.secret, style)?;
        output.flush()?;
    }
    loop {
        if parse_env_file(env_file)
            .vars
            .get(name)
            .is_some_and(|value| !value.trim().is_empty())
        {
            if text_field {
                writeln!(output)?;
            }
            return Ok(LiveSetupInput::External);
        }
        if !event::poll(std::time::Duration::from_millis(150))? {
            continue;
        }
        let event = event::read()?;
        match event {
            Event::Paste(pasted) if text_field => {
                value.push_str(pasted.trim_end_matches(['\r', '\n']));
                render_live_value(output, &value, variable.secret, style)?;
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    return Ok(LiveSetupInput::Cancel);
                }
                KeyCode::Esc => return Ok(LiveSetupInput::Cancel),
                KeyCode::Tab if variable.setup_url.is_some() => {
                    if text_field {
                        writeln!(output)?;
                    }
                    let url = variable.setup_url.as_deref().unwrap();
                    devme_ui::copy_to_clipboard(url);
                    writeln!(output, "  {}  Copied URL", style.dim(devme_ui::glyph::BAR))?;
                    if text_field {
                        render_live_value(output, &value, variable.secret, style)?;
                    }
                }
                KeyCode::BackTab if variable.setup_url.is_some() => {
                    if text_field {
                        writeln!(output)?;
                    }
                    let url = variable.setup_url.as_deref().unwrap();
                    devme_config::browser::open_url(url)?;
                    writeln!(
                        output,
                        "  {}  Opened {url}",
                        style.dim(devme_ui::glyph::BAR)
                    )?;
                    if text_field {
                        render_live_value(output, &value, variable.secret, style)?;
                    }
                }
                KeyCode::Enter if text_field && !value.trim().is_empty() => {
                    writeln!(output)?;
                    return Ok(LiveSetupInput::Value(value));
                }
                KeyCode::Enter if text_field && variable.default.is_some() => {
                    writeln!(output)?;
                    return Ok(LiveSetupInput::Value(
                        variable.default.as_ref().unwrap().clone(),
                    ));
                }
                KeyCode::Enter
                    if text_field && matches!(field, SetupFieldKind::Generate { .. }) =>
                {
                    let SetupFieldKind::Generate { command } = field else {
                        unreachable!()
                    };
                    writeln!(output)?;
                    return run_generate(command, cwd)
                        .map(LiveSetupInput::Value)
                        .map_err(|error| std::io::Error::other(format!("{name}: {error}")));
                }
                KeyCode::Enter if text_field && !variable.required => {
                    writeln!(output)?;
                    return Ok(LiveSetupInput::Skip);
                }
                KeyCode::Backspace if text_field => {
                    value.pop();
                    render_live_value(output, &value, variable.secret, style)?;
                }
                KeyCode::Char(character)
                    if text_field
                        && !key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    value.push(character);
                    render_live_value(output, &value, variable.secret, style)?;
                }
                KeyCode::Up if matches!(field, SetupFieldKind::Choice { .. }) => {
                    let SetupFieldKind::Choice { values, .. } = field else {
                        unreachable!()
                    };
                    choice = choice.saturating_sub(1);
                    render_live_choice(output, &values[choice], style)?;
                }
                KeyCode::Down if matches!(field, SetupFieldKind::Choice { .. }) => {
                    let SetupFieldKind::Choice { values, .. } = field else {
                        unreachable!()
                    };
                    choice = (choice + 1).min(values.len() - 1);
                    render_live_choice(output, &values[choice], style)?;
                }
                KeyCode::Enter if matches!(field, SetupFieldKind::Choice { .. }) => {
                    let SetupFieldKind::Choice { values, .. } = field else {
                        unreachable!()
                    };
                    return Ok(LiveSetupInput::Value(values[choice].clone()));
                }
                _ => {}
            },
            _ => {}
        }
        output.flush()?;
    }
}

fn render_live_value(
    output: &mut impl Write,
    value: &str,
    secret: bool,
    style: Style,
) -> std::io::Result<()> {
    let rendered = if secret {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    write!(
        output,
        "\r\x1b[2K  {}  › {rendered}",
        style.dim(devme_ui::glyph::BAR)
    )?;
    output.flush()
}

fn render_live_choice(output: &mut impl Write, value: &str, style: Style) -> std::io::Result<()> {
    write!(
        output,
        "\r\x1b[2K  {}  Selected: {value}",
        style.dim(devme_ui::glyph::BAR)
    )?;
    output.flush()
}

fn unquote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let mut value = String::new();
        let mut chars = s[1..s.len() - 1].chars().peekable();
        while let Some(character) = chars.next() {
            if character == '\\' && chars.peek() == Some(&'"') {
                chars.next();
                value.push('"');
            } else {
                value.push(character);
            }
        }
        value
    } else if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
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

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.lock()?;

    if let Ok(existing) = std::fs::read_to_string(path)
        && !existing.is_empty()
        && !existing.ends_with('\n')
    {
        writeln!(file)?;
    }

    for (key, value) in vars {
        write_env_var(&mut file, key, value)?;
    }

    for key in skipped {
        writeln!(file, "# {key}=")?;
    }

    file.sync_data()?;
    Ok(())
}

fn write_env_var(output: &mut impl Write, key: &str, value: &str) -> std::io::Result<()> {
    if value.chars().any(char::is_whitespace) || value.contains('"') || value.contains('#') {
        writeln!(output, "{key}=\"{}\"", value.replace('"', "\\\""))
    } else {
        writeln!(output, "{key}={value}")
    }
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

fn required_env_error(name: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "required environment variable {name} is missing; run devme interactively to configure it"
        ),
    )
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
        .filter(|(name, var)| {
            existing
                .get(name.as_str())
                .is_none_or(|value| value.trim().is_empty())
                && (var.required || !previously_skipped.contains(name.as_str()))
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
        let field = SetupFieldKind::from_variable(var);

        // --- Generate vars: prompt with Enter-to-generate ---
        if let SetupFieldKind::Generate { command } = field {
            if interactive {
                sec.field(name, var.help.as_deref())?;
                sec.prompt("Enter to auto-generate, or type a value ›")?;

                match read_line_safe(input)? {
                    None => {
                        if var.required {
                            return Err(required_env_error(name));
                        }
                        sec.newline()?;
                        break;
                    }
                    Some(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            match run_generate(command, cwd) {
                                Ok(value) => {
                                    sec.sub(Item::Ok, "Generated")?;
                                    resolved.push(((*name).clone(), value));
                                }
                                Err(e) => {
                                    if var.required {
                                        return Err(std::io::Error::new(
                                            std::io::ErrorKind::InvalidInput,
                                            format!(
                                                "could not generate required environment variable {name}: {e}"
                                            ),
                                        ));
                                    }
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
                match run_generate(command, cwd) {
                    Ok(value) => {
                        sec.item(Item::Ok, name, Some("Generated"))?;
                        resolved.push(((*name).clone(), value));
                    }
                    Err(e) if var.required => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("could not generate required environment variable {name}: {e}"),
                        ));
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
                sec.item(
                    Item::Ok,
                    name,
                    Some(if var.secret { "Configured" } else { d }),
                )?;
                resolved.push(((*name).clone(), d.clone()));
            } else if var.required {
                return Err(required_env_error(name));
            } else {
                sec.item(Item::Skip, name, Some("skipped"))?;
                skipped.push((*name).clone());
            }
            continue;
        }

        // --- Choice selector ---
        if let SetupFieldKind::Choice {
            values,
            initial: default_idx,
        } = field
        {
            sec.field(name, var.help.as_deref())?;

            // Shared single-select prompt: arrow-key picker on a TTY,
            // numbered fallback when stdin is piped (CI, tests).
            let picked =
                crate::prompt::select_one(input, sec.writer(), values, default_idx, style)?;
            match picked {
                Some(idx) => {
                    let value = values[idx].clone();
                    sec.sub(Item::Ok, &value)?;
                    resolved.push(((*name).clone(), value));
                }
                None => {
                    if var.required {
                        return Err(required_env_error(name));
                    } else {
                        sec.sub(Item::Skip, "Skipped")?;
                        skipped.push((*name).clone());
                    }
                }
            }
            continue;
        }

        // --- Free-text prompt ---
        sec.field(name, var.help.as_deref())?;

        let prompt_hint = if var.secret && var.default.is_some() {
            "Enter to use default, or type a value ›".to_string()
        } else if let Some(d) = &var.default {
            format!("Enter for {d}, or type a value ›")
        } else if var.required {
            "required ›".to_string()
        } else {
            "Enter to skip, or type a value ›".to_string()
        };
        sec.prompt(&prompt_hint)?;

        loop {
            let Some(line) = read_line_safe(input)? else {
                if var.required {
                    return Err(required_env_error(name));
                }
                sec.newline()?;
                break;
            };
            let trimmed = line.trim();
            let value = if trimmed.is_empty() {
                var.default.clone().unwrap_or_default()
            } else {
                trimmed.to_string()
            };

            if !value.is_empty() {
                sec.sub(Item::Ok, if var.secret { "Set" } else { &value })?;
                resolved.push(((*name).clone(), value));
                break;
            }
            if !var.required {
                sec.sub(Item::Skip, "Skipped")?;
                skipped.push((*name).clone());
                break;
            }

            sec.sub(Item::Fail, "This variable is required.")?;
            sec.prompt("›")?;
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
            setup_url: None,
            secret: false,
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
    fn buffered_resolver_never_renders_secret_values() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env.local");
        let mut variable = make_env_var(
            true,
            Some("default-secret"),
            Some("Provider secret"),
            None,
            vec![],
        );
        variable.secret = true;
        let declared = vec![("API_SECRET".into(), variable)];
        let mut input = Cursor::new(b"typed-secret\n");
        let mut output = Vec::new();

        resolve_env_vars(
            &declared,
            &env_path,
            dir.path(),
            &mut input,
            &mut output,
            true,
            Style::PLAIN,
        )
        .unwrap();

        let rendered = String::from_utf8(output).unwrap();
        assert!(!rendered.contains("default-secret"), "{rendered}");
        assert!(!rendered.contains("typed-secret"), "{rendered}");
        assert!(rendered.contains("Set"), "{rendered}");
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
    fn required_var_reprompts_until_it_has_a_value() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env.local");
        std::fs::write(&env_path, "").unwrap();
        let declared = vec![(
            "REQUIRED_KEY".into(),
            make_env_var(true, None, None, None, vec![]),
        )];
        let mut input = Cursor::new(b"\n\nconfigured\n");
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

        assert_eq!(
            result.resolved,
            vec![("REQUIRED_KEY".to_string(), "configured".to_string())]
        );
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn required_var_blocks_noninteractive_startup_when_missing() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env.local");
        let declared = vec![(
            "REQUIRED_KEY".into(),
            make_env_var(true, None, None, None, vec![]),
        )];
        let mut input = Cursor::new(b"");
        let mut output = Vec::new();

        let error = resolve_env_vars(
            &declared,
            &env_path,
            dir.path(),
            &mut input,
            &mut output,
            false,
            Style::PLAIN,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("REQUIRED_KEY"));
        assert!(!env_path.exists());
    }

    #[test]
    fn required_var_is_reopened_after_it_was_previously_skipped() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env.local");
        std::fs::write(&env_path, "# REQUIRED_KEY=\n").unwrap();
        let declared = vec![(
            "REQUIRED_KEY".into(),
            make_env_var(true, None, None, None, vec![]),
        )];
        let mut input = Cursor::new(b"configured\n");
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

        assert_eq!(result.resolved[0].0, "REQUIRED_KEY");
        assert_eq!(result.resolved[0].1, "configured");
    }

    #[test]
    fn required_var_with_an_empty_assignment_remains_missing() {
        let dir = TempDir::new().unwrap();
        let env_path = dir.path().join(".env.local");
        std::fs::write(&env_path, "REQUIRED_KEY=\n").unwrap();
        let declared = vec![(
            "REQUIRED_KEY".into(),
            make_env_var(true, None, None, None, vec![]),
        )];
        let mut input = Cursor::new(b"");
        let mut output = Vec::new();

        let error = resolve_env_vars(
            &declared,
            &env_path,
            dir.path(),
            &mut input,
            &mut output,
            false,
            Style::PLAIN,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("REQUIRED_KEY"));
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
    fn setup_snapshot_reports_presence_without_exposing_values() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env.auth.local");
        std::fs::write(&path, "GOOGLE_CLIENT_SECRET=do-not-print\n# OPTIONAL=\n").unwrap();
        let mut secret = make_env_var(true, None, Some("OAuth secret"), None, vec![]);
        secret.secret = true;
        secret.setup_url = Some("https://console.example.test/credentials".into());
        let declared = vec![
            ("GOOGLE_CLIENT_SECRET".into(), secret),
            (
                "GOOGLE_WEB_CLIENT_ID".into(),
                make_env_var(true, None, Some("OAuth client ID"), None, vec![]),
            ),
            (
                "OPTIONAL".into(),
                make_env_var(false, None, None, None, vec![]),
            ),
        ];

        let snapshot = setup_snapshot(&declared, &path);
        assert_eq!(snapshot.status, SetupStatus::Incomplete);
        assert_eq!(snapshot.missing_required, 1);
        assert_eq!(snapshot.env_file, path);
        assert_eq!(snapshot.variables[0].state, SetupValueState::Configured);
        assert_eq!(snapshot.variables[1].state, SetupValueState::Missing);
        assert_eq!(snapshot.variables[2].state, SetupValueState::Skipped);
        assert!(snapshot.variables[0].secret);
        assert_eq!(
            snapshot.variables[0].setup_url.as_deref(),
            Some("https://console.example.test/credentials")
        );
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("do-not-print"), "{json}");
    }

    #[test]
    fn setup_snapshot_never_serializes_secret_choices() {
        let temp = TempDir::new().unwrap();
        let mut secret = make_env_var(
            true,
            None,
            None,
            None,
            vec!["first-secret", "second-secret"],
        );
        secret.secret = true;
        let declared = vec![("API_SECRET".to_string(), secret)];

        let snapshot = setup_snapshot(&declared, &temp.path().join(".env.local"));
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("first-secret"));
        assert!(!json.contains("second-secret"));
        assert!(snapshot.variables[0].choices.is_empty());
    }

    #[test]
    fn set_env_value_is_idempotent_and_refuses_a_different_overwrite() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env.auth.local");
        let declared = vec![(
            "GOOGLE_CLIENT_SECRET".into(),
            make_env_var(true, None, None, None, vec![]),
        )];

        let snapshot =
            set_env_value(&declared, &path, "GOOGLE_CLIENT_SECRET", "secret-value").unwrap();
        assert_eq!(snapshot.status, SetupStatus::Complete);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            parse_env_file(&path)
                .vars
                .get("GOOGLE_CLIENT_SECRET")
                .map(String::as_str),
            Some("secret-value")
        );

        let retry =
            set_env_value(&declared, &path, "GOOGLE_CLIENT_SECRET", "secret-value").unwrap();
        assert_eq!(retry.status, SetupStatus::Complete);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches("GOOGLE_CLIENT_SECRET=").count(), 1);

        let error =
            set_env_value(&declared, &path, "GOOGLE_CLIENT_SECRET", "replacement").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            parse_env_file(&path)
                .vars
                .get("GOOGLE_CLIENT_SECRET")
                .map(String::as_str),
            Some("secret-value")
        );
    }

    #[test]
    fn set_env_value_retry_handles_values_that_require_env_quoting() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env.local");
        let declared = vec![(
            "DISPLAY_NAME".into(),
            make_env_var(true, None, None, None, vec![]),
        )];
        let value = "Sambu #1 \"Home\"";

        set_env_value(&declared, &path, "DISPLAY_NAME", value).unwrap();
        set_env_value(&declared, &path, "DISPLAY_NAME", value).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches("DISPLAY_NAME=").count(), 1);
        assert_eq!(parse_env_file(&path).vars["DISPLAY_NAME"], value);
    }

    #[test]
    fn set_env_value_retry_preserves_leading_and_trailing_tabs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env.local");
        let declared = vec![(
            "TAB_VALUE".into(),
            make_env_var(true, None, None, None, vec![]),
        )];
        let value = "\tvalue\t";

        set_env_value(&declared, &path, "TAB_VALUE", value).unwrap();
        set_env_value(&declared, &path, "TAB_VALUE", value).unwrap();

        assert_eq!(parse_env_file(&path).vars["TAB_VALUE"], value);
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
