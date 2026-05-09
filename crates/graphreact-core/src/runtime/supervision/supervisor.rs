//! Supervisor restart strategies — OTP `lib/stdlib/src/supervisor.erl`
//! adapted for the GraphReact scheduler.
//!
//! A `Supervisor` owns a set of child specs and a [`RestartStrategy`].
//! When a supervised child unmounts abnormally, the supervisor decides
//! which siblings to terminate and which to restart, gated by an
//! intensity/period [`RestartTracker`] that bounds runaway restart loops.
//!
//! This module is data-only — the [`Scheduler`] owns the live wiring.
//! Tests hit the strategy logic directly via [`Supervisor::plan`].
//!
//! [`Scheduler`]: crate::runtime::scheduler::Scheduler

use thiserror::Error;

use crate::runtime::supervision::monitor::ExitReason;

/// Identifier for a supervised child within a [`Supervisor`].
pub type ChildId = String;

/// Restart strategy — what to do when a child terminates abnormally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
    /// Restart only the dead child.
    OneForOne,
    /// Terminate every sibling, then restart all of them.
    OneForAll,
    /// Terminate the dead child + every child started after it; restart
    /// in original order.
    RestForOne,
    /// Dynamic worker pool — children are uniform instances with no
    /// pre-declared specs. Fault recovery is per-child like
    /// `OneForOne`, but the supervisor can spawn additional workers
    /// programmatically.
    SimpleOneForOne,
}

/// Per-child restart policy. Mirrors OTP's `restart` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Always restart on any exit.
    Permanent,
    /// Restart only on abnormal exit.
    Transient,
    /// Never restart; the child is gone after exit.
    Temporary,
}

impl RestartPolicy {
    /// Whether a child with this policy should be restarted given the
    /// supplied exit reason.
    #[must_use]
    pub fn should_restart(self, reason: &ExitReason) -> bool {
        match self {
            RestartPolicy::Permanent => true,
            RestartPolicy::Transient => reason.is_abnormal(),
            RestartPolicy::Temporary => false,
        }
    }
}

/// Tracker for restart history within an intensity/period window.
///
/// Mirrors OTP's intensity (`max_restarts`) + period (`max_seconds`)
/// throttling (supervisor.erl:2258-2290).
#[derive(Debug, Clone)]
pub struct RestartTracker {
    pub max_restarts: u32,
    pub period: std::time::Duration,
    history: Vec<std::time::Instant>,
}

impl RestartTracker {
    #[must_use]
    pub fn new(max_restarts: u32, period: std::time::Duration) -> Self {
        Self {
            max_restarts,
            period,
            history: Vec::new(),
        }
    }

    /// Record a restart at `now`. Returns `Ok(())` if the rate is still
    /// within the policy; `Err` if the supervisor must give up.
    pub fn record(&mut self, now: std::time::Instant) -> Result<(), SupervisorError> {
        let cutoff = now - self.period;
        self.history.retain(|&t| t >= cutoff);
        if self.history.len() >= self.max_restarts as usize {
            return Err(SupervisorError::IntensityExceeded {
                max: self.max_restarts,
                period: self.period,
            });
        }
        self.history.push(now);
        Ok(())
    }

    /// Restarts recorded within the active window.
    #[must_use]
    pub fn count(&self) -> usize {
        self.history.len()
    }

    /// Forget all history.
    pub fn reset(&mut self) {
        self.history.clear();
    }
}

/// Errors raised by supervisor operations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    #[error("restart intensity exceeded: {max} restarts within {period:?}")]
    IntensityExceeded {
        max: u32,
        period: std::time::Duration,
    },
    #[error("child '{id}' not registered with supervisor")]
    ChildMissing { id: String },
    #[error("supervisor has no children to operate on")]
    Empty,
}

/// Per-child entry stored on a [`Supervisor`].
///
/// Carries the full re-mount payload so the supervisor can rebuild a
/// failed child without the caller re-supplying any of it. The
/// `definition` is reused on every restart — make sure the component
/// closure captures only data that survives across re-evaluations.
#[derive(Clone)]
pub struct ChildSpec {
    pub id: ChildId,
    pub policy: RestartPolicy,
    pub definition: crate::runtime::node_instance::NodeDefinition,
    pub props: crate::graph::Attrs,
    pub connect_to: Vec<String>,
    pub height: u32,
}

