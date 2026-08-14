//! Bridge between the storage-agnostic core activation boundary
//! and the versioned persistence records.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use graphreact_core::{DurableActivation, RuntimeError, Scheduler};
use graphreact_persistence::{
    ActivationCommit, AttemptId, CorrelationId, DurableMessage, DurableStateWrite, LocalConfig,
    LocalPersistence, MessageId, PersistenceEngine, RunId, RunStatus, RuntimeRecordV1,
};

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

    Ok(RuntimeRecordV1::ActivationCommitted(ActivationCommit {
        run_id,
        attempt_id,
        input_message_id,
        state_writes,
        emitted_messages,
        timers_scheduled: Vec::new(),
        timers_cancelled: Vec::new(),
        effect_intents: Vec::new(),
        artifacts: Vec::new(),
        resulting_status: RunStatus::Running,
    }))
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_core::{
        use_durable_state, use_message, Attrs, CommunicationGraph, NodeContext, NodeDefinition,
        PermissionManifest,
    };

    #[test]
    fn local_committer_persists_activation_before_ack() {
        let directory = tempfile::tempdir().unwrap();
        let deployment_id = graphreact_persistence::DeploymentId::new();
        let digest = graphreact_persistence::DeploymentDigest::of(b"test-deployment");
        let config = LocalConfig::new(directory.path(), deployment_id, digest);

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
        let engine = install_local(&mut scheduler, config).unwrap();
        scheduler.start().unwrap();
        scheduler.send_durable(
            "counter",
            "increment",
            serde_json::Value::Null,
            RunId::new().to_string(),
            AttemptId::new().to_string(),
        );
        scheduler.process_batch().unwrap();

        let replayed = futures::executor::block_on(engine.replay(0)).unwrap();
        let RuntimeRecordV1::ActivationCommitted(commit) = &replayed[0] else {
            panic!("expected activation commit");
        };
        assert_eq!(commit.state_writes.len(), 1);
        assert_eq!(commit.state_writes[0].key, "count");
        assert_eq!(commit.state_writes[0].value, serde_json::json!(1));
    }
}
