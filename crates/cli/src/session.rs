//! CLI boundary for resource-bound session composition.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, anyhow};
use devme_config::Stack;
use devme_core::{ClientMessage, ErrorCode, ServerMessage, SessionSnapshot, SessionState};

use crate::OutputFormat;

/// Owns the supervisor connection that contributes one live-session client.
/// Keep this value alive across a TUI/native-app interaction; dropping it
/// starts the configured linger interval.
pub struct SessionHandle {
    _client: devme_client::Client,
}

pub struct OpenedSession {
    pub exit_code: i32,
    pub handle: SessionHandle,
}

#[derive(Debug)]
pub struct SessionCommandError {
    pub code: ErrorCode,
    pub message: String,
}

impl std::fmt::Display for SessionCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SessionCommandError {}

/// Open a session and keep the IPC connection alive while its optional task
/// runs. Dropping the connection starts the configured linger interval.
pub async fn open(stack: &Stack, root: &Path, name: &str, format: OutputFormat) -> Result<i32> {
    Ok(open_held(stack, root, name, format).await?.exit_code)
}

/// Open through readiness, run the optional task with allocated identifiers,
/// and return the still-attached client for a longer-lived caller such as the
/// TUI.
pub async fn open_held(
    stack: &Stack,
    root: &Path,
    name: &str,
    format: OutputFormat,
) -> Result<OpenedSession> {
    if !stack.session.contains_key(name) {
        return Err(SessionCommandError {
            code: ErrorCode::NotFound,
            message: unknown_session(stack, name),
        }
        .into());
    }
    let socket = devme_config::paths::supervisor_socket(root)?;
    let mut client = devme_client::Client::connect(&socket).await?;
    client
        .send(ClientMessage::OpenSession {
            session: name.to_string(),
        })
        .await?;
    let mut announced = false;
    loop {
        let Some(message) = client.next_event().await? else {
            return Err(anyhow!(
                "supervisor disconnected while opening session {name:?}"
            ));
        };
        match message {
            ServerMessage::SessionPending { resources, .. } => {
                if !announced {
                    let detail = if resources.is_empty() {
                        "starting required services".to_string()
                    } else {
                        format!("waiting for resources: {}", resources.join(", "))
                    };
                    devme_ui::info(format!("session {name}: {detail}"));
                    announced = true;
                }
            }
            ServerMessage::SessionReady {
                joined,
                services,
                env,
                run,
                ..
            } => {
                if let Some(task) = run.filter(|_| !joined) {
                    let result =
                        crate::task::execute_with_env(stack, root, &task, &[], format, &env)
                            .await?;
                    return Ok(OpenedSession {
                        exit_code: result.exit_code,
                        handle: SessionHandle { _client: client },
                    });
                }
                emit_ready(name, joined, &services, &env, format)?;
                return Ok(OpenedSession {
                    exit_code: 0,
                    handle: SessionHandle { _client: client },
                });
            }
            ServerMessage::Error { code, message } => {
                return Err(SessionCommandError { code, message }.into());
            }
            _ => {}
        }
    }
}

pub async fn stop(stack: &Stack, root: &Path, name: &str, format: OutputFormat) -> Result<i32> {
    if !stack.session.contains_key(name) {
        return Err(SessionCommandError {
            code: ErrorCode::NotFound,
            message: unknown_session(stack, name),
        }
        .into());
    }
    let socket = devme_config::paths::supervisor_socket(root)?;
    let mut client = match devme_client::Client::connect(&socket).await {
        Ok(client) => client,
        Err(_) => {
            emit_stopped(name, true, format)?;
            return Ok(0);
        }
    };
    client
        .send(ClientMessage::StopSession {
            session: name.to_string(),
        })
        .await?;
    loop {
        let Some(message) = client.next_event().await? else {
            return Err(anyhow!(
                "supervisor disconnected while stopping session {name:?}"
            ));
        };
        match message {
            ServerMessage::SessionStopped {
                already_stopped, ..
            } => {
                emit_stopped(name, already_stopped, format)?;
                return Ok(0);
            }
            ServerMessage::Error { code, message } => {
                return Err(SessionCommandError { code, message }.into());
            }
            _ => {}
        }
    }
}

