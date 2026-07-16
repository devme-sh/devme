use devme_config::{Stack, TaskKind, TaskVisibility};

#[test]
fn existing_tasks_default_to_utility() {
    let stack = Stack::parse("schema_version = 1\n[task.build]\ncmd = \"true\"\n").unwrap();
    assert_eq!(stack.task["build"].kind, TaskKind::Utility);
}

#[test]
fn task_kinds_parse_and_reject_unknown_values() {
    let stack = Stack::parse(
        "schema_version = 1\n[task.ios]\nkind = \"launch\"\ncmd = \"true\"\n[task.verify]\nkind = \"check\"\ncmd = \"true\"\n",
    )
    .unwrap();
    assert_eq!(stack.task["ios"].kind, TaskKind::Launch);
    assert_eq!(stack.task["verify"].kind, TaskKind::Check);
    assert!(Stack::parse("schema_version = 1\n[task.x]\nkind = \"daemon\"\n").is_err());
}

#[test]
fn tasks_default_to_home_and_accept_internal_visibility() {
    let stack = Stack::parse(
        "schema_version = 1\n[task.launch]\ncmd = \"true\"\n[task.codegen]\nvisibility = \"internal\"\ncmd = \"true\"\n",
    )
    .unwrap();

    assert_eq!(stack.task["launch"].visibility, TaskVisibility::Home);
    assert_eq!(stack.task["codegen"].visibility, TaskVisibility::Internal);
    assert!(
        Stack::parse(
            "schema_version = 1\n[task.broken]\nvisibility = \"private\"\ncmd = \"true\"\n"
        )
        .is_err()
    );
}

#[test]
fn task_artifacts_are_plain_paths() {
    let stack = Stack::parse(
        "schema_version = 1\n[task.e2e]\ncmd = \"true\"\nartifacts = [\".devme/result-{slot}.xcresult\", \"screenshots\"]\n",
    )
    .unwrap();

    assert_eq!(
        stack.task["e2e"].artifacts,
        [".devme/result-{slot}.xcresult", "screenshots"]
    );
}
