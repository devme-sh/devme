//! Process liveness probe — abstracted so tests can simulate dead PIDs.
//!
//! V1 only checks "does a process with this PID exist?". A future revision
//! (ADR-0006) will pair this with start-time validation to defeat PID-reuse
//! races; for now we accept the slim risk that a recycled PID could keep a
//! stale slot looking live until something else triggers a release.

use std::sync::Arc;

/// Anything that can answer "is this PID currently alive?".
pub trait Liveness: Send + Sync {
    fn is_alive(&self, pid: u32) -> bool;
}

/// The real-world liveness probe — delegates to `process_alive`.
pub struct SystemLiveness;

impl Liveness for SystemLiveness {
    fn is_alive(&self, pid: u32) -> bool {
        matches!(
            process_alive::state(process_alive::Pid::from(pid)),
            process_alive::State::Alive
        )
    }
}

impl<F: Fn(u32) -> bool + Send + Sync> Liveness for F {
    fn is_alive(&self, pid: u32) -> bool {
        (self)(pid)
    }
}

/// Convenience: erase to a heap-allocated probe.
pub fn boxed<L: Liveness + 'static>(l: L) -> Arc<dyn Liveness> {
    Arc::new(l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process;

    #[test]
    fn system_liveness_says_current_process_is_alive() {
        let probe = SystemLiveness;
        assert!(probe.is_alive(process::id()));
    }

    #[test]
    fn closure_implements_liveness() {
        let probe = |pid: u32| pid == 42;
        assert!(probe.is_alive(42));
        assert!(!probe.is_alive(43));
    }
}
