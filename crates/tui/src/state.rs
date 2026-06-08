//! TUI state model. Pure data: absorb daemon messages, expose what the
//! renderer needs to draw, route key events to selection / scroll updates.
//!
//! Multi-stack ready: the state is `Vec<InstanceData>`. Single-daemon use
//! has one entry; future socket-discovery work appends more. Accessors
//! return the *currently-selected* instance's data, so the renderer can
//! treat the TUI as if it were single-stack and let the navigation layer
//! handle which stack is in focus.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use base64::Engine;
use devme_config::GlobalConfig;
use devme_core::{InstanceInfo, ServerMessage, ServiceSnapshot, ServiceState, StepSnapshot};

use crate::theme::Palette;

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
    /// Commits this worktree's branch is ahead/behind its upstream, refreshed
    /// in the background. `None` until the first git query lands (or when the
    /// branch has no upstream).
    pub git_ahead_behind: Option<(usize, usize)>,
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
            git_ahead_behind: None,
        }
    }
}

/// A transient corner notification — service crashed, came up, etc. Auto-
/// expires after [`TOAST_TTL`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub kind: ToastKind,
    pub title: String,
    pub body: String,
    born: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    /// A service failed or entered a crash loop.
    Failed,
    /// A service became healthy.
    Ready,
    /// Neutral information.
    Info,
}

/// How long a toast stays on screen before the tick loop drops it.
const TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(5);
/// Cap on simultaneously-visible toasts (oldest evicted first).
const MAX_TOASTS: usize = 4;

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

/// Aggregate health of a stack, summarised into a single sidebar status
/// dot. Mirrors the per-service states but collapses them: any failure
/// dominates, then a mix, then all-up, then idle, then "no daemon yet".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackHealth {
    /// Discovered worktree with no daemon bound yet (no services).
    Placeholder,
    /// Every service is up and healthy.
    AllRunning,
    /// Some services up, some not.
    SomeRunning,
    /// Services exist but none are running.
    Idle,
    /// At least one service has failed or is crash-looping.
    Failed,
}

/// One editable row in the in-TUI settings overlay. The overlay is a thin
/// front-end over the same `global.toml` keys `devme config set` writes, so
/// nothing here is TUI-only state — changes persist to disk and apply live.
pub struct SettingDef {
    /// Config key, e.g. `tui.theme`.
    pub key: &'static str,
    pub label: &'static str,
    pub desc: &'static str,
    pub control: SettingControl,
    /// Value shown when the key is unset.
    pub default: &'static str,
}

#[derive(Clone, Copy)]
pub enum SettingControl {
    /// A boolean, stored as the strings "true"/"false".
    Toggle,
    /// One of a fixed set of values.
    Choice(&'static [&'static str]),
}

/// The settings the overlay exposes. A deliberately small set — the rest of
/// `devme config`'s keys (e.g. `docker.daemon`) stay CLI-only.
pub const SETTINGS: &[SettingDef] = &[
    SettingDef {
        key: "tui.theme",
        label: "Theme",
        desc: "Colour palette for the TUI",
        control: SettingControl::Choice(&["mocha", "latte", "auto"]),
        default: "mocha",
    },
    SettingDef {
        key: "hints.skills",
        label: "Skill hint",
        desc: "Show the AI-skill install hint in the footer",
        control: SettingControl::Toggle,
        default: "true",
    },
    SettingDef {
        key: "skill.auto_update",
        label: "Auto-update skill",
        desc: "Refresh the embedded AI skill when devme updates",
        control: SettingControl::Toggle,
        default: "false",
    },
];

/// Live state for the settings overlay: which row is focused and the current
/// value of each setting (parallel to [`SETTINGS`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsState {
    pub cursor: usize,
    pub values: Vec<String>,
}

/// Which flavour of skill prompt the startup modal is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillPrompt {
    /// No devme-managed skill is installed yet — offer to install it.
    Install,
    /// A devme-managed install is out of date — offer to refresh it.
    Update,
}

/// A pending skill prompt, shown as a modal when the TUI starts. For
/// `Install`, no skill is present and the human picks install / global /
/// not-now. For `Update`, a devme-managed install is stale (and
/// `skill.auto_update` is off) and the human picks update / always / not-now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDialog {
    pub kind: SkillPrompt,
    /// Versions for the Update case (`from` → `to`). Empty for Install.
    pub from: String,
    pub to: String,
    /// How many devme-managed installs are stale (Update case).
    pub count: usize,
}

/// What the user can do about a port-conflict crash. Carries the data the
/// event loop needs to carry the action out off-thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortConflictAction {
    /// `docker stop <name>` — graceful, restartable.
    StopContainer(String),
    /// `docker compose -p <project> down` — tear down the whole project.
    ComposeDown(String),
    /// `kill <pids>` — SIGTERM the listening host process(es).
    KillProcess(Vec<u32>),
    /// Leave it; just close the modal.
    Skip,
}

