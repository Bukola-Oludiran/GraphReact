use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use graphreact_core::Scheduler;
use graphreact_persistence::{
    CorrelationId, DurableMessage, EffectId, EffectIntent, EffectOutcome, MessageId,
    PersistenceEngine, ReplayPolicy, RunStatus, RuntimeRecordV1,
};

use crate::DurableRuntimeError;

pub type AdapterFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, EffectAdapterError>> + Send + 'a>>;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum EffectAdapterError {
    #[error("effect adapter `{0}` is not registered")]
    NotRegistered(String),
    #[error("effect operation `{operation}` failed: {message}")]
    Operation { operation: String, message: String },
    #[error("effect adapter registry lock is poisoned")]
    RegistryPoisoned,
}

/// Host implementation for externally observable durable effects.
///
/// `effect_id` is the stable idempotency key and must be reused for every
/// retry of one committed intent. Implementations declaring `NonReplayable`
/// are never automatically called again after ambiguous dispatch.
pub trait DurableEffectAdapter: std::fmt::Debug + Send + Sync {
    fn replay_policy(&self) -> ReplayPolicy;

    fn execute<'a>(
        &'a self,
        effect_id: &'a EffectId,
        operation: &'a str,
        input: &'a serde_json::Value,
    ) -> AdapterFuture<'a, serde_json::Value>;

    /// Best-effort cooperative cancellation. It cannot roll back an external
    /// side effect that already completed.
    fn cancel<'a>(&'a self, _effect_id: &'a EffectId) -> AdapterFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug, Default)]