impl std::fmt::Debug for ChildSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildSpec")
            .field("id", &self.id)
            .field("policy", &self.policy)
            .field("definition", &self.definition.name)
            .field("connect_to", &self.connect_to)
            .field("height", &self.height)
            .finish()
    }
}

/// What a strategy decided to do in response to a child failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartPlan {
    /// Children to terminate (in this order). Excludes the failed
    /// child (already gone) and any `Temporary` children.
    pub terminate: Vec<ChildId>,
    /// Children to (re)start, in original declaration order.
    pub restart: Vec<ChildId>,
}

impl RestartPlan {
    fn empty() -> Self {
        Self {
            terminate: Vec::new(),
            restart: Vec::new(),
        }
    }
}

/// In-memory supervisor — children, strategy, restart tracker.
///
/// Strategy logic is pure ([`Supervisor::plan`]); execution against
/// the scheduler lands when supervisors get formally installed onto
/// the runtime.
#[derive(Debug, Clone)]
pub struct Supervisor {
    pub strategy: RestartStrategy,
    pub tracker: RestartTracker,
    children: Vec<ChildSpec>,
}

impl Supervisor {
    #[must_use]
    pub fn new(strategy: RestartStrategy, tracker: RestartTracker) -> Self {
        Self {
            strategy,
            tracker,
            children: Vec::new(),
        }
    }

    /// Add a child spec at the end of the start-order list.
    pub fn add_child(&mut self, spec: ChildSpec) {
        self.children.push(spec);
    }

    /// Children in start order.
    #[must_use]
    pub fn children(&self) -> &[ChildSpec] {
        &self.children
    }

    /// Lookup a child by ID.
    #[must_use]
    pub fn find_child(&self, id: &str) -> Option<&ChildSpec> {
        self.children.iter().find(|c| c.id == id)
    }

