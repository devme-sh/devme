//! The TUI's main-view keymap — the single source of truth for keybindings.
//!
//! Every main-view shortcut is an [`Action`]. The [`BINDINGS`] table maps each
//! action to the key label and description shown in the help overlay, and
//! [`resolve`] maps a real key event to its action. The event loop dispatches
//! through `resolve` and a `match action`, and the help overlay renders from
//! `BINDINGS`, so the two can't drift: the dispatch `match` is exhaustive (a
//! new `Action` won't compile until it's handled) and [`tests`] assert every
//! action is both documented in `BINDINGS` and reachable from `resolve`. Add a
//! shortcut → you're forced to give it a help entry.
//!
//! This covers the *main view* only. Modal sub-handlers (copy-mode, zoom,
//! settings, quit-confirm, port-conflict, skill prompt) own their own keys and
//! are self-documenting within their overlays.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A main-view action a key can trigger. One variant per distinct behavior.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Action {
    // navigation
    NextService,
    PrevService,
    NextStack,
    PrevStack,
    ToggleSidebar,
    // log viewport
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    LineDown,
    LineUp,
    ScrollTop,
    ScrollBottom,
    CopyVisibleLogs,
    CopyAllLogs,
    CopyDebugPrompt,
    CopyMode,
    ZoomLogs,
    // service actions
    StartService,
    StopService,
    RestartService,
    OpenUrl,
    CopyUrl,
    // worktrees
    AddWorktree,
    RemoveWorktree,
    // session
    StackInfo,
    Settings,
    Notifications,
    ReloadConfig,
    Detach,
    Quit,
    ToggleHelp,
}

/// Help-overlay grouping. Order here is the order sections render.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Navigation,
    LogViewport,
    ServiceActions,
    Worktrees,
    Session,
}

impl Section {
    pub const ORDER: [Section; 5] = [
        Section::Navigation,
        Section::LogViewport,
        Section::ServiceActions,
        Section::Worktrees,
        Section::Session,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Navigation => "navigation",
            Section::LogViewport => "log viewport",
            Section::ServiceActions => "service actions",
            Section::Worktrees => "worktrees",
            Section::Session => "session",
        }
    }
}

/// A terse hint shown in the bottom status bar — a compact key label and a
/// one-word action, distinct from the help overlay's fuller `keys`/`desc`
/// (the footer says `o open`, the overlay says `o / c — open / copy URL`).
pub struct FooterHint {
    pub keys: &'static str,
    pub label: &'static str,
}

/// One help line: a key label, its description, and the action(s) it documents.
/// A line can cover more than one action (e.g. `o / c` → open + copy) so the
/// overlay stays compact while still documenting every action. `footer` opts
/// the binding into the bottom status bar (most are overlay-only).
pub struct Binding {
    pub keys: &'static str,
    pub desc: &'static str,
    pub section: Section,
    pub actions: &'static [Action],
    pub footer: Option<FooterHint>,
}

/// The bottom-bar hints, in `BINDINGS` order — the single source the footer
/// renders from, so a binding flagged `footer` can't go missing from the bar.
pub fn footer_hints() -> impl Iterator<Item = &'static FooterHint> {
    BINDINGS.iter().filter_map(|b| b.footer.as_ref())
}

/// One non-keyboard hint: a pointer label and what it does.
pub struct MouseNote {
    pub label: &'static str,
    pub desc: &'static str,
}

/// Mouse behaviors shown in the help overlay's `mouse` section. These are
/// pointer interactions, not key chords, so they have no [`Action`] and live
/// here rather than in [`BINDINGS`] — but the overlay's single source of truth
/// should still cover them, since shift+drag in particular is invisible
/// otherwise. Clicks (select a row/tab, drag the scrollbar) are handled by the
/// event loop's mouse arm; the wheel scrolls; shift+drag is handled by the
/// terminal *emulator* (we capture the mouse, so it intercepts shift+drag
/// before it reaches us). `v` copy mode is the companion for clean multi-line
/// selection.
pub const MOUSE_NOTES: &[MouseNote] = &[
    MouseNote {
        label: "click",
        desc: "select stack / service tab",
    },
    MouseNote {
        label: "wheel",
        desc: "scroll logs (over tab row: scroll tabs)",
    },
    MouseNote {
        label: "drag bar",
        desc: "scrollbar → jump to position",
    },
    MouseNote {
        label: "shift+drag",
        desc: "select text (v = clean multi-line)",
    },
];

