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
    /// Workspaces/worktrees visible in the sidebar. v1 has just one (the
    /// local instance), but the navigation model is set up for future
    /// multi-instance support (e.g. shared-supervisor or a TUI attached to
    /// multiple worktrees at once).
    instances: Vec<String>,
    selected_instance: Option<usize>,
    logs: HashMap<String, VecDeque<String>>,
    /// How many lines from the bottom we're scrolled back, per service.
    /// 0 = pinned to the tail; a non-zero value freezes the viewport so new
    /// lines arrive into the buffer without disturbing what's on screen.
    log_scroll: HashMap<String, usize>,
    /// Whether the `?` help overlay is showing. Pure UI state.
    help_visible: bool,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            services: Vec::new(),
            steps: Vec::new(),
            selected_service: None,
            focus: Focus::Tabs,
            instances: Vec::new(),
            selected_instance: None,
            logs: HashMap::new(),
            log_scroll: HashMap::new(),
            help_visible: false,
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

    /// Is the `?` help overlay currently shown?
    pub fn help_visible(&self) -> bool {
        self.help_visible
    }

    /// Toggle the help overlay. `?` shows or hides it; `Esc` also hides.
    pub fn toggle_help(&mut self) {
        self.help_visible = !self.help_visible;
    }

    pub fn hide_help(&mut self) {
        self.help_visible = false;
    }

    /// Human-friendly label for the currently selected instance, or "" if
    /// none. Surfaced in the sidebar header.
    pub fn instance_label(&self) -> &str {
        self.selected_instance
            .and_then(|i| self.instances.get(i))
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    /// All known instances, in the order they appear in the sidebar.
    pub fn instances(&self) -> &[String] {
        &self.instances
    }

    pub fn selected_instance_index(&self) -> Option<usize> {
        self.selected_instance
    }

    /// Replace the instance list with a single label and select it. For v1
    /// the TUI shows exactly one instance.
    pub fn set_instance_label(&mut self, label: impl Into<String>) {
        self.instances = vec![label.into()];
        self.selected_instance = Some(0);
    }

    /// Append a new instance to the sidebar list. The first call also
    /// selects it. Reserved for multi-instance support.
    pub fn add_instance(&mut self, label: impl Into<String>) {
        self.instances.push(label.into());
        if self.selected_instance.is_none() {
            self.selected_instance = Some(0);
        }
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

    /// Lines-from-bottom scroll offset for the selected service. 0 = tail.
    pub fn log_scroll_offset(&self) -> usize {
        match self.selected_service() {
            Some(s) => self.log_scroll.get(&s.name).copied().unwrap_or(0),
            None => 0,
        }
    }

    fn set_scroll_for_selected(&mut self, value: usize) {
        if let Some(name) = self.selected_service().map(|s| s.name.clone()) {
            if value == 0 {
                self.log_scroll.remove(&name);
            } else {
                self.log_scroll.insert(name, value);
            }
        }
    }

    /// Scroll the log viewport one screen back (older lines). `viewport`
    /// is the current draw height; the offset is clamped to the buffer
    /// length so we can't scroll past the start.
    pub fn log_page_up(&mut self, viewport: usize) {
        let max = self.max_scroll_for_selected();
        let cur = self.log_scroll_offset();
        let next = cur.saturating_add(viewport.max(1)).min(max);
        self.set_scroll_for_selected(next);
    }

    pub fn log_page_down(&mut self, viewport: usize) {
        let cur = self.log_scroll_offset();
        let next = cur.saturating_sub(viewport.max(1));
        self.set_scroll_for_selected(next);
    }

    /// One line back / forward — for j/k or arrow nudges in the viewport.
    pub fn log_scroll_up(&mut self, lines: usize) {
        let max = self.max_scroll_for_selected();
        let next = self.log_scroll_offset().saturating_add(lines).min(max);
        self.set_scroll_for_selected(next);
    }

    pub fn log_scroll_down(&mut self, lines: usize) {
        let next = self.log_scroll_offset().saturating_sub(lines);
        self.set_scroll_for_selected(next);
    }

    pub fn log_scroll_top(&mut self) {
        let max = self.max_scroll_for_selected();
        self.set_scroll_for_selected(max);
    }

    pub fn log_scroll_bottom(&mut self) {
        self.set_scroll_for_selected(0);
    }

    fn max_scroll_for_selected(&self) -> usize {
        self.selected_service()
            .map(|s| self.service_logs(&s.name).len())
            .unwrap_or(0)
    }

    /// Absorb a [`ServerMessage`] coming off the IPC stream.
    pub fn apply(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Subscribed { services, steps, instance: _ } => {
                // The `instance` field is on the wire so the TUI can route
                // per-daemon state once multi-stack lands (task #21). For
                // now we accept it but leave the sidebar's instance list
                // alone — the runtime caller still drives that explicitly
                // via set_instance_label so user-set labels survive.
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
                        .entry(service.clone())
                        .or_insert_with(|| VecDeque::with_capacity(TUI_LOG_CAP.min(64)));
                    let len_before = buf.len();
                    // LogChunk payloads are single lines from the PTY reader,
                    // but split defensively in case that ever changes.
                    let mut pushed = 0usize;
                    for line in text.split('\n') {
                        let line = line.trim_end_matches('\r');
                        if line.is_empty() {
                            continue;
                        }
                        if buf.len() == TUI_LOG_CAP {
                            buf.pop_front();
                        }
                        buf.push_back(line.to_string());
                        pushed += 1;
                    }
                    let len_after = buf.len();
                    // Stable viewport while scrolled off-tail: when the user
                    // has scrolled up (offset > 0), bump the offset by the
                    // net growth of the buffer so `end = len - offset` keeps
                    // pointing at the same lines. When the buffer is at cap
                    // (eviction == push count) net growth is 0 — the window
                    // slowly drifts in absolute terms, but that's unavoidable
                    // without monotonic line IDs, and the user will hit `G`
                    // long before drifting matters.
                    let net_growth = len_after.saturating_sub(len_before);
                    if pushed > 0
                        && net_growth > 0
                        && let Some(off) = self.log_scroll.get_mut(&service)
                        && *off > 0
                    {
                        *off = off.saturating_add(net_growth).min(len_after);
                    }
                }
            }
            // Notice, Error, Goodbye don't yet move the model.
            _ => {}
        }
    }

    /// Horizontal navigation across service tabs (`h`/`l` / `←`/`→`).
    pub fn select_next_service(&mut self) {
        if self.services.is_empty() {
            return;
        }
        let next = match self.selected_service {
            Some(i) => (i + 1) % self.services.len(),
            None => 0,
        };
        self.selected_service = Some(next);
    }

    pub fn select_prev_service(&mut self) {
        if self.services.is_empty() {
            return;
        }
        let prev = match self.selected_service {
            Some(0) | None => self.services.len() - 1,
            Some(i) => i - 1,
        };
        self.selected_service = Some(prev);
    }

    /// Vertical navigation through the sidebar's instance list
    /// (`j`/`k` / `↑`/`↓`).
    pub fn select_next_instance(&mut self) {
        if self.instances.is_empty() {
            return;
        }
        let next = match self.selected_instance {
            Some(i) => (i + 1) % self.instances.len(),
            None => 0,
        };
        self.selected_instance = Some(next);
    }

    pub fn select_prev_instance(&mut self) {
        if self.instances.is_empty() {
            return;
        }
        let prev = match self.selected_instance {
            Some(0) | None => self.instances.len() - 1,
            Some(i) => i - 1,
        };
        self.selected_instance = Some(prev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devme_core::{InstanceInfo, ServiceState, StepState};

    fn test_instance() -> InstanceInfo {
        InstanceInfo {
            id: "test-id".into(),
            label: "test".into(),
            cwd: "/tmp/test".into(),
        }
    }

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
            instance: test_instance(),
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
            instance: test_instance(),
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
    fn select_next_service_wraps_around_at_end() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["a", "b", "c"]));
        s.select_next_service();
        s.select_next_service();
        assert_eq!(s.selected_service().unwrap().name, "c");
        s.select_next_service(); // wraps to "a"
        assert_eq!(s.selected_service().unwrap().name, "a");
    }

    #[test]
    fn select_prev_service_wraps_around_at_start() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["a", "b", "c"]));
        s.select_prev_service();
        assert_eq!(s.selected_service().unwrap().name, "c");
    }

    #[test]
    fn log_scroll_pages_back_then_returns_to_tail() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["api"]));
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
        for i in 0..50 {
            s.apply(ServerMessage::LogChunk {
                service: "api".into(),
                bytes: enc(&format!("line {i}")),
                ts: i as u64,
            });
        }
        assert_eq!(s.log_scroll_offset(), 0);
        s.log_page_up(10);
        assert_eq!(s.log_scroll_offset(), 10);
        s.log_page_up(10);
        assert_eq!(s.log_scroll_offset(), 20);
        s.log_page_down(15);
        assert_eq!(s.log_scroll_offset(), 5);
        s.log_scroll_top();
        assert_eq!(s.log_scroll_offset(), 50);
        s.log_scroll_bottom();
        assert_eq!(s.log_scroll_offset(), 0);
    }

    #[test]
    fn scrolled_viewport_stays_stable_when_new_logs_arrive() {
        // Once the user has scrolled up, new lines must NOT push the window
        // forward — they should accumulate behind the user. The visible
        // "end" of the logs (logs.len() - scroll_offset) must point to the
        // same line before and after.
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["api"]));
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
        for i in 0..30 {
            s.apply(ServerMessage::LogChunk {
                service: "api".into(),
                bytes: enc(&format!("line {i}")),
                ts: i as u64,
            });
        }
        s.log_page_up(10);
        // Visible "end" line index right after page-up.
        let end_before =
            s.service_logs("api").len() - s.log_scroll_offset();
        let line_before = s
            .service_logs("api")
            .get(end_before - 1)
            .cloned()
            .unwrap();
        for i in 30..40 {
            s.apply(ServerMessage::LogChunk {
                service: "api".into(),
                bytes: enc(&format!("line {i}")),
                ts: i as u64,
            });
        }
        let end_after = s.service_logs("api").len() - s.log_scroll_offset();
        let line_after = s
            .service_logs("api")
            .get(end_after - 1)
            .cloned()
            .unwrap();
        assert_eq!(
            line_before, line_after,
            "viewport bottom drifted while user was scrolled off-tail"
        );
    }

    #[test]
    fn scroll_at_tail_keeps_following_new_logs() {
        // The complement of the above: when offset == 0, new lines should
        // continue to be visible (auto-follow), i.e., offset stays at 0.
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["api"]));
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
        for i in 0..10 {
            s.apply(ServerMessage::LogChunk {
                service: "api".into(),
                bytes: enc(&format!("line {i}")),
                ts: i as u64,
            });
        }
        assert_eq!(s.log_scroll_offset(), 0);
    }

    #[test]
    fn log_scroll_clamps_to_buffer_length() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["api"]));
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
        for i in 0..5 {
            s.apply(ServerMessage::LogChunk {
                service: "api".into(),
                bytes: enc(&format!("line {i}")),
                ts: i as u64,
            });
        }
        s.log_page_up(100);
        assert_eq!(s.log_scroll_offset(), 5, "should clamp to buffer length");
    }

    #[test]
    fn log_scroll_is_independent_per_service() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["a", "b"]));
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
        for n in 0..20 {
            s.apply(ServerMessage::LogChunk {
                service: "a".into(),
                bytes: enc(&format!("a-{n}")),
                ts: n,
            });
            s.apply(ServerMessage::LogChunk {
                service: "b".into(),
                bytes: enc(&format!("b-{n}")),
                ts: n,
            });
        }
        s.log_page_up(5); // scrolling "a"
        assert_eq!(s.log_scroll_offset(), 5);
        s.select_next_service(); // now "b" is selected
        assert_eq!(s.log_scroll_offset(), 0, "b should not inherit a's scroll");
        s.select_prev_service(); // back to "a"
        assert_eq!(s.log_scroll_offset(), 5, "a's scroll should persist");
    }

    #[test]
    fn instance_navigation_wraps_through_added_instances() {
        let mut s = TuiState::default();
        s.add_instance("first");
        s.add_instance("second");
        s.add_instance("third");
        assert_eq!(s.instance_label(), "first");
        s.select_next_instance();
        assert_eq!(s.instance_label(), "second");
        s.select_next_instance();
        s.select_next_instance(); // wraps back to first
        assert_eq!(s.instance_label(), "first");
        s.select_prev_instance(); // wraps to last
        assert_eq!(s.instance_label(), "third");
    }

    #[test]
    fn instance_navigation_is_a_noop_with_a_single_instance() {
        let mut s = TuiState::default();
        s.set_instance_label("only");
        s.select_next_instance();
        s.select_prev_instance();
        assert_eq!(s.instance_label(), "only");
    }

    #[test]
    fn service_and_instance_navigation_are_independent() {
        let mut s = TuiState::default();
        s.add_instance("repo-a");
        s.add_instance("repo-b");
        s.apply(snapshot_msg(&["api", "db"]));
        s.select_next_service();
        assert_eq!(s.selected_service().unwrap().name, "db");
        assert_eq!(s.instance_label(), "repo-a");
        s.select_next_instance();
        assert_eq!(s.instance_label(), "repo-b");
        assert_eq!(s.selected_service().unwrap().name, "db");
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

}