/// One selectable remediation row in the port-conflict modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortConflictOption {
    pub label: String,
    pub action: PortConflictAction,
}

/// A reactive port-conflict modal: a running service crash-looped on
/// `address already in use`. Mirrors the pre-launch picker, but in-session
/// and ratatui-rendered — same Stop / Compose-down / Kill / Skip choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortConflictDialog {
    /// The instance (daemon) that reported the crash — where Restart is sent.
    pub instance_id: String,
    /// The crashing service.
    pub service: String,
    /// The port it couldn't bind.
    pub port: u16,
    /// One-line description of who's holding the port.
    pub holder_desc: String,
    /// Remediation rows; "Skip" is always last.
    pub options: Vec<PortConflictOption>,
    /// Currently-highlighted row.
    pub selected: usize,
}

impl PortConflictDialog {
    /// Build the modal from an identified port holder, deriving the relevant
    /// remediation rows (Stop / Compose-down for a container, Kill for a
    /// process) with "Skip" always last.
    fn from_holder(
        instance_id: String,
        service: String,
        port: u16,
        holder: devme_supervisor::port_preflight::Holder,
    ) -> Self {
        use devme_supervisor::port_preflight::Holder;

        let mut options = Vec::new();
        let holder_desc = match &holder {
            Holder::Container { name, project } => match project {
                Some(p) => format!("container {name} (compose: {p})"),
                None => format!("container {name}"),
            },
            Holder::Process(pids) => pids
                .iter()
                .map(|(pid, n)| match n {
                    Some(n) => format!("{n} ({pid})"),
                    None => format!("pid {pid}"),
                })
                .collect::<Vec<_>>()
                .join(", "),
            Holder::Unknown => "an unknown process".to_string(),
        };

        match holder {
            Holder::Container { name, project } => {
                options.push(PortConflictOption {
                    label: format!("Stop container {name}"),
                    action: PortConflictAction::StopContainer(name),
                });
                if let Some(p) = project {
                    options.push(PortConflictOption {
                        label: format!("Compose down {p} (stops the whole project)"),
                        action: PortConflictAction::ComposeDown(p),
                    });
                }
            }
            Holder::Process(pids) => {
                let label = pids
                    .iter()
                    .map(|(pid, n)| match n {
                        Some(n) => format!("{n} ({pid})"),
                        None => format!("pid {pid}"),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let ids = pids.into_iter().map(|(p, _)| p).collect();
                options.push(PortConflictOption {
                    label: format!("Kill {label}"),
                    action: PortConflictAction::KillProcess(ids),
                });
            }
            Holder::Unknown => {}
        }
        options.push(PortConflictOption {
            label: "Skip".into(),
            action: PortConflictAction::Skip,
        });

        Self { instance_id, service, port, holder_desc, options, selected: 0 }
    }
}

/// True if any of the last few log lines look like a port-already-in-use
/// error. Covers Node (`EADDRINUSE`), Python/Go/Rust/Postgres
/// (`address already in use`).
fn logs_show_addr_in_use(logs: &VecDeque<String>) -> bool {
    logs.iter().rev().take(40).any(|l| {
        let low = l.to_ascii_lowercase();
        low.contains("address already in use") || low.contains("eaddrinuse")
    })
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
    /// Pending stale-skill prompt, if any. Takes modal priority over help.
    skill_dialog: Option<SkillDialog>,
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
    /// Active colour palette (mocha / latte / terminal-detected).
    palette: Palette,
    /// Monotonic animation counter, bumped each tick. Drives the spinner on
    /// starting/restarting services.
    spinner_tick: u64,
    /// Live corner notifications, newest last. Expired on tick.
    toasts: Vec<Toast>,
    /// Index of the first stack row painted in the sidebar (vertical scroll
    /// when the list is taller than the pane).
    sidebar_scroll: usize,
    /// When true the sidebar is hidden, giving the log pane full width.
    sidebar_collapsed: bool,
    /// User global config, loaded once at startup. The settings overlay reads
    /// and writes through this.
    config: GlobalConfig,
    /// The settings overlay, when open. Modal like the help/skill dialogs.
    settings: Option<SettingsState>,
    /// A reactive port-conflict crash detected during ingest, awaiting holder
    /// identification (a blocking docker/lsof probe) in the event loop.
    /// `(instance_id, service, port)`.
    pending_port_conflict: Option<(String, String, u16)>,
    /// The active port-conflict modal, once the holder is known.
    port_conflict: Option<PortConflictDialog>,
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
            skill_dialog: None,
            viewport_height: 20,
            copy_mode: false,
            skill_hint_eligible: check_skill_hint_eligible(),
            started_at: std::time::Instant::now(),
            palette: Palette::default(),
            spinner_tick: 0,
            toasts: Vec::new(),
            sidebar_scroll: 0,
            sidebar_collapsed: false,
            config: GlobalConfig::default(),
            settings: None,
            pending_port_conflict: None,
            port_conflict: None,
        }
    }
}

