use std::collections::{BTreeMap, BTreeSet};

use graphreact_core::Scheduler;
use graphreact_persistence::{
    CorrelationId, DurableMessage, MessageId, PersistenceEngine, RunId, RuntimeRecordV1, TimerId,
    TimerIntent,
};

use crate::DurableRuntimeError;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimerDispatchSummary {
    pub fired: usize,
    pub cancelled: usize,
    pub pending: usize,
}

/// Fire every timer whose absolute deadline has passed. Each firing and its
/// message share one durable record, so restart can enqueue the message but
/// can never fire the timer a second time.
pub async fn fire_due_timers(
    scheduler: &Scheduler,
    engine: &dyn PersistenceEngine,
    fencing_token: u64,
    now_unix_ms: u64,
) -> Result<TimerDispatchSummary, DurableRuntimeError> {
    let snapshot = engine.read_snapshot().await?;
    let replay_from = snapshot
        .as_ref()
        .map_or(0, |value| value.through_sequence.saturating_add(1));
    let mut pending = BTreeMap::<TimerId, TimerIntent>::new();
    let mut cancelled_runs = BTreeSet::<RunId>::new();
    if let Some(snapshot) = snapshot {
        for timer in snapshot.timers {
            pending.insert(timer.id.clone(), timer);
        }
    }
    for record in engine.replay(replay_from).await? {
        match record {
            RuntimeRecordV1::ActivationCommitted(commit) => {
                for id in commit.timers_cancelled {
                    pending.remove(&id);
                }
                for timer in commit.timers_scheduled {
                    pending.insert(timer.id.clone(), timer);
                }
            }
            RuntimeRecordV1::TimerFired { timer_id, .. }
            | RuntimeRecordV1::TimerCancelled { timer_id, .. } => {
                pending.remove(&timer_id);
            }
            RuntimeRecordV1::CancellationRequested { run_id, .. } => {
                cancelled_runs.insert(run_id);
            }
            _ => {}
        }
    }

    let mut summary = TimerDispatchSummary::default();
    for (timer_id, timer) in pending {
        if cancelled_runs.contains(&timer.run_id) {
            engine
                .commit(
                    fencing_token,
                    &[RuntimeRecordV1::TimerCancelled {
                        timer_id,
                        cancelled_at_ms: now_unix_ms,
                    }],
                )
                .await?;
            summary.cancelled += 1;
        } else if timer.deadline_unix_ms <= now_unix_ms {
            let message = DurableMessage {
                id: MessageId::new(),
                run_id: timer.run_id,
                attempt_id: timer.attempt_id,
                correlation_id: CorrelationId::new(),
                from: "__timer__".into(),
                to: timer.node_id,
                message_type: timer.message_type,
                payload: timer.payload,
                ttl: graphreact_core::DEFAULT_TTL,
                created_at_ms: now_unix_ms,
                direct: true,
            };
            engine
                .commit(
                    fencing_token,
                    &[RuntimeRecordV1::TimerFired {
                        timer_id,
                        result_message: message.clone(),
                        fired_at_ms: now_unix_ms,
                    }],
                )
                .await?;
            scheduler.enqueue_recovered_message(super::core_message(message));
            summary.fired += 1;
        } else {
            summary.pending += 1;
        }
    }
    Ok(summary)
}
