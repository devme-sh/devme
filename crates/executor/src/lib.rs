//! Graph executor — the brain of a devme daemon.
//!
//! Pure logic: consumes events from the I/O layer (the supervisor), updates
//! its per-node state machine, and emits actions for the supervisor to
//! enact. No I/O, no async, no clock — the supervisor owns those.
//!
//! See ADR-0001 (unified graph) and ADR-0005 (override-aware failure model).

use std::collections::HashMap;

use devme_config::{DepStatus, Graph, NodeKind, SatisfactionOutcome};
use devme_core::{ServiceState, StepState};

/// What the supervisor should do next. The executor never performs these
/// itself — it just announces them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Spawn the Step's `check` command and report back via
    /// [`Event::StepCheckCompleted`].
    RunCheck(String),
    /// Spawn the Step's `provision` command and report back via
    /// [`Event::StepProvisionCompleted`].
    RunProvision(String),
    /// Spawn the Service.
    StartService(String),
    /// Health-check an `external` Service instead of spawning it. The
    /// supervisor probes its `health` until it passes, then reports back via
    /// [`Event::ExternalHealthy`]. Used for repo-scoped services owned by the
    /// shared supervisor — the instance daemon must not spawn its own copy.
    ProbeExternal(String),
    /// Terminate the Service.
    StopService(String),
}

/// Things that happen in the outside world (or that the user asks for) and
/// drive the state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Begin executing — runs all leaf checks and starts dependency-free
    /// services.
    Start,
    /// The Step's `check` finished. `passed=true` means the prerequisite
    /// was already met; `false` means it wasn't.
    StepCheckCompleted { name: String, passed: bool },
    /// The Step's `provision` finished. On success the check is re-run so
    /// the world is authoritatively verified, not just assumed-fixed.
    StepProvisionCompleted { name: String, passed: bool },
    /// The Service is healthy (passed its `health` probe, or simply
    /// reached "alive" if it has no probe).
    ServiceHealthy { name: String },
    /// An `external` Service's health probe passed. Transitions it to
    /// [`ServiceState::External`] `{ healthy: true }` so dependents proceed,
    /// without devme ever owning the process.
    ExternalHealthy { name: String },
    /// The Service exited or was killed. `exit_code = None` for signal exits.
    ServiceExited {
        name: String,
        exit_code: Option<i32>,
    },
    /// The supervisor's crash-loop breaker tripped: the Service kept dying
    /// right after spawn and auto-restart has been suspended. Parks the node
    /// in [`ServiceState::CrashLoop`] — terminal until the user resets it —
    /// so status snapshots report the quarantine, not a transient `Failed`.
    /// `reason`, when known, names the diagnosed cause (e.g. an occupied
    /// port).
    ServiceCrashLooped {
        name: String,
        restart_count: u32,
        reason: Option<String>,
    },
    /// User asked to stop a service.
    UserStop { name: String },
    /// User marked a Step as overridden (mark-as-installed / skip-this-run).
    UserOverride { name: String },
}

/// A snapshot of one node's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeStatus {
    Step(StepState),
    Service(ServiceState),
}

pub struct Executor {
    graph: Graph,
    nodes: HashMap<String, NodeStatus>,
}

impl Executor {
    pub fn new(graph: Graph) -> Self {
        Self {
            nodes: HashMap::new(),
            graph,
        }
    }

    pub fn state(&self, name: &str) -> Option<&NodeStatus> {
        self.nodes.get(name)
    }

