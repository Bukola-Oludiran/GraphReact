//! Bridge between the storage-agnostic core activation boundary
//! and the versioned persistence records.

mod effects;

pub use effects::{
    AdapterFuture, DeterministicIdempotentAdapter, DurableEffectAdapter, EffectAdapterError,
    EffectAdapterRegistry,
};

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use graphreact_core::{DurableActivation, RuntimeError, Scheduler};
use graphreact_persistence::{
    ActivationCommit, AttemptId, CorrelationId, DurableMessage, DurableStateWrite, EffectId,
    EffectIntent, LocalConfig, LocalPersistence, MessageId, PersistenceEngine, ReplayPolicy, RunId,
    RunStatus, RuntimeRecordV1,
};

#[derive(Debug, thiserror::Error)]
pub enum DurableRuntimeError {
    #[error(transparent)]
    Persistence(#[from] graphreact_persistence::PersistenceError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error("invalid persisted durable message: {0}")]
    InvalidMessage(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoverySummary {
    pub snapshot_sequence: Option<u64>,
    pub replayed_records: usize,
    pub pending_messages: usize,
    pub postponed_messages: usize,
}

/// Open a locked local backend and install its fsync-before-ack activation
/// committer on the scheduler. The returned engine must remain alive for the
/// lifetime of the scheduler and is also used for replay/snapshot operations.
pub fn install_local(
    scheduler: &mut Scheduler,
    config: LocalConfig,
) -> graphreact_persistence::Result<Arc<LocalPersistence>> {
    let fencing_token = config.fencing_token;
    let engine = Arc::new(LocalPersistence::open(config)?);
    install_engine(
        scheduler,
        Arc::clone(&engine) as Arc<dyn PersistenceEngine>,
        fencing_token,
    );
    Ok(engine)
}

/// Install any persistence backend behind the same activation boundary.
pub fn install_engine(
    scheduler: &mut Scheduler,
    engine: Arc<dyn PersistenceEngine>,
    fencing_token: u64,
) {
    scheduler.install_durable_activation_committer(Rc::new(move |activation| {
        let record = activation_record(activation)
            .map_err(|message| RuntimeError::DurableCommit { message })?;
        futures::executor::block_on(engine.commit(fencing_token, &[record]))
            .map(|_| ())
            .map_err(|error| RuntimeError::DurableCommit {
                message: error.to_string(),
            })
    }));
}

/// Persist an initial durable inbox message before making it visible to the
/// scheduler. Recovery reconstructs it if the process exits before delivery.
#[allow(clippy::too_many_arguments)]
pub fn enqueue_durable_message(
    scheduler: &Scheduler,
    engine: &dyn PersistenceEngine,
    fencing_token: u64,
    target: impl Into<String>,
    message_type: impl Into<String>,
    payload: serde_json::Value,
    run_id: RunId,
    attempt_id: AttemptId,
    correlation_id: CorrelationId,
) -> Result<MessageId, DurableRuntimeError> {
    let mut message =
        graphreact_core::create_message(message_type, "__external__", target, payload, None);
    let message_id = MessageId::new();
    message.id = message_id.to_string();
    message.run_id = Some(run_id.to_string());
    message.attempt_id = Some(attempt_id.to_string());
    message.correlation_id = Some(correlation_id.to_string());
    let durable = durable_message(&message).map_err(DurableRuntimeError::InvalidMessage)?;
    futures::executor::block_on(
        engine.commit(fencing_token, &[RuntimeRecordV1::MessageEnqueued(durable)]),
    )?;
    scheduler.enqueue_recovered_message(message);
    Ok(message_id)
}

/// Rebuild named state and pending work from a verified snapshot plus log.
/// Inputs consumed by a later activation commit are removed, while committed
/// outputs not yet consumed are re-enqueued without rerunning their producer.
pub async fn recover(
    scheduler: &Scheduler,
    engine: &dyn PersistenceEngine,
) -> Result<RecoverySummary, DurableRuntimeError> {
    let snapshot = engine.read_snapshot().await?;
    let snapshot_sequence = snapshot.as_ref().map(|value| value.through_sequence);
    let replay_from = snapshot_sequence.map_or(0, |sequence| sequence.saturating_add(1));
    let mut state = BTreeMap::<String, BTreeMap<String, serde_json::Value>>::new();
    let mut inbox = Vec::<DurableMessage>::new();
    let mut postponed = Vec::<DurableMessage>::new();
    if let Some(snapshot) = snapshot {
        apply_state_writes(&mut state, &snapshot.durable_state);
        inbox = snapshot.inbox;
        postponed = snapshot.postponed;
    }

    let records = engine.replay(replay_from).await?;
    for record in &records {
        match record {
            RuntimeRecordV1::MessageEnqueued(message) => {
                upsert_message(&mut inbox, message.clone());
            }
            RuntimeRecordV1::ActivationCommitted(commit) => {
                inbox.retain(|message| message.id != commit.input_message_id);
                postponed.retain(|message| message.id != commit.input_message_id);
                apply_state_writes(&mut state, &commit.state_writes);
                for message in &commit.emitted_messages {
                    upsert_message(&mut inbox, message.clone());
                }
            }
            _ => {}
        }
    }

    scheduler.restore_durable_state(graphreact_core::DurableStateSnapshot { nodes: state })?;
    let pending_messages = inbox.len();
    for message in inbox {
        scheduler.enqueue_recovered_message(core_message(message));
    }
    let postponed_messages = postponed.len();
    scheduler.restore_postponed_messages(postponed.into_iter().map(core_message))?;
    Ok(RecoverySummary {
        snapshot_sequence,
        replayed_records: records.len(),
        pending_messages,
        postponed_messages,
    })
}

fn apply_state_writes(
    state: &mut BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    writes: &[DurableStateWrite],
) {
    for write in writes {
        state
            .entry(write.node_id.clone())
            .or_default()
            .insert(write.key.clone(), write.value.clone());
    }
}

fn upsert_message(messages: &mut Vec<DurableMessage>, message: DurableMessage) {
    if let Some(existing) = messages.iter_mut().find(|value| value.id == message.id) {
        *existing = message;
    } else {
        messages.push(message);
    }
}

fn core_message(message: DurableMessage) -> graphreact_core::Message {
    graphreact_core::Message {
        id: message.id.to_string(),
        r#type: message.message_type,
        from: message.from,
        to: message.to,
        payload: message.payload,
        ttl: message.ttl,
        timestamp: message.created_at_ms,
        direct: message.direct,
        trace_context: None,
        correlation_id: Some(message.correlation_id.to_string()),
        run_id: Some(message.run_id.to_string()),
        attempt_id: Some(message.attempt_id.to_string()),
    }
}

fn activation_record(activation: &DurableActivation) -> Result<RuntimeRecordV1, String> {
    let run_id = required_id(activation.input.run_id.as_deref(), "run id", RunId::parse)?;
    let attempt_id = required_id(
        activation.input.attempt_id.as_deref(),
        "attempt id",
        AttemptId::parse,
    )?;
    let input_message_id = MessageId::parse(activation.input.id.clone())
        .map_err(|error| format!("invalid durable input message id: {error}"))?;
    let state_writes = state_diff(
        &activation.state_before.nodes,
        &activation.state_after.nodes,
    );
    let emitted_messages = activation
        .emitted_messages
        .iter()
        .map(durable_message)
        .collect::<Result<Vec<_>, _>>()?;
    let effect_intents = activation
        .effect_intents
        .iter()
        .map(durable_effect_intent)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(RuntimeRecordV1::ActivationCommitted(ActivationCommit {
        run_id,
        attempt_id,
        input_message_id,
        state_writes,
        emitted_messages,
        timers_scheduled: Vec::new(),
        timers_cancelled: Vec::new(),
        effect_intents,
        artifacts: Vec::new(),
        resulting_status: RunStatus::Running,
    }))
}

fn durable_effect_intent(
    intent: &graphreact_core::DurableEffectIntent,
) -> Result<EffectIntent, String> {
    Ok(EffectIntent {
        id: EffectId::parse(intent.id.clone())
            .map_err(|error| format!("invalid durable effect id: {error}"))?,
        key: intent.key.clone(),
        run_id: RunId::parse(intent.run_id.clone())
            .map_err(|error| format!("invalid durable effect run id: {error}"))?,
        attempt_id: AttemptId::parse(intent.attempt_id.clone())
            .map_err(|error| format!("invalid durable effect attempt id: {error}"))?,
        node_id: intent.node_id.clone(),
        adapter: intent.adapter.clone(),
        operation: intent.operation.clone(),
        input: intent.input.clone(),
        result_type: intent.result_type.clone(),
        error_type: intent.error_type.clone(),
        replay_policy: match intent.replay_policy {
            graphreact_core::DurableReplayPolicy::Idempotent => ReplayPolicy::Idempotent,
            graphreact_core::DurableReplayPolicy::NonReplayable => ReplayPolicy::NonReplayable,
        },
    })
}

fn required_id<T, E>(
    value: Option<&str>,
    label: &str,
    parse: impl FnOnce(String) -> Result<T, E>,
) -> Result<T, String>
where
    E: std::fmt::Display,
{
    let value = value.ok_or_else(|| format!("durable activation is missing {label}"))?;
    parse(value.to_owned()).map_err(|error| format!("invalid durable {label}: {error}"))
}

fn state_diff(
    before: &BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    after: &BTreeMap<String, BTreeMap<String, serde_json::Value>>,
) -> Vec<DurableStateWrite> {
    after
        .iter()
        .flat_map(|(node_id, values)| {
            values
                .iter()
                .filter(move |(key, value)| {
                    before.get(node_id).and_then(|old| old.get(*key)) != Some(*value)
                })
                .map(move |(key, value)| DurableStateWrite {
                    node_id: node_id.clone(),
                    key: key.clone(),
                    value: value.clone(),
                })
        })
        .collect()
}

fn durable_message(message: &graphreact_core::Message) -> Result<DurableMessage, String> {
    let run_id = required_id(
        message.run_id.as_deref(),
        "emitted message run id",
        RunId::parse,
    )?;
    let attempt_id = required_id(
        message.attempt_id.as_deref(),
        "emitted message attempt id",
        AttemptId::parse,
    )?;
    let id = MessageId::parse(message.id.clone())
        .map_err(|error| format!("invalid emitted message id: {error}"))?;
    let correlation_id = message
        .correlation_id
        .clone()
        .and_then(|value| CorrelationId::parse(value).ok())
        .unwrap_or_default();
    Ok(DurableMessage {
        id,
        run_id,
        attempt_id,
        correlation_id,
        from: message.from.clone(),
        to: message.to.clone(),
        message_type: message.r#type.clone(),
        payload: message.payload.clone(),
        ttl: message.ttl,
        created_at_ms: message.timestamp,
        direct: message.direct,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_core::{
        use_durable_state, use_message, Attrs, CommunicationGraph, NodeContext, NodeDefinition,
        PermissionManifest,
    };

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

    #[test]
    fn local_committer_persists_activation_before_ack() {
        let directory = tempfile::tempdir().unwrap();
        let deployment_id = graphreact_persistence::DeploymentId::new();
        let digest = graphreact_persistence::DeploymentDigest::of(b"test-deployment");
        let config = LocalConfig::new(directory.path(), deployment_id, digest);

        let mut scheduler = counter_scheduler();
        let engine = install_local(&mut scheduler, config).unwrap();
        scheduler.start().unwrap();
        enqueue_durable_message(
            &scheduler,
            engine.as_ref(),
            1,
            "counter",
            "increment",
            serde_json::Value::Null,
            RunId::new(),
            AttemptId::new(),
            CorrelationId::new(),
        )
        .unwrap();
        scheduler.process_batch().unwrap();

        let replayed = futures::executor::block_on(engine.replay(0)).unwrap();
        let commit = replayed
            .iter()
            .find_map(|record| match record {
                RuntimeRecordV1::ActivationCommitted(commit) => Some(commit),
                _ => None,
            })
            .expect("expected activation commit");
        assert_eq!(commit.state_writes.len(), 1);
        assert_eq!(commit.state_writes[0].key, "count");
        assert_eq!(commit.state_writes[0].value, serde_json::json!(1));
    }

    #[test]
    fn recovery_retries_uncommitted_input() {
        let directory = tempfile::tempdir().unwrap();
        let config = LocalConfig::new(
            directory.path(),
            graphreact_persistence::DeploymentId::new(),
            graphreact_persistence::DeploymentDigest::of(b"recovery-deployment"),
        );
        let mut first = counter_scheduler();
        let first_engine = install_local(&mut first, config.clone()).unwrap();
        first.start().unwrap();
        enqueue_durable_message(
            &first,
            first_engine.as_ref(),
            1,
            "counter",
            "increment",
            serde_json::Value::Null,
            RunId::new(),
            AttemptId::new(),
            CorrelationId::new(),
        )
        .unwrap();
        drop(first);
        drop(first_engine);

        let mut recovered = counter_scheduler();
        let recovered_engine = install_local(&mut recovered, config).unwrap();
        let summary =
            futures::executor::block_on(recover(&recovered, recovered_engine.as_ref())).unwrap();
        assert_eq!(summary.pending_messages, 1);
        recovered.start().unwrap();
        assert_eq!(
            recovered.snapshot_durable_state().nodes["counter"]["count"],
            serde_json::json!(1)
        );
    }

    #[test]
    fn recovery_does_not_rerun_committed_activation() {
        let directory = tempfile::tempdir().unwrap();
        let config = LocalConfig::new(
            directory.path(),
            graphreact_persistence::DeploymentId::new(),
            graphreact_persistence::DeploymentDigest::of(b"committed-deployment"),
        );
        let mut first = counter_scheduler();
        let first_engine = install_local(&mut first, config.clone()).unwrap();
        first.start().unwrap();
        enqueue_durable_message(
            &first,
            first_engine.as_ref(),
            1,
            "counter",
            "increment",
            serde_json::Value::Null,
            RunId::new(),
            AttemptId::new(),
            CorrelationId::new(),
        )
        .unwrap();
        first.process_batch().unwrap();
        drop(first);
        drop(first_engine);

        let mut recovered = counter_scheduler();
        let recovered_engine = install_local(&mut recovered, config).unwrap();
        let summary =
            futures::executor::block_on(recover(&recovered, recovered_engine.as_ref())).unwrap();
        assert_eq!(summary.pending_messages, 0);
        recovered.start().unwrap();
        assert_eq!(
            recovered.snapshot_durable_state().nodes["counter"]["count"],
            serde_json::json!(1)
        );
    }

    #[test]
    fn effect_intent_is_persisted_in_the_activation_record() {
        let directory = tempfile::tempdir().unwrap();
        let config = LocalConfig::new(
            directory.path(),
            graphreact_persistence::DeploymentId::new(),
            graphreact_persistence::DeploymentDigest::of(b"effect-deployment"),
        );
        let definition = NodeDefinition::new("Agent", |_ctx: &NodeContext| {
            let effect = graphreact_core::use_durable_effect();
            use_message("start", move |_| {
                effect(graphreact_core::DurableEffectRequest {
                    key: "tool-call".into(),
                    adapter: "deterministic".into(),
                    operation: "record".into(),
                    input: serde_json::json!({"value": 42}),
                    result_type: "tool_result".into(),
                    error_type: "tool_error".into(),
                    replay_policy: graphreact_core::DurableReplayPolicy::Idempotent,
                })
                .unwrap();
            });
            PermissionManifest::new(vec!["start"], Vec::<String>::new())
        });
        let mut graph = CommunicationGraph::new();
        graph.add_node("agent", Attrs::new());
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "agent", Attrs::new(), 0)
            .unwrap();
        let engine = install_local(&mut scheduler, config).unwrap();
        scheduler.start().unwrap();
        enqueue_durable_message(
            &scheduler,
            engine.as_ref(),
            1,
            "agent",
            "start",
            serde_json::Value::Null,
            RunId::new(),
            AttemptId::new(),
            CorrelationId::new(),
        )
        .unwrap();
        scheduler.process_batch().unwrap();

        let records = futures::executor::block_on(engine.replay(0)).unwrap();
        let commit = records
            .iter()
            .find_map(|record| match record {
                RuntimeRecordV1::ActivationCommitted(commit) => Some(commit),
                _ => None,
            })
            .unwrap();
        assert_eq!(commit.effect_intents.len(), 1);
        assert_eq!(commit.effect_intents[0].key, "tool-call");
        assert_eq!(
            commit.effect_intents[0].replay_policy,
            ReplayPolicy::Idempotent
        );
    }
}
