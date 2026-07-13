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
}

impl HomeState {
    pub fn from_stack(stack: &Stack, recent: Vec<RecentResult>) -> Self {
        let mut actions = stack
            .task
            .iter()
            .map(|(name, task)| HomeAction {
                task: name.clone(),
                label: display_label(name),
                description: task.description.clone().unwrap_or_default(),
                kind: task.kind,
            })
            .collect::<Vec<_>>();
        actions.sort_by_key(|action| kind_order(action.kind));
        Self {
            actions,
            selected: 0,
            recent,
            running: None,
            logs: Vec::new(),
            visible: true,
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

fn display_label(name: &str) -> String {
    let mut parts = name.rsplit("::");
    let leaf = parts.next().unwrap_or(name);
    let generic = matches!(leaf, "launch" | "run" | "dev" | "test" | "verify" | "check");
    parts
        .next()
        .filter(|_| generic)
        .unwrap_or(leaf)
        .replace(['-', '_'], " ")
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
}
