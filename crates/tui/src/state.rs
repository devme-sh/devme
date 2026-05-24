//! TUI state model. Pure data: absorb daemon messages, expose what the
//! renderer needs to draw, route key events to selection / scroll updates.

use devme_core::{ServerMessage, ServiceSnapshot, StepSnapshot};

/// Which top-level focus the user has. Drives keybinding behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// Sidebar — choosing among worktrees.
    Sidebar,
    /// Top tabs — choosing among services within the current worktree.
    Tabs,
    /// Main viewport — the log pane scrolls/searches.
    Viewport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiState {
    services: Vec<ServiceSnapshot>,
    steps: Vec<StepSnapshot>,
    selected_service: Option<usize>,
    focus: Focus,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            services: Vec::new(),
            steps: Vec::new(),
            selected_service: None,
            focus: Focus::Tabs,
        }
    }
}

impl TuiState {
    pub fn services(&self) -> &[ServiceSnapshot] {
        &self.services
    }

    pub fn steps(&self) -> &[StepSnapshot] {
        &self.steps
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// The currently-focused service, if any.
    pub fn selected_service(&self) -> Option<&ServiceSnapshot> {
        self.selected_service.and_then(|i| self.services.get(i))
    }

    /// Absorb a [`ServerMessage`] coming off the IPC stream.
    pub fn apply(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Subscribed { services, steps } => {
                self.services = services;
                self.steps = steps;
                self.selected_service = if self.services.is_empty() { None } else { Some(0) };
            }
            ServerMessage::StatusUpdate { service, state, .. } => {
                if let Some(s) = self.services.iter_mut().find(|s| s.name == service) {
                    s.state = state;
                }
            }
            ServerMessage::StepStatusUpdate { step, state } => {
                if let Some(s) = self.steps.iter_mut().find(|s| s.name == step) {
                    s.state = state;
                }
            }
            // LogChunk, Notice, Error, Goodbye don't yet move the model —
            // routed by the runner directly to the log viewport.
            _ => {}
        }
    }

    /// Move selection within the focused pane.
    pub fn select_next(&mut self) {
        if matches!(self.focus, Focus::Tabs) && !self.services.is_empty() {
            let next = match self.selected_service {
                Some(i) => (i + 1) % self.services.len(),
                None => 0,
            };
            self.selected_service = Some(next);
        }
    }

    pub fn select_prev(&mut self) {
        if matches!(self.focus, Focus::Tabs) && !self.services.is_empty() {
            let prev = match self.selected_service {
                Some(0) | None => self.services.len() - 1,
                Some(i) => i - 1,
            };
            self.selected_service = Some(prev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devme_core::{ServiceState, StepState};

    fn svc(name: &str) -> ServiceSnapshot {
        ServiceSnapshot {
            name: name.into(),
            state: ServiceState::Stopped,
            pid: None,
            port: None,
            restart_count: 0,
        }
    }

    fn snapshot_msg(names: &[&str]) -> ServerMessage {
        ServerMessage::Subscribed {
            services: names.iter().map(|n| svc(n)).collect(),
            steps: vec![],
        }
    }

    #[test]
    fn default_state_has_no_selection() {
        let s = TuiState::default();
        assert!(s.services().is_empty());
        assert!(s.selected_service().is_none());
    }

    #[test]
    fn subscribed_message_populates_services_and_selects_first() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["db", "backend"]));
        assert_eq!(s.services().len(), 2);
        assert_eq!(s.selected_service().unwrap().name, "db");
    }

    #[test]
    fn empty_subscribed_clears_selection() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["db"]));
        s.apply(snapshot_msg(&[]));
        assert!(s.selected_service().is_none());
    }

    #[test]
    fn status_update_replaces_service_state() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["db"]));
        s.apply(ServerMessage::StatusUpdate {
            service: "db".into(),
            state: ServiceState::Running {
                degraded: false,
                started_without: vec![],
            },
            pid: Some(1234),
            port: Some(5432),
            restart_count: 0,
        });
        assert!(matches!(
            s.services()[0].state,
            ServiceState::Running { .. }
        ));
    }

    #[test]
    fn status_update_for_unknown_service_is_ignored() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["db"]));
        s.apply(ServerMessage::StatusUpdate {
            service: "ghost".into(),
            state: ServiceState::Running {
                degraded: false,
                started_without: vec![],
            },
            pid: None,
            port: None,
            restart_count: 0,
        });
        // No new service was added; "db" is unchanged.
        assert_eq!(s.services().len(), 1);
        assert!(matches!(s.services()[0].state, ServiceState::Stopped));
    }

    #[test]
    fn step_status_update_replaces_step_state() {
        let mut s = TuiState::default();
        s.apply(ServerMessage::Subscribed {
            services: vec![],
            steps: vec![StepSnapshot {
                name: "tools".into(),
                state: StepState::Unknown,
            }],
        });
        s.apply(ServerMessage::StepStatusUpdate {
            step: "tools".into(),
            state: StepState::Passed,
        });
        assert_eq!(s.steps()[0].state, StepState::Passed);
    }

    #[test]
    fn select_next_wraps_around_at_end() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["a", "b", "c"]));
        s.select_next();
        s.select_next();
        assert_eq!(s.selected_service().unwrap().name, "c");
        s.select_next(); // wraps to "a"
        assert_eq!(s.selected_service().unwrap().name, "a");
    }

    #[test]
    fn select_prev_wraps_around_at_start() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["a", "b", "c"]));
        s.select_prev();
        assert_eq!(s.selected_service().unwrap().name, "c");
    }

    #[test]
    fn select_does_nothing_when_focus_isnt_tabs() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["a", "b"]));
        // Manually shift focus; the next() call should be a no-op now.
        s = TuiState {
            focus: Focus::Sidebar,
            ..s
        };
        s.select_next();
        assert_eq!(s.selected_service().unwrap().name, "a");
    }
}