    /// Forget a node's tracked state so [`Self::handle(Event::Start)`] (or
    /// any other call into [`Self::advance`]) treats it as eligible to run
    /// again. Used by the supervisor to honor user-initiated restarts.
    pub fn reset(&mut self, name: &str) -> Vec<Action> {
        self.nodes.remove(name);
        self.advance()
    }

    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Start => self.advance(),
            Event::StepCheckCompleted { name, passed } => {
                if passed {
                    self.nodes
                        .insert(name, NodeStatus::Step(StepState::Passed));
                    self.advance()
                } else if self.graph.has_provision(&name) {
                    self.nodes
                        .insert(name.clone(), NodeStatus::Step(StepState::Unknown));
                    vec![Action::RunProvision(name)]
                } else {
                    self.nodes
                        .insert(name, NodeStatus::Step(StepState::Failed));
                    self.advance()
                }
            }
            Event::UserStop { name } => {
                self.nodes
                    .insert(name.clone(), NodeStatus::Service(ServiceState::Stopped));
                vec![Action::StopService(name)]
            }
            Event::UserOverride { name } => {
                self.nodes
                    .insert(name, NodeStatus::Step(StepState::Overridden));
                self.advance()
            }
            Event::ServiceExited { name, exit_code } => {
                self.nodes.insert(
                    name,
                    NodeStatus::Service(ServiceState::Failed { exit_code }),
                );
                self.advance()
            }
            Event::ServiceCrashLooped {
                name,
                restart_count,
                reason,
            } => {
                self.nodes.insert(
                    name,
                    NodeStatus::Service(ServiceState::CrashLoop {
                        restart_count,
                        reason,
                    }),
                );
                self.advance()
            }
            Event::ServiceHealthy { name } => {
                self.nodes.insert(
                    name,
                    NodeStatus::Service(ServiceState::Running {
                        degraded: false,
                        started_without: Vec::new(),
                    }),
                );
                self.advance()
            }
            Event::ExternalHealthy { name } => {
                self.nodes.insert(
                    name,
                    NodeStatus::Service(ServiceState::External { healthy: true }),
                );
                self.advance()
            }
            Event::StepProvisionCompleted { name, passed } => {
                if passed {
                    // Re-run the check — provision succeeded, but the check
                    // is the only authoritative answer to "is this OK now?".
                    self.nodes
                        .insert(name.clone(), NodeStatus::Step(StepState::Unknown));
                    vec![Action::RunCheck(name)]
                } else {
                    self.nodes
                        .insert(name, NodeStatus::Step(StepState::ProvisionFailed));
                    self.advance()
                }
            }
        }
    }

    /// Look at the graph and current state; emit actions for anything that
    /// can move forward.
    fn advance(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        for name in self.graph.nodes().to_vec() {
            if self.nodes.contains_key(&name) {
                continue;
            }
            if !self.required_deps_satisfied(&name) {
                continue;
            }
            match self.graph.kind(&name) {
                Some(NodeKind::Step) => {
                    out.push(Action::RunCheck(name.clone()));
                    self.nodes
                        .insert(name, NodeStatus::Step(StepState::Unknown));
                }
                Some(NodeKind::Service) if self.graph.is_external(&name) => {
                    // Don't spawn — the process is owned elsewhere (e.g. the
                    // shared supervisor). Probe its health and wait for an
                    // ExternalHealthy event before unblocking dependents.
                    out.push(Action::ProbeExternal(name.clone()));
                    self.nodes.insert(
                        name,
                        NodeStatus::Service(ServiceState::External { healthy: false }),
                    );
                }
                Some(NodeKind::Service) => {
                    out.push(Action::StartService(name.clone()));
                    self.nodes
                        .insert(name, NodeStatus::Service(ServiceState::Starting));
                }
                None => {}
            }
        }
        out
    }

    fn required_deps_satisfied(&self, name: &str) -> bool {
        matches!(
            self.graph.deps_satisfied(name, |dep| self.dep_status(dep)),
            SatisfactionOutcome::Ready
        )
    }

    fn dep_status(&self, name: &str) -> DepStatus {
        match self.nodes.get(name) {
            None => DepStatus::Pending,
            Some(NodeStatus::Step(s)) if s.is_satisfied() => DepStatus::Satisfied,
            Some(NodeStatus::Step(StepState::Failed | StepState::ProvisionFailed)) => {
                DepStatus::Failed
            }
            Some(NodeStatus::Step(_)) => DepStatus::Pending,
            Some(NodeStatus::Service(s)) if s.is_up() => DepStatus::Satisfied,
            Some(NodeStatus::Service(ServiceState::Failed { .. } | ServiceState::CrashLoop { .. })) => {
                DepStatus::Failed
            }
            Some(NodeStatus::Service(_)) => DepStatus::Pending,
        }
    }

    /// True while the graph is actively advancing: some node is in a
    /// non-terminal state — a step check/provision is running, a service is
    /// starting or restarting, or an external is still being probed.
    fn run_in_flight(&self) -> bool {
        self.nodes.values().any(|n| {
            matches!(
                n,
                NodeStatus::Step(StepState::Unknown)
                    | NodeStatus::Service(
                        ServiceState::Starting
                            | ServiceState::Restarting { .. }
                            | ServiceState::External { healthy: false }
                    )
            )
        })
    }

    /// For a node the run hasn't reached yet, the first required dependency
    /// holding it back. `Some` only while the graph is actively advancing —
    /// on an idle daemon (nothing started, or everything settled) untouched
    /// services still read as plain Stopped, not "waiting".
    pub fn blocked_by(&self, name: &str) -> Option<String> {
        if self.nodes.contains_key(name) || !self.run_in_flight() {
            return None;
        }
        self.graph
            .dependencies(name)
            .iter()
            .find(|dep| dep.required && self.dep_status(&dep.name) != DepStatus::Satisfied)
            .map(|dep| dep.name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devme_config::Stack;

    fn graph(toml_str: &str) -> Graph {
        Graph::from_stack(&Stack::parse(toml_str).expect("parse"))
    }

    #[test]
    fn unreached_service_reports_blocking_dep_only_while_run_in_flight() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[step.migrate]
check = "true"

[service.api]
cmd = "serve"
depends_on = ["migrate"]

[service.web]
cmd = "serve"
depends_on = ["api"]
"#,
        ));
        // Idle daemon: nothing in flight, unreached services are just Stopped.
        assert_eq!(e.blocked_by("api"), None);

        // Start → migrate's check is running; api/web are waiting, not stopped.
        e.handle(Event::Start);
        assert_eq!(e.blocked_by("api").as_deref(), Some("migrate"));
        assert_eq!(e.blocked_by("web").as_deref(), Some("api"));

        // migrate passes → api gets its own entry (Starting); web still waits.
        e.handle(Event::StepCheckCompleted {
            name: "migrate".into(),
            passed: true,
        });
        assert_eq!(e.blocked_by("api"), None);
        assert_eq!(e.blocked_by("web").as_deref(), Some("api"));

        // api healthy → web starts; nothing is left waiting.
        e.handle(Event::ServiceHealthy { name: "api".into() });
        assert_eq!(e.blocked_by("web"), None);
    }

    #[test]
    fn empty_graph_start_emits_no_actions() {
        let mut e = Executor::new(graph("schema_version = 1"));
        let actions = e.handle(Event::Start);
        assert!(actions.is_empty());
    }

    #[test]
    fn start_emits_start_service_for_a_leaf_service() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[service.db]