/// The source of truth for the help overlay. Every [`Action`] must appear in
/// some entry's `actions` (enforced by `tests::every_action_is_documented`).
pub const BINDINGS: &[Binding] = &[
    // navigation
    Binding {
        keys: "←→ / hl",
        desc: "service tab",
        section: Section::Navigation,
        actions: &[Action::PrevService, Action::NextService],
        footer: Some(FooterHint {
            keys: "hl",
            label: "svc",
        }),
    },
    Binding {
        keys: "↑↓ / jk",
        desc: "stack",
        section: Section::Navigation,
        actions: &[Action::PrevStack, Action::NextStack],
        footer: Some(FooterHint {
            keys: "jk",
            label: "stack",
        }),
    },
    Binding {
        keys: "`",
        desc: "collapse / expand sidebar",
        section: Section::Navigation,
        actions: &[Action::ToggleSidebar],
        footer: None,
    },
    // log viewport
    Binding {
        keys: "b / space",
        desc: "page up / down (f also pages down)",
        section: Section::LogViewport,
        actions: &[Action::PageUp, Action::PageDown],
        footer: None,
    },
    Binding {
        keys: "^u / ^d",
        desc: "half-page up / down",
        section: Section::LogViewport,
        actions: &[Action::HalfPageUp, Action::HalfPageDown],
        footer: None,
    },
    Binding {
        keys: "J / K",
        desc: "scroll one line",
        section: Section::LogViewport,
        actions: &[Action::LineDown, Action::LineUp],
        footer: None,
    },
    Binding {
        keys: "g / G",
        desc: "top / live tail",
        section: Section::LogViewport,
        actions: &[Action::ScrollTop, Action::ScrollBottom],
        footer: None,
    },
    Binding {
        keys: "y / Y",
        desc: "copy visible / all logs",
        section: Section::LogViewport,
        actions: &[Action::CopyVisibleLogs, Action::CopyAllLogs],
        footer: None,
    },
    Binding {
        keys: "p",
        desc: "copy debug prompt to clipboard",
        section: Section::LogViewport,
        actions: &[Action::CopyDebugPrompt],
        footer: None,
    },
    Binding {
        keys: "v",
        desc: "copy mode (select text)",
        section: Section::LogViewport,
        actions: &[Action::CopyMode],
        footer: None,
    },
    Binding {
        keys: "z",
        desc: "zoom logs (fullscreen)",
        section: Section::LogViewport,
        actions: &[Action::ZoomLogs],
        footer: None,
    },
    // service actions
    Binding {
        keys: "S / s / r",
        desc: "start / stop / restart selected",
        section: Section::ServiceActions,
        actions: &[
            Action::StartService,
            Action::StopService,
            Action::RestartService,
        ],
        footer: Some(FooterHint {
            keys: "S/s/r",
            label: "start/stop/restart",
        }),
    },
    Binding {
        keys: "o / c",
        desc: "open / copy service URL",
        section: Section::ServiceActions,
        actions: &[Action::OpenUrl, Action::CopyUrl],
        footer: Some(FooterHint {
            keys: "o",
            label: "open",
        }),
    },
    // worktrees
    Binding {
        keys: "w",
        desc: "new worktree (prompts for branch)",
        section: Section::Worktrees,
        actions: &[Action::AddWorktree],
        footer: None,
    },
    Binding {
        keys: "x",
        desc: "remove selected worktree (confirms)",
        section: Section::Worktrees,
        actions: &[Action::RemoveWorktree],
        footer: None,
    },
    // session
    Binding {
        keys: "i",
        desc: "stack info (branch / path / slot / PR)",
        section: Section::Session,
        actions: &[Action::StackInfo],
        footer: None,
    },
    Binding {
        keys: ",",
        desc: "settings",
        section: Section::Session,
        actions: &[Action::Settings],
        footer: None,
    },
    Binding {
        keys: "n",
        desc: "notifications history",
        section: Section::Session,
        actions: &[Action::Notifications],
        footer: None,
    },
    Binding {
        keys: "R",
        desc: "reload global config",
        section: Section::Session,
        actions: &[Action::ReloadConfig],
        footer: None,
    },
    Binding {
        keys: "D",
        desc: "detach (keep services running)",
        section: Section::Session,
        actions: &[Action::Detach],
        footer: None,
    },
    Binding {
        keys: "q / ^c / Esc",
        desc: "quit: stop all, detach, or cancel",
        section: Section::Session,
        actions: &[Action::Quit],
        footer: Some(FooterHint {
            keys: "q",
            label: "quit",
        }),
    },
    Binding {
        keys: "?",
        desc: "toggle this overlay",
        section: Section::Session,
        actions: &[Action::ToggleHelp],
        footer: Some(FooterHint {
            keys: "?",
            label: "help",
        }),
    },
];

