use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use devme_config::{Stack, TaskKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeAction {
    pub task: String,
    pub label: String,
    pub description: String,
    pub kind: TaskKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentResult {
    pub task: String,
    pub kind: TaskKind,
    pub status: String,
    pub finished_at: u64,
}

impl RecentResult {
    pub fn wording(&self) -> String {
        let noun = match self.kind {
            TaskKind::Launch => "launch",
            TaskKind::Check => "check",
            TaskKind::Utility => "run",
        };
        let status = match self.status.as_str() {
            "passed" => "succeeded",
            "cancelled" => "cancelled",
            "timed_out" => "timed out",
            _ => "failed",
        };
        format!("last {noun} {status}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskUpdate {
    Progress(String),
    Output(String),
    Finished(RecentResult),
}

pub type RunFuture = Pin<Box<dyn Future<Output = anyhow::Result<RecentResult>> + Send>>;
pub type TaskRunner = Arc<
    dyn Fn(
            String,
            tokio::sync::mpsc::UnboundedSender<TaskUpdate>,
            tokio::sync::watch::Receiver<bool>,
        ) -> RunFuture
        + Send
        + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeState {
    pub actions: Vec<HomeAction>,
    pub selected: usize,
    pub recent: Vec<RecentResult>,
    pub running: Option<String>,
    pub logs: Vec<String>,
    pub visible: bool,
    member_focus: Option<String>,
}

impl HomeState {
    pub fn from_stack(stack: &Stack, recent: Vec<RecentResult>) -> Self {
        Self::from_stack_with_member_focus(stack, None, recent)
    }

    pub fn from_stack_with_member_focus(
        stack: &Stack,
        member_focus: Option<&str>,
        recent: Vec<RecentResult>,
    ) -> Self {
        let mut actions = stack
            .task
            .iter()
            .map(|(name, task)| HomeAction {
                task: name.clone(),
                label: display_label(name, member_focus),
                description: task.description.clone().unwrap_or_default(),
                kind: task.kind,
            })
            .collect::<Vec<_>>();
        actions.sort_by_key(|action| kind_order(action.kind));
        let mut state = Self {
            actions,
            selected: 0,
            recent: Vec::new(),
            running: None,
            logs: Vec::new(),
            visible: true,
            member_focus: member_focus.map(str::to_owned),
        };
        for result in recent {
            state.record_result(result);
        }
        state
    }

    pub fn task_label(&self, task: &str) -> String {
        display_label(task, self.member_focus.as_deref())
    }

    pub fn record_result(&mut self, result: RecentResult) {
        self.recent.retain(|existing| existing.task != result.task);
        self.recent.push(result);
        if self.recent.len() > 5 {
            self.recent.remove(0);
        }
    }

    pub fn move_next(&mut self) {
        if !self.actions.is_empty() {
            self.selected = (self.selected + 1) % self.actions.len();
        }
    }
    pub fn move_previous(&mut self) {
        if !self.actions.is_empty() {
            self.selected = (self.selected + self.actions.len() - 1) % self.actions.len();
        }
    }
    pub fn selected_task(&self) -> Option<String> {
        self.actions.get(self.selected).map(|a| a.task.clone())
    }
}

fn kind_order(kind: TaskKind) -> u8 {
    match kind {
        TaskKind::Launch => 0,
        TaskKind::Check => 1,
        TaskKind::Utility => 2,
    }
}

fn display_label(name: &str, member_focus: Option<&str>) -> String {
    let focused_prefix = member_focus.map(|member| format!("{member}::"));
    let visible = focused_prefix
        .as_deref()
        .and_then(|prefix| name.strip_prefix(prefix))
        .unwrap_or(name);
    visible.replace("::", " ").replace(['-', '_'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_group_run_then_check_then_utility() {
        let stack = Stack::parse("schema_version = 1\n[task.z]\ncmd=\"true\"\n[task.verify]\nkind=\"check\"\ncmd=\"true\"\n[task.ios]\nkind=\"launch\"\ncmd=\"true\"\n").unwrap();
        let home = HomeState::from_stack(&stack, vec![]);
        assert_eq!(
            home.actions
                .iter()
                .map(|a| a.task.as_str())
                .collect::<Vec<_>>(),
            ["ios", "verify", "z"]
        );
    }

    #[test]
    fn launch_history_never_claims_runtime_observation() {
        let result = RecentResult {
            task: "ios".into(),
            kind: TaskKind::Launch,
            status: "passed".into(),
            finished_at: 1,
        };
        assert_eq!(result.wording(), "last launch succeeded");
    }

    #[test]
    fn root_workspace_labels_keep_member_names() {
        let stack = Stack::parse(
            "schema_version = 1\n[task.\"backend::launch\"]\nkind=\"launch\"\ncmd=\"true\"\n[task.\"backend::test\"]\nkind=\"check\"\ncmd=\"true\"\n[task.\"ios::e2e\"]\nkind=\"check\"\ncmd=\"true\"\n[task.\"android::e2e\"]\nkind=\"check\"\ncmd=\"true\"\n",
        )
        .unwrap();

        let home = HomeState::from_stack(&stack, vec![]);
        assert_eq!(
            home.actions
                .iter()
                .map(|action| action.label.as_str())
                .collect::<Vec<_>>(),
            ["backend launch", "backend test", "ios e2e", "android e2e"]
        );
    }

    #[test]
    fn member_workspace_labels_remove_only_the_focused_namespace() {
        let stack = Stack::parse(
            "schema_version = 1\n[task.\"ios::simulator\"]\nkind=\"launch\"\ncmd=\"true\"\n[task.\"ios::test\"]\nkind=\"check\"\ncmd=\"true\"\n",
        )
        .unwrap();

        let home = HomeState::from_stack_with_member_focus(&stack, Some("ios"), vec![]);
        assert_eq!(
            home.actions
                .iter()
                .map(|action| action.label.as_str())
                .collect::<Vec<_>>(),
            ["simulator", "test"]
        );
        assert_eq!(home.task_label("ios::device-e2e"), "device e2e");
    }

    #[test]
    fn recent_results_keep_only_the_latest_run_per_task() {
        let stack = Stack::parse("schema_version = 1\n[task.simulator]\ncmd=\"true\"\n").unwrap();
        let result = |task: &str, finished_at| RecentResult {
            task: task.to_string(),
            kind: TaskKind::Launch,
            status: "passed".into(),
            finished_at,
        };
        let mut home = HomeState::from_stack(
            &stack,
            vec![
                result("simulator", 1),
                result("test", 2),
                result("simulator", 3),
            ],
        );

        assert_eq!(
            home.recent
                .iter()
                .map(|recent| (recent.task.as_str(), recent.finished_at))
                .collect::<Vec<_>>(),
            [("test", 2), ("simulator", 3)]
        );

        home.record_result(result("test", 4));
        assert_eq!(
            home.recent
                .iter()
                .map(|recent| (recent.task.as_str(), recent.finished_at))
                .collect::<Vec<_>>(),
            [("simulator", 3), ("test", 4)]
        );
    }
}