pub struct EffectAdapterRegistry {
    adapters: BTreeMap<String, Arc<dyn DurableEffectAdapter>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EffectDispatchSummary {
    pub dispatched: usize,
    pub retried: usize,
    pub indeterminate: usize,
    pub result_messages: usize,
}

/// Dispatch every committed effect without a durable completion record.
/// Ambiguous idempotent effects reuse the original `EffectId`; ambiguous
/// non-replayable effects become indeterminate without another adapter call.
pub async fn dispatch_pending_effects(
    scheduler: &Scheduler,
    engine: &dyn PersistenceEngine,
    fencing_token: u64,
    registry: &EffectAdapterRegistry,
) -> Result<EffectDispatchSummary, DurableRuntimeError> {
    let snapshot = engine.read_snapshot().await?;
    let replay_from = snapshot
        .as_ref()
        .map_or(0, |value| value.through_sequence.saturating_add(1));
    let mut pending = BTreeMap::<EffectId, EffectIntent>::new();
    let mut dispatched = BTreeSet::<EffectId>::new();
    if let Some(snapshot) = snapshot {
        for intent in snapshot.effects {
            pending.insert(intent.id.clone(), intent);
        }
    }
    for record in engine.replay(replay_from).await? {
        match record {
            RuntimeRecordV1::ActivationCommitted(commit) => {
                for intent in commit.effect_intents {
                    pending.insert(intent.id.clone(), intent);
                }
            }
            RuntimeRecordV1::EffectDispatched { effect_id, .. } => {
                dispatched.insert(effect_id);
            }
            RuntimeRecordV1::EffectCompleted { effect_id, .. } => {
                pending.remove(&effect_id);
                dispatched.remove(&effect_id);
            }
            _ => {}
        }
    }

    let mut summary = EffectDispatchSummary::default();
    for (effect_id, intent) in pending {
        let was_dispatched = dispatched.contains(&effect_id);
        if was_dispatched && intent.replay_policy == ReplayPolicy::NonReplayable {
            let now = unix_time_ms();
            engine
                .commit(
                    fencing_token,
                    &[
                        RuntimeRecordV1::EffectCompleted {
                            effect_id,
                            outcome: EffectOutcome::Indeterminate,
                            result_message: None,
                            completed_at_ms: now,
                        },
                        RuntimeRecordV1::RunStatusChanged {
                            run_id: intent.run_id,
                            status: RunStatus::Indeterminate,
                            result: None,
                            error: Some(
                                "non-replayable effect dispatch outcome is ambiguous".into(),
                            ),
                            changed_at_ms: now,
                        },
                    ],
                )
                .await?;
            summary.indeterminate += 1;
            continue;
        }

        let adapter = registry.require(&intent.adapter)?;
        if adapter.replay_policy() != intent.replay_policy {
            return Err(DurableRuntimeError::InvalidMessage(format!(
                "effect adapter `{}` declares {:?}, intent requires {:?}",
                intent.adapter,
                adapter.replay_policy(),
                intent.replay_policy
            )));
        }
        if was_dispatched {
            summary.retried += 1;
        } else {
            engine
                .commit(
                    fencing_token,
                    &[RuntimeRecordV1::EffectDispatched {
                        effect_id: effect_id.clone(),
                        dispatched_at_ms: unix_time_ms(),
                    }],
                )
                .await?;
        }

        let execution = adapter
            .execute(&effect_id, &intent.operation, &intent.input)
            .await;
        let (outcome, message_type, payload) = match execution {
            Ok(value) => (
                EffectOutcome::Succeeded(value.clone()),
                intent.result_type.clone(),
                value,
            ),
            Err(error) => (
                EffectOutcome::Failed {
                    code: "adapter_error".into(),
                    message: error.to_string(),
                },
                intent.error_type.clone(),
                serde_json::json!({"error": error.to_string()}),
            ),
        };
        let result_message = DurableMessage {
            id: MessageId::new(),
            run_id: intent.run_id,
            attempt_id: intent.attempt_id,
            correlation_id: CorrelationId::new(),
            from: "__effect__".into(),
            to: intent.node_id,
            message_type,
            payload,
            ttl: graphreact_core::DEFAULT_TTL,
            created_at_ms: unix_time_ms(),
            direct: true,
        };
        engine
            .commit(
                fencing_token,
                &[RuntimeRecordV1::EffectCompleted {
                    effect_id,
                    outcome,
                    result_message: Some(result_message.clone()),
                    completed_at_ms: unix_time_ms(),
                }],
            )
            .await?;
        scheduler.enqueue_recovered_message(super::core_message(result_message));
        summary.dispatched += 1;
        summary.result_messages += 1;
    }
    Ok(summary)
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

impl EffectAdapterRegistry {
    pub fn register(
        &mut self,
        name: impl Into<String>,
        adapter: Arc<dyn DurableEffectAdapter>,
    ) -> Option<Arc<dyn DurableEffectAdapter>> {
        self.adapters.insert(name.into(), adapter)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn DurableEffectAdapter>> {
        self.adapters.get(name).cloned()
    }

    pub fn require(&self, name: &str) -> Result<Arc<dyn DurableEffectAdapter>, EffectAdapterError> {
        self.get(name)
            .ok_or_else(|| EffectAdapterError::NotRegistered(name.to_owned()))
    }
}

/// Deterministic adapter used by crash-boundary and exactly-once tests. The
/// first call for an `EffectId` records one external side effect; all retries
/// return the stored result without recording it again.
#[derive(Debug, Default)]
pub struct DeterministicIdempotentAdapter {
    results: Mutex<BTreeMap<EffectId, serde_json::Value>>,
    external_side_effects: AtomicU64,
    invocations: AtomicU64,
}

impl DeterministicIdempotentAdapter {
    #[must_use]
    pub fn external_side_effect_count(&self) -> u64 {
        self.external_side_effects.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn invocation_count(&self) -> u64 {
        self.invocations.load(Ordering::SeqCst)
    }
}

impl DurableEffectAdapter for DeterministicIdempotentAdapter {
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
            self.invocations.fetch_add(1, Ordering::SeqCst);
            let mut results = self
                .results
                .lock()
                .map_err(|_| EffectAdapterError::RegistryPoisoned)?;
            if let Some(result) = results.get(effect_id) {
                return Ok(result.clone());
            }
            let result = serde_json::json!({
                "effect_id": effect_id.to_string(),
                "operation": operation,
                "input": input,
            });
            results.insert(effect_id.clone(), result.clone());
            self.external_side_effects.fetch_add(1, Ordering::SeqCst);
            Ok(result)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_persistence::{
        ActivationCommit, AttemptId, DeploymentDigest, DeploymentId, LocalConfig, LocalPersistence,
        RunId,
    };

    fn test_store() -> (tempfile::TempDir, LocalPersistence) {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalPersistence::open(LocalConfig::new(
            directory.path(),
            DeploymentId::new(),
            DeploymentDigest::of(b"effect-dispatch-test"),
        ))
        .unwrap();
        (directory, store)
    }

    fn test_intent(policy: ReplayPolicy) -> EffectIntent {
        EffectIntent {
            id: EffectId::new(),
            key: "tool-call".into(),
            run_id: RunId::new(),
            attempt_id: AttemptId::new(),
            node_id: "agent".into(),
            adapter: "test".into(),
            operation: "record".into(),
            input: serde_json::json!({"value": 42}),
            result_type: "tool_result".into(),
            error_type: "tool_error".into(),
            replay_policy: policy,
        }
    }

    fn activation_with(intent: EffectIntent) -> RuntimeRecordV1 {
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

    #[test]
    fn deterministic_adapter_deduplicates_by_effect_id() {
        let adapter = DeterministicIdempotentAdapter::default();
        let effect_id = EffectId::new();
        let first = futures::executor::block_on(adapter.execute(
            &effect_id,
            "record",
            &serde_json::json!({"value": 42}),
        ))
        .unwrap();
        let retry = futures::executor::block_on(adapter.execute(
            &effect_id,
            "record",
            &serde_json::json!({"value": 999}),
        ))
        .unwrap();

        assert_eq!(retry, first);
        assert_eq!(adapter.invocation_count(), 2);
        assert_eq!(adapter.external_side_effect_count(), 1);
        assert_eq!(adapter.replay_policy(), ReplayPolicy::Idempotent);
    }

    #[test]
    fn registry_fails_closed_for_unknown_adapter() {
        let registry = EffectAdapterRegistry::default();
        assert_eq!(
            registry.require("missing").unwrap_err(),
            EffectAdapterError::NotRegistered("missing".into())
        );
    }

    #[test]
    fn ambiguous_idempotent_effect_retries_same_id_once() {
        let (_directory, store) = test_store();
        let intent = test_intent(ReplayPolicy::Idempotent);
        futures::executor::block_on(store.commit(
            1,
            &[
                activation_with(intent.clone()),
                RuntimeRecordV1::EffectDispatched {
                    effect_id: intent.id.clone(),
                    dispatched_at_ms: 1,
                },
            ],
        ))
        .unwrap();
        let adapter = Arc::new(DeterministicIdempotentAdapter::default());
        let mut registry = EffectAdapterRegistry::default();
        registry.register("test", adapter.clone());
        let scheduler = Scheduler::default();

        let summary =
            futures::executor::block_on(dispatch_pending_effects(&scheduler, &store, 1, &registry))
                .unwrap();
        assert_eq!(summary.retried, 1);
        assert_eq!(summary.result_messages, 1);
        assert_eq!(adapter.external_side_effect_count(), 1);

        let second =
            futures::executor::block_on(dispatch_pending_effects(&scheduler, &store, 1, &registry))
                .unwrap();
        assert_eq!(second, EffectDispatchSummary::default());
        assert_eq!(adapter.external_side_effect_count(), 1);
    }

    #[derive(Debug, Default)]
    struct NonReplayableAdapter {
        calls: AtomicU64,
    }

    impl DurableEffectAdapter for NonReplayableAdapter {
        fn replay_policy(&self) -> ReplayPolicy {
            ReplayPolicy::NonReplayable
        }

        fn execute<'a>(
            &'a self,
            _effect_id: &'a EffectId,
            _operation: &'a str,
            _input: &'a serde_json::Value,
        ) -> AdapterFuture<'a, serde_json::Value> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::Value::Null)
            })
        }
    }

    #[test]
    fn ambiguous_non_replayable_effect_becomes_indeterminate() {
        let (_directory, store) = test_store();
        let intent = test_intent(ReplayPolicy::NonReplayable);
        futures::executor::block_on(store.commit(
            1,
            &[
                activation_with(intent.clone()),
                RuntimeRecordV1::EffectDispatched {
                    effect_id: intent.id.clone(),
                    dispatched_at_ms: 1,
                },
            ],
        ))
        .unwrap();
        let adapter = Arc::new(NonReplayableAdapter::default());
        let mut registry = EffectAdapterRegistry::default();
        registry.register("test", adapter.clone());
        let scheduler = Scheduler::default();

        let summary =
            futures::executor::block_on(dispatch_pending_effects(&scheduler, &store, 1, &registry))
                .unwrap();
        assert_eq!(summary.indeterminate, 1);
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
        let records = futures::executor::block_on(store.replay(0)).unwrap();
        assert!(records.iter().any(|record| matches!(
            record,
            RuntimeRecordV1::RunStatusChanged {
                status: RunStatus::Indeterminate,
                ..
            }
        )));
    }
}
