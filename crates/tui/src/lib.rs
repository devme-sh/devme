//! devme TUI — state model and renderer.
//!
//! Lazygit-style fixed three-pane layout (see ADR-0010). The state model
//! here is pure data: it absorbs `ServerMessage`s from the daemon and key
//! events from the terminal, and produces the snapshot the renderer
//! consumes.

pub mod render;
pub mod state;
