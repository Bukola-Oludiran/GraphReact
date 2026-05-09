//! Live supervisor execution tests — closes out M5.
//!
//! Verifies the full loop: a supervised child unmounts with
//! `ExitReason::Crashed`, the scheduler consults the governing
//! supervisor's `plan()`, terminates listed siblings, and re-mounts
//! the restart set in declaration order. Intensity throttling stops
//! the loop after `max_restarts` failures inside `period`.

use graphreact_core::{
    Attrs, ChildSpec, CommunicationGraph, ExitReason, NodeContext, NodeDefinition,
    PermissionManifest, RestartPolicy, RestartStrategy, RestartTracker, Scheduler, Supervisor,
};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// Records how many times a given role-tagged component runs (mount + re-mount).
fn counting_definition(name: &'static str, counter: Rc<Cell<u32>>) -> NodeDefinition {
    NodeDefinition::new(name, move |_ctx: &NodeContext| {
        counter.set(counter.get() + 1);
        PermissionManifest::default()
    })
}

fn make_supervisor(strategy: RestartStrategy, intensity: u32) -> Supervisor {
    Supervisor::new(
        strategy,
        RestartTracker::new(intensity, Duration::from_secs(60)),
    )
}

#[test]
fn one_for_one_restarts_only_failed_child_in_runtime() {
    let mut scheduler = Scheduler::new(CommunicationGraph::new());
    scheduler.start().unwrap();

    let sup = scheduler.install_supervisor(make_supervisor(RestartStrategy::OneForOne, 5));

    let runs_a = Rc::new(Cell::new(0u32));
    let runs_b = Rc::new(Cell::new(0u32));

    let key_a = scheduler
        .supervise(
            sup,
            ChildSpec {
                id: "a".into(),
                policy: RestartPolicy::Permanent,
                definition: counting_definition("A", Rc::clone(&runs_a)),
                props: Attrs::new(),
                connect_to: vec![],
                height: 0,
            },
        )
        .unwrap();
    let _key_b = scheduler
        .supervise(
            sup,
            ChildSpec {
                id: "b".into(),
                policy: RestartPolicy::Permanent,
                definition: counting_definition("B", Rc::clone(&runs_b)),
                props: Attrs::new(),
                connect_to: vec![],
                height: 0,
            },
        )
        .unwrap();
    scheduler.process_batch().unwrap();
    assert_eq!(runs_a.get(), 1);
    assert_eq!(runs_b.get(), 1);

    // Crash a — supervisor should restart only a.
    scheduler.unmount_with_reason(
        key_a,
        ExitReason::Crashed {
            detail: "boom".into(),
        },
    );
    scheduler.process_batch().unwrap();

    assert_eq!(runs_a.get(), 2, "a re-mounted after crash");
    assert_eq!(runs_b.get(), 1, "b untouched");
    assert!(scheduler.has_node("a"));
    assert!(scheduler.has_node("b"));
}

#[test]
fn one_for_all_terminates_and_restarts_every_child() {
    let mut scheduler = Scheduler::new(CommunicationGraph::new());
    scheduler.start().unwrap();
    let sup = scheduler.install_supervisor(make_supervisor(RestartStrategy::OneForAll, 5));

    let counts = ["a", "b", "c"]
        .iter()
        .map(|name| (*name, Rc::new(Cell::new(0u32))))
        .collect::<Vec<_>>();
    let mut keys = Vec::new();
    for (name, counter) in &counts {
        let key = scheduler
            .supervise(
                sup,
                ChildSpec {
                    id: (*name).into(),
                    policy: RestartPolicy::Permanent,
                    definition: counting_definition(
                        Box::leak(format!("Role-{name}").into_boxed_str()),
                        Rc::clone(counter),
                    ),
                    props: Attrs::new(),
                    connect_to: vec![],
                    height: 0,
                },
            )
            .unwrap();
        keys.push((*name, key));
    }
    scheduler.process_batch().unwrap();

    // All three mounted exactly once.
    for (_, c) in &counts {
        assert_eq!(c.get(), 1);
    }

    // Crash b — strategy terminates a and c, then re-mounts all three.
    let b_key = keys.iter().find(|(n, _)| *n == "b").unwrap().1;
    scheduler.unmount_with_reason(
        b_key,
        ExitReason::Crashed {
            detail: "boom".into(),
        },
    );
    scheduler.process_batch().unwrap();

    for (_, c) in &counts {
        assert_eq!(c.get(), 2, "every child re-evaluated after one-for-all");
    }
}

