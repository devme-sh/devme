//! TUI state model. Pure data: absorb daemon messages, expose what the
//! renderer needs to draw, route key events to selection / scroll updates.

use std::collections::{HashMap, VecDeque};

use base64::Engine;
use devme_core::{ServerMessage, ServiceSnapshot, StepSnapshot};

/// Per-service log cap inside the TUI. The daemon's ring is the source of
/// truth (~2000 lines); the TUI keeps a smaller working buffer so even on a
/// chatty service the viewport draw stays cheap.
const TUI_LOG_CAP: usize = 1000;

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
    instance_label: String,
    logs: HashMap<String, VecDeque<String>>,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            services: Vec::new(),
            steps: Vec::new(),
            selected_service: None,
            focus: Focus::Tabs,
            instance_label: String::new(),
            logs: HashMap::new(),
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

    /// Human-friendly label for this instance — e.g. the repo basename.
    /// Surfaced in the sidebar.
    pub fn instance_label(&self) -> &str {
        &self.instance_label
    }

    pub fn set_instance_label(&mut self, label: impl Into<String>) {
        self.instance_label = label.into();
    }

    /// The currently-focused service, if any.
    pub fn selected_service(&self) -> Option<&ServiceSnapshot> {
        self.selected_service.and_then(|i| self.services.get(i))
    }

    /// Buffered log lines for `service`, oldest first. Empty if nothing has
    /// arrived for that service yet.
    pub fn service_logs(&self, service: &str) -> &VecDeque<String> {
        static EMPTY: std::sync::OnceLock<VecDeque<String>> = std::sync::OnceLock::new();
        self.logs
            .get(service)
            .unwrap_or_else(|| EMPTY.get_or_init(VecDeque::new))
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
            ServerMessage::LogChunk { service, bytes, .. } => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(bytes.as_bytes())
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok());
                if let Some(text) = decoded {
                    let buf = self
                        .logs
                        .entry(service)
                        .or_insert_with(|| VecDeque::with_capacity(TUI_LOG_CAP.min(64)));
                    // LogChunk payloads are single lines from the PTY reader,
                    // but split defensively in case that ever changes.
                    for line in text.split('\n') {
                        let line = line.trim_end_matches('\r');
                        if line.is_empty() {
                            continue;
                        }
                        if buf.len() == TUI_LOG_CAP {
                            buf.pop_front();
                        }
                        buf.push_back(line.to_string());
                    }
                }
            }
            // Notice, Error, Goodbye don't yet move the model.
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
    fn log_chunks_append_to_per_service_buffer() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["db", "api"]));
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());

        s.apply(ServerMessage::LogChunk {
            service: "db".into(),
            bytes: enc("postgres ready"),
            ts: 1,
        });
        s.apply(ServerMessage::LogChunk {
            service: "api".into(),
            bytes: enc("listening on 8080"),
            ts: 2,
        });
        s.apply(ServerMessage::LogChunk {
            service: "db".into(),
            bytes: enc("connection accepted"),
            ts: 3,
        });

        let db_logs: Vec<_> = s.service_logs("db").iter().cloned().collect();
        let api_logs: Vec<_> = s.service_logs("api").iter().cloned().collect();
        assert_eq!(db_logs, vec!["postgres ready", "connection accepted"]);
        assert_eq!(api_logs, vec!["listening on 8080"]);
        assert!(s.service_logs("ghost").is_empty());
    }

    #[test]
    fn log_buffer_drops_oldest_when_capacity_reached() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["svc"]));
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
        // Push more than TUI_LOG_CAP lines.
        for i in 0..(super::TUI_LOG_CAP + 5) {
            s.apply(ServerMessage::LogChunk {
                service: "svc".into(),
                bytes: enc(&format!("line {i}")),
                ts: i as u64,
            });
        }
        let buf: Vec<_> = s.service_logs("svc").iter().cloned().collect();
        assert_eq!(buf.len(), super::TUI_LOG_CAP);
        // Oldest survivor should be line 5 (lines 0..=4 evicted).
        assert_eq!(buf.first().unwrap(), "line 5");
        assert_eq!(buf.last().unwrap(), &format!("line {}", super::TUI_LOG_CAP + 4));
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
