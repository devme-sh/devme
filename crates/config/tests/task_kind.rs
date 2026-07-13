use devme_config::{Stack, TaskKind};

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
