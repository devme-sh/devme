use std::fs;

use devme_config::{Focus, ResolvedWorkspace, WorkspaceError};
use tempfile::TempDir;

#[test]
fn resolving_from_member_composes_one_focused_workspace() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("backend")).unwrap();
    fs::create_dir_all(dir.path().join("apps/ios/Sources")).unwrap();
    fs::write(
        dir.path().join("devme.toml"),
        r#"schema_version = 1

[workspace.members]
backend = "backend"
ios = "apps/ios"

[resource.codesigning]
scope = "host"

[task.check]
depends_on = ["ios::test"]
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("backend/devme.toml"),
        r#"schema_version = 1

[step.dependencies]
check = "test -d node_modules"
provision = "bun install"
trust = "auto"

[service.api]
cmd = "bun run dev"
depends_on = ["dependencies"]
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("apps/ios/devme.toml"),
        r#"schema_version = 1

[step.dependencies]
check = "test -d .devme/packages"
provision = "swift package resolve"
trust = "auto"

[service.logs]
cmd = "./scripts/ios-logs"
depends_on = ["dependencies", "backend::api"]

[task.test]
cmd = "xcodebuild test"
steps = ["dependencies"]
services = ["backend::api"]
resources = ["root::codesigning"]
"#,
    )
    .unwrap();

    let resolved = ResolvedWorkspace::resolve(&dir.path().join("apps/ios/Sources")).unwrap();

    assert_eq!(resolved.root(), dir.path());
    assert_eq!(resolved.focus(), &Focus::Member("ios".into()));
    assert_eq!(resolved.focus_name("test"), "ios::test");
    assert_eq!(resolved.focus_name("backend::api"), "backend::api");
    assert_eq!(resolved.focus_name("root::check"), "check");
    assert_eq!(resolved.focus_services().unwrap(), ["ios::logs"]);
    assert!(resolved.stack().step.contains_key("backend::dependencies"));
    assert!(resolved.stack().service.contains_key("backend::api"));
    assert!(resolved.stack().step.contains_key("ios::dependencies"));
    assert!(resolved.stack().service.contains_key("ios::logs"));
    assert!(resolved.stack().task.contains_key("ios::test"));
    assert_eq!(
        resolved.stack().task["ios::test"].cwd.as_deref(),
        Some("apps/ios")
    );
    assert_eq!(
        resolved.stack().task["ios::test"].steps,
        ["ios::dependencies"]
    );
    assert_eq!(
        resolved.stack().task["ios::test"].services,
        ["backend::api"]
    );
    assert_eq!(
        resolved.stack().task["ios::test"].resources,
        ["codesigning"]
    );
    assert_eq!(
        resolved.stack().service["ios::logs"].depends_on[0].name,
        "ios::dependencies"
    );
    assert_eq!(
        resolved.stack().service["ios::logs"].depends_on[1].name,
        "backend::api"
    );
    assert!(
        resolved.stack().step["ios::dependencies"]
            .check
            .starts_with("cd 'apps/ios' && ")
    );
    assert_eq!(
        resolved.origin("ios::test").unwrap().config,
        dir.path().join("apps/ios/devme.toml")
    );
}

#[test]
fn standalone_config_remains_compatible_from_a_subdirectory() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("src/deep")).unwrap();
    fs::write(
        dir.path().join("devme.toml"),
        "schema_version = 1\n\n[task.check]\ncmd = \"true\"\n",
    )
    .unwrap();

    let resolved = ResolvedWorkspace::resolve(&dir.path().join("src/deep")).unwrap();

    assert_eq!(resolved.root(), dir.path());
    assert_eq!(resolved.focus(), &Focus::Root);
    assert!(resolved.stack().task.contains_key("check"));
}

#[test]
fn rejects_recursive_workspace_members() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("apps/ios")).unwrap();
    fs::write(
        dir.path().join("devme.toml"),
        "schema_version = 1\n[workspace.members]\nios = \"apps/ios\"\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("apps/ios/devme.toml"),
        "schema_version = 1\n[workspace.members]\nfeature = \"feature\"\n",
    )
    .unwrap();

    let error = ResolvedWorkspace::resolve(&dir.path().join("apps/ios")).unwrap_err();

    assert!(matches!(
        error,
        WorkspaceError::NestedWorkspace { member } if member == "ios"
    ));
}

#[test]
fn rejects_member_paths_that_escape_or_overlap() {
    let escape = TempDir::new().unwrap();
    fs::write(
        escape.path().join("devme.toml"),
        "schema_version = 1\n[workspace.members]\nbad = \"../outside\"\n",
    )
    .unwrap();
    assert!(matches!(
        ResolvedWorkspace::resolve(escape.path()).unwrap_err(),
        WorkspaceError::InvalidMemberPath { member, .. } if member == "bad"
    ));

    let overlap = TempDir::new().unwrap();
    fs::write(
        overlap.path().join("devme.toml"),
        "schema_version = 1\n[workspace.members]\napps = \"apps\"\nios = \"apps/ios\"\n",
    )
    .unwrap();
    assert!(matches!(
        ResolvedWorkspace::resolve(overlap.path()).unwrap_err(),
        WorkspaceError::OverlappingMembers { .. }
    ));
}

#[test]
fn rejects_shared_log_policy_in_a_child() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("backend")).unwrap();
    fs::write(
        dir.path().join("devme.toml"),
        "schema_version = 1\n[workspace.members]\nbackend = \"backend\"\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("backend/devme.toml"),
        "schema_version = 1\n[logs]\nretention_bytes = 1024\n",
    )
    .unwrap();

    assert!(matches!(
        ResolvedWorkspace::resolve(&dir.path().join("backend")).unwrap_err(),
        WorkspaceError::ChildLogs { .. }
    ));
}

#[test]
fn rejects_unclaimed_nested_configs_and_reserved_names() {
    let unclaimed = TempDir::new().unwrap();
    fs::create_dir_all(unclaimed.path().join("tools")).unwrap();
    fs::write(
        unclaimed.path().join("devme.toml"),
        "schema_version = 1\n[workspace.members]\nios = \"apps/ios\"\n",
    )
    .unwrap();
    fs::write(
        unclaimed.path().join("tools/devme.toml"),
        "schema_version = 1\n",
    )
    .unwrap();
    assert!(matches!(
        ResolvedWorkspace::resolve(&unclaimed.path().join("tools")).unwrap_err(),
        WorkspaceError::UnclaimedConfig { .. }
    ));

    let reserved = TempDir::new().unwrap();
    fs::create_dir_all(reserved.path().join("apps/ios")).unwrap();
    fs::write(
        reserved.path().join("devme.toml"),
        "schema_version = 1\n[workspace.members]\nios = \"apps/ios\"\n",
    )
    .unwrap();
    fs::write(
        reserved.path().join("apps/ios/devme.toml"),
        "schema_version = 1\n[task.\"bad::name\"]\ncmd = \"true\"\n",
    )
    .unwrap();
    assert!(matches!(
        ResolvedWorkspace::resolve(&reserved.path().join("apps/ios")).unwrap_err(),
        WorkspaceError::InvalidNodeName { .. }
    ));
}