cmd = "docker run postgres"
"#,
        ));
        let actions = e.handle(Event::Start);
        assert_eq!(actions, vec![Action::StartService("db".into())]);
    }

    #[test]
    fn successful_provision_reruns_the_check() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[step.uv]
check = "command -v uv"
provision = "brew install uv"
"#,
        ));
        e.handle(Event::Start);
        e.handle(Event::StepCheckCompleted {
            name: "uv".into(),
            passed: false,
        });
        let after_provision = e.handle(Event::StepProvisionCompleted {
            name: "uv".into(),
            passed: true,
        });
        assert_eq!(after_provision, vec![Action::RunCheck("uv".into())]);
    }

    #[test]
    fn failed_provision_marks_step_as_provision_failed() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[step.uv]
check = "command -v uv"
provision = "brew install uv"
"#,
        ));
        e.handle(Event::Start);
        e.handle(Event::StepCheckCompleted {
            name: "uv".into(),
            passed: false,
        });
        e.handle(Event::StepProvisionCompleted {
            name: "uv".into(),
            passed: false,
        });
        assert_eq!(
            e.state("uv"),
            Some(&NodeStatus::Step(StepState::ProvisionFailed))
        );
    }

    #[test]
    fn user_stop_running_service_emits_stop_action() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[service.db]
cmd = "postgres"
"#,
        ));
        e.handle(Event::Start);
        e.handle(Event::ServiceHealthy { name: "db".into() });
        let stop_actions = e.handle(Event::UserStop { name: "db".into() });
        assert_eq!(stop_actions, vec![Action::StopService("db".into())]);
    }

    #[test]
    fn user_override_on_failed_step_unblocks_dependents() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[step.tools]
check = "false"

[service.backend]
cmd = "server"
depends_on = ["tools"]
"#,
        ));
        e.handle(Event::Start);
        e.handle(Event::StepCheckCompleted {
            name: "tools".into(),
            passed: false,
        });
        // Step is now Failed (no provision) and backend is blocked.
        let override_actions = e.handle(Event::UserOverride { name: "tools".into() });
        assert_eq!(
            override_actions,
            vec![Action::StartService("backend".into())]
        );
        assert!(matches!(
            e.state("tools"),
            Some(NodeStatus::Step(StepState::Overridden))
        ));
    }

    #[test]
    fn service_exit_marks_service_failed_and_blocks_dependents() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[service.db]
cmd = "postgres"

[service.backend]
cmd = "server"
depends_on = ["db"]
"#,
        ));
        e.handle(Event::Start);
        let next = e.handle(Event::ServiceExited {
            name: "db".into(),
            exit_code: Some(1),
        });
        // backend mustn't auto-start when its dep failed.
        assert!(next.is_empty());
        assert!(matches!(
            e.state("db"),
            Some(NodeStatus::Service(ServiceState::Failed { .. }))
        ));
    }

    #[test]
    fn crash_looped_service_is_parked_terminal_and_blocks_dependents() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[service.db]
cmd = "postgres"