fn check_skill_hint_eligible() -> bool {
    let cfg = devme_config::GlobalConfig::load();
    if cfg.get("hints.skills") == Some("false".into()) {
        return false;
    }
    // Don't hint to *install* a skill that's already here — whether devme
    // installed it or it arrived via `npx skills`. (Staleness is handled by
    // the update modal / auto-update, not this hint.)
    if !cfg.skill_installs().is_empty() || skill_present_anywhere() {
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

/// The devme config dir (`$XDG_CONFIG_HOME/devme` or `~/.config/devme`).
fn config_dir() -> Option<std::path::PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        Some(std::path::PathBuf::from(xdg).join("devme"))
    } else {
        std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".config").join("devme"))
    }
}

/// Advance the skills-hint backoff counter (mirrors the CLI's hint writer), so
/// the install modal doesn't reappear on every launch.
fn record_skill_hint_shown() {
    let Some(dir) = config_dir() else { return };
    let state_file = dir.join("skills-hint-state");
    let count: u32 = std::fs::read_to_string(&state_file)
        .ok()
        .and_then(|c| c.lines().next().and_then(|s| s.parse().ok()))
        .unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&state_file, format!("{}\n{now}", count + 1));
}

/// True if a skill is already installed at either default Claude Code
/// location (project or global) — devme-managed or not. Used so we don't
/// pester someone who installed via `npx skills`.
fn skill_present_anywhere() -> bool {
    use devme_config::skill::{InstallStatus, skill_file, status_at};
    [skill_file(false), skill_file(true)]
        .into_iter()
        .flatten()
        .any(|p| status_at(&p, None) != InstallStatus::Missing)
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

    #[cfg(test)]
    pub fn set_skill_dialog_for_test(&mut self, kind: SkillPrompt) {
        self.skill_dialog = Some(SkillDialog {
            kind,
            from: "0.1.2".into(),
            to: "0.1.3".into(),
            count: 1,
        });
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

    // ── stale-skill prompt ──────────────────────────────────────────────

    /// Decide, once at startup, whether to show a skill modal:
    ///
    /// - devme-managed install present + `skill.auto_update`: silently refresh,
    ///   show nothing.
    /// - devme-managed install present + stale: `Update` modal.
    /// - nothing installed at all (and the install hint is still eligible):
    ///   `Install` modal — same prompt, for first-time setup.
    ///
    /// The `Install` ask reuses the footer-hint backoff so it isn't naggy: it
    /// appears at most a handful of times, and showing it both suppresses the
    /// footer hint for this session and advances the persisted backoff.
    pub fn check_skill_prompt(&mut self) {
        let mut cfg = devme_config::GlobalConfig::load();

        if !cfg.skill_installs().is_empty() {
            if cfg.skill_auto_update() {
                devme_config::skill::auto_update(&mut cfg);
                return;
            }
            let stale = devme_config::skill::stale_installs(&cfg);
            if let Some(first) = stale.first() {
                self.skill_dialog = Some(SkillDialog {
                    kind: SkillPrompt::Update,
                    from: first.from.clone(),
                    to: first.to.clone(),
                    count: stale.len(),
                });
            }
            return;
        }

        // Nothing devme-managed. Offer to install — but only if the skill
        // isn't already present some other way (e.g. `npx skills`) and the
        // backoff still allows it.
        if !self.skill_hint_eligible || skill_present_anywhere() {
            return;
        }
        self.skill_dialog = Some(SkillDialog {
            kind: SkillPrompt::Install,
            from: String::new(),
            to: String::new(),
            count: 0,
        });
        // Don't double up: hide the footer hint this session and back off so
        // we don't pop the modal every launch.
        self.skill_hint_eligible = false;
        record_skill_hint_shown();
    }

    pub fn skill_dialog(&self) -> Option<&SkillDialog> {
        self.skill_dialog.as_ref()
    }

    pub fn skill_dialog_visible(&self) -> bool {
        self.skill_dialog.is_some()
    }

    pub fn dismiss_skill_dialog(&mut self) {
        self.skill_dialog = None;
    }

    /// Apply the stale-skill update: regenerate every stale, devme-managed,
    /// unmodified install. With `always`, also flips on `skill.auto_update`
    /// so future updates happen silently. Closes the dialog either way.
    pub fn apply_skill_update(&mut self, always: bool) {
        let mut cfg = devme_config::GlobalConfig::load();
        if always {
            let _ = cfg.set("skill.auto_update", "true");
            let _ = cfg.save();
        }
        devme_config::skill::auto_update(&mut cfg);
        self.skill_dialog = None;
    }

    /// Apply the install prompt: write the embedded skill into the chosen
    /// scope (project by default, `~/.claude/...` with `global`). Closes the
    /// dialog regardless of outcome (a failure just means it stays uninstalled).
    pub fn apply_skill_install(&mut self, global: bool) {
        let _ = devme_config::skill::install(global, false);
        self.skill_dialog = None;
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

    /// Dependency checks ("tools") to show in the sidebar.
    ///
    /// Steps are host/repo-level: every worktree of a repo shares the same
    /// `devme.toml`, so its `[step.*]` checks (uv, gcloud, redis…) are the
    /// same regardless of which worktree you're viewing. A placeholder
    /// worktree — discovered but with no daemon bound yet — has an empty
    /// `steps` list of its own. Rather than let the tools pane blink out of
    /// existence when you switch onto such a stack, we fall back to the
    /// checks reported by any subscribed sibling.
    pub fn steps(&self) -> &[StepSnapshot] {
        let own = self.current_instance().map(|i| i.steps.as_slice()).unwrap_or(&[]);
        if !own.is_empty() {
            return own;
        }
        self.instances
            .iter()
            .map(|i| i.steps.as_slice())
            .find(|s| !s.is_empty())
            .unwrap_or(&[])
    }

    /// Aggregate health of the stack at `idx`, for its sidebar status dot.
    /// A stack with no services of its own is a [`StackHealth::Placeholder`]
    /// (discovered worktree, no daemon yet).
    pub fn instance_health(&self, idx: usize) -> StackHealth {
        let Some(inst) = self.instances.get(idx) else {
            return StackHealth::Placeholder;
        };
        Self::aggregate_health(&inst.services)
    }

    /// Aggregate health of the shared (repo-scoped) services.
    pub fn shared_health(&self) -> StackHealth {
        Self::aggregate_health(&self.shared.services)
    }

    fn aggregate_health(services: &[ServiceSnapshot]) -> StackHealth {
        use devme_core::ServiceState as S;
        if services.is_empty() {
            return StackHealth::Placeholder;
        }
        if services
            .iter()
            .any(|s| matches!(s.state, S::Failed { .. } | S::CrashLoop { .. }))
        {
            return StackHealth::Failed;
        }
        let healthy = services
            .iter()
            .filter(|s| {
                matches!(
                    s.state,
                    S::Running { .. } | S::External { healthy: true }
                )
            })
            .count();
        if healthy == 0 {
            StackHealth::Idle
        } else if healthy == services.len() {
            StackHealth::AllRunning
        } else {
            StackHealth::SomeRunning
        }
    }

    /// Running / total service counts for the stack at `idx` — the secondary
    /// sidebar line ("2/3 up").
    pub fn instance_service_summary(&self, idx: usize) -> Option<(usize, usize)> {
        let inst = self.instances.get(idx)?;
        if inst.services.is_empty() {
            return None;
        }
        let total = inst.services.len();
        let up = inst
            .services
            .iter()
            .filter(|s| {
                matches!(
                    s.state,
                    ServiceState::Running { .. } | ServiceState::External { healthy: true }
                )
            })
            .count();
        Some((up, total))
    }

    /// Ahead/behind commit counts for the stack at `idx`, if known.
    pub fn instance_ahead_behind(&self, idx: usize) -> Option<(usize, usize)> {
        self.instances.get(idx)?.git_ahead_behind
    }

    // ── theme ───────────────────────────────────────────────────────────

    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    // ── animation + toasts ──────────────────────────────────────────────

    /// Current spinner frame for animated (starting/restarting) glyphs.
    pub fn spinner_frame(&self) -> char {
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        FRAMES[(self.spinner_tick % FRAMES.len() as u64) as usize]
    }

    /// Advance one animation tick: bump the spinner and drop expired toasts.
    /// Returns true if anything visible changed (so the loop can avoid a
    /// redraw when nothing did — though a redraw is cheap either way).
    pub fn tick(&mut self) -> bool {
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
        let before = self.toasts.len();
        self.toasts.retain(|t| t.born.elapsed() < TOAST_TTL);
        // Always redraw while a spinner is live so the animation runs.
        self.toasts.len() != before || self.has_animated_service()
    }

    fn has_animated_service(&self) -> bool {
        let animated = |svcs: &[ServiceSnapshot]| {
            svcs.iter().any(|s| {
                matches!(s.state, ServiceState::Starting | ServiceState::Restarting { .. })
            })
        };
        self.instances.iter().any(|i| animated(&i.services)) || animated(&self.shared.services)
    }

    pub fn toasts(&self) -> &[Toast] {
        &self.toasts
    }

    fn push_toast(&mut self, kind: ToastKind, title: impl Into<String>, body: impl Into<String>) {
        self.toasts.push(Toast {
            kind,
            title: title.into(),
            body: body.into(),
            born: Instant::now(),
        });
        if self.toasts.len() > MAX_TOASTS {
            self.toasts.remove(0);
        }
    }

    /// Emit a toast for a noteworthy service transition (`old` → `new`).
    /// Quiet about routine moves (e.g. stopped→starting); only crashes and
    /// recoveries get surfaced.
    fn toast_for_transition(&mut self, service: &str, old: &ServiceState, new: &ServiceState) {
        use ServiceState as S;
        let was_failed = matches!(old, S::Failed { .. } | S::CrashLoop { .. });
        let now_failed = matches!(new, S::Failed { .. } | S::CrashLoop { .. });
        let now_running = matches!(new, S::Running { degraded: false, .. });
        let was_up = matches!(old, S::Running { .. } | S::External { healthy: true });

        if now_failed && !was_failed {
            let detail = match new {
                S::Failed { exit_code: Some(c) } => format!("crashed (exit {c})"),
                S::CrashLoop { .. } => "crash-looping".to_string(),
                _ => "crashed".to_string(),
            };
            self.push_toast(ToastKind::Failed, service.to_string(), detail);
        } else if now_running && (was_failed || !was_up) {
            self.push_toast(ToastKind::Ready, service.to_string(), "ready");
        }
    }

    // ── reactive port-conflict modal ────────────────────────────────────

    /// On a fresh crash into Failed/CrashLoop, if the service's recent logs
    /// look like an `address already in use` error and we know its port,
    /// queue a port-conflict probe. Crash-loops re-enter the failure edge
    /// (Restarting→Failed) repeatedly, so a log line that arrived late still
    /// gets caught on a later cycle. Never stacks a second prompt.
    fn flag_port_conflict_if_addr_in_use(
        &mut self,
        instance_id: &str,
        service: &str,
        old: &ServiceState,
        new: &ServiceState,
        port: Option<u16>,
        shared: bool,
    ) {
        use ServiceState as S;
        let was_failed = matches!(old, S::Failed { .. } | S::CrashLoop { .. });
        let now_failed = matches!(new, S::Failed { .. } | S::CrashLoop { .. });
        // Only the fresh edge into failure — not repeated failed states.
        if !now_failed || was_failed {
            return;
        }
        if self.port_conflict.is_some() || self.pending_port_conflict.is_some() {
            return;
        }
        let Some(port) = port else { return };
        let hit = if shared {
            self.shared.logs.get(service)
        } else {
            self.find_instance(instance_id).and_then(|i| self.instances[i].logs.get(service))
        }
        .map(logs_show_addr_in_use)
        .unwrap_or(false);
        if hit {
            self.pending_port_conflict =
                Some((instance_id.to_string(), service.to_string(), port));
        }
    }

    /// Take the queued port-conflict probe, if any, so the event loop can
    /// identify the holder (a blocking docker/lsof call) off the UI thread.
    pub fn take_pending_port_conflict(&mut self) -> Option<(String, String, u16)> {
        self.pending_port_conflict.take()
    }

    /// Open the modal once the holder is known. Won't replace an open one.
    pub fn open_port_conflict(
        &mut self,
        instance_id: String,
        service: String,
        port: u16,
        holder: devme_supervisor::port_preflight::Holder,
    ) {
        if self.port_conflict.is_some() {
            return;
        }
        self.port_conflict =
            Some(PortConflictDialog::from_holder(instance_id, service, port, holder));
    }

    pub fn port_conflict(&self) -> Option<&PortConflictDialog> {
        self.port_conflict.as_ref()
    }

    pub fn port_conflict_visible(&self) -> bool {
        self.port_conflict.is_some()
    }

    /// Move the highlighted option by `delta`, wrapping.
    pub fn port_conflict_move(&mut self, delta: i32) {
        if let Some(d) = &mut self.port_conflict {
            let n = d.options.len() as i32;
            if n > 0 {
                d.selected = (((d.selected as i32 + delta) % n + n) % n) as usize;
            }
        }
    }

    pub fn dismiss_port_conflict(&mut self) {
        self.port_conflict = None;
    }

    /// Take the highlighted choice and close the modal. Returns
    /// `(instance_id, service, action)` for the event loop to carry out.
    pub fn take_port_conflict_choice(
        &mut self,
    ) -> Option<(String, String, PortConflictAction)> {
        let d = self.port_conflict.take()?;
        let action = d
            .options
            .get(d.selected)
            .map(|o| o.action.clone())
            .unwrap_or(PortConflictAction::Skip);
        Some((d.instance_id, d.service, action))
    }

    /// Surface the outcome of a remediation as a corner toast.
    pub fn push_port_conflict_result(&mut self, ok: bool, detail: impl Into<String>) {
        let kind = if ok { ToastKind::Info } else { ToastKind::Failed };
        self.push_toast(kind, "port", detail);
    }

    // ── sidebar layout (scroll + collapse) ──────────────────────────────

    pub fn sidebar_collapsed(&self) -> bool {
        self.sidebar_collapsed
    }

    pub fn toggle_sidebar(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
    }

    pub fn sidebar_scroll(&self) -> usize {
        self.sidebar_scroll
    }

    /// Keep the selected stack within a `visible_rows`-tall window by nudging
    /// the scroll offset. Called from the renderer once it knows the height.
    pub fn ensure_stack_visible(&mut self, visible_rows: usize) {
        let total = self.instances.len();
        if visible_rows == 0 || total <= visible_rows {
            self.sidebar_scroll = 0;
            return;
        }
        let sel = self.selected_instance.unwrap_or(0);
        if sel < self.sidebar_scroll {
            self.sidebar_scroll = sel;
        } else if sel >= self.sidebar_scroll + visible_rows {
            self.sidebar_scroll = sel + 1 - visible_rows;
        }
        self.sidebar_scroll = self.sidebar_scroll.min(total.saturating_sub(visible_rows));
    }

    // ── git status ──────────────────────────────────────────────────────

    /// Record a background git refresh for the instance with `id`.
    pub fn set_git_ahead_behind(&mut self, id: &str, ahead: usize, behind: usize) {
        if let Some(idx) = self.find_instance(id) {
            self.instances[idx].git_ahead_behind = Some((ahead, behind));
        }
    }

    /// (id, cwd) pairs for every instance — the background git refresher
    /// iterates these.
    pub fn instance_id_cwd_pairs(&self) -> Vec<(String, String)> {
        self.instances
            .iter()
            .map(|i| (i.info.id.clone(), i.info.cwd.clone()))
            .collect()
    }

    // ── settings overlay ────────────────────────────────────────────────

    /// Seed the in-memory config (theme/hints/…) loaded at startup.
    pub fn set_config(&mut self, config: GlobalConfig) {
        self.config = config;
    }

    /// Surface a startup config-parse warning as a (longer-lived) toast.
    pub fn push_config_warning(&mut self, message: String) {
        self.push_toast(ToastKind::Failed, "config", message);
    }

    pub fn settings_visible(&self) -> bool {
        self.settings.is_some()
    }

    pub fn settings(&self) -> Option<&SettingsState> {
        self.settings.as_ref()
    }

    /// Open the settings overlay, snapshotting each key's current value.
    pub fn open_settings(&mut self) {
        let values = SETTINGS
            .iter()
            .map(|s| self.config.get(s.key).unwrap_or_else(|| s.default.to_string()))
            .collect();
        self.settings = Some(SettingsState { cursor: 0, values });
    }

    pub fn close_settings(&mut self) {
        self.settings = None;
    }

    pub fn settings_move(&mut self, delta: i32) {
        if let Some(s) = &mut self.settings {
            let n = SETTINGS.len() as i32;
            s.cursor = (((s.cursor as i32 + delta) % n + n) % n) as usize;
        }
    }

    /// Change the focused setting. `dir` is +1 (right/next/activate), -1
    /// (left/prev). Updates the value, applies it live, and returns the
    /// `(key, value)` to persist — or `None` if nothing is open.
    pub fn settings_change(&mut self, dir: i32) -> Option<(&'static str, String)> {
        let s = self.settings.as_mut()?;
        let def = &SETTINGS[s.cursor];
        let current = &s.values[s.cursor];
        let next = match def.control {
            SettingControl::Toggle => {
                if current == "true" { "false".to_string() } else { "true".to_string() }
            }
            SettingControl::Choice(opts) => {
                let i = opts.iter().position(|o| *o == current).unwrap_or(0) as i32;
                let n = opts.len() as i32;
                opts[(((i + dir) % n + n) % n) as usize].to_string()
            }
        };
        s.values[s.cursor] = next.clone();
        // Keep the in-memory config in sync and apply side effects live.
        let _ = self.config.set(def.key, &next);
        if def.key == "tui.theme" {
            self.palette = Palette::preview(&next);
        }
        Some((def.key, next))
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
            ServerMessage::StatusUpdate { service, state, port: msg_port, .. } => {
                let mut transition = None;
                let mut port = msg_port;
                if let Some(idx) = self.find_instance(source_id)
                    && let Some(s) = self.instances[idx]
                        .services
                        .iter_mut()
                        .find(|s| s.name == service)
                {
                    transition = Some(s.state.clone());
                    port = port.or(s.port);
                    s.state = state.clone();
                }
                if let Some(old) = transition {
                    self.toast_for_transition(&service, &old, &state);
                    self.flag_port_conflict_if_addr_in_use(
                        source_id, &service, &old, &state, port, false,
                    );
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

    fn apply_shared(&mut self, source_id: &str, msg: ServerMessage) {
        match msg {
            ServerMessage::Subscribed { services, instance, .. } => {
                self.shared.id = Some(instance.id);
                self.shared.services = services;
                if self.shared.selected_service.is_none() && !self.shared.services.is_empty() {
                    self.shared.selected_service = Some(0);
                }
            }
            ServerMessage::StatusUpdate { service, state, port: msg_port, .. } => {
                let mut transition = None;
                let mut port = msg_port;
                if let Some(s) = self.shared.services.iter_mut().find(|s| s.name == service) {
                    transition = Some(s.state.clone());
                    port = port.or(s.port);
                    s.state = state.clone();
                }
                if let Some(old) = transition {
                    self.toast_for_transition(&service, &old, &state);
                    self.flag_port_conflict_if_addr_in_use(
                        source_id, &service, &old, &state, port, true,
                    );
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
    fn steps_persist_when_switching_onto_a_placeholder_stack() {
        // A subscribed stack with dependency checks, plus a placeholder
        // worktree (discovered, no daemon yet → no steps of its own).
        let mut s = TuiState::default();
        s.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "real".into(),
                label: "feature/a".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![svc("db")],
            steps: vec![StepSnapshot { name: "uv".into(), state: StepState::Passed }],
        });
        s.add_placeholder_instance("place", "feature/b", "/tmp/b");

        // Switch onto the placeholder. Its own steps are empty…
        s.select_instance_by_id("place");
        assert!(s.current_instance().unwrap().steps.is_empty());
        // …but the tools pane still shows the repo's checks (the sibling's),
        // rather than blinking out of existence.
        assert_eq!(s.steps().len(), 1);
        assert_eq!(s.steps()[0].name, "uv");

        // The placeholder reports as such for its status dot.
        let place_idx = s.find_instance("place").unwrap();
        assert_eq!(s.instance_health(place_idx), StackHealth::Placeholder);
    }

    #[test]
    fn crash_and_recovery_raise_toasts() {
        let running = || ServiceState::Running { degraded: false, started_without: vec![] };
        let status = |state: ServiceState| ServerMessage::StatusUpdate {
            service: "api".into(),
            state,
            pid: None,
            port: None,
            restart_count: 0,
        };
        let mut s = TuiState::default();
        // Start the service already up so the first transition is a crash.
        s.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![ServiceSnapshot { state: running(), ..svc("api") }],
            steps: vec![],
        });
        assert!(s.toasts().is_empty());

        // Running → Failed surfaces a crash toast…
        s.apply(status(ServiceState::Failed { exit_code: Some(2) }));
        assert_eq!(s.toasts().last().map(|t| t.kind), Some(ToastKind::Failed));
        // …and Failed → Running a recovery toast.
        s.apply(status(running()));
        assert_eq!(s.toasts().last().map(|t| t.kind), Some(ToastKind::Ready));
        assert_eq!(s.toasts().len(), 2);
        // A routine stopped→starting move stays quiet.
        let before = s.toasts().len();
        s.apply(status(ServiceState::Starting));
        assert_eq!(s.toasts().len(), before);
    }

    #[test]
    fn sidebar_scroll_keeps_selection_in_view() {
        let mut s = TuiState::default();
        for i in 0..6 {
            s.add_instance(format!("stack-{i}"));
        }
        // A 3-row window: selecting the last stack scrolls it into view.
        s.select_instance_by_id("local::stack-5");
        s.ensure_stack_visible(3);
        assert_eq!(s.sidebar_scroll(), 3); // shows 3,4,5
        // Selecting the first scrolls back to the top.
        s.select_instance_by_id("local::stack-0");
        s.ensure_stack_visible(3);
        assert_eq!(s.sidebar_scroll(), 0);
    }

    #[test]
    fn toggle_sidebar_flips_collapsed() {
        let mut s = TuiState::default();
        assert!(!s.sidebar_collapsed());
        s.toggle_sidebar();
        assert!(s.sidebar_collapsed());
        s.toggle_sidebar();
        assert!(!s.sidebar_collapsed());
    }

    #[test]
    fn settings_overlay_cycles_and_toggles() {
        let mut s = TuiState::default();
        s.open_settings();
        assert!(s.settings_visible());

        // Row 0 is the theme choice (mocha → latte → auto → mocha).
        assert_eq!(s.settings().unwrap().values[0], "mocha");
        let change = s.settings_change(1).unwrap();
        assert_eq!(change, ("tui.theme", "latte".to_string()));
        assert_eq!(s.settings().unwrap().values[0], "latte");
        // Live application swaps the palette.
        assert_eq!(*s.palette(), crate::theme::Palette::latte());

        // Move to the skill-hint toggle and flip it.
        s.settings_move(1);
        assert_eq!(s.settings().unwrap().cursor, 1);
        let change = s.settings_change(1).unwrap();
        assert_eq!(change, ("hints.skills", "false".to_string()));

        s.close_settings();
        assert!(!s.settings_visible());
    }

    #[test]
    fn settings_move_wraps() {
        let mut s = TuiState::default();
        s.open_settings();
        s.settings_move(-1);
        assert_eq!(s.settings().unwrap().cursor, SETTINGS.len() - 1);
    }

    #[test]
    fn git_ahead_behind_attaches_to_instance() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["api"]));
        let idx = s.find_instance("test-id").unwrap();
        assert_eq!(s.instance_ahead_behind(idx), None);
        s.set_git_ahead_behind("test-id", 3, 1);
        assert_eq!(s.instance_ahead_behind(idx), Some((3, 1)));
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

    // ── reactive port-conflict modal ────────────────────────────────────

    use devme_supervisor::port_preflight::Holder;

    #[test]
    fn addr_in_use_detected_in_logs() {
        let mut logs = VecDeque::new();
        logs.push_back("Error: listen EADDRINUSE: address already in use :::3000".to_string());
        assert!(logs_show_addr_in_use(&logs));

        let mut pg = VecDeque::new();
        pg.push_back("FATAL: could not create lock file: Address already in use".to_string());
        assert!(logs_show_addr_in_use(&pg));

        let mut clean = VecDeque::new();
        clean.push_back("server listening on 3000".to_string());
        assert!(!logs_show_addr_in_use(&clean));
    }

    #[test]
    fn options_from_container_with_project() {
        let h = Holder::Container { name: "pg-1".into(), project: Some("proj".into()) };
        let d = PortConflictDialog::from_holder("id".into(), "db".into(), 5432, h);
        assert_eq!(d.options.len(), 3);
        assert!(d.options[0].label.contains("Stop container pg-1"));
        assert!(d.options[1].label.contains("Compose down proj"));
        assert_eq!(d.options[2].action, PortConflictAction::Skip);
        assert_eq!(d.holder_desc, "container pg-1 (compose: proj)");
    }

    #[test]
    fn options_from_process_offer_kill_then_skip() {
        let h = Holder::Process(vec![(123, Some("node".into()))]);
        let d = PortConflictDialog::from_holder("id".into(), "web".into(), 3000, h);
        assert_eq!(d.options.len(), 2);
        assert!(d.options[0].label.contains("Kill node (123)"));
        assert_eq!(d.options[0].action, PortConflictAction::KillProcess(vec![123]));
        assert_eq!(d.options[1].action, PortConflictAction::Skip);
    }

    #[test]
    fn port_conflict_move_wraps_both_ways() {
        let mut s = TuiState::default();
        s.open_port_conflict(
            "id".into(),
            "db".into(),
            5432,
            Holder::Container { name: "c".into(), project: None },
        );
        // options: [Stop, Skip]
        assert_eq!(s.port_conflict().unwrap().selected, 0);
        s.port_conflict_move(-1);
        assert_eq!(s.port_conflict().unwrap().selected, 1);
        s.port_conflict_move(1);
        assert_eq!(s.port_conflict().unwrap().selected, 0);
    }

    #[test]
    fn take_choice_returns_selection_and_closes() {
        let mut s = TuiState::default();
        s.open_port_conflict("inst".into(), "db".into(), 5432, Holder::Process(vec![(9, None)]));
        let (id, svc, action) = s.take_port_conflict_choice().unwrap();
        assert_eq!(id, "inst");
        assert_eq!(svc, "db");
        assert_eq!(action, PortConflictAction::KillProcess(vec![9]));
        assert!(!s.port_conflict_visible());
    }

    #[test]
    fn crash_with_addr_in_use_log_queues_a_probe() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["db"]));
        // A log line showing the bind failure arrives first…
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
        s.apply(ServerMessage::LogChunk {
            service: "db".into(),
            bytes: enc("Error: bind: address already in use"),
            ts: 1,
        });
        // …then the daemon marks the service Failed, carrying the port.
        s.apply(ServerMessage::StatusUpdate {
            service: "db".into(),
            state: ServiceState::Failed { exit_code: Some(1) },
            pid: None,
            port: Some(5432),
            restart_count: 0,
        });
        assert_eq!(
            s.take_pending_port_conflict(),
            Some((test_instance().id, "db".to_string(), 5432)),
        );
    }

    #[test]
    fn crash_without_addr_in_use_does_not_queue() {
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["db"]));
        let enc = |t: &str| base64::engine::general_purpose::STANDARD.encode(t.as_bytes());
        s.apply(ServerMessage::LogChunk {
            service: "db".into(),
            bytes: enc("panic: nil pointer dereference"),
            ts: 1,
        });
        s.apply(ServerMessage::StatusUpdate {
            service: "db".into(),
            state: ServiceState::Failed { exit_code: Some(2) },
            pid: None,
            port: Some(5432),
            restart_count: 0,
        });
        assert_eq!(s.take_pending_port_conflict(), None);
    }
}
