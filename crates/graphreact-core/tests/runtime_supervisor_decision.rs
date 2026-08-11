//! M14.10 — partition-aware supervisor decisions.
//!
//! Verifies the new `SuperviseDecisionHook` surface:
//! 1. With no hook installed, `execute_supervisor_plan` runs the plan
//!    identically to today (zero overhead path).
//! 2. A hook returning `Defer` parks the decision; the plan does NOT
//!    run, and `pending_supervisor_decision_count` reports 1.
//! 3. On the next `process_batch`, the hook is re-evaluated; if it
//!    now returns `Proceed`, the plan runs.
//! 4. If the hook keeps deferring past the `retry_after` deadline,
//!    the plan runs anyway (escalation).

use graphreact_core::{
    Attrs, ChildSpec, CommunicationGraph, ExitReason, NodeContext, NodeDefinition,
    PermissionManifest, RestartPolicy, RestartStrategy, RestartTracker, Scheduler,
    SuperviseDecision, Supervisor,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn counting_definition(name: &'static str, counter: Rc<Cell<u32>>) -> NodeDefinition {
    NodeDefinition::new(name, move |_ctx: &NodeContext| {
        counter.set(counter.get() + 1);
        PermissionManifest::default()
    })
}

fn build_one_child(
    scheduler: &mut Scheduler,
    runs: Rc<Cell<u32>>,
) -> graphreact_core::SupervisorKey {
    let sup = scheduler.install_supervisor(Supervisor::new(
        RestartStrategy::OneForOne,
        RestartTracker::new(5, Duration::from_secs(60)),
    ));
    scheduler
        .supervise(
            sup,
            ChildSpec {
                id: "child".into(),
                policy: RestartPolicy::Permanent,
                definition: counting_definition("Child", runs),
                props: Attrs::new(),
                connect_to: vec![],
                height: 0,
            },
        )
        .unwrap();
    sup
}

#[test]
fn supervise_decision_proceed_runs_plan_synchronously() {
    let mut scheduler = Scheduler::new(CommunicationGraph::new());
    scheduler.start().unwrap();
    let runs = Rc::new(Cell::new(0u32));
    let _sup = build_one_child(&mut scheduler, Rc::clone(&runs));
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 1, "child mounts on first process_batch");

    // Hook installed but returns Proceed: plan must still run.
    scheduler.install_supervise_decision_hook(Arc::new(|_, _, _| SuperviseDecision::Proceed));

    let key = scheduler.key_for("child").unwrap();
    scheduler.unmount_with_reason(
        key,
        ExitReason::Crashed {
            detail: "boom".into(),
        },
    );
    scheduler.process_batch().unwrap();

    assert_eq!(
        runs.get(),
        2,
        "child re-mounted after crash with Proceed hook"
    );
    assert_eq!(
        scheduler.pending_supervisor_decision_count(),
        0,
        "no decisions parked when hook returns Proceed"
    );
}

#[test]
fn supervise_decision_defer_parks_then_proceed_on_next_drain() {
    let mut scheduler = Scheduler::new(CommunicationGraph::new());
    scheduler.start().unwrap();
    let runs = Rc::new(Cell::new(0u32));
    let _sup = build_one_child(&mut scheduler, Rc::clone(&runs));
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 1);

    // Hook flips from Defer to Proceed on its second invocation. Use
    // an AtomicUsize counter so the closure stays Send+Sync.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_hook = Arc::clone(&calls);
    scheduler.install_supervise_decision_hook(Arc::new(move |_, _, _| {
        let prev = calls_for_hook.fetch_add(1, Ordering::SeqCst);
        if prev == 0 {
            SuperviseDecision::Defer {
                reason: "first call defers".into(),
                retry_after: Some(Duration::from_secs(60)),
            }
        } else {
            SuperviseDecision::Proceed
        }
    }));

    let key = scheduler.key_for("child").unwrap();
    scheduler.unmount_with_reason(
        key,
        ExitReason::Crashed {
            detail: "boom".into(),
        },
    );

    // unmount_with_reason fired execute_supervisor_plan directly,
    // which consulted the hook (call 1 → Defer) and parked the
    // decision. No process_batch yet → the parked decision hasn't
    // been re-evaluated.
    assert_eq!(runs.get(), 1, "plan deferred — child has not re-mounted");
    assert_eq!(scheduler.pending_supervisor_decision_count(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // process_batch's drain re-evaluates the parked decision; the
    // second hook call returns Proceed and the plan runs in the
    // same batch.
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 2, "plan ran on drain after second Proceed call");
    assert_eq!(scheduler.pending_supervisor_decision_count(), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn supervise_decision_escalates_after_retry_after_deadline() {
    let mut scheduler = Scheduler::new(CommunicationGraph::new());
    scheduler.start().unwrap();
    let runs = Rc::new(Cell::new(0u32));
    let _sup = build_one_child(&mut scheduler, Rc::clone(&runs));
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 1);

    // Hook ALWAYS defers with a tight retry_after. The drain
    // path should escalate (run plan unconditionally) once the
    // deadline elapses.
    let retry_after = Duration::from_millis(40);
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_hook = Arc::clone(&calls);
    scheduler.install_supervise_decision_hook(Arc::new(move |_, _, _| {
        calls_for_hook.fetch_add(1, Ordering::SeqCst);
        SuperviseDecision::Defer {
            reason: "always defer until escalation".into(),
            retry_after: Some(retry_after),
        }
    }));

    let key = scheduler.key_for("child").unwrap();
    scheduler.unmount_with_reason(
        key,
        ExitReason::Crashed {
            detail: "boom".into(),
        },
    );
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 1);
    assert_eq!(scheduler.pending_supervisor_decision_count(), 1);

    // Wait past the retry_after deadline. The drain should
    // escalate without consulting the hook again.
    std::thread::sleep(retry_after + Duration::from_millis(20));

    let calls_before = calls.load(Ordering::SeqCst);
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 2, "plan ran on escalation past retry_after");
    assert_eq!(scheduler.pending_supervisor_decision_count(), 0);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        calls_before,
        "escalation must NOT re-call the hook (otherwise an always-deferring hook would loop)"
    );
}

#[test]
fn pending_supervisor_decision_count_starts_at_zero() {
    let scheduler = Scheduler::new(CommunicationGraph::new());
    assert_eq!(scheduler.pending_supervisor_decision_count(), 0);
}
