//! TUI state model. Pure data: absorb daemon messages, expose what the
//! renderer needs to draw, route key events to selection / scroll updates.
//!
//! Multi-stack ready: the state is `Vec<InstanceData>`. Single-daemon use
//! has one entry; future socket-discovery work appends more. Accessors
//! return the *currently-selected* instance's data, so the renderer can
//! treat the TUI as if it were single-stack and let the navigation layer
//! handle which stack is in focus.

use std::collections::{HashMap, VecDeque};

use base64::Engine;
use devme_core::{InstanceInfo, ServerMessage, ServiceSnapshot, StepSnapshot};

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

/// One daemon's worth of TUI state. Selected by `selected_instance`; all
/// the per-stack mutations route here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceData {
    pub info: InstanceInfo,
    pub services: Vec<ServiceSnapshot>,
    pub steps: Vec<StepSnapshot>,
    pub selected_service: Option<usize>,
    pub logs: HashMap<String, VecDeque<String>>,
    /// How many lines from the bottom we're scrolled back, per service.
    /// 0 = pinned to tail; non-zero freezes the viewport so new lines
    /// accumulate behind the user without disturbing what's on screen.
    pub log_scroll: HashMap<String, usize>,
}

impl InstanceData {
    fn new(info: InstanceInfo) -> Self {
        Self {
            info,
            services: Vec::new(),
            steps: Vec::new(),
            selected_service: None,
            logs: HashMap::new(),
            log_scroll: HashMap::new(),
        }
    }
}

/// Repo-scoped shared daemon state. Shows as a separate sidebar entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SharedData {
    /// Instance id of the shared daemon (e.g. `shared::<repo_id>`).
    id: Option<String>,
    services: Vec<ServiceSnapshot>,
    logs: HashMap<String, VecDeque<String>>,
    log_scroll: HashMap<String, usize>,
    selected_service: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiState {
    instances: Vec<InstanceData>,
    selected_instance: Option<usize>,
    /// Shared (repo-scoped) daemon state with its own sidebar row.
    shared: SharedData,
    /// When true, the sidebar focus is on the "shared" row and the main pane
    /// shows repo-scoped services.
    shared_selected: bool,
    focus: Focus,
    help_visible: bool,
    /// Last-known log viewport height (rows). Updated each render pass so
    /// scroll clamping accounts for how many lines the user can see at once.
    viewport_height: usize,
    /// Full-screen log view for text selection. Mouse capture is disabled
    /// so the terminal handles selection natively.
    copy_mode: bool,
    /// Whether the skill install hint is eligible to show in the footer.
    /// Checked once at startup from the backoff state file and config.
    skill_hint_eligible: bool,
    /// Instant when the TUI started, used to time hint rotation.
    started_at: std::time::Instant,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            instances: Vec::new(),
            selected_instance: None,
            shared: SharedData::default(),
            shared_selected: false,
            focus: Focus::Tabs,
            help_visible: false,
            viewport_height: 20,
            copy_mode: false,
            skill_hint_eligible: check_skill_hint_eligible(),
            started_at: std::time::Instant::now(),
        }
    }
}

fn check_skill_hint_eligible() -> bool {
    let cfg = devme_config::GlobalConfig::load();
    if cfg.get("hints.skills") == Some("false".into()) {
        return false;
    }
    let config_dir = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        std::path::PathBuf::from(xdg).join("devme")
    } else if let Some(home) = std::env::var_os("HOME") {
        std::path::PathBuf::from(home).join(".config").join("devme")
    } else {
        return false;
    };
    let state_file = config_dir.join("skills-hint-state");
    match std::fs::read_to_string(&state_file) {
        Ok(contents) => {
            let count: u32 = contents.lines().next().and_then(|s| s.parse().ok()).unwrap_or(0);
            count < 4
        }
        Err(_) => true,
    }
}

/// Returns true if the instance id belongs to the shared supervisor.
fn is_shared_id(id: &str) -> bool {
    id.starts_with("shared::")
}

impl TuiState {
    // ── instance navigation ─────────────────────────────────────────────

    /// The currently selected InstanceData, if any.
    pub fn current_instance(&self) -> Option<&InstanceData> {
        self.selected_instance.and_then(|i| self.instances.get(i))
    }

    fn current_instance_mut(&mut self) -> Option<&mut InstanceData> {
        self.selected_instance
            .and_then(|i| self.instances.get_mut(i))
    }