/// Every action, used by tests to enforce documentation + resolution coverage.
/// `exhaustive_marker` below makes adding an `Action` a compile error here.
pub const ALL_ACTIONS: &[Action] = &[
    Action::NextService,
    Action::PrevService,
    Action::NextStack,
    Action::PrevStack,
    Action::ToggleSidebar,
    Action::PageUp,
    Action::PageDown,
    Action::HalfPageUp,
    Action::HalfPageDown,
    Action::LineDown,
    Action::LineUp,
    Action::ScrollTop,
    Action::ScrollBottom,
    Action::CopyVisibleLogs,
    Action::CopyAllLogs,
    Action::CopyDebugPrompt,
    Action::CopyMode,
    Action::ZoomLogs,
    Action::StartService,
    Action::StopService,
    Action::RestartService,
    Action::OpenUrl,
    Action::CopyUrl,
    Action::AddWorktree,
    Action::RemoveWorktree,
    Action::StackInfo,
    Action::Settings,
    Action::Notifications,
    Action::ReloadConfig,
    Action::Detach,
    Action::Quit,
    Action::ToggleHelp,
];

/// Compile-time guard: adding an [`Action`] variant breaks this exhaustive
/// match, forcing the author to also add it to [`ALL_ACTIONS`] (and, via the
/// tests, to [`BINDINGS`] and [`resolve`]).
#[allow(dead_code)]
fn exhaustive_marker(a: Action) {
    match a {
        Action::NextService
        | Action::PrevService
        | Action::NextStack
        | Action::PrevStack
        | Action::ToggleSidebar
        | Action::PageUp
        | Action::PageDown
        | Action::HalfPageUp
        | Action::HalfPageDown
        | Action::LineDown
        | Action::LineUp
        | Action::ScrollTop
        | Action::ScrollBottom
        | Action::CopyVisibleLogs
        | Action::CopyAllLogs
        | Action::CopyDebugPrompt
        | Action::CopyMode
        | Action::ZoomLogs
        | Action::StartService
        | Action::StopService
        | Action::RestartService
        | Action::OpenUrl
        | Action::CopyUrl
        | Action::AddWorktree
        | Action::RemoveWorktree
        | Action::StackInfo
        | Action::Settings
        | Action::Notifications
        | Action::ReloadConfig
        | Action::Detach
        | Action::Quit
        | Action::ToggleHelp => {}
    }
}