/// List configured sessions without spawning a supervisor. If one is already
/// live, merge in its compact state through the read-only IPC query.
pub async fn list(stack: &Stack, root: &Path, format: OutputFormat) -> Result<()> {
    let socket = devme_config::paths::supervisor_socket(root)?;
    let live = if let Ok(mut client) = devme_client::Client::connect(&socket).await {
        match client.request(ClientMessage::ListSessions).await? {
            ServerMessage::Sessions { sessions } => sessions,
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    emit_list(stack, &live, format)
}

fn emit_ready(
    name: &str,
    joined: bool,
    services: &[String],
    env: &std::collections::BTreeMap<String, String>,
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Human => println!(
            "session {name} ready{} ({})",
            if joined { " (joined)" } else { "" },
            if services.is_empty() {
                "no services".to_string()
            } else {
                format!("{} services", services.len())
            }
        ),
        OutputFormat::Json => devme_ui::json(&serde_json::json!({
            "schema_version": 1,
            "session": name,
            "status": "ready",
            "joined": joined,
            "services": services,
            "env": env,
        })),
        OutputFormat::Toon => {
            let mut output = format!(
                "session:\n  name: {}\n  status: ready\n  joined: {joined}\n  services: {}\nallocations[{}]{{env,id}}:",
                toon_string(name),
                toon_array(services),
                env.len()
            );
            for (key, value) in env {
                output.push_str(&format!("\n  {},{}", toon_string(key), toon_string(value)));
            }
            print!("{output}");
        }
    }
    Ok(())
}

fn emit_stopped(name: &str, already_stopped: bool, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Human => println!(
            "session {name} {}",
            if already_stopped {
                "already stopped"
            } else {
                "stopped"
            }
        ),
        OutputFormat::Json => devme_ui::json(&serde_json::json!({
            "schema_version": 1,
            "session": name,
            "status": "stopped",
            "already_stopped": already_stopped,
        })),
        OutputFormat::Toon => print!(
            "session:\n  name: {}\n  status: stopped\n  already_stopped: {already_stopped}",
            toon_string(name)
        ),
    }
    Ok(())
}

fn emit_list(stack: &Stack, live: &[SessionSnapshot], format: OutputFormat) -> Result<()> {
    let live = live
        .iter()
        .map(|session| (session.name.as_str(), session))
        .collect::<HashMap<_, _>>();
    let rows = stack
        .session
        .iter()
        .map(|(name, session)| {
            let state = live
                .get(name.as_str())
                .map_or("stopped", |snapshot| state_name(snapshot.state));
            (name, state, session.needs.len(), session.resources.len())
        })
        .collect::<Vec<_>>();
    match format {
        OutputFormat::Human => {
            if rows.is_empty() {
                println!("No sessions are declared in devme.toml.");
            } else {
                for (name, state, services, resources) in rows {
                    println!("{name:<20} {state:<10} {services} services, {resources} resources");
                }
            }
        }
        OutputFormat::Json => {
            let sessions = rows
                .iter()
                .map(|(name, state, services, resources)| {
                    serde_json::json!({
                        "name": name,
                        "status": state,
                        "services": services,
                        "resources": resources,
                    })
                })
                .collect::<Vec<_>>();
            devme_ui::json(&serde_json::json!({
                "schema_version": 1,
                "count": sessions.len(),
                "sessions": sessions,
            }));
        }
        OutputFormat::Toon => {
            let mut output = format!(
                "count: {}\nsessions[{}]{{name,status,services,resources}}:",
                rows.len(),
                rows.len()
            );
            for (name, state, services, resources) in rows {
                output.push_str(&format!(
                    "\n  {},{state},{services},{resources}",
                    toon_string(name)
                ));
            }
            if stack.session.is_empty() {
                output.push_str("\nhelp: No sessions are declared in devme.toml");
            } else {
                output.push_str("\nhelp: Run `devme session <name>` to open a session");
            }
            print!("{output}");
        }
    }
    Ok(())
}

fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Waiting => "waiting",
        SessionState::Starting => "starting",
        SessionState::Ready => "ready",
        SessionState::Stopping => "stopping",
    }
}

fn unknown_session(stack: &Stack, name: &str) -> String {
    format!(
        "no session named {name:?}; available sessions: {}",
        stack.session.keys().cloned().collect::<Vec<_>>().join(", ")
    )
}

fn toon_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| toon_string(value))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn toon_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}
