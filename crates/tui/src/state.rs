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

impl Toast {
    /// How long ago this notification was raised — drives the relative
    /// timestamp ("12s ago") in the notifications-history modal.
    pub fn age(&self) -> std::time::Duration {
        self.born.elapsed()
    }

    /// Compact relative age for display/copy: `3s`, `12m`, `2h`, `1d`.
    pub fn age_label(&self) -> String {
        let s = self.age().as_secs();
        if s < 60 {
            format!("{s}s")
        } else if s < 3600 {
            format!("{}m", s / 60)
        } else if s < 86_400 {
            format!("{}h", s / 3600)
        } else {
            format!("{}d", s / 86_400)
        }
    }
}

/// How long a toast stays on screen before the tick loop drops it.
const TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(5);
/// Cap on simultaneously-visible toasts (oldest evicted first).
const MAX_TOASTS: usize = 4;
/// Cap on the retained notification scrollback (oldest evicted first).
const MAX_NOTIF_HISTORY: usize = 200;

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

/// Which group a main-pane tab belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    /// One of the worktree's own services.
    Owned,
    /// A repo-scoped shared service (access-only on a stack).
    Shared,
    /// Synthetic leading tab shown when the worktree owns no services —
    /// selecting it explains why (no devme.toml / none declared) so the
    /// trailing shared tabs aren't mistaken for this worktree's own.
    Placeholder,
}

/// One entry in the main-pane tab row. On a stack the row is the worktree's
/// own services followed by an access-only group of repo-scoped shared
/// services (with a leading [`TabKind::Placeholder`] when it owns none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabEntry {
    /// Display label — a service's name, or a short status for the placeholder.
    pub label: String,
    pub kind: TabKind,
    /// Backing service snapshot; `None` for the placeholder tab.
    pub snapshot: Option<ServiceSnapshot>,
}

impl TabEntry {
    pub fn is_shared(&self) -> bool {
        matches!(self.kind, TabKind::Shared)
    }
    pub fn is_placeholder(&self) -> bool {
        matches!(self.kind, TabKind::Placeholder)
    }
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

/// What a stack's secondary sidebar line shows for its service counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackSummary {
    /// No daemon has reported for this worktree yet (a placeholder row).
    NoDaemon,
    /// The daemon is up but the worktree owns no services of its own — it
    /// only pulls in repo-scoped shared services (counted in their own row).
    SharedOnly,
    /// `up`/`total` of the worktree's own (non-shared) services.
    Counted { up: usize, total: usize },
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
    /// A choice value that means "leave the key unset" — selecting it removes
    /// the key from `global.toml` rather than writing a literal. Used for the
    /// `(auto)` option on keys whose absence triggers auto-detection.
    pub unset_value: Option<&'static str>,
}

#[derive(Clone, Copy)]
pub enum SettingControl {
    /// A boolean, stored as the strings "true"/"false".
    Toggle,
    /// One of a fixed set of values.
    Choice(&'static [&'static str]),
}

/// What an overlay edit should do to `global.toml`: write a value, or remove
/// the key entirely (the `(auto)` / default case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingWrite {
    Set { key: &'static str, value: String },
    Unset { key: &'static str },
}

impl SettingWrite {
    pub fn key(&self) -> &'static str {
        match self {
            SettingWrite::Set { key, .. } | SettingWrite::Unset { key } => key,
        }
    }
}

/// The settings the overlay exposes — a thin front-end over the same
/// `global.toml` keys `devme config set` writes.
pub const SETTINGS: &[SettingDef] = &[
    SettingDef {
        key: "tui.theme",
        label: "Theme",
        desc: "Colour palette for the TUI",
        control: SettingControl::Choice(&["mocha", "latte", "auto"]),
        default: "mocha",
        unset_value: None,
    },
    SettingDef {
        key: "tui.toasts",
        label: "Notifications",
        desc: "Pop a toast when a service crashes or recovers",
        control: SettingControl::Toggle,
        default: "true",
        unset_value: None,
    },
    SettingDef {
        key: "tui.confirm_quit",
        label: "Confirm quit",
        desc: "Ask before quitting (which stops every service)",
        control: SettingControl::Toggle,
        default: "true",
        unset_value: None,
    },
    SettingDef {
        key: "hints.skills",
        label: "Skill hint",
        desc: "Show the AI-skill install hint in the footer",
        control: SettingControl::Toggle,
        default: "true",
        unset_value: None,
    },
    SettingDef {
        key: "skill.auto_update",
        label: "Auto-update skill",
        desc: "Refresh the embedded AI skill when devme updates",
        control: SettingControl::Toggle,
        default: "false",
        unset_value: None,
    },
    SettingDef {
        key: "remote.default",
        label: "Remote by default",
        desc: "Bare `devme` syncs + attaches to the remote host (needs remote.host)",
        control: SettingControl::Toggle,
        default: "false",
        unset_value: None,
    },
    SettingDef {
        key: "docker.daemon",
        label: "Docker daemon",
        desc: "Which daemon to start when Docker isn't running",
        control: SettingControl::Choice(&[
            "auto",
            "orbstack",
            "docker-desktop",
            "colima",
            "rancher-desktop",
        ]),
        default: "auto",
        // `auto` is the absence of the key — selecting it unsets `docker.daemon`
        // so devme falls back to auto-detection.
        unset_value: Some("auto"),
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

/// A clickable element the renderer records each frame so the event loop can
/// hit-test pointer clicks against it (we capture the mouse anyway for scroll,
/// so clicks would otherwise be discarded). Coordinates are screen cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickTarget {
    /// Select the stack (instance) at this index in the sidebar.
    Stack(usize),
    /// Select the "shared" sidebar row.
    Shared,
    /// Select the service tab at this index in [`TuiState::tab_services`].
    Tab(usize),
    /// Copy the notification at this display index (newest-first) in the
    /// notifications-history modal.
    Notif(usize),
}

/// A recorded clickable region: a screen rectangle and what clicking it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClickRegion {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    target: ClickTarget,
}

impl ClickRegion {
    fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.x && col < self.x + self.w && row >= self.y && row < self.y + self.h
    }
}

/// Geometry of the log scrollbar's track, recorded each frame so a click or
/// drag on it can be mapped back to a scroll offset. The track is one column
/// wide at `x`, spanning rows `[y, y + h)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollbarHit {
    x: u16,
    y: u16,
    h: u16,
    /// Total log lines (track represents the whole buffer).
    content_len: usize,
    /// Visible rows (the thumb's span).
    viewport: usize,
}

/// Default sidebar width in columns. Wide enough for stack names + a health
/// dot without crowding; the user can drag the divider to taste.
pub const DEFAULT_SIDEBAR_WIDTH: u16 = 28;
/// Narrowest the sidebar may be dragged before names start truncating badly.
const MIN_SIDEBAR_WIDTH: u16 = 16;
/// Columns the main pane must keep; the sidebar can't be dragged past
/// `total_width - MIN_MAIN_WIDTH`.
const MIN_MAIN_WIDTH: u16 = 24;

