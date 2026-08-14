use graphreact_core::Scheduler;
use graphreact_grx_parser::GrxIr;
use graphreact_persistence::{
    canonical_json_bytes, AttemptId, CorrelationId, DurableMessage, MessageId, PersistenceEngine,
    RunId, RunStatus, RuntimeRecordV1,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::DurableRuntimeError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunInfo {
    pub run_id: RunId,
    pub entrypoint: String,
    pub status: RunStatus,
    pub created_at_ms: u64,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRunOutcome {
    pub run_id: RunId,
    pub created: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RunServiceError {
    #[error(transparent)]
    Runtime(#[from] DurableRuntimeError),
    #[error("idempotency key must not be empty")]
    EmptyIdempotencyKey,
    #[error("unknown deployment entrypoint `{0}`")]
    UnknownEntrypoint(String),
    #[error("idempotency key was already used for different request content")]
    IdempotencyConflict,
    #[error("durable run `{0}` was not found")]
    RunNotFound(RunId),
    #[error("durable run `{run_id}` is already terminal with status {status:?}")]
    RunTerminal { run_id: RunId, status: RunStatus },
}

#[allow(clippy::too_many_arguments)]
pub async fn start_run(
    scheduler: &Scheduler,
    engine: &dyn PersistenceEngine,
    fencing_token: u64,
    ir: &GrxIr,
    entrypoint_name: &str,
    payload: serde_json::Value,
    idempotency_key: &str,
) -> Result<StartRunOutcome, RunServiceError> {
    if idempotency_key.trim().is_empty() {
        return Err(RunServiceError::EmptyIdempotencyKey);
    }
    let entrypoint = ir
        .entrypoints
        .iter()
        .find(|entrypoint| entrypoint.name == entrypoint_name)
        .ok_or_else(|| RunServiceError::UnknownEntrypoint(entrypoint_name.to_owned()))?;
    let request_hash: [u8; 32] = Sha256::digest(canonical_json_bytes(&serde_json::json!({
        "entrypoint": entrypoint_name,
        "payload": payload,
    })))
    .into();

    for record in engine.replay(0).await.map_err(DurableRuntimeError::from)? {
        if let RuntimeRecordV1::RunCreated {
            run_id,
            request_hash: existing_hash,
            idempotency_key: existing_key,
            ..
        } = record
        {
            if existing_key == idempotency_key {
                if existing_hash == request_hash {
                    return Ok(StartRunOutcome {
                        run_id,
                        created: false,
                    });
                }
                return Err(RunServiceError::IdempotencyConflict);
            }
        }
    }

    let run_id = RunId::new();
    let attempt_id = AttemptId::new();
    let correlation_id = CorrelationId::new();
    let message_id = MessageId::new();
    let created_at_ms = unix_time_ms();
    let durable_message = DurableMessage {
        id: message_id.clone(),
        run_id: run_id.clone(),
        attempt_id: attempt_id.clone(),
        correlation_id: correlation_id.clone(),
        from: "__external__".into(),
        to: entrypoint.target.clone(),
        message_type: entrypoint.request.clone(),
        payload: payload.clone(),
        ttl: graphreact_core::DEFAULT_TTL,
        created_at_ms,
        direct: true,
    };
    engine
        .commit(
            fencing_token,
            &[
                RuntimeRecordV1::RunCreated {
                    run_id: run_id.clone(),
                    entrypoint: entrypoint_name.to_owned(),
                    request_hash,
                    idempotency_key: idempotency_key.to_owned(),
                    created_at_ms,
                },
                RuntimeRecordV1::AttemptStarted {
                    run_id: run_id.clone(),
                    attempt_id: attempt_id.clone(),
                    number: 1,
                    started_at_ms: created_at_ms,
                },
                RuntimeRecordV1::MessageEnqueued(durable_message),
            ],
        )
        .await
        .map_err(DurableRuntimeError::from)?;

    let mut message = graphreact_core::create_message(
        entrypoint.request.clone(),
        "__external__",
        entrypoint.target.clone(),
        payload,
        None,
    );
    message.id = message_id.to_string();
    message.run_id = Some(run_id.to_string());
    message.attempt_id = Some(attempt_id.to_string());
    message.correlation_id = Some(correlation_id.to_string());
    message.direct = true;
    scheduler.enqueue_recovered_message(message);

    Ok(StartRunOutcome {
        run_id,
        created: true,
    })
}

pub async fn get_run(
    engine: &dyn PersistenceEngine,
    run_id: &RunId,
) -> Result<RunInfo, RunServiceError> {
    let mut info = None;
    for record in engine.replay(0).await.map_err(DurableRuntimeError::from)? {
        match record {
            RuntimeRecordV1::RunCreated {
                run_id: record_run_id,
                entrypoint,
                created_at_ms,
                ..
            } if &record_run_id == run_id => {
                info = Some(RunInfo {
                    run_id: record_run_id,
                    entrypoint,
                    status: RunStatus::Queued,
                    created_at_ms,
                    result: None,
                    error: None,
                });
            }
            RuntimeRecordV1::ActivationCommitted(commit) if &commit.run_id == run_id => {
                if let Some(info) = info.as_mut() {
                    info.status = commit.resulting_status;
                }
            }
            RuntimeRecordV1::CancellationRequested {
                run_id: record_run_id,
                ..
            } if &record_run_id == run_id => {
                if let Some(info) = info.as_mut() {
                    info.status = RunStatus::Cancelling;
                }
            }
            RuntimeRecordV1::RunStatusChanged {
                run_id: record_run_id,
                status,
                result,
                error,
                ..
            } if &record_run_id == run_id => {
                if let Some(info) = info.as_mut() {
                    info.status = status;
                    info.result = result;
                    info.error = error;
                }
            }
            _ => {}
        }
    }
    info.ok_or_else(|| RunServiceError::RunNotFound(run_id.clone()))
}

pub async fn cancel_run(
    scheduler: &Scheduler,
    engine: &dyn PersistenceEngine,
    fencing_token: u64,
    run_id: &RunId,
) -> Result<RunInfo, RunServiceError> {
    let mut info = get_run(engine, run_id).await?;
    if info.status == RunStatus::Cancelling || info.status == RunStatus::Cancelled {
        return Ok(info);
    }
    if info.status.is_terminal() {
        return Err(RunServiceError::RunTerminal {
            run_id: run_id.clone(),
            status: info.status,
        });
    }
    let changed_at_ms = unix_time_ms();
    engine
        .commit(
            fencing_token,
            &[
                RuntimeRecordV1::CancellationRequested {
                    run_id: run_id.clone(),
                    requested_at_ms: changed_at_ms,
                },
                RuntimeRecordV1::RunStatusChanged {
                    run_id: run_id.clone(),
                    status: RunStatus::Cancelling,
                    result: None,
                    error: None,
                    changed_at_ms,
                },
            ],
        )
        .await
        .map_err(DurableRuntimeError::from)?;
    scheduler.cancel_durable_run(run_id.to_string());
    info.status = RunStatus::Cancelling;
    Ok(info)
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_core::CommunicationGraph;
    use graphreact_persistence::{DeploymentDigest, DeploymentId, LocalConfig, LocalPersistence};

    fn fixture() -> (tempfile::TempDir, LocalPersistence, Scheduler, GrxIr) {
        let directory = tempfile::tempdir().unwrap();
        let engine = LocalPersistence::open(LocalConfig::new(
            directory.path(),
            DeploymentId::new(),
            DeploymentDigest::of(b"runs"),
        ))
        .unwrap();
        let scheduler = Scheduler::new(CommunicationGraph::new());
        let ir = graphreact_grx_parser::ir::parse_to_ir(
            r#"<Graph id="g"><Agent id="agent" /><entrypoint name="summarize" target="agent" request="prompt" reply="completion" /></Graph>"#,
        )
        .unwrap();
        (directory, engine, scheduler, ir)
    }

    #[test]
    fn same_idempotency_request_returns_original_run() {
        futures::executor::block_on(async {
            let (_directory, engine, scheduler, ir) = fixture();
            let first = start_run(
                &scheduler,
                &engine,
                1,
                &ir,
                "summarize",
                serde_json::json!({"prompt": "hello"}),
                "request-1",
            )
            .await
            .unwrap();
            let second = start_run(
                &scheduler,
                &engine,
                1,
                &ir,
                "summarize",
                serde_json::json!({"prompt": "hello"}),
                "request-1",
            )
            .await
            .unwrap();
            assert!(first.created);
            assert!(!second.created);
            assert_eq!(first.run_id, second.run_id);
        });
    }

    #[test]
    fn changed_request_conflicts_and_cancellation_is_durable() {
        futures::executor::block_on(async {
            let (_directory, engine, scheduler, ir) = fixture();
            let started = start_run(
                &scheduler,
                &engine,
                1,
                &ir,
                "summarize",
                serde_json::json!({"prompt": "hello"}),
                "request-1",
            )
            .await
            .unwrap();
            let conflict = start_run(
                &scheduler,
                &engine,
                1,
                &ir,
                "summarize",
                serde_json::json!({"prompt": "different"}),
                "request-1",
            )
            .await
            .unwrap_err();
            assert!(matches!(conflict, RunServiceError::IdempotencyConflict));

            let cancelled = cancel_run(&scheduler, &engine, 1, &started.run_id)
                .await
                .unwrap();
            assert_eq!(cancelled.status, RunStatus::Cancelling);
            assert!(scheduler.is_durable_run_cancelled(started.run_id.as_str()));
            assert_eq!(
                get_run(&engine, &started.run_id).await.unwrap().status,
                RunStatus::Cancelling
            );
        });
    }
}
