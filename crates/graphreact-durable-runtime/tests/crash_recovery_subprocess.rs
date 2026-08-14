use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;

use graphreact_core::{
    use_durable_state, use_message, Attrs, CommunicationGraph, NodeContext, NodeDefinition,
    PermissionManifest, Scheduler,
};
use graphreact_durable_runtime::{
    dispatch_pending_effects, enqueue_durable_message, install_local, recover, AdapterFuture,
    DurableEffectAdapter, EffectAdapterError, EffectAdapterRegistry,
};
use graphreact_persistence::{
    ActivationCommit, AttemptId, CorrelationId, DeploymentDigest, DeploymentId, EffectId,
    EffectIntent, LocalConfig, MessageId, PersistenceEngine, ReplayPolicy, RunId, RunStatus,
    RuntimeRecordV1,
};

const CHILD_MODE_ENV: &str = "GRX_CRASH_RECOVERY_CHILD_MODE";
const CHILD_ROOT_ENV: &str = "GRX_CRASH_RECOVERY_ROOT";
const DEFAULT_SOAK_CYCLES: usize = 1;

fn counter_scheduler() -> Scheduler {
    let definition = NodeDefinition::new("Counter", |_ctx: &NodeContext| {
        let count = use_durable_state("count", serde_json::json!(0));
        use_message("increment", move |_| {
            count.update(|value| serde_json::json!(value.as_u64().unwrap_or_default() + 1));
        });
        PermissionManifest::new(vec!["increment"], Vec::<String>::new())
    });
    let mut graph = CommunicationGraph::new();
    graph.add_node("counter", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(definition, "counter", Attrs::new(), 0)
        .unwrap();
    scheduler
}

fn config(root: &Path) -> LocalConfig {
    LocalConfig::new(
        root,
        DeploymentId::parse("018f0000-0000-7000-8000-000000000001").unwrap(),
        DeploymentDigest::of(b"subprocess-crash-recovery"),
    )
}

fn enqueue(scheduler: &Scheduler, engine: &dyn graphreact_persistence::PersistenceEngine) {
    enqueue_durable_message(
        scheduler,
        engine,
        1,
        "counter",
        "increment",
        serde_json::Value::Null,
        RunId::new(),
        AttemptId::new(),
        CorrelationId::new(),
    )
    .unwrap();
}

#[derive(Debug)]
struct FileIdempotentAdapter {
    directory: PathBuf,
    crash_after_new_effect: bool,
}

impl DurableEffectAdapter for FileIdempotentAdapter {
    fn replay_policy(&self) -> ReplayPolicy {
        ReplayPolicy::Idempotent
    }

    fn execute<'a>(
        &'a self,
        effect_id: &'a EffectId,
        operation: &'a str,
        input: &'a serde_json::Value,
    ) -> AdapterFuture<'a, serde_json::Value> {
        Box::pin(async move {
            std::fs::create_dir_all(&self.directory).map_err(|error| {
                EffectAdapterError::Operation {
                    operation: operation.to_owned(),
                    message: error.to_string(),
                }
            })?;
            let path = self.directory.join(effect_id.to_string());
            let created = match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
            {
                Ok(file) => {
                    serde_json::to_writer(file, input).map_err(|error| {
                        EffectAdapterError::Operation {
                            operation: operation.to_owned(),
                            message: error.to_string(),
                        }
                    })?;
                    true
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
                Err(error) => {
                    return Err(EffectAdapterError::Operation {
                        operation: operation.to_owned(),
                        message: error.to_string(),
                    });
                }
            };
            if created && self.crash_after_new_effect {
                std::process::abort();
            }
            Ok(serde_json::json!({
                "effect_id": effect_id.to_string(),
                "operation": operation,
                "input": input,
            }))
        })
    }
}

fn effect_intent() -> EffectIntent {
    EffectIntent {
        id: EffectId::new(),
        key: "tool-call".into(),
        run_id: RunId::new(),
        attempt_id: AttemptId::new(),
        node_id: "counter".into(),
        adapter: "file-idempotent".into(),
        operation: "record".into(),
        input: serde_json::json!({ "value": 42 }),
        result_type: "tool_result".into(),
        error_type: "tool_error".into(),
        replay_policy: ReplayPolicy::Idempotent,
    }
}

fn activation_with_effect(intent: EffectIntent) -> RuntimeRecordV1 {
    RuntimeRecordV1::ActivationCommitted(ActivationCommit {
        run_id: intent.run_id.clone(),
        attempt_id: intent.attempt_id.clone(),
        input_message_id: MessageId::new(),
        state_writes: Vec::new(),
        emitted_messages: Vec::new(),
        timers_scheduled: Vec::new(),
        timers_cancelled: Vec::new(),
        effect_intents: vec![intent],
        artifacts: Vec::new(),
        resulting_status: RunStatus::WaitingEffect,
    })
}

