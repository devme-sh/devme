//! Shared types used by every devstack crate. No I/O, no async — pure data.
//!
//! See `CONTEXT.md` at the repo root for the domain vocabulary modeled here.

mod dep;
mod port;
mod restart;
mod scope;
mod slot;
mod state;
mod trust;

pub use dep::Dependency;
pub use port::PortSpec;
pub use restart::RestartPolicy;
pub use scope::Scope;
pub use slot::Slot;
pub use state::{ServiceState, StepState};
pub use trust::Trust;
