//! devme TUI — state model and renderer.
//!
//! Lazygit-style fixed three-pane layout (see ADR-0010). The state model
//! here is pure data: it absorbs `ServerMessage`s from the daemon and key
//! events from the terminal, and produces the snapshot the renderer
//! consumes.

pub mod discovery;
mod event_loop;
pub mod keymap;
pub mod render;
pub mod state;
pub mod theme;
pub mod worktree;

pub use event_loop::launch;