    /// Compute the [`RestartPlan`] for the failure of `failed_id` with
    /// `reason`. Pure — does not mutate the supervisor or tracker.
    pub fn plan(
        &self,
        failed_id: &str,
        reason: &ExitReason,
    ) -> Result<RestartPlan, SupervisorError> {
        let Some(failed) = self.find_child(failed_id) else {
            return Err(SupervisorError::ChildMissing {
                id: failed_id.to_string(),
            });
        };
        if failed.policy == RestartPolicy::Temporary {
            return Ok(RestartPlan::empty());
        }
        if !failed.policy.should_restart(reason) {
            return Ok(RestartPlan::empty());
        }
        match self.strategy {
            RestartStrategy::OneForOne | RestartStrategy::SimpleOneForOne => Ok(RestartPlan {
                terminate: vec![],
                restart: vec![failed_id.to_string()],
            }),
            RestartStrategy::OneForAll => {
                let alive_others: Vec<ChildId> = self
                    .children
                    .iter()
                    .filter(|c| c.id != failed_id)
                    .map(|c| c.id.clone())
                    .collect();
                let restart_all: Vec<ChildId> =
                    self.children.iter().map(|c| c.id.clone()).collect();
                Ok(RestartPlan {
                    terminate: alive_others,
                    restart: restart_all,
                })
            }
            RestartStrategy::RestForOne => {
                let idx = self
                    .children
                    .iter()
                    .position(|c| c.id == failed_id)
                    .expect("found above");
                let after: Vec<ChildId> = self.children[idx + 1..]
                    .iter()
                    .map(|c| c.id.clone())
                    .collect();
                let restart: Vec<ChildId> =
                    self.children[idx..].iter().map(|c| c.id.clone()).collect();
                Ok(RestartPlan {
                    terminate: after,
                    restart,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn dummy_definition(name: &str) -> crate::runtime::node_instance::NodeDefinition {
        crate::runtime::node_instance::NodeDefinition::new(name, |_| {
            crate::runtime::types::PermissionManifest::default()
        })
    }

    fn supervisor_of(strategy: RestartStrategy, kids: &[(&str, RestartPolicy)]) -> Supervisor {
        let mut s = Supervisor::new(strategy, RestartTracker::new(10, Duration::from_secs(60)));
        for (id, policy) in kids {
            s.add_child(ChildSpec {
                id: (*id).to_string(),
                policy: *policy,
                definition: dummy_definition(id),
                props: crate::graph::Attrs::new(),
                connect_to: Vec::new(),
                height: 0,
            });
        }
        s
    }

    fn crashed() -> ExitReason {
        ExitReason::Crashed {
            detail: "test".into(),
        }
    }

    #[test]
    fn restart_tracker_allows_within_intensity() {
        let mut tr = RestartTracker::new(3, Duration::from_secs(60));
        let now = Instant::now();
        assert!(tr.record(now).is_ok());
        assert!(tr.record(now).is_ok());
        assert!(tr.record(now).is_ok());
        assert!(matches!(
            tr.record(now),
            Err(SupervisorError::IntensityExceeded { .. })
        ));
    }

    #[test]
    fn restart_tracker_drops_old_entries() {
        let mut tr = RestartTracker::new(2, Duration::from_millis(10));
        let now = Instant::now();
        assert!(tr.record(now).is_ok());
        assert!(tr.record(now).is_ok());
        let later = now + Duration::from_millis(100);
        assert!(tr.record(later).is_ok());
        assert_eq!(tr.count(), 1);
    }

    #[test]
    fn one_for_one_only_restarts_failed() {
        let s = supervisor_of(
            RestartStrategy::OneForOne,
            &[
                ("a", RestartPolicy::Permanent),
                ("b", RestartPolicy::Permanent),
                ("c", RestartPolicy::Permanent),
            ],
        );
        let plan = s.plan("b", &crashed()).unwrap();
        assert_eq!(plan.terminate, Vec::<ChildId>::new());
        assert_eq!(plan.restart, vec!["b".to_string()]);
    }

    #[test]
    fn one_for_all_terminates_siblings_and_restarts_all() {
        let s = supervisor_of(
            RestartStrategy::OneForAll,
            &[
                ("a", RestartPolicy::Permanent),
                ("b", RestartPolicy::Permanent),
                ("c", RestartPolicy::Permanent),
            ],
        );
        let plan = s.plan("b", &crashed()).unwrap();
        assert_eq!(plan.terminate, vec!["a".to_string(), "c".to_string()]);
        assert_eq!(
            plan.restart,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn rest_for_one_only_restarts_after_failed() {
        let s = supervisor_of(
            RestartStrategy::RestForOne,
            &[
                ("a", RestartPolicy::Permanent),
                ("b", RestartPolicy::Permanent),
                ("c", RestartPolicy::Permanent),
                ("d", RestartPolicy::Permanent),
            ],
        );
        let plan = s.plan("b", &crashed()).unwrap();
        assert_eq!(plan.terminate, vec!["c".to_string(), "d".to_string()]);
        assert_eq!(
            plan.restart,
            vec!["b".to_string(), "c".to_string(), "d".to_string()]
        );
    }

    #[test]
    fn temporary_children_do_not_restart() {
        let s = supervisor_of(
            RestartStrategy::OneForAll,
            &[
                ("a", RestartPolicy::Permanent),
                ("b", RestartPolicy::Temporary),
            ],
        );
        let plan = s.plan("b", &crashed()).unwrap();
        assert!(plan.restart.is_empty());
        assert!(plan.terminate.is_empty());
    }

    #[test]
    fn transient_children_restart_only_on_abnormal_exit() {
        let s = supervisor_of(
            RestartStrategy::OneForOne,
            &[("a", RestartPolicy::Transient)],
        );
        let normal = s.plan("a", &ExitReason::Normal).unwrap();
        assert!(normal.restart.is_empty());
        let crash = s.plan("a", &crashed()).unwrap();
        assert_eq!(crash.restart, vec!["a".to_string()]);
    }

    #[test]
    fn missing_child_returns_error() {
        let s = supervisor_of(RestartStrategy::OneForOne, &[]);
        assert!(matches!(
            s.plan("ghost", &crashed()),
            Err(SupervisorError::ChildMissing { .. })
        ));
    }

    #[test]
    fn simple_one_for_one_treats_each_child_independently() {
        let s = supervisor_of(
            RestartStrategy::SimpleOneForOne,
            &[
                ("worker-1", RestartPolicy::Permanent),
                ("worker-2", RestartPolicy::Permanent),
            ],
        );
        let plan = s.plan("worker-1", &crashed()).unwrap();
        assert_eq!(plan.terminate, Vec::<ChildId>::new());
        assert_eq!(plan.restart, vec!["worker-1".to_string()]);
    }
}
