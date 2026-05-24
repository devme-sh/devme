//! Shared types used by every devme crate. No I/O, no async — pure data.
//!
//! See `CONTEXT.md` at the repo root for the domain vocabulary modeled here.

mod dep;
mod health;
pub mod ipc;
mod port;
mod restart;
mod scope;
mod slot;
mod state;
mod trust;
pub mod wizard;

pub use dep::Dependency;
pub use health::HealthCheck;
pub use ipc::{
    ClientMessage, Envelope, ErrorCode, InstanceInfo, NoticeLevel, SCHEMA_VERSION, ServerMessage,
    ServiceSnapshot, StepSnapshot,
};
pub use port::PortSpec;
pub use restart::RestartPolicy;
pub use scope::Scope;
pub use slot::Slot;
pub use state::{ServiceState, StepState};
pub use trust::Trust;
pub use wizard::{AskPrompt, FormField, WizardEvent, WizardLogLevel, WizardResponse};
