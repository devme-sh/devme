use devme_config::{Stack, validate};
use devme_core::Scope;
use tempfile::TempDir;

#[test]
fn resource_bound_session_is_a_valid_composition_over_existing_nodes() {
    let stack = Stack::parse(
        r#"schema_version = 1

[resource.device]
scope = "host"
capacity = 2
env = "DEVICE_ID"

[service.backend]
cmd = "sleep 30"

[service.device_logs]
cmd = "printf '%s' \"$DEVICE_ID\"; sleep 30"
scope = "session"
depends_on = ["backend"]

[task.launch]
cmd = "printf '%s' \"$DEVICE_ID\""

[session.mobile]
needs = ["device_logs"]
resources = ["device"]
run = "launch"
linger = 7
"#,
    )
    .unwrap();

    validate(&stack).unwrap();
    assert_eq!(stack.service["device_logs"].scope, Scope::Session);
    let session = &stack.session["mobile"];
    assert_eq!(session.needs, ["device_logs"]);
    assert_eq!(session.resources, ["device"]);
    assert_eq!(session.run.as_deref(), Some("launch"));
    assert_eq!(session.linger, 7);
}

#[test]
fn session_scoped_service_must_have_exactly_one_owner() {
    let stack = Stack::parse(
        r#"schema_version = 1
[service.logs]
cmd = "sleep 30"
scope = "session"
"#,
    )
    .unwrap();

    let errors = validate(&stack).unwrap_err();
    assert!(errors.iter().any(|error| error.to_string().contains(
        "session-scoped service 'logs' must belong to exactly one [session] dependency closure"
    )));
}

#[test]
fn member_session_references_are_namespaced_with_its_nodes() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join("apps/ios")).unwrap();
    std::fs::write(
        root.path().join("devme.toml"),
        "schema_version=1\n[workspace.members]\nios='apps/ios'\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("apps/ios/devme.toml"),
        r#"schema_version=1
[resource.device]
env="DEVICE_ID"
[service.logs]
cmd="sleep 30"
scope="session"
[task.launch]
cmd="true"
[session.dev]
needs=["logs"]
resources=["device"]
run="launch"
"#,
    )
    .unwrap();

    let resolved = devme_config::ResolvedWorkspace::resolve(&root.path().join("apps/ios")).unwrap();
    let session = &resolved.stack().session["ios::dev"];
    assert_eq!(session.needs, ["ios::logs"]);
    assert_eq!(session.resources, ["ios::device"]);
    assert_eq!(session.run.as_deref(), Some("ios::launch"));
}

#[test]
fn ordinary_tasks_cannot_bypass_session_leases_or_reacquire_them() {
    let stack = Stack::parse(
        r#"schema_version=1
[resource.device]
[service.logs]
cmd="sleep 30"
scope="session"
[task.launch]
cmd="true"
services=["logs"]
resources=["device"]
[session.dev]
needs=["logs"]
resources=["device"]
run="launch"
"#,
    )
    .unwrap();

    let messages = validate(&stack)
        .unwrap_err()
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(messages.iter().any(|message| message.contains(
        "task 'launch' cannot require session-scoped service 'logs'; open its owning session"
    )));
    assert!(messages.iter().any(|message| message.contains(
        "session 'dev' task 'launch' cannot declare resources; declare them on the session"
    )));
}