fn effect_registry(root: &Path, crash_after_new_effect: bool) -> EffectAdapterRegistry {
    let mut registry = EffectAdapterRegistry::default();
    registry.register(
        "file-idempotent",
        Arc::new(FileIdempotentAdapter {
            directory: root.join("external-effects"),
            crash_after_new_effect,
        }),
    );
    registry
}

#[test]
#[ignore = "subprocess worker; invoked by crash_boundaries_recover_across_processes"]
fn crash_recovery_worker() {
    let mode = std::env::var(CHILD_MODE_ENV).expect("worker mode");
    let root = PathBuf::from(std::env::var_os(CHILD_ROOT_ENV).expect("worker root"));
    let mut scheduler = counter_scheduler();
    let engine = install_local(&mut scheduler, config(&root)).unwrap();

    match mode.as_str() {
        "crash_before_commit" => {
            scheduler.start().unwrap();
            enqueue(&scheduler, engine.as_ref());
            std::process::abort();
        }
        "recover_uncommitted" => {
            let summary =
                futures::executor::block_on(recover(&scheduler, engine.as_ref())).unwrap();
            assert_eq!(summary.pending_messages, 1);
            scheduler.start().unwrap();
            assert_eq!(
                scheduler.snapshot_durable_state().nodes["counter"]["count"],
                serde_json::json!(1)
            );
        }
        "crash_after_commit" => {
            scheduler.start().unwrap();
            enqueue(&scheduler, engine.as_ref());
            scheduler.process_batch().unwrap();
            std::process::abort();
        }
        "recover_committed" => {
            let summary =
                futures::executor::block_on(recover(&scheduler, engine.as_ref())).unwrap();
            assert_eq!(summary.pending_messages, 0);
            scheduler.start().unwrap();
            assert_eq!(
                scheduler.snapshot_durable_state().nodes["counter"]["count"],
                serde_json::json!(1)
            );
        }
        "effect_intent_committed_crash" => {
            futures::executor::block_on(
                engine.commit(1, &[activation_with_effect(effect_intent())]),
            )
            .unwrap();
            std::process::abort();
        }
        "effect_dispatch_ambiguous_crash" => {
            let registry = effect_registry(&root, true);
            let _ = futures::executor::block_on(dispatch_pending_effects(
                &scheduler,
                engine.as_ref(),
                1,
                &registry,
            ));
            panic!("adapter should abort after recording the external effect");
        }
        "effect_recover_retry" => {
            let registry = effect_registry(&root, false);
            let summary = futures::executor::block_on(dispatch_pending_effects(
                &scheduler,
                engine.as_ref(),
                1,
                &registry,
            ))
            .unwrap();
            assert_eq!(summary.retried, 1);
            assert_eq!(summary.result_messages, 1);
            let second = futures::executor::block_on(dispatch_pending_effects(
                &scheduler,
                engine.as_ref(),
                1,
                &registry,
            ))
            .unwrap();
            assert_eq!(second.dispatched, 0);
            assert_eq!(
                std::fs::read_dir(root.join("external-effects"))
                    .unwrap()
                    .count(),
                1
            );
            let completed = futures::executor::block_on(engine.replay(0))
                .unwrap()
                .into_iter()
                .filter(|record| matches!(record, RuntimeRecordV1::EffectCompleted { .. }))
                .count();
            assert_eq!(completed, 1);
        }
        other => panic!("unknown worker mode {other}"),
    }
}

fn run_worker(mode: &str, root: &Path) -> ExitStatus {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "crash_recovery_worker",
            "--nocapture",
        ])
        .env(CHILD_MODE_ENV, mode)
        .env(CHILD_ROOT_ENV, root)
        .output()
        .unwrap();
    let expects_crash = mode.starts_with("crash_") || mode.ends_with("_crash");
    assert!(
        output.status.success() || expects_crash,
        "worker {mode} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.status
}

fn soak_cycles() -> usize {
    std::env::var("GRX_CRASH_SOAK_CYCLES")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|cycles| *cycles != 0)
        .unwrap_or(DEFAULT_SOAK_CYCLES)
}

#[test]
fn crash_boundaries_recover_across_processes() {
    let directory = tempfile::tempdir().unwrap();
    for cycle in 0..soak_cycles() {
        let before = directory.path().join(format!("cycle-{cycle}-before"));
        let after = directory.path().join(format!("cycle-{cycle}-after"));
        let effect = directory.path().join(format!("cycle-{cycle}-effect"));

        assert!(!run_worker("crash_before_commit", &before).success());
        assert!(run_worker("recover_uncommitted", &before).success());
        assert!(!run_worker("crash_after_commit", &after).success());
        assert!(run_worker("recover_committed", &after).success());
        assert!(!run_worker("effect_intent_committed_crash", &effect).success());
        assert!(!run_worker("effect_dispatch_ambiguous_crash", &effect).success());
        assert!(run_worker("effect_recover_retry", &effect).success());
    }
}
