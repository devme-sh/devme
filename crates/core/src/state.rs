use serde::{Deserialize, Serialize};

/// Runtime state of a Service inside a Daemon.
///
/// See ADR-0005 and ADR-0007. Some variants carry context surfaced in the
/// TUI / CLI as the "started in degraded state" status (e.g.
/// `Running { degraded: true, .. }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceState {
    /// Not yet started this run.
    Stopped,
    /// Spawning; waiting for the process to be alive and (if applicable) healthy.
    Starting,
    /// Process is alive. `degraded` is true when force-started without one or
    /// more required deps; `started_without` enumerates the skipped deps.
    Running {
        #[serde(default)]
        degraded: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        started_without: Vec<String>,
    },
    /// Blocked because a required dependency has not yet started successfully.
    WaitingOnDependency { blocked_by: String },
    /// Restarting after a crash; the next start attempt is scheduled.
    Restarting { attempt: u32 },
    /// Crashed too many times in too short a window; auto-restart suspended.
    /// Resumes when the user hits `r` in the TUI or `devme restart`.
    CrashLoop { restart_count: u32 },
    /// Exited or crashed. `exit_code = None` indicates terminated by signal.
    Failed { exit_code: Option<i32> },
    /// Declared `external = true` in config. Devme only health-checks.
    External { healthy: bool },
}

impl ServiceState {
    /// True if the process should be considered "up" by an observer.
    /// `Running` and healthy `External` count; everything else doesn't.
    pub fn is_up(&self) -> bool {
        matches!(self, ServiceState::Running { .. } | ServiceState::External { healthy: true })
    }
}

/// Runtime state of a Step's `check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepState {
    /// Check hasn't run yet this launch.
    Unknown,
    /// Check ran and passed.
    Passed,
    /// Check ran and failed; user has not yet chosen an action.
    Failed,
    /// User chose "skip once" in the failure overlay; persists for this run only.
    SkippedThisRun,
    /// User has a persisted mark-as-installed override for this Step.
    Overridden,
    /// Provision ran and errored out.
    ProvisionFailed,
}

impl StepState {
    /// True if downstream Services can proceed past this Step.
    pub fn is_satisfied(&self) -> bool {
        matches!(
            self,
            StepState::Passed | StepState::SkippedThisRun | StepState::Overridden
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_running_is_up() {
        let s = ServiceState::Running { degraded: false, started_without: vec![] };
        assert!(s.is_up());
    }

    #[test]
    fn service_running_degraded_still_up() {
        let s = ServiceState::Running { degraded: true, started_without: vec!["proxy".into()] };
        assert!(s.is_up());
    }

    #[test]
    fn external_healthy_is_up_external_unhealthy_isnt() {
        assert!(ServiceState::External { healthy: true }.is_up());
        assert!(!ServiceState::External { healthy: false }.is_up());
    }

    #[test]
    fn stopped_and_failed_arent_up() {
        assert!(!ServiceState::Stopped.is_up());
        assert!(!ServiceState::Failed { exit_code: Some(1) }.is_up());
        assert!(!ServiceState::WaitingOnDependency { blocked_by: "db".into() }.is_up());
        assert!(!ServiceState::CrashLoop { restart_count: 5 }.is_up());
    }

    #[test]
    fn step_passed_overridden_skipped_all_satisfied() {
        assert!(StepState::Passed.is_satisfied());
        assert!(StepState::Overridden.is_satisfied());
        assert!(StepState::SkippedThisRun.is_satisfied());
    }

    #[test]
    fn step_unknown_failed_provision_failed_arent_satisfied() {
        assert!(!StepState::Unknown.is_satisfied());
        assert!(!StepState::Failed.is_satisfied());
        assert!(!StepState::ProvisionFailed.is_satisfied());
    }

    #[test]
    fn service_state_round_trips_via_json() {
        let cases = vec![
            ServiceState::Stopped,
            ServiceState::Starting,
            ServiceState::Running { degraded: false, started_without: vec![] },
            ServiceState::Running { degraded: true, started_without: vec!["proxy".into()] },
            ServiceState::WaitingOnDependency { blocked_by: "db".into() },
            ServiceState::Restarting { attempt: 3 },
            ServiceState::CrashLoop { restart_count: 5 },
            ServiceState::Failed { exit_code: Some(137) },
            ServiceState::Failed { exit_code: None },
            ServiceState::External { healthy: true },
        ];
        for state in cases {
            let json = serde_json::to_string(&state).unwrap();
            let back: ServiceState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back, "round-trip failed for {state:?}");
        }
    }

    #[test]
    fn step_state_round_trips_via_json() {
        let cases = [
            StepState::Unknown,
            StepState::Passed,
            StepState::Failed,
            StepState::SkippedThisRun,
            StepState::Overridden,
            StepState::ProvisionFailed,
        ];
        for state in cases {
            let json = serde_json::to_string(&state).unwrap();
            let back: StepState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back);
        }
    }

    #[test]
    fn service_state_running_serializes_without_empty_started_without() {
        let s = ServiceState::Running { degraded: false, started_without: vec![] };
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("started_without"), "got: {json}");
    }
}