/// Map a key event to its main-view action, or `None` if unbound. This is the
/// only place key chords are decoded — the help overlay never re-encodes them.
pub fn resolve(k: &KeyEvent) -> Option<Action> {
    use Action::*;
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    Some(match (k.code, ctrl) {
        // navigation
        (KeyCode::Right | KeyCode::Char('l'), false) => NextService,
        (KeyCode::Left | KeyCode::Char('h'), false) => PrevService,
        (KeyCode::Down | KeyCode::Char('j'), false) => NextStack,
        (KeyCode::Up | KeyCode::Char('k'), false) => PrevStack,
        (KeyCode::Char('`'), false) => ToggleSidebar,
        // log viewport
        (KeyCode::PageUp | KeyCode::Char('b'), false) => PageUp,
        (KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f'), false) => PageDown,
        (KeyCode::Char('u'), true) => HalfPageUp,
        (KeyCode::Char('d'), true) => HalfPageDown,
        (KeyCode::Char('J'), false) => LineDown,
        (KeyCode::Char('K'), false) => LineUp,
        (KeyCode::Char('g'), false) => ScrollTop,
        (KeyCode::Char('G'), false) => ScrollBottom,
        (KeyCode::Char('y'), false) => CopyVisibleLogs,
        (KeyCode::Char('Y'), false) => CopyAllLogs,
        (KeyCode::Char('p'), false) => CopyDebugPrompt,
        (KeyCode::Char('v'), false) => CopyMode,
        (KeyCode::Char('z'), false) => ZoomLogs,
        // service actions
        (KeyCode::Char('S'), false) => StartService,
        (KeyCode::Char('s'), false) => StopService,
        (KeyCode::Char('r'), false) => RestartService,
        (KeyCode::Char('o'), false) => OpenUrl,
        (KeyCode::Char('c'), false) => CopyUrl,
        // worktrees
        (KeyCode::Char('w'), false) => AddWorktree,
        (KeyCode::Char('x'), false) => RemoveWorktree,
        // session
        (KeyCode::Char('i'), false) => StackInfo,
        (KeyCode::Char(','), false) => Settings,
        (KeyCode::Char('n'), false) => Notifications,
        (KeyCode::Char('R'), false) => ReloadConfig,
        (KeyCode::Char('D'), false) => Detach,
        (KeyCode::Char('q') | KeyCode::Esc, false) => Quit,
        (KeyCode::Char('c'), true) => Quit,
        (KeyCode::Char('?'), false) => ToggleHelp,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_is_documented() {
        for &action in ALL_ACTIONS {
            let found = BINDINGS.iter().any(|b| b.actions.contains(&action));
            assert!(found, "{action:?} has no help entry in BINDINGS");
        }
    }

    #[test]
    fn every_binding_action_is_known() {
        // Guards against a typo / stale action lingering in BINDINGS.
        for b in BINDINGS {
            for action in b.actions {
                assert!(
                    ALL_ACTIONS.contains(action),
                    "{action:?} in BINDINGS is not in ALL_ACTIONS"
                );
            }
        }
    }

    #[test]
    fn every_action_is_reachable_from_resolve() {
        // Sweep the keys resolve knows about and collect the actions they
        // yield; every action must be produced by at least one key. Catches a
        // documented-but-undispatchable shortcut.
        let plain = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let code = |kc: KeyCode| KeyEvent::new(kc, KeyModifiers::NONE);

        let mut events: Vec<KeyEvent> = Vec::new();
        for c in "lhjkbfgGJKyYpvzSsrociqDwx,nR?` ".chars() {
            events.push(plain(c));
        }
        for c in ['u', 'd', 'c'] {
            events.push(ctrl(c));
        }
        for kc in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Esc,
        ] {
            events.push(code(kc));
        }

        let mut seen = std::collections::HashSet::new();
        for e in &events {
            if let Some(a) = resolve(e) {
                seen.insert(a);
            }
        }
        for &action in ALL_ACTIONS {
            assert!(
                seen.contains(&action),
                "{action:?} is not reachable from resolve"
            );
        }
    }
}
