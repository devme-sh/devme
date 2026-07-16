//! CLI adapters for the presentation-free Task runner.

use std::collections::HashSet;

use anyhow::Result;
use devme_config::Stack;

use crate::{OutputFormat, TaskAction};

pub use devme_task_runner::{
    Approval, ApprovalHandler, ApprovalRequest, BorrowedRunRequest, DaemonStarter,
    ResourceWaitRecord, RunRequest, TaskEvent, TaskOutputEvent, TaskResult, TaskRunner,
    UnknownTask, load, read_history, read_resource_waiters, readiness_timeout_for,
    record_interrupted, resolve, services_for, steps_for,
};

pub fn emit_result(result: &TaskResult, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Human => {
            print!("{}", result.stdout);
            eprint!("{}", result.stderr);
            devme_ui::info(format!(
                "task {} {} in {}ms",
                result.task, result.status, result.duration_ms
            ));
            for artifact in &result.artifacts {
                devme_ui::info(format!("artifact {artifact}"));
            }
        }
        OutputFormat::Json => {
            let mut value = serde_json::to_value(result)?;
            let object = value
                .as_object_mut()
                .expect("Task result serializes as an object");
            object.remove("output_events");
            object.insert("schema_version".into(), serde_json::json!(1));
            devme_ui::json(&value);
        }
        OutputFormat::Toon => {
            print!(
                "result:\n  task: {}\n  status: {}\n  exit_code: {}\n  duration_ms: {}\n  timed_out: {}\n  cancelled: {}\n  interrupted: {}\n  truncated: {}\n  stdout: {}\n  stderr: {}",
                toon_string(&result.task),
                result.status,
                result.exit_code,
                result.duration_ms,
                result.timed_out,
                result.cancelled,
                result.interrupted,
                result.truncated,
                toon_string(&result.stdout),
                toon_string(&result.stderr),
            );
            if !result.artifacts.is_empty() {
                print!("\n  artifacts[{}]:", result.artifacts.len());
                for artifact in &result.artifacts {
                    print!("\n    {}", toon_string(artifact));
                }
            }
        }
    }
    Ok(())
}

fn toon_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

pub fn show(stack: &Stack, action: Option<TaskAction>, format: OutputFormat) -> Result<()> {
    match action {
        Some(TaskAction::Show { task }) => {
            devme_task_runner::services_for(stack, &task)?;
            let value = stack.task.get(&task).expect("Task validation succeeded");
            match format {
                OutputFormat::Json => devme_ui::json(&serde_json::json!({
                    "schema_version": 1,
                    "name": task,
                    "task": value,
                })),
                OutputFormat::Toon => print!(
                    "{}",
                    toon_format::encode_default(&serde_json::json!({
                        "task": {
                            "name": task,
                            "kind": value.kind,
                            "description": value.description,
                            "command": value.cmd,
                            "cwd": value.cwd,
                            "environment": value.env,
                            "dependencies": value.depends_on,
                            "steps": value.steps,
                            "services": value.services,
                            "resources": value.resources,
                            "artifacts": value.artifacts,
                            "timeout_seconds": value.timeout,
                            "readiness_timeout_seconds": value.readiness_timeout,
                        }
                    }))?
                ),
                OutputFormat::Human => println!("{}", toml::to_string_pretty(value)?),
            }
        }
        None => match format {
            OutputFormat::Json => {
                let rows = stack
                    .task
                    .iter()
                    .map(|(name, task)| {
                        serde_json::json!({
                            "name": name,
                            "task_kind": task.kind,
                            "visibility": task.visibility,
                            "description": task.description,
                            "has_command": task.cmd.is_some(),
                        })
                    })
                    .collect::<Vec<_>>();
                devme_ui::json(&serde_json::json!({
                    "schema_version": 1,
                    "count": rows.len(),
                    "tasks": rows,
                }));
            }
            OutputFormat::Toon => {
                let mut output = format!(
                    "count: {}\ntasks[{}]{{name,description,kind,task_kind,visibility}}:",
                    stack.task.len(),
                    stack.task.len()
                );
                for (name, task) in &stack.task {
                    let task_kind = match task.kind {
                        devme_config::TaskKind::Launch => "launch",
                        devme_config::TaskKind::Check => "check",
                        devme_config::TaskKind::Utility => "utility",
                    };
                    output.push_str(&format!(
                        "\n  {},{},{},{},{}",
                        toon_string(name),
                        toon_string(task.description.as_deref().unwrap_or("")),
                        if task.cmd.is_some() {
                            "command"
                        } else {
                            "aggregate"
                        },
                        task_kind,
                        match task.visibility {
                            devme_config::TaskVisibility::Home => "home",
                            devme_config::TaskVisibility::Internal => "internal",
                        },
                    ));
                }
                output.push_str(if stack.task.is_empty() {
                    "\nhelp: No tasks are declared in devme.toml"
                } else {
                    "\nhelp: Run `devme tasks show <name>` for details"
                });
                print!("{output}");
            }
            OutputFormat::Human => {
                if stack.task.is_empty() {
                    println!("No tasks are declared in devme.toml.");
                }
                for (name, task) in &stack.task {
                    println!(
                        "{name:<20} {}",
                        task.description
                            .as_deref()
                            .unwrap_or(if task.cmd.is_some() {
                                "command"
                            } else {
                                "aggregate"
                            })
                    );
                }
            }
        },
    }
    Ok(())
}

pub fn history_names(stack: &Stack) -> HashSet<String> {
    stack.task.keys().cloned().collect()
}