    /// Find an instance by id. Returns its index.
    fn find_instance(&self, id: &str) -> Option<usize> {
        self.instances.iter().position(|i| i.info.id == id)
    }

    /// Ensure an InstanceData exists for `info` and return its index. New
    /// instances become the selected one only if no instance is selected.
    fn upsert_instance(&mut self, info: InstanceInfo) -> usize {
        match self.find_instance(&info.id) {
            Some(idx) => {
                // Refresh the info in case label/cwd changed (worktree
                // renamed, daemon restarted with new identity).
                self.instances[idx].info = info;
                idx
            }
            None => {
                self.instances.push(InstanceData::new(info));
                let idx = self.instances.len() - 1;
                if self.selected_instance.is_none() {
                    self.selected_instance = Some(idx);
                }
                idx
            }
        }
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn copy_mode(&self) -> bool {
        self.copy_mode
    }

    /// True when the sidebar "shared" row is focused.
    pub fn shared_selected(&self) -> bool {
        self.shared_selected
    }

    /// The repo-scoped services (from the shared supervisor).
    pub fn shared_services(&self) -> &[ServiceSnapshot] {
        &self.shared.services
    }

    pub fn enter_copy_mode(&mut self) {
        self.copy_mode = true;
    }

    pub fn exit_copy_mode(&mut self) {
        self.copy_mode = false;
    }

    pub fn help_visible(&self) -> bool {
        self.help_visible
    }

    #[cfg(test)]
    pub fn suppress_skill_hint(&mut self) {
        self.skill_hint_eligible = false;
    }

    /// True when the footer should show the skill hint instead of keybindings.
    /// Alternates: 8s hint, 30s keybindings, repeating.
    pub fn show_skill_hint(&self) -> bool {
        if !self.skill_hint_eligible {
            return false;
        }
        let elapsed = self.started_at.elapsed().as_secs();
        let cycle = 38; // 8s hint + 30s keys
        let pos = elapsed % cycle;
        pos < 8
    }

    pub fn toggle_help(&mut self) {
        self.help_visible = !self.help_visible;
    }

    pub fn hide_help(&mut self) {
        self.help_visible = false;
    }

    // ── sidebar labels (back-compat with single-stack callers) ──────────

    /// Human-friendly label for the currently selected instance, or "" if
    /// none. Surfaced in the sidebar header.
    pub fn instance_label(&self) -> &str {
        self.current_instance()
            .map(|i| i.info.label.as_str())
            .unwrap_or("")
    }

    /// All instance labels, in sidebar order. Mostly for the renderer.
    pub fn instances(&self) -> Vec<&str> {
        self.instances.iter().map(|i| i.info.label.as_str()).collect()
    }

    pub fn selected_instance_index(&self) -> Option<usize> {
        self.selected_instance
    }

    /// Replace the instance list with a single label and select it. Used by
    /// `devme-tui`'s startup path to pre-populate the sidebar from cwd
    /// before the daemon's own InstanceInfo arrives via `Subscribed`.
    pub fn set_instance_label(&mut self, label: impl Into<String>) {
        let label = label.into();
        let info = InstanceInfo {
            id: format!("local::{label}"),
            label,
            cwd: ".".into(),
        };
        self.instances = vec![InstanceData::new(info)];
        self.selected_instance = Some(0);
    }

    /// Append a new instance to the sidebar list. The first call also
    /// selects it.
    pub fn add_instance(&mut self, label: impl Into<String>) {
        let label = label.into();
        let info = InstanceInfo {
            id: format!("local::{label}"),
            label,
            cwd: ".".into(),
        };
        self.instances.push(InstanceData::new(info));
        if self.selected_instance.is_none() {
            self.selected_instance = Some(0);
        }
    }

    /// Add a placeholder for a worktree the autospawner has seen but for
    /// which no daemon has bound yet (typically because the worktree has
    /// no `devme.toml`). The `id` must match the future
    /// `paths::instance_id(path)` so the real `Subscribed` message — when
    /// it eventually arrives — upserts the same row instead of adding a
    /// duplicate.
    pub fn add_placeholder_instance(
        &mut self,
        id: impl Into<String>,
        label: impl Into<String>,
        cwd: impl Into<String>,
    ) {
        let id = id.into();
        if self.find_instance(&id).is_some() {
            return;
        }
        let info = InstanceInfo { id, label: label.into(), cwd: cwd.into() };
        self.instances.push(InstanceData::new(info));
        if self.selected_instance.is_none() {
            self.selected_instance = Some(0);
        }
    }

    /// True if the currently-selected instance has no services attached
    /// (i.e. no daemon ever responded with a `Subscribed`). The renderer
    /// uses this to show a friendlier "waiting for devme.toml" message
    /// instead of an empty tab row.
    pub fn current_instance_is_placeholder(&self) -> bool {
        if self.shared_selected {
            return self.shared.services.is_empty();
        }
        self.current_instance()
            .map(|i| i.services.is_empty() && i.steps.is_empty())
            .unwrap_or(false)
    }

    /// Filesystem cwd of the currently-selected instance. The renderer
    /// uses this to surface "drop a devme.toml here" hints for
    /// placeholders.
    pub fn current_instance_cwd(&self) -> &str {
        self.current_instance().map(|i| i.info.cwd.as_str()).unwrap_or("")
    }

    // ── proxies to the selected instance ────────────────────────────────

    /// Instance services excluding external stubs for repo-scoped services.
    fn instance_only_services(&self) -> Vec<&ServiceSnapshot> {
        let Some(inst) = self.current_instance() else {
            return Vec::new();
        };
        inst.services
            .iter()
            .filter(|s| !self.shared.services.iter().any(|sh| sh.name == s.name))
            .collect()
    }

    /// Services visible in the main pane tabs. When the shared row is
    /// selected, returns only repo-scoped services. Otherwise returns only
    /// instance-local services (repo-scoped stubs filtered out).
    pub fn services(&self) -> Vec<ServiceSnapshot> {
        if self.shared_selected {
            self.shared.services.clone()
        } else {
            self.instance_only_services().into_iter().cloned().collect()
        }
    }

    fn active_service_count(&self) -> usize {
        if self.shared_selected {
            self.shared.services.len()
        } else {
            self.instance_only_services().len()
        }
    }

    fn active_selected_service_idx(&self) -> Option<usize> {
        if self.shared_selected {
            self.shared.selected_service
        } else {
            self.current_instance()?.selected_service
        }
    }

    fn set_active_selected_service(&mut self, idx: Option<usize>) {
        if self.shared_selected {
            self.shared.selected_service = idx;
        } else if let Some(inst) = self.current_instance_mut() {
            inst.selected_service = idx;
        }
    }

    pub fn steps(&self) -> &[StepSnapshot] {
        self.current_instance().map(|i| i.steps.as_slice()).unwrap_or(&[])
    }

    pub fn selected_service(&self) -> Option<&ServiceSnapshot> {
        let idx = self.active_selected_service_idx()?;
        let svcs = self.services();
        if idx < svcs.len() {
            if self.shared_selected {
                self.shared.services.get(idx)
            } else {
                let inst_only = self.instance_only_services();
                inst_only.get(idx).copied()
            }
        } else {
            None
        }
    }

    /// Buffered log lines for `service` — checks both instance and shared logs.
    pub fn service_logs(&self, service: &str) -> &VecDeque<String> {
        static EMPTY: std::sync::OnceLock<VecDeque<String>> = std::sync::OnceLock::new();
        if self.shared_selected {
            self.shared.logs.get(service)
        } else {
            self.current_instance()
                .and_then(|i| i.logs.get(service))
                .or_else(|| self.shared.logs.get(service))
        }
        .unwrap_or_else(|| EMPTY.get_or_init(VecDeque::new))
    }

    /// Lines-from-bottom scroll offset for the selected service. 0 = tail.
    pub fn log_scroll_offset(&self) -> usize {
        let Some(svc) = self.selected_service() else {
            return 0;
        };
        let name = &svc.name;
        if self.shared_selected {
            self.shared.log_scroll.get(name).copied().unwrap_or(0)
        } else {
            self.current_instance()
                .and_then(|i| i.log_scroll.get(name))
                .copied()
                .unwrap_or(0)
        }
    }

    fn set_scroll_for_selected(&mut self, value: usize) {
        let Some(svc) = self.selected_service() else {
            return;
        };
        let name = svc.name.clone();
        if self.shared_selected {
            if value == 0 {
                self.shared.log_scroll.remove(&name);
            } else {
                self.shared.log_scroll.insert(name, value);
            }
        } else {
            let inst = self.current_instance_mut().unwrap();
            if value == 0 {
                inst.log_scroll.remove(&name);
            } else {
                inst.log_scroll.insert(name, value);
            }
        }
    }

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

    /// The visible log lines for the selected service (what's currently in
    /// the viewport). Returns an empty vec if nothing is selected.
    pub fn visible_log_lines(&self) -> Vec<&str> {
        let Some(svc) = self.selected_service() else {
            return Vec::new();
        };
        let logs = self.service_logs(&svc.name);
        if logs.is_empty() {
            return Vec::new();
        }
        let viewport = self.viewport_height;
        let offset = self.log_scroll_offset();
        let end = logs.len().saturating_sub(offset);
        let start = end.saturating_sub(viewport);
        logs.iter().skip(start).take(end - start).map(|s| s.as_str()).collect()
    }

    /// All log lines for the selected service.
    pub fn all_log_lines(&self) -> Vec<&str> {
        let Some(svc) = self.selected_service() else {
            return Vec::new();
        };
        self.service_logs(&svc.name).iter().map(|s| s.as_str()).collect()
    }

    /// Update the viewport height from the last render pass. Called by the
    /// renderer so scroll clamping stays in sync with the actual terminal size.
    pub fn set_viewport_height(&mut self, h: usize) {
        self.viewport_height = h.max(1);
    }

    fn max_scroll_for_selected(&self) -> usize {
        let Some(svc) = self.selected_service() else {
            return 0;
        };
        let name = &svc.name;
        let total = if self.shared_selected {
            self.shared.logs.get(name).map(|l| l.len()).unwrap_or(0)
        } else {
            self.current_instance()
                .and_then(|i| i.logs.get(name))
                .map(|l| l.len())
                .unwrap_or(0)
        };
        total.saturating_sub(self.viewport_height)
    }

    /// Absorb a [`ServerMessage`] tagged with the id of the daemon that
    /// sent it. Messages from the shared daemon (`shared::*`) are routed to
    /// [`SharedData`]; everything else goes to the matching [`InstanceData`].
    pub fn apply_from(&mut self, source_id: &str, msg: ServerMessage) {
        if is_shared_id(source_id) {
            self.apply_shared(source_id, msg);
            return;
        }
        match msg {
            ServerMessage::Subscribed { services, steps, instance } => {
                let idx = self.upsert_instance(instance);
                let inst = &mut self.instances[idx];
                inst.services = services;
                inst.steps = steps;
                if inst.selected_service.is_none() && !inst.services.is_empty() {
                    inst.selected_service = Some(0);
                }
            }
            ServerMessage::StatusUpdate { service, state, .. } => {
                if let Some(idx) = self.find_instance(source_id)
                    && let Some(s) = self.instances[idx]
                        .services
                        .iter_mut()
                        .find(|s| s.name == service)
                {
                    s.state = state;
                }
            }
            ServerMessage::StepStatusUpdate { step, state } => {
                if let Some(idx) = self.find_instance(source_id)
                    && let Some(s) = self.instances[idx].steps.iter_mut().find(|s| s.name == step)
                {
                    s.state = state;
                }
            }
            ServerMessage::LogChunk { service, bytes, .. } => {
                Self::apply_log_chunk_to(
                    self.find_instance(source_id).map(|idx| &mut self.instances[idx]),
                    &service,
                    &bytes,
                );
            }
            _ => {}
        }
    }

    fn apply_shared(&mut self, _source_id: &str, msg: ServerMessage) {
        match msg {
            ServerMessage::Subscribed { services, instance, .. } => {
                self.shared.id = Some(instance.id);
                self.shared.services = services;
                if self.shared.selected_service.is_none() && !self.shared.services.is_empty() {
                    self.shared.selected_service = Some(0);
                }
            }
            ServerMessage::StatusUpdate { service, state, .. } => {
                if let Some(s) = self.shared.services.iter_mut().find(|s| s.name == service) {
                    s.state = state;
                }
            }
            ServerMessage::LogChunk { service, bytes, .. } => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(bytes.as_bytes())
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok());
                let Some(text) = decoded else {
                    return;
                };
                let buf = self.shared
                    .logs
                    .entry(service.clone())
                    .or_insert_with(|| VecDeque::with_capacity(TUI_LOG_CAP.min(64)));
                let len_before = buf.len();
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
                let net_growth = len_after.saturating_sub(len_before);
                if pushed > 0
                    && net_growth > 0
                    && let Some(off) = self.shared.log_scroll.get_mut(&service)
                    && *off > 0
                {
                    *off = off.saturating_add(net_growth).min(len_after);
                }
            }
            _ => {}
        }
    }

    fn apply_log_chunk_to(inst: Option<&mut InstanceData>, service: &str, bytes: &str) {
        let Some(inst) = inst else {
            return;
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(bytes.as_bytes())
            .ok()
            .and_then(|b| String::from_utf8(b).ok());
        let Some(text) = decoded else {
            return;
        };
        let buf = inst
            .logs
            .entry(service.to_string())
            .or_insert_with(|| VecDeque::with_capacity(TUI_LOG_CAP.min(64)));
        let len_before = buf.len();
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
        let net_growth = len_after.saturating_sub(len_before);
        if pushed > 0
            && net_growth > 0
            && let Some(off) = inst.log_scroll.get_mut(service)
            && *off > 0
        {
            *off = off.saturating_add(net_growth).min(len_after);
        }
    }

    /// Single-source convenience for tests and the preview example. Looks
    /// up the source id from `Subscribed` if present, else falls back to
    /// the currently-selected instance.
    pub fn apply(&mut self, msg: ServerMessage) {
        let source_id = match &msg {
            ServerMessage::Subscribed { instance, .. } => instance.id.clone(),
            _ => self
                .current_instance()
                .map(|i| i.info.id.clone())
                .unwrap_or_default(),
        };
        self.apply_from(&source_id, msg);
    }

    /// True if an InstanceData with this id is in the sidebar.
    pub fn has_instance(&self, id: &str) -> bool {
        self.find_instance(id).is_some()
    }

    /// Ids of instances that have at least one service — i.e. whose
    /// daemon has responded with a `Subscribed`. The caller uses this to
    /// drive "send Start once" semantics; placeholders (no daemon yet)
    /// are skipped. Includes the shared daemon if it has services.
    pub fn attached_instance_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.instances
            .iter()
            .filter(|i| !i.services.is_empty())
            .map(|i| i.info.id.clone())
            .collect();
        if !self.shared.services.is_empty()
            && let Some(id) = &self.shared.id {
                ids.push(id.clone());
            }
        ids
    }

    /// Move selection to the instance with this id, if it exists.
    pub fn select_instance_by_id(&mut self, id: &str) {
        if let Some(idx) = self.find_instance(id) {
            self.selected_instance = Some(idx);
        }
    }

    /// `(instance_id, service_name)` of the currently-selected service, for
    /// routing commands back to the right daemon. Returns the shared daemon's
    /// id when viewing repo-scoped services.
    pub fn selected_instance_and_service(&self) -> Option<(String, String)> {
        let svc = self.selected_service()?;
        let name = svc.name.clone();
        if self.shared_selected {
            let id = self.shared.id.clone()?;
            Some((id, name))
        } else {
            let inst = self.current_instance()?;
            Some((inst.info.id.clone(), name))
        }
    }

    /// Horizontal navigation across service tabs (`h`/`l` / `←`/`→`).
    pub fn select_next_service(&mut self) {
        let total = self.active_service_count();
        if total == 0 {
            return;
        }
        let next = match self.active_selected_service_idx() {
            Some(i) => (i + 1) % total,
            None => 0,
        };
        self.set_active_selected_service(Some(next));
    }

    pub fn select_prev_service(&mut self) {
        let total = self.active_service_count();
        if total == 0 {
            return;
        }
        let prev = match self.active_selected_service_idx() {
            Some(0) | None => total - 1,
            Some(i) => i - 1,
        };
        self.set_active_selected_service(Some(prev));
    }

    /// Vertical navigation through the sidebar. The "shared" row (if present)
    /// appears after all instances. Wraps around.
    pub fn select_next_instance(&mut self) {
        let has_shared = !self.shared.services.is_empty();
        let inst_count = self.instances.len();
        if inst_count == 0 && !has_shared {
            return;
        }
        if self.shared_selected {
            // Wrap from shared → first instance (or stay if no instances)
            if inst_count > 0 {
                self.shared_selected = false;
                self.selected_instance = Some(0);
            }
        } else {
            let cur = self.selected_instance.unwrap_or(0);
            if cur + 1 < inst_count {
                self.selected_instance = Some(cur + 1);
            } else if has_shared {
                self.shared_selected = true;
            } else {
                // Wrap to first instance
                self.selected_instance = Some(0);
            }
        }
    }

    pub fn select_prev_instance(&mut self) {
        let has_shared = !self.shared.services.is_empty();
        let inst_count = self.instances.len();
        if inst_count == 0 && !has_shared {
            return;
        }
        if self.shared_selected {
            // Move up to last instance
            if inst_count > 0 {
                self.shared_selected = false;
                self.selected_instance = Some(inst_count - 1);
            }
        } else {
            let cur = self.selected_instance.unwrap_or(0);
            if cur > 0 {
                self.selected_instance = Some(cur - 1);
            } else if has_shared {
                self.shared_selected = true;
            } else {
                // Wrap to last instance
                self.selected_instance = Some(inst_count - 1);
            }
        }
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
        // Default viewport_height is 20, so max scroll = 50 - 20 = 30.
        assert_eq!(s.log_scroll_offset(), 30);
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
        // 5 lines, viewport 20 → all lines fit, max scroll = 0.
        assert_eq!(s.log_scroll_offset(), 0, "should clamp: all lines fit viewport");
    }

    #[test]
    fn log_scroll_is_independent_per_service() {
        let mut s = TuiState::default();
        s.set_viewport_height(10);
        s.apply(snapshot_msg(&["a", "b"]));
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
        for n in 0..30 {
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
        // Multi-instance semantics: each instance carries its own
        // services + selected_service. Switching between instances must
        // not reset the per-instance service selection.
        let mut s = TuiState::default();

        let info_a = InstanceInfo {
            id: "a".into(),
            label: "repo-a".into(),
            cwd: "/a".into(),
        };
        let info_b = InstanceInfo {
            id: "b".into(),
            label: "repo-b".into(),
            cwd: "/b".into(),
        };
        s.apply(ServerMessage::Subscribed {
            instance: info_a,
            services: vec![svc("api"), svc("db")],
            steps: vec![],
        });
        s.apply(ServerMessage::Subscribed {
            instance: info_b,
            services: vec![svc("api"), svc("db")],
            steps: vec![],
        });

        // Both instances exist; we're still on the first (repo-a).
        assert_eq!(s.instance_label(), "repo-a");
        s.select_next_service();
        assert_eq!(s.selected_service().unwrap().name, "db");

        // Switch to repo-b — its own selected_service starts at index 0.
        s.select_next_instance();
        assert_eq!(s.instance_label(), "repo-b");
        assert_eq!(s.selected_service().unwrap().name, "api");

        // Switch back — repo-a's selection survived.
        s.select_prev_instance();
        assert_eq!(s.instance_label(), "repo-a");
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
    fn apply_from_routes_log_chunks_to_the_source_instance_only() {
        // Two instances subscribed; log chunks tagged with each instance's
        // id must land in that instance's buffer, not the currently-
        // selected one.
        let mut s = TuiState::default();
        let a = InstanceInfo { id: "A".into(), label: "a".into(), cwd: "/a".into() };
        let b = InstanceInfo { id: "B".into(), label: "b".into(), cwd: "/b".into() };
        s.apply_from(
            "A",
            ServerMessage::Subscribed {
                instance: a.clone(),
                services: vec![svc("api")],
                steps: vec![],
            },
        );
        s.apply_from(
            "B",
            ServerMessage::Subscribed {
                instance: b.clone(),
                services: vec![svc("api")],
                steps: vec![],
            },
        );
        // Even though instance "B" is the second one added, route a log
        // chunk tagged from "A" — it must land in A.
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
        s.apply_from(
            "A",
            ServerMessage::LogChunk {
                service: "api".into(),
                bytes: enc("only for A"),
                ts: 0,
            },
        );

        // Switch selection to A, read its logs — should see the line.
        s.select_instance_by_id("A");
        let a_logs: Vec<_> = s.service_logs("api").iter().cloned().collect();
        assert_eq!(a_logs, vec!["only for A"]);

        // Switch selection to B, read its logs — should be empty.
        s.select_instance_by_id("B");
        let b_logs: Vec<_> = s.service_logs("api").iter().cloned().collect();
        assert!(b_logs.is_empty(), "log chunk leaked to B: {b_logs:?}");
    }

    #[test]
    fn selected_instance_and_service_returns_id_and_service_name() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["db", "api"]));
        let (id, name) = s.selected_instance_and_service().unwrap();
        assert_eq!(id, "test-id");
        assert_eq!(name, "db");
        s.select_next_service();
        let (_, name) = s.selected_instance_and_service().unwrap();
        assert_eq!(name, "api");
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