/// Geometry of the sidebar/main divider, recorded each frame so a click or
/// drag on it can resize the sidebar. The divider is the main pane's left
/// border, one column wide at `x`, spanning rows `[y, y + h)`. `total_width`
/// is the full frame width, used to clamp the sidebar so the main pane keeps
/// room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SidebarDivider {
    x: u16,
    y: u16,
    h: u16,
    total_width: u16,
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
    /// Durable log of every notification raised this session, newest last —
    /// the corner `toasts` above auto-expire, so this is the scrollback the
    /// notifications modal (`n`) renders. Capped at [`MAX_NOTIF_HISTORY`].
    notif_history: Vec<Toast>,
    /// When true the notifications-history modal is open.
    notif_visible: bool,
    /// Selected row in the notifications modal, as a display index (0 = newest,
    /// counting back through `notif_history`). The render derives the scroll
    /// window from this so the cursor is always visible; `c`/click copy it.
    notif_cursor: usize,
    /// Index of the first stack row painted in the sidebar (vertical scroll
    /// when the list is taller than the pane).
    sidebar_scroll: usize,
    /// When true the sidebar is hidden, giving the log pane full width.
    sidebar_collapsed: bool,
    /// Sidebar width in columns. Adjustable by dragging the divider, clamped
    /// to keep both the sidebar and the main pane usable.
    sidebar_width: u16,
    /// Fullscreen "zoom" mode: the selected service's logs fill the screen,
    /// all chrome (sidebar, tabs, footer) hidden. Live tail + scroll stay
    /// active (unlike copy mode, which is for native text selection).
    zoom: bool,
    /// When set, a "really quit?" confirmation modal is up (gated on
    /// `tui.confirm_quit`).
    quit_confirm: bool,
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
    /// True once at least one instance daemon has attached (sent a
    /// `Subscribed`). Gates [`TuiState::all_daemons_shut_down`] so a
    /// sidebar that's only ever held placeholders never auto-exits.
    had_live_daemon: bool,
    /// Clickable regions recorded by the renderer this frame, hit-tested by
    /// the event loop on a left-click. Cleared and repopulated every render.
    click_regions: Vec<ClickRegion>,
    /// Log scrollbar geometry recorded this frame, for click/drag-to-scroll.
    scrollbar_hit: Option<ScrollbarHit>,
    /// True while the user holds the mouse on the scrollbar, so drag events
    /// keep steering it even if the pointer slips off the 1-column track.
    scrollbar_dragging: bool,
    /// Sidebar/main divider geometry recorded this frame, for click/drag-to-resize.
    sidebar_divider: Option<SidebarDivider>,
    /// True while the user holds the mouse on the divider, so drag events keep
    /// resizing even if the pointer slips off the 1-column divider.
    sidebar_dragging: bool,
    /// After an external `devme down`/quit drained every daemon, the TUI parks
    /// in this "stopped" state instead of exiting — a durable dashboard that
    /// repopulates the moment a later `devme up` reattaches a daemon. The
    /// event loop sets this from [`TuiState::all_daemons_shut_down`].
    stopped: bool,
    /// Friendly repo label captured when entering the stopped state, shown on
    /// the hero card. `None` if it couldn't be resolved.
    stopped_repo: Option<String>,
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
            notif_history: Vec::new(),
            notif_visible: false,
            notif_cursor: 0,
            sidebar_scroll: 0,
            sidebar_collapsed: false,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            zoom: false,
            quit_confirm: false,
            config: GlobalConfig::default(),
            settings: None,
            pending_port_conflict: None,
            port_conflict: None,
            had_live_daemon: false,
            click_regions: Vec::new(),
            scrollbar_hit: None,
            scrollbar_dragging: false,
            sidebar_divider: None,
            sidebar_dragging: false,
            stopped: false,
            stopped_repo: None,
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

    /// Drop the instance row for `id` (its daemon shut down), keeping the
    /// selection on a valid row.
    fn remove_instance(&mut self, id: &str) {
        let Some(idx) = self.find_instance(id) else {
            return;
        };
        self.instances.remove(idx);
        if self.instances.is_empty() {
            self.selected_instance = None;
        } else if let Some(sel) = self.selected_instance
            && sel >= self.instances.len()
        {
            self.selected_instance = Some(self.instances.len() - 1);
        }
    }

    /// True once a daemon has attached and every instance daemon has since
    /// shut down — i.e. the user ran `devme down`/quit and there's nothing
    /// left to watch. Placeholders (worktrees with no daemon) don't count as
    /// live, so a sidebar of only placeholders still reads as "all gone". A
    /// daemon *crash* leaves its row in place (services keep their last-known
    /// state), so this stays false and the TUI keeps running. The event loop
    /// polls this after each daemon message and exits cleanly when it's true.
    pub fn all_daemons_shut_down(&self) -> bool {
        self.had_live_daemon
            && self
                .instances
                .iter()
                .all(|i| i.services.is_empty() && i.steps.is_empty())
    }

    /// True while the TUI is parked in the stopped state — every daemon went
    /// away via `devme down`/quit elsewhere, but the dashboard stays up rather
    /// than exiting. Drives the full-screen "all stopped" hero.
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// The repo label to show on the stopped hero card, if one was captured.
    pub fn stopped_repo(&self) -> Option<&str> {
        self.stopped_repo.as_deref()
    }

    /// Enter the stopped state. Every daemon has gone, but instead of exiting
    /// the TUI becomes a durable dashboard waiting for the next `devme up`.
    /// `repo` is a friendly name for the hero card. Any in-flight overlay is
    /// dismissed so the stopped screen owns the frame and its keys aren't
    /// shadowed by a stale modal handler.
    pub fn enter_stopped(&mut self, repo: Option<String>) {
        self.stopped = true;
        self.stopped_repo = repo;
        self.help_visible = false;
        self.settings = None;
        self.notif_visible = false;
        self.skill_dialog = None;
        self.port_conflict = None;
        self.pending_port_conflict = None;
        self.quit_confirm = false;
        self.zoom = false;
        self.copy_mode = false;
    }

    /// Leave the stopped state — a daemon reattached (a fresh `devme up`), so
    /// the live dashboard takes over again.
    pub fn clear_stopped(&mut self) {
        self.stopped = false;
        self.stopped_repo = None;
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

    /// As [`current_instance_is_placeholder`](Self::current_instance_is_placeholder)
    /// but for the stack at `idx` — a discovered worktree with no daemon
    /// bound (steady-state: no `devme.toml`).
    pub fn instance_is_placeholder(&self, idx: usize) -> bool {
        self.instances
            .get(idx)
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

    /// The services a stack *owns* — used for the sidebar/title counts and
    /// the debug prompt. On a stack this is the worktree's own services
    /// (repo-scoped stubs filtered out); when the shared row is focused it is
    /// the repo-scoped services. This is deliberately *not* the tab list:
    /// shared services appear as access-only trailing tabs on a stack
    /// (see [`tab_services`](Self::tab_services)) but never inflate the
    /// owned-service count.
    pub fn services(&self) -> Vec<ServiceSnapshot> {
        if self.shared_selected {
            self.shared.services.clone()
        } else {
            self.instance_only_services().into_iter().cloned().collect()
        }
    }

    /// The ordered main-pane tab list. On a stack: the worktree's own
    /// services first, then an access-only group of repo-scoped shared
    /// services (so all logs relevant to the stack are reachable without
    /// leaving it). When the worktree owns no services but shared ones
    /// exist, a leading [`TabKind::Placeholder`] tab explains why. When the
    /// shared row is focused: just the shared services.
    pub fn tab_services(&self) -> Vec<TabEntry> {
        let shared_tab = |snapshot: ServiceSnapshot| TabEntry {
            label: snapshot.name.clone(),
            kind: TabKind::Shared,
            snapshot: Some(snapshot),
        };
        if self.shared_selected {
            return self.shared.services.iter().cloned().map(shared_tab).collect();
        }
        let owned: Vec<TabEntry> = self
            .instance_only_services()
            .into_iter()
            .cloned()
            .map(|snapshot| TabEntry {
                label: snapshot.name.clone(),
                kind: TabKind::Owned,
                snapshot: Some(snapshot),
            })
            .collect();
        let shared = self.shared.services.iter().cloned().map(shared_tab);

        if owned.is_empty() && !self.shared.services.is_empty() {
            // Lead with a tab explaining the empty owned side, so the shared
            // tabs read as repo-scoped rather than this worktree's own.
            let mut tabs = vec![TabEntry {
                label: self.placeholder_tab_label(),
                kind: TabKind::Placeholder,
                snapshot: None,
            }];
            tabs.extend(shared);
            tabs
        } else {
            let mut tabs = owned;
            tabs.extend(shared);
            tabs
        }
    }

    /// Whether the current stack view leads with a placeholder tab — true
    /// when (on a stack) the worktree owns no services but shared ones exist.
    fn has_placeholder_tab(&self) -> bool {
        !self.shared_selected
            && !self.shared.services.is_empty()
            && self.instance_only_services().is_empty()
    }

    /// Short label for the placeholder tab, naming the cause.
    fn placeholder_tab_label(&self) -> String {
        if self.current_instance_is_placeholder() {
            "no devme.toml".to_string()
        } else {
            "no services".to_string()
        }
    }

    /// The detailed explanation shown in the viewport when the placeholder
    /// tab is focused.
    pub fn placeholder_explanation(&self) -> String {
        let mut msg = if self.current_instance_is_placeholder() {
            format!(
                "No devme.toml in {} — add one to start services.",
                self.current_instance_cwd()
            )
        } else {
            "No services declared in this worktree's devme.toml.".to_string()
        };
        // Only mention the trailing tabs when there actually are shared
        // services to point at.
        if !self.shared.services.is_empty() {
            msg.push_str(
                "\n\nThe tabs to the right are repo-scoped shared services this worktree can use.",
            );
        }
        msg
    }

    fn active_service_count(&self) -> usize {
        self.tab_services().len()
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
        // Owned services only — repo-scoped shared services have their own
        // sidebar row and dot, so a stack's health reflects what it owns.
        let owned: Vec<ServiceSnapshot> = inst
            .services
            .iter()
            .filter(|s| !self.shared.services.iter().any(|sh| sh.name == s.name))
            .cloned()
            .collect();
        Self::aggregate_health(&owned)
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
    /// sidebar line ("2/3 up"). Counts only the stack's *own* services:
    /// repo-scoped shared services are bound to this daemon as stubs but are
    /// summarised in their own sidebar row, so folding them in here would
    /// over-count (e.g. "5/5" for a 3-service stack with 2 shared deps).
    pub fn instance_service_summary(&self, idx: usize) -> StackSummary {
        let Some(inst) = self.instances.get(idx) else {
            return StackSummary::NoDaemon;
        };
        if inst.services.is_empty() {
            return StackSummary::NoDaemon;
        }
        let owned: Vec<&ServiceSnapshot> = inst
            .services
            .iter()
            .filter(|s| !self.shared.services.iter().any(|sh| sh.name == s.name))
            .collect();
        if owned.is_empty() {
            // Daemon is up, but every service it reports is a shared stub.
            return StackSummary::SharedOnly;
        }
        let total = owned.len();
        let up = owned
            .iter()
            .filter(|s| {
                matches!(
                    s.state,
                    ServiceState::Running { .. } | ServiceState::External { healthy: true }
                )
            })
            .count();
        StackSummary::Counted { up, total }
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

    /// Fire a neutral info toast for a deliberate user action so it's never a
    /// silent no-op, and record it in the durable history. Always shown,
    /// regardless of the crash-toast setting, since the user just asked for it.
    pub fn notify(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.push_toast_inner(ToastKind::Info, title, body, true);
    }

    /// Like [`Self::notify`], but ephemeral: shows the corner toast for
    /// feedback yet is *not* recorded into the notification history. Used for
    /// clipboard-copy acknowledgements — they're momentary "yes, copied"
    /// signals, not events worth remembering, and recording them would pollute
    /// the very history the user browses (copy-in-modal feeding itself).
    pub fn notify_transient(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.push_toast_inner(ToastKind::Info, title, body, false);
    }

    fn push_toast(&mut self, kind: ToastKind, title: impl Into<String>, body: impl Into<String>) {
        self.push_toast_inner(kind, title, body, true);
    }

    /// Shared toast machinery. `record` controls whether the toast also lands
    /// in the durable history (true for events, false for transient acks).
    fn push_toast_inner(
        &mut self,
        kind: ToastKind,
        title: impl Into<String>,
        body: impl Into<String>,
        record: bool,
    ) {
        let toast = Toast {
            kind,
            title: title.into(),
            body: body.into(),
            born: Instant::now(),
        };
        // The corner stack auto-expires; the history is the durable scrollback
        // the modal renders. Events are recorded there; transient acks aren't.
        if record {
            self.notif_history.push(toast.clone());
            if self.notif_history.len() > MAX_NOTIF_HISTORY {
                self.notif_history.remove(0);
            }
        }
        self.toasts.push(toast);
        if self.toasts.len() > MAX_TOASTS {
            self.toasts.remove(0);
        }
    }

    /// The full notification scrollback, oldest first. Rendered newest-first by
    /// the modal.
    pub fn notifications(&self) -> &[Toast] {
        &self.notif_history
    }

    pub fn notifications_visible(&self) -> bool {
        self.notif_visible
    }

    /// The selected row as a display index (0 = newest). Read by the renderer
    /// to highlight the row and derive the scroll window.
    pub fn notif_cursor(&self) -> usize {
        self.notif_cursor
    }

    /// Toggle the notifications-history modal. Opening resets the cursor to the
    /// newest entry.
    pub fn toggle_notifications(&mut self) {
        self.notif_visible = !self.notif_visible;
        if self.notif_visible {
            self.notif_cursor = 0;
        }
    }

    pub fn close_notifications(&mut self) {
        self.notif_visible = false;
    }

    /// Move the cursor toward older entries (down the list), clamped to the
    /// oldest.
    pub fn notif_cursor_down(&mut self, n: usize) {
        let max = self.notif_history.len().saturating_sub(1);
        self.notif_cursor = (self.notif_cursor + n).min(max);
    }

    /// Move the cursor back toward the newest entry.
    pub fn notif_cursor_up(&mut self, n: usize) {
        self.notif_cursor = self.notif_cursor.saturating_sub(n);
    }

    /// The selected notification as copy text, or `None` when the history is
    /// empty. Used by `c`/Enter and click-to-copy. Includes the relative age
    /// for context once pasted elsewhere — see [`Self::notif_line_text`].
    pub fn notif_selected_text(&self) -> Option<String> {
        let len = self.notif_history.len();
        if len == 0 {
            return None;
        }
        // Cursor is a display index (0 = newest); history is oldest-first.
        let d = self.notif_cursor.min(len - 1);
        Some(Self::notif_line_text(&self.notif_history[len - 1 - d]))
    }

    /// The whole scrollback as copy text, newest first, one line each.
    pub fn notif_all_text(&self) -> String {
        self.notif_history
            .iter()
            .rev()
            .map(Self::notif_line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One notification's copy line: `[3m ago] title: body`. The age gives a
    /// little context when the line lands in a chat message or agent prompt;
    /// it's relative (no wall-clock dependency) so it reads best when pasted
    /// soon after copying.
    fn notif_line_text(t: &Toast) -> String {
        format!("[{} ago] {}: {}", t.age_label(), t.title, t.body)
    }

    /// Hit-test a click at `(col, row)` against the notification rows recorded
    /// this frame. On a hit, move the cursor to that row and return its text to
    /// copy; otherwise `None` (the click missed a row).
    pub fn notif_copy_at(&mut self, col: u16, row: u16) -> Option<String> {
        let target = self
            .click_regions
            .iter()
            .find(|r| r.contains(col, row))
            .map(|r| r.target)?;
        let ClickTarget::Notif(d) = target else {
            return None;
        };
        self.notif_cursor = d;
        self.notif_selected_text()
    }

    /// Emit a toast for a noteworthy service transition (`old` → `new`).
    /// Quiet about routine moves (e.g. stopped→starting); only crashes and
    /// recoveries get surfaced.
    fn toast_for_transition(&mut self, service: &str, old: &ServiceState, new: &ServiceState) {
        use ServiceState as S;
        // Service crash/recovery toasts are opt-out (`tui.toasts`). Config-parse
        // warnings bypass this — they go through `push_config_warning` directly.
        if !self.toasts_enabled() {
            return;
        }
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

    /// Current sidebar width in columns (only meaningful when not collapsed).
    pub fn sidebar_width(&self) -> u16 {
        self.sidebar_width
    }

    /// Clamp and store a new sidebar width. `total_width` is the full frame
    /// width; the sidebar can't shrink below [`MIN_SIDEBAR_WIDTH`] nor grow
    /// past `total_width - MIN_MAIN_WIDTH` (keeping the main pane usable).
    fn set_sidebar_width(&mut self, width: u16, total_width: u16) {
        let max = total_width.saturating_sub(MIN_MAIN_WIDTH).max(MIN_SIDEBAR_WIDTH);
        self.sidebar_width = width.clamp(MIN_SIDEBAR_WIDTH, max);
    }

    /// Record the divider's geometry for this frame (its column, vertical
    /// extent, and the frame width used to clamp drags).
    pub fn set_sidebar_divider(&mut self, x: u16, y: u16, h: u16, total_width: u16) {
        self.sidebar_divider = Some(SidebarDivider { x, y, h, total_width });
    }

    /// Whether `(col, row)` falls on the sidebar divider. The hit zone is two
    /// columns wide (the divider plus the blank gutter to its left) so it's
    /// easy to grab.
    pub fn sidebar_divider_at(&self, col: u16, row: u16) -> bool {
        match self.sidebar_divider {
            Some(d) => {
                row >= d.y
                    && row < d.y + d.h
                    && col <= d.x
                    && col + 1 >= d.x
            }
            None => false,
        }
    }

    pub fn sidebar_dragging(&self) -> bool {
        self.sidebar_dragging
    }

    pub fn begin_sidebar_drag(&mut self) {
        self.sidebar_dragging = true;
    }

    pub fn end_sidebar_drag(&mut self) {
        self.sidebar_dragging = false;
    }

    /// Resize the sidebar so its divider sits at pointer column `col`. The new
    /// width is the column itself (the sidebar spans `[0, col)`), clamped.
    pub fn sidebar_drag_to(&mut self, col: u16) {
        let Some(d) = self.sidebar_divider else {
            return;
        };
        self.set_sidebar_width(col, d.total_width);
    }

    // ── fullscreen log zoom ─────────────────────────────────────────────

    pub fn zoom(&self) -> bool {
        self.zoom
    }

    pub fn toggle_zoom(&mut self) {
        self.zoom = !self.zoom;
    }

    pub fn exit_zoom(&mut self) {
        self.zoom = false;
    }

    // ── quit confirmation (gated on `tui.confirm_quit`) ──────────────────

    /// Whether quitting should pop a confirmation first. Defaults to on when
    /// the key is unset — quitting stops every service (and the shared ones),
    /// so a deliberate confirm is the safer default; opt out via
    /// `tui.confirm_quit = false`.
    pub fn confirm_quit_enabled(&self) -> bool {
        self.config.get("tui.confirm_quit").as_deref() != Some("false")
    }

    pub fn quit_confirm_visible(&self) -> bool {
        self.quit_confirm
    }

    pub fn open_quit_confirm(&mut self) {
        self.quit_confirm = true;
    }

    pub fn cancel_quit_confirm(&mut self) {
        self.quit_confirm = false;
    }

    /// Whether service crash/recovery toasts are shown (opt-out via
    /// `tui.toasts`). Defaults to on when the key is unset.
    fn toasts_enabled(&self) -> bool {
        self.config.get("tui.toasts").as_deref() != Some("false")
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

    /// Apply a background git refresh for the instance with `id`: re-label its
    /// sidebar row to `branch` (so checking out a different branch in the
    /// worktree updates the name in place) and refresh its ahead/behind counts.
    ///
    /// A `None` branch means git couldn't name one (detached HEAD, transient
    /// failure, non-repo) — we keep the existing label rather than blank it.
    /// When the branch *is* known, the ahead/behind counts are taken verbatim,
    /// so switching to a branch with no upstream clears the previous branch's
    /// stale `↑/↓` instead of leaving it stuck on screen.
    pub fn apply_git_refresh(
        &mut self,
        id: &str,
        branch: Option<String>,
        ahead_behind: Option<(usize, usize)>,
    ) {
        let Some(idx) = self.find_instance(id) else {
            return;
        };
        match branch {
            Some(branch) => {
                if self.instances[idx].info.label != branch {
                    self.instances[idx].info.label = branch;
                }
                self.instances[idx].git_ahead_behind = ahead_behind;
            }
            // Git couldn't resolve a branch — don't disturb the last-known
            // label or counts on a transient hiccup.
            None => {
                if let Some(ab) = ahead_behind {
                    self.instances[idx].git_ahead_behind = Some(ab);
                }
            }
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

    /// Re-read `global.toml` from disk and apply it live — theme, toasts,
    /// confirm-quit, docker daemon, etc. Lets a config change made *outside*
    /// the TUI (a `devme config set` in another shell, an agent in a herdr
    /// pane, a hand-edit) take effect without restarting. The theme uses
    /// `Palette::preview` rather than `resolve` because the `auto` OSC-11
    /// query can't round-trip once the alt-screen is up. Surfaces a parse
    /// error as a toast, or confirms the reload.
    pub fn reload_config(&mut self) {
        let (cfg, warning) = GlobalConfig::load_checked();
        let theme = cfg.get("tui.theme").unwrap_or_else(|| "mocha".into());
        self.palette = Palette::preview(&theme);
        self.config = cfg;
        match warning {
            Some(w) => self.push_config_warning(w),
            None => self.notify("config", "reloaded from disk"),
        }
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
    /// [`SettingWrite`] to persist — or `None` if nothing is open.
    pub fn settings_change(&mut self, dir: i32) -> Option<SettingWrite> {
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
        // A value equal to the setting's `unset_value` (e.g. docker `auto`)
        // means "remove the key"; otherwise keep the in-memory config in sync.
        let is_unset = def.unset_value == Some(next.as_str());
        if is_unset {
            let _ = self.config.unset(def.key);
        } else {
            let _ = self.config.set(def.key, &next);
        }
        if def.key == "tui.theme" {
            self.palette = Palette::preview(&next);
        }
        Some(if is_unset {
            SettingWrite::Unset { key: def.key }
        } else {
            SettingWrite::Set { key: def.key, value: next }
        })
    }

    pub fn selected_service(&self) -> Option<&ServiceSnapshot> {
        let idx = self.active_selected_service_idx()?;
        if self.shared_selected {
            return self.shared.services.get(idx);
        }
        if self.has_placeholder_tab() {
            // Tab 0 is the (serviceless) placeholder; shared services follow.
            return idx.checked_sub(1).and_then(|i| self.shared.services.get(i));
        }
        // Stack view: owned services first, then the shared group.
        let inst_only = self.instance_only_services();
        if idx < inst_only.len() {
            inst_only.get(idx).copied()
        } else {
            self.shared.services.get(idx - inst_only.len())
        }
    }

    /// True when the focused tab is the synthetic placeholder (the worktree
    /// owns no services). The placeholder is tab 0 and the default focus, so
    /// an unset selection counts as focused-on-placeholder.
    pub fn selected_tab_is_placeholder(&self) -> bool {
        self.has_placeholder_tab()
            && matches!(self.active_selected_service_idx(), None | Some(0))
    }

    /// Index of the highlighted tab in [`tab_services`](Self::tab_services).
    /// Falls back to the first tab (the default focus) when unset.
    pub fn selected_tab_index(&self) -> usize {
        match self.active_selected_service_idx() {
            Some(i) if i < self.tab_services().len() => i,
            _ => 0,
        }
    }

    /// Whether the currently-selected tab is a repo-scoped shared service —
    /// true on the shared row, and also when a shared service is selected as
    /// a trailing tab on a stack. Routes logs/scroll/commands to the shared
    /// daemon's state rather than the instance's.
    fn selected_is_shared(&self) -> bool {
        if self.shared_selected {
            return true;
        }
        match self.selected_service() {
            Some(svc) => self.shared.services.iter().any(|s| s.name == svc.name),
            None => false,
        }
    }

    /// Buffered log lines for `service`. A repo-scoped shared service always
    /// resolves to the shared daemon's buffer (whether reached via the shared
    /// row or as a trailing tab on a stack); everything else to the instance.
    pub fn service_logs(&self, service: &str) -> &VecDeque<String> {
        static EMPTY: std::sync::OnceLock<VecDeque<String>> = std::sync::OnceLock::new();
        let is_shared = self.shared.services.iter().any(|s| s.name == service);
        if is_shared {
            self.shared.logs.get(service)
        } else {
            self.current_instance().and_then(|i| i.logs.get(service))
        }
        .unwrap_or_else(|| EMPTY.get_or_init(VecDeque::new))
    }

    /// Lines-from-bottom scroll offset for the selected service. 0 = tail.
    pub fn log_scroll_offset(&self) -> usize {
        let Some(svc) = self.selected_service() else {
            return 0;
        };
        let name = &svc.name;
        if self.selected_is_shared() {
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
        if self.selected_is_shared() {
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

    // ── mouse hit-testing ────────────────────────────────────────────────
    //
    // The renderer is the only place that knows where things land on screen,
    // so it records clickable regions (sidebar rows, tabs) and the scrollbar
    // track each frame; the event loop hit-tests pointer clicks against them.
    // Everything is rebuilt per frame, so stale geometry can't accumulate.

    /// Clear the per-frame hit map. Called at the top of every render pass.
    /// The drag flag is deliberately *not* cleared — a drag spans frames.
    pub fn begin_frame_hits(&mut self) {
        self.click_regions.clear();
        self.scrollbar_hit = None;
        self.sidebar_divider = None;
    }

    /// Record a clickable region (screen cells) and what activating it does.
    pub fn push_click_region(&mut self, x: u16, y: u16, w: u16, h: u16, target: ClickTarget) {
        self.click_regions.push(ClickRegion { x, y, w, h, target });
    }

    /// Record the log scrollbar's track geometry for click/drag-to-scroll.
    pub fn set_scrollbar_hit(&mut self, x: u16, y: u16, h: u16, content_len: usize, viewport: usize) {
        self.scrollbar_hit = Some(ScrollbarHit { x, y, h, content_len, viewport });
    }

    /// Whether any modal overlay is up. Clicks shouldn't fall through a modal
    /// to the chrome rendered underneath it.
    pub fn any_modal_open(&self) -> bool {
        self.help_visible
            || self.skill_dialog.is_some()
            || self.quit_confirm
            || self.settings.is_some()
            || self.port_conflict.is_some()
            || self.notif_visible
    }

    /// Apply a left-click at screen `(col, row)` to the recorded hit map.
    /// Returns true if it landed on a clickable region (so the caller knows
    /// the click was consumed). Selecting mirrors the keyboard nav exactly —
    /// it just sets the same selection state, no extra side effects.
    pub fn click_at(&mut self, col: u16, row: u16) -> bool {
        let Some(region) = self.click_regions.iter().find(|r| r.contains(col, row)) else {
            return false;
        };
        match region.target {
            ClickTarget::Stack(i) => {
                self.shared_selected = false;
                self.selected_instance = Some(i);
            }
            ClickTarget::Shared => {
                self.shared_selected = true;
            }
            ClickTarget::Tab(i) => {
                self.set_active_selected_service(Some(i));
            }
            // Notification rows are clickable only while the modal is open,
            // which routes through `notif_copy_at` (it copies, with a side
            // effect `click_at` deliberately avoids). Nothing to do here.
            ClickTarget::Notif(_) => return false,
        }
        true
    }

    /// Whether `(col, row)` falls on the log scrollbar track.
    pub fn scrollbar_at(&self, col: u16, row: u16) -> bool {
        match self.scrollbar_hit {
            Some(h) => col == h.x && row >= h.y && row < h.y + h.h,
            None => false,
        }
    }

    pub fn scrollbar_dragging(&self) -> bool {
        self.scrollbar_dragging
    }

    pub fn begin_scrollbar_drag(&mut self) {
        self.scrollbar_dragging = true;
    }

    pub fn end_scrollbar_drag(&mut self) {
        self.scrollbar_dragging = false;
    }

    /// Map a pointer `row` on the scrollbar track to a scroll offset and apply
    /// it to the selected service. Top of the track is the oldest line
    /// (`max` offset), bottom is the live tail (offset 0).
    pub fn scrollbar_drag_to(&mut self, row: u16) {
        let Some(hit) = self.scrollbar_hit else {
            return;
        };
        let max = hit.content_len.saturating_sub(hit.viewport);
        if max == 0 {
            return;
        }
        let offset = if hit.h <= 1 {
            // Degenerate track — snap to whichever end the row is nearer.
            if row <= hit.y { max } else { 0 }
        } else {
            let rel = row.saturating_sub(hit.y).min(hit.h - 1);
            // fraction from top: 0.0 at the top row, 1.0 at the bottom row.
            let frac = f64::from(rel) / f64::from(hit.h - 1);
            ((max as f64) * (1.0 - frac)).round() as usize
        };
        self.set_scroll_for_selected(offset.min(max));
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
        let total = if self.selected_is_shared() {
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
                self.had_live_daemon = true;
                let idx = self.upsert_instance(instance);
                let inst = &mut self.instances[idx];
                inst.services = services;
                inst.steps = steps;
                if inst.selected_service.is_none() && !inst.services.is_empty() {
                    inst.selected_service = Some(0);
                }
            }
            // A daemon shut down deliberately (`devme down` / quit). Drop its
            // row; the event loop exits once no live daemon remains. A crash
            // closes the socket *without* a Goodbye and never lands here, so
            // the row sticks around and the TUI keeps watching for a restart.
            ServerMessage::Goodbye { .. } => {
                self.remove_instance(source_id);
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
                    // Carry the resolved port forward: the daemon may not know
                    // it yet in the initial `Subscribed` snapshot (service not
                    // spawned), then send it on the first StatusUpdate. Persist
                    // it on the snapshot so `o`/`c`/`url` see a real port, not
                    // just the toast below.
                    port = port.or(s.port);
                    s.port = port;
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
                    s.port = port;
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
            // Shared daemon stopped — clear its row. Instance Goodbyes drive
            // the actual TUI exit; this just keeps the shared view honest.
            ServerMessage::Goodbye { .. } => {
                self.shared = SharedData::default();
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
        if self.selected_is_shared() {
            let id = self.shared.id.clone()?;
            Some((id, name))
        } else {
            let inst = self.current_instance()?;
            Some((inst.info.id.clone(), name))
        }
    }

    /// Horizontal navigation across service tabs (`h`/`l` / `←`/`→`). An unset
    /// selection counts as the default focus (tab 0), so the first press steps
    /// off it rather than onto it.
    pub fn select_next_service(&mut self) {
        let total = self.active_service_count();
        if total == 0 {
            return;
        }
        let cur = self.active_selected_service_idx().unwrap_or(0);
        self.set_active_selected_service(Some((cur + 1) % total));
    }

    pub fn select_prev_service(&mut self) {
        let total = self.active_service_count();
        if total == 0 {
            return;
        }
        let cur = self.active_selected_service_idx().unwrap_or(0);
        let prev = if cur == 0 { total - 1 } else { cur - 1 };
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
            url: None,
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

    fn running(name: &str) -> ServiceSnapshot {
        ServiceSnapshot {
            name: name.into(),
            state: ServiceState::Running { degraded: false, started_without: vec![] },
            pid: None,
            port: None,
            url: None,
            restart_count: 0,
        }
    }

    /// A stack with 3 owned services that also depends on 2 repo-scoped
    /// shared services. The instance daemon reports all 5 (the 2 shared ones
    /// as stubs); the shared daemon reports the 2 shared ones.
    fn stack_with_shared() -> TuiState {
        let mut s = TuiState::default();
        // Shared daemon first, so the instance's stub names are recognised.
        s.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "shared::repo".into(),
                label: "shared".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![running("db"), running("cache")],
            steps: vec![],
        });
        s.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "inst".into(),
                label: "feature/a".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![
                running("api"),
                running("web"),
                running("worker"),
                running("db"),
                running("cache"),
            ],
            steps: vec![],
        });
        s
    }

    #[test]
    fn stack_count_excludes_shared_services() {
        let s = stack_with_shared();
        // Owned-only: 3/3, not 5/5.
        assert_eq!(
            s.instance_service_summary(0),
            StackSummary::Counted { up: 3, total: 3 }
        );
    }

    #[test]
    fn stack_owning_only_shared_services_reads_shared_only() {
        let mut s = TuiState::default();
        s.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "shared::repo".into(),
                label: "shared".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![running("db"), running("cache")],
            steps: vec![],
        });
        // Worktree's daemon reports only the two shared stubs — owns nothing.
        s.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "inst".into(),
                label: "feature/a".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![running("db"), running("cache")],
            steps: vec![],
        });
        assert_eq!(s.instance_service_summary(0), StackSummary::SharedOnly);
    }

    #[test]
    fn worktree_without_devme_toml_leads_with_placeholder_tab() {
        let mut s = TuiState::default();
        s.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "shared::repo".into(),
                label: "shared".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![running("proxy"), running("postgres")],
            steps: vec![],
        });
        // A discovered worktree with no devme.toml: a placeholder row, no daemon.
        s.add_placeholder_instance("inst", "feature/x", "/tmp/a");

        let tabs = s.tab_services();
        let labels: Vec<&str> = tabs.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["no devme.toml", "proxy", "postgres"]);
        assert!(tabs[0].is_placeholder());
        assert!(tabs[1].is_shared() && tabs[2].is_shared());

        // The placeholder is the default focus and backs no service.
        assert!(s.selected_tab_is_placeholder());
        assert!(s.selected_service().is_none());
        assert_eq!(s.selected_tab_index(), 0);

        // One step right leaves the placeholder for the first shared service.
        s.select_next_service();
        assert!(!s.selected_tab_is_placeholder());
        assert_eq!(s.selected_service().unwrap().name, "proxy");
        // Acting on it routes to the shared daemon.
        assert_eq!(
            s.selected_instance_and_service(),
            Some(("shared::repo".into(), "proxy".into()))
        );
    }

    #[test]
    fn placeholder_explanation_mentions_shared_tabs_only_when_present() {
        // No devme.toml and no shared services: just the "add one" line.
        let mut s = TuiState::default();
        s.add_placeholder_instance("inst", "feature/x", "/tmp/x");
        let without = s.placeholder_explanation();
        assert!(without.contains("add one to start services"), "{without}");
        assert!(
            !without.contains("tabs to the right"),
            "should not mention shared tabs when there are none:\n{without}"
        );

        // With shared services, the trailing-tabs hint is appended.
        let mut s2 = TuiState::default();
        s2.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "shared::repo".into(),
                label: "shared".into(),
                cwd: "/tmp/x".into(),
            },
            services: vec![running("proxy")],
            steps: vec![],
        });
        s2.add_placeholder_instance("inst", "feature/x", "/tmp/x");
        let with = s2.placeholder_explanation();
        assert!(with.contains("tabs to the right"), "{with}");
    }

    #[test]
    fn placeholder_instance_summary_reads_no_daemon_but_is_flagged_placeholder() {
        let mut s = TuiState::default();
        s.add_placeholder_instance("inst", "feature/x", "/tmp/x");
        assert_eq!(s.instance_service_summary(0), StackSummary::NoDaemon);
        assert!(s.instance_is_placeholder(0));
    }

    #[test]
    fn stack_owning_only_shared_services_labels_placeholder_no_services() {
        let mut s = TuiState::default();
        s.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "shared::repo".into(),
                label: "shared".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![running("db")],
            steps: vec![],
        });
        // Daemon is up but reports only the shared stub — has a devme.toml.
        s.apply(ServerMessage::Subscribed {
            instance: InstanceInfo {
                id: "inst".into(),
                label: "feature/a".into(),
                cwd: "/tmp/a".into(),
            },
            services: vec![running("db")],
            steps: vec![],
        });
        let tabs = s.tab_services();
        assert!(tabs[0].is_placeholder());
        assert_eq!(tabs[0].label, "no services");
    }

    #[test]
    fn stack_tabs_list_owned_then_shared_group() {
        let s = stack_with_shared();
        let tabs = s.tab_services();
        let names: Vec<&str> = tabs.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(names, ["api", "web", "worker", "db", "cache"]);
        let shared: Vec<bool> = tabs.iter().map(|t| t.is_shared()).collect();
        assert_eq!(shared, [false, false, false, true, true]);
    }

    #[test]
    fn shared_tabs_are_reachable_and_route_to_shared_daemon() {
        let mut s = stack_with_shared();
        // selection starts on the first owned service
        assert_eq!(s.selected_service().unwrap().name, "api");
        // step into the shared group (indices 3, 4)
        for _ in 0..3 {
            s.select_next_service();
        }
        assert_eq!(s.selected_service().unwrap().name, "db");
        // commands for a shared tab route to the shared daemon, not the stack
        let (id, name) = s.selected_instance_and_service().unwrap();
        assert_eq!(id, "shared::repo");
        assert_eq!(name, "db");
        // and wrap-around spans the whole 5-tab row
        s.select_next_service();
        assert_eq!(s.selected_service().unwrap().name, "cache");
        s.select_next_service();
        assert_eq!(s.selected_service().unwrap().name, "api");
    }

    #[test]
    fn owned_tab_still_routes_to_instance_daemon() {
        let s = stack_with_shared();
        let (id, name) = s.selected_instance_and_service().unwrap();
        assert_eq!(id, "inst");
        assert_eq!(name, "api");
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
    fn notification_history_outlives_corner_toasts() {
        let mut s = TuiState::default();
        // More than MAX_TOASTS notifications: the corner stack is capped, but
        // the history modal's scrollback retains all of them.
        for i in 0..(MAX_TOASTS + 3) {
            s.notify("open", format!("note {i}"));
        }
        assert_eq!(s.toasts().len(), MAX_TOASTS);
        assert_eq!(s.notifications().len(), MAX_TOASTS + 3);
        // Newest is last in history; rendered reversed by the modal.
        assert_eq!(s.notifications().last().unwrap().body, format!("note {}", MAX_TOASTS + 2));
    }

    #[test]
    fn notification_modal_cursor_clamps_and_copies() {
        let mut s = TuiState::default();
        for i in 0..3 {
            s.notify("svc", format!("event {i}"));
        }
        assert!(!s.notifications_visible());
        s.toggle_notifications();
        assert!(s.notifications_visible());
        // Cursor starts on the newest entry (display index 0). Copied text
        // carries the relative age for context, e.g. "[0s ago] svc: event 2".
        assert_eq!(s.notif_cursor(), 0);
        let newest = s.notif_selected_text().unwrap();
        assert!(newest.contains("svc: event 2"), "got {newest:?}");
        assert!(newest.starts_with("[0s ago]"), "missing age prefix: {newest:?}");

        // Cursor never runs past the oldest…
        s.notif_cursor_down(99);
        assert_eq!(s.notif_cursor(), 2); // len 3 → max index 2
        assert!(s.notif_selected_text().unwrap().contains("svc: event 0"));
        // …and never before the newest.
        s.notif_cursor_up(99);
        assert_eq!(s.notif_cursor(), 0);

        // Copy-all is newest-first, one line each.
        let all = s.notif_all_text();
        let bodies: Vec<&str> = all.lines().map(|l| l.trim()).collect();
        assert_eq!(bodies.len(), 3);
        assert!(bodies[0].contains("svc: event 2"));
        assert!(bodies[1].contains("svc: event 1"));
        assert!(bodies[2].contains("svc: event 0"));

        s.close_notifications();
        assert!(!s.notifications_visible());
    }

    #[test]
    fn transient_notifications_show_but_are_not_recorded() {
        let mut s = TuiState::default();
        s.notify("config", "parse warning"); // durable event
        s.notify_transient("copy", "copied url"); // ephemeral ack
        // Both appear as corner toasts…
        assert_eq!(s.toasts().len(), 2);
        // …but only the durable one enters the scrollback the modal shows.
        assert_eq!(s.notifications().len(), 1);
        assert_eq!(s.notifications()[0].body, "parse warning");
    }

    #[test]
    fn notif_click_copies_the_clicked_row() {
        let mut s = TuiState::default();
        for i in 0..3 {
            s.notify("svc", format!("event {i}"));
        }
        s.toggle_notifications();
        // Simulate the renderer registering rows: display index 0 (newest) at
        // row 1, index 1 at row 2, index 2 at row 3.
        s.push_click_region(0, 1, 60, 1, ClickTarget::Notif(0));
        s.push_click_region(0, 2, 60, 1, ClickTarget::Notif(1));
        s.push_click_region(0, 3, 60, 1, ClickTarget::Notif(2));

        // Click the middle row → copies it and moves the cursor there.
        let copied = s.notif_copy_at(10, 2).unwrap();
        assert!(copied.contains("svc: event 1"), "got {copied:?}");
        assert_eq!(s.notif_cursor(), 1);
        // A click that misses every row copies nothing.
        assert_eq!(s.notif_copy_at(10, 9), None);
    }

    #[test]
    fn status_update_carries_port_onto_the_snapshot() {
        // The daemon often doesn't know a service's port in the initial
        // `Subscribed` snapshot (not spawned yet) and sends it on the first
        // StatusUpdate. The TUI must persist that port on the snapshot so
        // `o`/`c`/`url` resolve a real port — regression for the bug where the
        // handler used the port only for its toast and dropped it.
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["web"]));
        assert_eq!(s.selected_service().unwrap().port, None);

        s.apply(ServerMessage::StatusUpdate {
            service: "web".into(),
            state: ServiceState::Running { degraded: false, started_without: vec![] },
            pid: Some(123),
            port: Some(3030),
            restart_count: 0,
        });
        assert_eq!(s.selected_service().unwrap().port, Some(3030));

        // A later update that omits the port (e.g. a plain state change) must
        // not wipe the known port back to None.
        s.apply(ServerMessage::StatusUpdate {
            service: "web".into(),
            state: ServiceState::Stopped,
            pid: None,
            port: None,
            restart_count: 0,
        });
        assert_eq!(s.selected_service().unwrap().port, Some(3030));
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
    fn sidebar_divider_drag_resizes_within_bounds() {
        let mut s = TuiState::default();
        assert_eq!(s.sidebar_width(), DEFAULT_SIDEBAR_WIDTH);
        // Divider recorded for an 80-col frame, divider at column 28.
        s.set_sidebar_divider(28, 0, 24, 80);
        // The hit zone is two columns wide (divider + gutter to its left).
        assert!(s.sidebar_divider_at(28, 5));
        assert!(s.sidebar_divider_at(27, 5));
        assert!(!s.sidebar_divider_at(26, 5));
        assert!(!s.sidebar_divider_at(28, 30)); // outside the vertical track

        // Dragging the divider sets the width to the pointer column.
        s.sidebar_drag_to(40);
        assert_eq!(s.sidebar_width(), 40);

        // Clamped at the narrow end…
        s.sidebar_drag_to(2);
        assert_eq!(s.sidebar_width(), MIN_SIDEBAR_WIDTH);
        // …and at the wide end, leaving the main pane room.
        s.sidebar_drag_to(79);
        assert_eq!(s.sidebar_width(), 80 - MIN_MAIN_WIDTH);
    }

    #[test]
    fn settings_overlay_cycles_and_toggles() {
        let mut s = TuiState::default();
        s.open_settings();
        assert!(s.settings_visible());

        // Row 0 is the theme choice (mocha → latte → auto → mocha).
        assert_eq!(s.settings().unwrap().values[0], "mocha");
        let change = s.settings_change(1).unwrap();
        assert_eq!(change, SettingWrite::Set { key: "tui.theme", value: "latte".into() });
        assert_eq!(s.settings().unwrap().values[0], "latte");
        // Live application swaps the palette.
        assert_eq!(*s.palette(), crate::theme::Palette::latte());

        // Row 1 is the notifications toggle (default on → off).
        s.settings_move(1);
        assert_eq!(s.settings().unwrap().cursor, 1);
        let change = s.settings_change(1).unwrap();
        assert_eq!(change, SettingWrite::Set { key: "tui.toasts", value: "false".into() });

        s.close_settings();
        assert!(!s.settings_visible());
    }

    #[test]
    fn docker_daemon_auto_choice_unsets_the_key() {
        let mut s = TuiState::default();
        s.open_settings();
        // Jump to the docker.daemon row (last setting).
        s.settings_move(-1);
        assert_eq!(s.settings().unwrap().cursor, SETTINGS.len() - 1);
        // Defaults to `auto` (the unset sentinel).
        let row = s.settings().unwrap().cursor;
        assert_eq!(s.settings().unwrap().values[row], "auto");
        // Forward to a real daemon → a Set.
        let change = s.settings_change(1).unwrap();
        assert_eq!(change, SettingWrite::Set { key: "docker.daemon", value: "orbstack".into() });
        // Back to `auto` → an Unset.
        let change = s.settings_change(-1).unwrap();
        assert_eq!(change, SettingWrite::Unset { key: "docker.daemon" });
    }

    #[test]
    fn disabling_toasts_suppresses_transition_notifications() {
        let running = || ServiceState::Running { degraded: false, started_without: vec![] };
        let mut s = TuiState::default();
        s.set_config({
            let mut c = GlobalConfig::default();
            c.set("tui.toasts", "false").unwrap();
            c
        });
        s.apply(ServerMessage::Subscribed {
            instance: test_instance(),
            services: vec![ServiceSnapshot { state: running(), ..svc("api") }],
            steps: vec![],
        });
        // A crash that would normally raise a toast stays silent.
        s.apply(ServerMessage::StatusUpdate {
            service: "api".into(),
            state: ServiceState::Failed { exit_code: Some(1) },
            pid: None,
            port: None,
            restart_count: 0,
        });
        assert!(s.toasts().is_empty(), "toasts should be suppressed when disabled");
        // But a config-parse warning still surfaces (it bypasses the gate).
        s.push_config_warning("broken".into());
        assert_eq!(s.toasts().len(), 1);
    }

    #[test]
    fn confirm_quit_reads_config_and_toggles_modal() {
        let mut s = TuiState::default();
        // Default-on: an unset key means "confirm before quitting".
        assert!(s.confirm_quit_enabled());
        // Explicit opt-out disables it.
        s.set_config({
            let mut c = GlobalConfig::default();
            c.set("tui.confirm_quit", "false").unwrap();
            c
        });
        assert!(!s.confirm_quit_enabled());
        // Re-enabling restores the confirm gate.
        s.set_config({
            let mut c = GlobalConfig::default();
            c.set("tui.confirm_quit", "true").unwrap();
            c
        });
        assert!(s.confirm_quit_enabled());
        assert!(!s.quit_confirm_visible());
        s.open_quit_confirm();
        assert!(s.quit_confirm_visible());
        s.cancel_quit_confirm();
        assert!(!s.quit_confirm_visible());
    }

    #[test]
    fn toggle_zoom_flips_fullscreen_logs() {
        let mut s = TuiState::default();
        assert!(!s.zoom());
        s.toggle_zoom();
        assert!(s.zoom());
        s.exit_zoom();
        assert!(!s.zoom());
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
    fn git_refresh_relabels_on_branch_checkout() {
        // The worktree row starts labelled "test"; a background refresh that
        // reports a new branch re-labels it in place and updates the counts.
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["api"]));
        let idx = s.find_instance("test-id").unwrap();
        assert_eq!(s.instances()[idx], "test");

        s.apply_git_refresh("test-id", Some("feature/x".into()), Some((2, 0)));
        assert_eq!(s.instances()[idx], "feature/x");
        assert_eq!(s.instance_ahead_behind(idx), Some((2, 0)));

        // Switching to a branch with no upstream clears the stale counts but
        // still re-labels.
        s.apply_git_refresh("test-id", Some("main".into()), None);
        assert_eq!(s.instances()[idx], "main");
        assert_eq!(s.instance_ahead_behind(idx), None);
    }

    #[test]
    fn git_refresh_keeps_label_when_branch_unknown() {
        // A detached HEAD / transient git failure (branch = None) must not
        // blank the last-known label.
        let mut s = TuiState::default();
        s.apply(snapshot_msg(&["api"]));
        let idx = s.find_instance("test-id").unwrap();
        s.apply_git_refresh("test-id", None, None);
        assert_eq!(s.instances()[idx], "test");
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
    fn click_selects_stack_and_shared() {
        let mut s = TuiState::default();
        // Two stack rows (2 tall each) then a shared row, as the sidebar lays
        // them out.
        s.push_click_region(0, 1, 22, 2, ClickTarget::Stack(0));
        s.push_click_region(0, 3, 22, 2, ClickTarget::Stack(1));
        s.push_click_region(0, 6, 22, 2, ClickTarget::Shared);

        assert!(s.click_at(5, 4)); // inside Stack(1)
        assert_eq!(s.selected_instance_index(), Some(1));
        assert!(!s.shared_selected());

        assert!(s.click_at(0, 6)); // shared row
        assert!(s.shared_selected());

        assert!(!s.click_at(50, 50)); // nothing there
    }

    #[test]
    fn begin_frame_hits_clears_hit_map() {
        let mut s = TuiState::default();
        s.push_click_region(0, 0, 10, 2, ClickTarget::Stack(0));
        s.set_scrollbar_hit(40, 0, 10, 100, 20);
        s.begin_frame_hits();
        assert!(!s.click_at(1, 1)); // region gone
        assert!(!s.scrollbar_at(40, 3)); // scrollbar gone
    }

    #[test]
    fn scrollbar_at_matches_track_column_and_rows() {
        let mut s = TuiState::default();
        s.set_scrollbar_hit(40, 2, 10, 100, 20);
        assert!(s.scrollbar_at(40, 2)); // top of track
        assert!(s.scrollbar_at(40, 11)); // bottom of track (y+h-1)
        assert!(!s.scrollbar_at(40, 12)); // just below
        assert!(!s.scrollbar_at(41, 5)); // wrong column
        assert!(!s.scrollbar_at(40, 1)); // just above
    }

    #[test]
    fn scrollbar_drag_maps_top_to_oldest_bottom_to_tail() {
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
        // 50 lines, viewport 20 → max scroll 30. Track spans rows 0..10.
        s.set_scrollbar_hit(40, 0, 10, 50, 20);

        s.scrollbar_drag_to(0); // top → oldest
        assert_eq!(s.log_scroll_offset(), 30);
        s.scrollbar_drag_to(9); // bottom → live tail
        assert_eq!(s.log_scroll_offset(), 0);
        s.scrollbar_drag_to(4); // ~middle, lands between the ends
        let mid = s.log_scroll_offset();
        assert!((1..30).contains(&mid), "mid offset {mid} not strictly between ends");
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

    #[test]
    fn fresh_state_never_signals_shutdown() {
        // No daemon ever attached → nothing to shut down, never auto-exit.
        let s = TuiState::default();
        assert!(!s.all_daemons_shut_down());
    }

    #[test]
    fn placeholder_only_sidebar_does_not_signal_shutdown() {
        // A discovered worktree with no daemon doesn't count as a live daemon,
        // so the TUI stays up waiting rather than exiting on launch.
        let mut s = TuiState::default();
        s.add_placeholder_instance("place", "feature/b", "/tmp/b");
        assert!(!s.all_daemons_shut_down());
    }

    #[test]
    fn goodbye_removes_instance_and_signals_shutdown_when_last() {
        let mut s = TuiState::default();
        s.apply_from(
            "A",
            ServerMessage::Subscribed {
                instance: InstanceInfo { id: "A".into(), label: "a".into(), cwd: "/a".into() },
                services: vec![svc("api")],
                steps: vec![],
            },
        );
        assert!(s.has_instance("A"));
        assert!(!s.all_daemons_shut_down(), "live daemon present");

        // The daemon shuts down deliberately → Goodbye drops the row, and
        // with no daemon left the TUI should exit.
        s.apply_from("A", ServerMessage::Goodbye { reason: "shutdown requested".into() });
        assert!(!s.has_instance("A"), "Goodbye should drop the instance row");
        assert!(s.all_daemons_shut_down());
    }

    #[test]
    fn goodbye_for_one_worktree_keeps_tui_up_while_another_lives() {
        let mut s = TuiState::default();
        for id in ["A", "B"] {
            s.apply_from(
                id,
                ServerMessage::Subscribed {
                    instance: InstanceInfo { id: id.into(), label: id.into(), cwd: format!("/{id}") },
                    services: vec![svc("api")],
                    steps: vec![],
                },
            );
        }
        // Down one worktree — the other is still live, so don't exit.
        s.apply_from("A", ServerMessage::Goodbye { reason: "shutdown requested".into() });
        assert!(!s.has_instance("A"));
        assert!(s.has_instance("B"));
        assert!(!s.all_daemons_shut_down());

        // Down the last one — now exit.
        s.apply_from("B", ServerMessage::Goodbye { reason: "shutdown requested".into() });
        assert!(s.all_daemons_shut_down());
    }

    #[test]
    fn crashed_daemon_does_not_signal_shutdown() {
        // A crash closes the socket without a Goodbye: the row stays, its
        // service keeps a (failed) state, and the TUI keeps running so it can
        // re-attach when the daemon restarts.
        let mut s = TuiState::default();
        s.apply_from(
            "A",
            ServerMessage::Subscribed {
                instance: InstanceInfo { id: "A".into(), label: "a".into(), cwd: "/a".into() },
                services: vec![svc("api")],
                steps: vec![],
            },
        );
        s.apply_from(
            "A",
            ServerMessage::StatusUpdate {
                service: "api".into(),
                state: ServiceState::Failed { exit_code: Some(1) },
                pid: None,
                port: None,
                restart_count: 0,
            },
        );
        // No Goodbye was sent — instance remains, so we keep watching.
        assert!(s.has_instance("A"));
        assert!(!s.all_daemons_shut_down());
    }

    #[test]
    fn external_down_parks_in_stopped_then_clears_on_reattach() {
        let mut s = TuiState::default();
        // A live daemon attaches…
        s.apply_from(
            "A",
            ServerMessage::Subscribed {
                instance: InstanceInfo { id: "A".into(), label: "a".into(), cwd: "/a".into() },
                services: vec![svc("api")],
                steps: vec![],
            },
        );
        // …then `devme down` elsewhere drains it.
        s.apply_from("A", ServerMessage::Goodbye { reason: "down".into() });
        assert!(s.all_daemons_shut_down());
        assert!(!s.stopped(), "not stopped until the event loop enters it");

        s.enter_stopped(Some("kpi-dash".into()));
        assert!(s.stopped());
        assert_eq!(s.stopped_repo(), Some("kpi-dash"));

        // A fresh `devme up` reattaches a daemon → no longer all-shut-down,
        // and the event loop clears the stopped state.
        s.apply_from(
            "A",
            ServerMessage::Subscribed {
                instance: InstanceInfo { id: "A".into(), label: "a".into(), cwd: "/a".into() },
                services: vec![svc("api")],
                steps: vec![],
            },
        );
        assert!(!s.all_daemons_shut_down());
        s.clear_stopped();
        assert!(!s.stopped());
        assert_eq!(s.stopped_repo(), None);
    }

    #[test]
    fn entering_stopped_dismisses_open_overlays() {
        let mut s = TuiState::default();
        s.open_settings();
        s.toggle_help();
        assert!(s.settings_visible() || s.help_visible());
        s.enter_stopped(None);
        assert!(!s.settings_visible());
        assert!(!s.help_visible());
        assert!(s.stopped());
    }
}
