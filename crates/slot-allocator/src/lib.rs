//! File-locked slot allocator — coordinates port-slot ownership across
//! worktrees on the same machine.
//!
//! Each devme daemon, on startup, calls [`SlotAllocator::claim`] with the
//! worktree's `instance_id` (a hash of the worktree path). The allocator
//! returns the lowest free slot, persisting the claim to disk so concurrent
//! daemons agree. On shutdown the daemon calls [`SlotAllocator::release`].
//! If a daemon crashes without releasing, the next claim will sweep its
//! entry once it observes the PID is no longer live.
//!
//! See ADR-0006 for the locking model and ADR-0003 for the daemon lifecycle.

mod allocator;
mod error;
mod liveness;
mod record;

pub use allocator::{DEFAULT_MAX_SLOTS, SlotAllocator};
pub use error::AllocError;
pub use liveness::{Liveness, SystemLiveness, boxed};
pub use record::{ClaimRecord, Registry};
