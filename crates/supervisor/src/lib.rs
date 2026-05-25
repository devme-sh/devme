//! Per-instance devme supervisor: spawns services in PTYs, watches them
//! for output and exit, feeds events into the executor, and serves an IPC
//! socket for the client/TUI.
//!
//! See ADR-0003 for daemon lifecycle and ADR-0007 for the two-tier model.

pub mod daemon;
pub mod env_resolve;
pub mod health;
pub mod process;
pub mod spawn;