#[test]
fn rest_for_one_restarts_only_children_after_failed() {
    let mut scheduler = Scheduler::new(CommunicationGraph::new());
    scheduler.start().unwrap();
    let sup = scheduler.install_supervisor(make_supervisor(RestartStrategy::RestForOne, 5));

    let runs_a = Rc::new(Cell::new(0u32));
    let runs_b = Rc::new(Cell::new(0u32));
    let runs_c = Rc::new(Cell::new(0u32));
    let runs_d = Rc::new(Cell::new(0u32));

    for (name, counter, role_name) in [
        ("a", &runs_a, "A"),
        ("b", &runs_b, "B"),
        ("c", &runs_c, "C"),
        ("d", &runs_d, "D"),
    ] {
        scheduler
            .supervise(
                sup,
                ChildSpec {
                    id: name.into(),
                    policy: RestartPolicy::Permanent,
                    definition: counting_definition(role_name, Rc::clone(counter)),
                    props: Attrs::new(),
                    connect_to: vec![],
                    height: 0,
                },
            )
            .unwrap();
    }
    scheduler.process_batch().unwrap();

    // Crash b → terminates c+d (started after), re-mounts b+c+d. a untouched.
    let b_key = scheduler.key_for("b").unwrap();
    scheduler.unmount_with_reason(
        b_key,
        ExitReason::Crashed {
            detail: "boom".into(),
        },
    );
    scheduler.process_batch().unwrap();

    assert_eq!(runs_a.get(), 1, "a (started before b) untouched");
    assert_eq!(runs_b.get(), 2);
    assert_eq!(runs_c.get(), 2);
    assert_eq!(runs_d.get(), 2);
}

#[test]
fn intensity_exceeded_supervisor_stops_restarting() {
    let mut scheduler = Scheduler::new(CommunicationGraph::new());
    scheduler.start().unwrap();
    // Only 2 restarts allowed in the window.
    let sup = scheduler.install_supervisor(make_supervisor(RestartStrategy::OneForOne, 2));

    let runs = Rc::new(Cell::new(0u32));
    let key = scheduler
        .supervise(
            sup,
            ChildSpec {
                id: "flapper".into(),
                policy: RestartPolicy::Permanent,
                definition: counting_definition("Flapper", Rc::clone(&runs)),
                props: Attrs::new(),
                connect_to: vec![],
                height: 0,
            },
        )
        .unwrap();
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 1);

    // Crash 1 → restart (count=1)
    scheduler.unmount_with_reason(key, ExitReason::Crashed { detail: "1".into() });
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 2);
    let key = scheduler.key_for("flapper").unwrap();

    // Crash 2 → restart (count=2)
    scheduler.unmount_with_reason(key, ExitReason::Crashed { detail: "2".into() });
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 3);
    let key = scheduler.key_for("flapper").unwrap();

    // Crash 3 → tracker.record returns IntensityExceeded; supervisor
    // gives up. The child stays down; runs counter does not advance.
    scheduler.unmount_with_reason(key, ExitReason::Crashed { detail: "3".into() });
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 3, "no restart after intensity exceeded");
    assert!(!scheduler.has_node("flapper"));
}

#[test]
fn transient_child_does_not_restart_on_normal_exit() {
    let mut scheduler = Scheduler::new(CommunicationGraph::new());
    scheduler.start().unwrap();
    let sup = scheduler.install_supervisor(make_supervisor(RestartStrategy::OneForOne, 5));

    let runs = Rc::new(Cell::new(0u32));
    let key = scheduler
        .supervise(
            sup,
            ChildSpec {
                id: "tr".into(),
                policy: RestartPolicy::Transient,
                definition: counting_definition("Tr", Rc::clone(&runs)),
                props: Attrs::new(),
                connect_to: vec![],
                height: 0,
            },
        )
        .unwrap();
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 1);

    // Clean shutdown — Transient must NOT restart.
    scheduler.unmount_with_reason(key, ExitReason::Normal);
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 1);
    assert!(!scheduler.has_node("tr"));
}

#[test]
fn temporary_child_never_restarts() {
    let mut scheduler = Scheduler::new(CommunicationGraph::new());
    scheduler.start().unwrap();
    let sup = scheduler.install_supervisor(make_supervisor(RestartStrategy::OneForOne, 5));

    let runs = Rc::new(Cell::new(0u32));
    let key = scheduler
        .supervise(
            sup,
            ChildSpec {
                id: "tmp".into(),
                policy: RestartPolicy::Temporary,
                definition: counting_definition("Tmp", Rc::clone(&runs)),
                props: Attrs::new(),
                connect_to: vec![],
                height: 0,
            },
        )
        .unwrap();
    scheduler.process_batch().unwrap();

    scheduler.unmount_with_reason(
        key,
        ExitReason::Crashed {
            detail: "boom".into(),
        },
    );
    scheduler.process_batch().unwrap();
    assert_eq!(runs.get(), 1, "temporary child stays gone");
    assert!(!scheduler.has_node("tmp"));
}