[service.backend]
cmd = "server"
depends_on = ["db"]
"#,
        ));
        e.handle(Event::Start);
        e.handle(Event::ServiceExited {
            name: "db".into(),
            exit_code: Some(1),
        });
        let next = e.handle(Event::ServiceCrashLooped {
            name: "db".into(),
            restart_count: 4,
            reason: Some("port 5432 already in use by another process".into()),
        });
        // Quarantine emits no new work and dependents stay blocked.
        assert!(next.is_empty());
        assert!(matches!(
            e.state("db"),
            Some(NodeStatus::Service(ServiceState::CrashLoop {
                restart_count: 4,
                reason: Some(_),
            }))
        ));
        // A later global Start must not resurrect the quarantined node —
        // only an explicit per-service reset (user restart) does.
        let restarted = e.handle(Event::Start);
        assert!(restarted.is_empty());
        let after_reset = e.reset("db");
        assert_eq!(after_reset, vec![Action::StartService("db".into())]);
    }

    #[test]
    fn service_healthy_unblocks_dependent_service() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[service.db]
cmd = "postgres"

[service.backend]
cmd = "server"
depends_on = ["db"]
"#,
        ));
        let first = e.handle(Event::Start);
        assert_eq!(first, vec![Action::StartService("db".into())]);

        let after_healthy = e.handle(Event::ServiceHealthy { name: "db".into() });
        assert_eq!(after_healthy, vec![Action::StartService("backend".into())]);
    }

    #[test]
    fn failed_check_with_provision_triggers_run_provision() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[step.uv]
check = "command -v uv"
provision = "brew install uv"
"#,
        ));
        e.handle(Event::Start);
        let next = e.handle(Event::StepCheckCompleted {
            name: "uv".into(),
            passed: false,
        });
        assert_eq!(next, vec![Action::RunProvision("uv".into())]);
    }

    #[test]
    fn step_check_passing_unblocks_dependent_service() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[step.tools]
check = "command -v uv"

[service.backend]
cmd = "uv run server"
depends_on = ["tools"]
"#,
        ));
        let first = e.handle(Event::Start);
        assert_eq!(first, vec![Action::RunCheck("tools".into())]);

        let second = e.handle(Event::StepCheckCompleted {
            name: "tools".into(),
            passed: true,
        });
        assert_eq!(second, vec![Action::StartService("backend".into())]);
    }

    #[test]
    fn service_waits_for_its_step_dependency() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[step.tools]
check = "command -v uv"

[service.backend]
cmd = "uv run server"
depends_on = ["tools"]
"#,
        ));
        let actions = e.handle(Event::Start);
        // Only the step's check runs first; the service waits.
        assert_eq!(actions, vec![Action::RunCheck("tools".into())]);
    }

    #[test]
    fn reset_after_stop_lets_service_start_again() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[service.db]
cmd = "postgres"
"#,
        ));
        e.handle(Event::Start);
        e.handle(Event::ServiceHealthy { name: "db".into() });
        e.handle(Event::UserStop { name: "db".into() });
        // After UserStop, db is marked Stopped; advance() wouldn't re-pick it.
        let actions = e.reset("db");
        assert_eq!(actions, vec![Action::StartService("db".into())]);
    }

    #[test]
    fn external_service_probes_instead_of_starting() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[service.proxy]
cmd = "cloud-sql-proxy"
external = true
health = { tcp = "localhost:15432" }
"#,
        ));
        let actions = e.handle(Event::Start);
        assert_eq!(actions, vec![Action::ProbeExternal("proxy".into())]);
        assert!(matches!(
            e.state("proxy"),
            Some(NodeStatus::Service(ServiceState::External { healthy: false }))
        ));
    }

    #[test]
    fn external_healthy_unblocks_dependent_service() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[service.proxy]
cmd = "cloud-sql-proxy"
external = true
health = { tcp = "localhost:15432" }

[service.backend]
cmd = "server"
depends_on = ["proxy"]
"#,
        ));
        let first = e.handle(Event::Start);
        // Only the external probe is requested; backend waits on it.
        assert_eq!(first, vec![Action::ProbeExternal("proxy".into())]);

        let after = e.handle(Event::ExternalHealthy { name: "proxy".into() });
        assert_eq!(after, vec![Action::StartService("backend".into())]);
        assert!(matches!(
            e.state("proxy"),
            Some(NodeStatus::Service(ServiceState::External { healthy: true }))
        ));
    }

    #[test]
    fn start_emits_run_check_for_a_leaf_step() {
        let mut e = Executor::new(graph(
            r#"
schema_version = 1

[step.tools]
check = "command -v uv"
"#,
        ));
        let actions = e.handle(Event::Start);
        assert_eq!(actions, vec![Action::RunCheck("tools".into())]);
    }
}
