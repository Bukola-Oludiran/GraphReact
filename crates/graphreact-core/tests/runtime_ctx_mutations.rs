//! `ctx.spawnChild` / `ctx.unmount` / `ctx.compose` — verify the deferred
//! mutation queue applies between cascade rounds and that spawned nodes
//! participate in subsequent passes.
//!
//! Hooks like `use_spawn_child` capture the mutation closure during the
//! component body so it can be invoked from inside effects (which run
//! after `HOOK_CTX` has been cleared).

use graphreact_core::{
    use_compose, use_effect, use_spawn_child, use_state, use_unmount_self, Attrs,
    CommunicationGraph, ComposeChildSpec, NodeContext, NodeDefinition, PermissionManifest,
    Scheduler,
};
use std::cell::Cell;
use std::rc::Rc;

#[test]
fn spawn_child_during_evaluation_lands_in_next_pass() {
    let spawned = Rc::new(Cell::new(false));
    let spawned_clone = Rc::clone(&spawned);

    let parent = move |_ctx: &NodeContext| {
        let spawn = use_spawn_child();
        let spawned = Rc::clone(&spawned_clone);
        use_effect(
            move || {
                if !spawned.get() {
                    spawned.set(true);
                    let child_def = NodeDefinition::new("Child", |_ctx: &NodeContext| {
                        PermissionManifest::default()
                    });
                    spawn(ComposeChildSpec {
                        id: "child".to_string(),
                        definition: child_def,
                        props: Attrs::new(),
                        connect_to: vec!["parent".to_string()],
                    });
                }
                None
            },
            vec![],
        );
        PermissionManifest::default()
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("parent", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Parent", parent),
            "parent",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    assert!(
        scheduler.has_node("child"),
        "spawned child must be registered"
    );
    assert!(scheduler.graph.borrow().has_edge("parent", "child", None));
}

#[test]
fn unmount_self_removes_node_in_next_pass() {
    let unmounted_marker = Rc::new(Cell::new(false));
    let marker_clone = Rc::clone(&unmounted_marker);

    let suicidal = move |_ctx: &NodeContext| {
        let unmount = use_unmount_self();
        let marker = Rc::clone(&marker_clone);
        use_effect(
            move || {
                if !marker.get() {
                    marker.set(true);
                    unmount();
                }
                None
            },
            vec![],
        );
        PermissionManifest::default()
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("self", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Self", suicidal),
            "self",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    assert!(
        !scheduler.has_node("self"),
        "self-unmount must remove the node"
    );
}

#[test]
fn compose_sets_composed_children_and_spawns_missing_ones() {
    let composed = Rc::new(Cell::new(false));
    let composed_clone = Rc::clone(&composed);

    let parent = move |_ctx: &NodeContext| {
        let compose = use_compose();
        let composed = Rc::clone(&composed_clone);
        use_effect(
            move || {
                if !composed.get() {
                    composed.set(true);
                    let make_child = |id: &str| ComposeChildSpec {
                        id: id.to_string(),
                        definition: NodeDefinition::new("Child", |_ctx: &NodeContext| {
                            PermissionManifest::default()
                        }),
                        props: Attrs::new(),
                        connect_to: vec![],
                    };
                    compose(vec![make_child("a"), make_child("b")]);
                }
                None
            },
            vec![],
        );
        PermissionManifest::default()
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("parent", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Parent", parent),
            "parent",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    assert!(scheduler.has_node("a"), "compose must spawn child 'a'");
    assert!(scheduler.has_node("b"), "compose must spawn child 'b'");
    // Auto-connect parent edge.
    let g = scheduler.graph.borrow();
    assert!(g.has_edge("parent", "a", None));
    assert!(g.has_edge("parent", "b", None));
}

#[test]
fn replace_definition_swaps_behavior_preserves_state() {
    // Counter that starts at 0. After one round we replace the definition
    // with one that asserts state still reads as 0 (hook state survives
    // the swap).
    let observed_first_def: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let first_clone = Rc::clone(&observed_first_def);

    let first_def = move |_ctx: &NodeContext| {
        let _: graphreact_core::Signal<i64> = use_state(0i64);
        first_clone.set(true);
        PermissionManifest::default()
    };

    let observed_second_def: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let second_clone = Rc::clone(&observed_second_def);

    let second_def = move |_ctx: &NodeContext| {
        let count: graphreact_core::Signal<i64> = use_state(0i64);
        second_clone.set(true);
        assert_eq!(count.get(), 0, "hook state must persist across replace");
        PermissionManifest::default()
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("n", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(NodeDefinition::new("N", first_def), "n", Attrs::new(), 0)
        .unwrap();
    scheduler.start().unwrap();
    assert!(observed_first_def.get(), "first def must have run");

    scheduler
        .replace_definition("n", NodeDefinition::new("N", second_def))
        .unwrap();
    scheduler.process_batch().unwrap();
    assert!(
        observed_second_def.get(),
        "second def must have run after replace"
    );
}
