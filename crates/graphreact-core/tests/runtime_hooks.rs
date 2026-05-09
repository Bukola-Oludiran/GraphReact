//! Behavioral spec for runtime hooks — ported from
//! `graphreact/src/__tests__/runtime/hooks.test.ts`.
//!
//! `use_state` returns a `Copy` `Signal<T>` (not a `[value, setter]` tuple
//! like TS) — captured into closures freely, value-on-read via `.get()`,
//! mutation via `.set()` / `.update()`.

use graphreact_core::{
    use_effect, use_memo, use_state, Attrs, CommunicationGraph, NodeContext, NodeDefinition,
    PermissionManifest, Scheduler, Signal,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

fn make_scheduler_with_node<F>(id: &str, definition_factory: F) -> Scheduler
where
    F: FnOnce(&mut Scheduler),
{
    let mut graph = CommunicationGraph::new();
    graph.add_node(id, Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    definition_factory(&mut scheduler);
    scheduler
}

// ---- use_state ----

#[test]
fn use_state_initializes_on_first_evaluation() {
    let captured = Rc::new(Cell::new(-1i32));
    let captured_clone = Rc::clone(&captured);

    let component = move |_ctx: &NodeContext| {
        let count: Signal<i32> = use_state(42);
        captured_clone.set(count.get());
        PermissionManifest::default()
    };

    let mut scheduler = make_scheduler_with_node("c", |s| {
        s.register_node(
            NodeDefinition::new("Counter", component),
            "c",
            Attrs::new(),
            0,
        )
        .unwrap();
    });

    scheduler.start().unwrap();
    assert_eq!(captured.get(), 42);
}

#[test]
fn use_state_set_triggers_re_evaluation() {
    let values: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let signal_holder: Rc<RefCell<Option<Signal<i32>>>> = Rc::new(RefCell::new(None));

    let component = {
        let values = Rc::clone(&values);
        let signal_holder = Rc::clone(&signal_holder);
        move |_ctx: &NodeContext| {
            let count: Signal<i32> = use_state(0);
            values.borrow_mut().push(count.get());
            *signal_holder.borrow_mut() = Some(count);
            PermissionManifest::default()
        }
    };

    let mut scheduler = make_scheduler_with_node("c", |s| {
        s.register_node(
            NodeDefinition::new("Counter", component),
            "c",
            Attrs::new(),
            0,
        )
        .unwrap();
    });
    scheduler.start().unwrap();
    assert_eq!(*values.borrow(), vec![0]);

    // Update the signal from outside the component — triggers re-eval.
    signal_holder.borrow().as_ref().unwrap().set(5);
    scheduler.process_batch().unwrap();
    assert_eq!(*values.borrow(), vec![0, 5]);
}

#[test]
fn use_state_supports_functional_updates_via_update() {
    let last = Rc::new(Cell::new(-1i32));
    let signal_holder: Rc<RefCell<Option<Signal<i32>>>> = Rc::new(RefCell::new(None));

    let component = {
        let last = Rc::clone(&last);
        let signal_holder = Rc::clone(&signal_holder);
        move |_ctx: &NodeContext| {
            let count: Signal<i32> = use_state(0);
            last.set(count.get());
            *signal_holder.borrow_mut() = Some(count);
            PermissionManifest::default()
        }
    };

    let mut scheduler = make_scheduler_with_node("c", |s| {
        s.register_node(
            NodeDefinition::new("Counter", component),
            "c",
            Attrs::new(),
            0,
        )
        .unwrap();
    });
    scheduler.start().unwrap();
    let signal = signal_holder.borrow().unwrap();
    signal.update(|prev| prev + 1);
    signal.update(|prev| prev + 1);
    scheduler.process_batch().unwrap();
    assert_eq!(last.get(), 2);
}

#[test]
fn use_state_skips_re_eval_when_value_unchanged() {
    let eval_count = Rc::new(Cell::new(0u32));
    let signal_holder: Rc<RefCell<Option<Signal<i32>>>> = Rc::new(RefCell::new(None));

    let component = {
        let eval_count = Rc::clone(&eval_count);
        let signal_holder = Rc::clone(&signal_holder);
        move |_ctx: &NodeContext| {
            eval_count.set(eval_count.get() + 1);
            let count: Signal<i32> = use_state(0);
            *signal_holder.borrow_mut() = Some(count);
            PermissionManifest::default()
        }
    };

    let mut scheduler = make_scheduler_with_node("c", |s| {
        s.register_node(
            NodeDefinition::new("Counter", component),
            "c",
            Attrs::new(),
            0,
        )
        .unwrap();
    });
    scheduler.start().unwrap();
    assert_eq!(eval_count.get(), 1);

    signal_holder.borrow().unwrap().set(0); // same value
    scheduler.process_batch().unwrap();
    assert_eq!(eval_count.get(), 1, "no re-eval when set to same value");
}

// ---- use_effect ----

#[test]
fn use_effect_runs_on_mount_with_empty_deps() {
    let mounted = Rc::new(Cell::new(false));

    let component = {
        let mounted = Rc::clone(&mounted);
        move |_ctx: &NodeContext| {
            let mounted = Rc::clone(&mounted);
            use_effect(
                move || {
                    mounted.set(true);
                    None
                },
                vec![],
            );
            PermissionManifest::default()
        }
    };

    let mut scheduler = make_scheduler_with_node("n", |s| {
        s.register_node(NodeDefinition::new("Node", component), "n", Attrs::new(), 0)
            .unwrap();
    });
    scheduler.start().unwrap();
    assert!(mounted.get());
}

#[test]
fn use_effect_runs_cleanup_on_re_evaluation() {
    let cleanup_count = Rc::new(Cell::new(0u32));
    let signal_holder: Rc<RefCell<Option<Signal<i32>>>> = Rc::new(RefCell::new(None));

    let component = {
        let cleanup_count = Rc::clone(&cleanup_count);
        let signal_holder = Rc::clone(&signal_holder);
        move |_ctx: &NodeContext| {
            let val: Signal<i32> = use_state(0);
            *signal_holder.borrow_mut() = Some(val);
            let cleanup_count_inner = Rc::clone(&cleanup_count);
            use_effect(
                move || {
                    let cc = Rc::clone(&cleanup_count_inner);
                    Some(Box::new(move || {
                        cc.set(cc.get() + 1);
                    }))
                },
                vec![serde_json::json!(val.get())],
            );
            PermissionManifest::default()
        }
    };

    let mut scheduler = make_scheduler_with_node("n", |s| {
        s.register_node(NodeDefinition::new("Node", component), "n", Attrs::new(), 0)
            .unwrap();
    });
    scheduler.start().unwrap();
    assert_eq!(cleanup_count.get(), 0);

    signal_holder.borrow().unwrap().set(1);
    scheduler.process_batch().unwrap();
    assert_eq!(cleanup_count.get(), 1, "cleanup from first effect ran");
}

#[test]
fn use_effect_skips_when_deps_unchanged() {
    let effect_count = Rc::new(Cell::new(0u32));
    let signal_holder: Rc<RefCell<Option<Signal<i32>>>> = Rc::new(RefCell::new(None));

    let component = {
        let effect_count = Rc::clone(&effect_count);
        let signal_holder = Rc::clone(&signal_holder);
        move |_ctx: &NodeContext| {
            let val: Signal<i32> = use_state(0);
            *signal_holder.borrow_mut() = Some(val);
            let ec_inner = Rc::clone(&effect_count);
            // Empty deps — should only run once.
            use_effect(
                move || {
                    ec_inner.set(ec_inner.get() + 1);
                    None
                },
                vec![],
            );
            PermissionManifest::default()
        }
    };

    let mut scheduler = make_scheduler_with_node("n", |s| {
        s.register_node(NodeDefinition::new("Node", component), "n", Attrs::new(), 0)
            .unwrap();
    });
    scheduler.start().unwrap();
    assert_eq!(effect_count.get(), 1);

    signal_holder.borrow().unwrap().set(1);
    scheduler.process_batch().unwrap();
    assert_eq!(effect_count.get(), 1, "effect not re-run with empty deps");
}

// ---- use_memo ----

#[test]
fn use_memo_memoizes_when_deps_unchanged() {
    let compute_count = Rc::new(Cell::new(0u32));
    let last_result: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let signal_holder: Rc<RefCell<Option<Signal<i32>>>> = Rc::new(RefCell::new(None));

    let component = {
        let compute_count = Rc::clone(&compute_count);
        let last_result = Rc::clone(&last_result);
        let signal_holder = Rc::clone(&signal_holder);
        move |_ctx: &NodeContext| {
            let val: Signal<i32> = use_state(0);
            *signal_holder.borrow_mut() = Some(val);
            let cc = Rc::clone(&compute_count);
            let v = val.get();
            let result = use_memo(
                move || {
                    cc.set(cc.get() + 1);
                    format!("computed-{v}")
                },
                vec![serde_json::json!(v)],
            );
            *last_result.borrow_mut() = result;
            PermissionManifest::default()
        }
    };

    let mut scheduler = make_scheduler_with_node("n", |s| {
        s.register_node(NodeDefinition::new("Node", component), "n", Attrs::new(), 0)
            .unwrap();
    });
    scheduler.start().unwrap();
    assert_eq!(compute_count.get(), 1);
    assert_eq!(*last_result.borrow(), "computed-0");

    // Same value — Signal::set short-circuits, no re-eval, memo stays.
    signal_holder.borrow().unwrap().set(0);
    scheduler.process_batch().unwrap();
    assert_eq!(compute_count.get(), 1);
}

#[test]
fn use_memo_recomputes_when_deps_change() {
    let compute_count = Rc::new(Cell::new(0u32));
    let signal_holder: Rc<RefCell<Option<Signal<i32>>>> = Rc::new(RefCell::new(None));

    let component = {
        let compute_count = Rc::clone(&compute_count);
        let signal_holder = Rc::clone(&signal_holder);
        move |_ctx: &NodeContext| {
            let val: Signal<i32> = use_state(0);
            *signal_holder.borrow_mut() = Some(val);
            let cc = Rc::clone(&compute_count);
            let v = val.get();
            let _result: String = use_memo(
                move || {
                    cc.set(cc.get() + 1);
                    format!("computed-{v}")
                },
                vec![serde_json::json!(v)],
            );
            PermissionManifest::default()
        }
    };

    let mut scheduler = make_scheduler_with_node("n", |s| {
        s.register_node(NodeDefinition::new("Node", component), "n", Attrs::new(), 0)
            .unwrap();
    });
    scheduler.start().unwrap();
    assert_eq!(compute_count.get(), 1);

    signal_holder.borrow().unwrap().set(1);
    scheduler.process_batch().unwrap();
    assert_eq!(compute_count.get(), 2);
}
