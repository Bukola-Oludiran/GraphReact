//! Shared language-runtime bridge for activation-scoped durable operations.

use graphreact_core::{
    use_durable_artifact, use_durable_cancellation, use_durable_effect, use_durable_timer,
    DurableArtifactRequest, DurableEffectRequest, DurableTimerCommand, DurableTimerRequest,
};
use graphreact_engine_zig::{BoxCallback, EngineError, HostValue};

fn decode<T: serde::de::DeserializeOwned>(
    operation: &str,
    value: Option<&HostValue>,
) -> Result<T, EngineError> {
    let json = value
        .cloned()
        .map(HostValue::into_json)
        .unwrap_or(serde_json::Value::Null);
    serde_json::from_value(json)
        .map_err(|error| EngineError::Internal(format!("{operation}: invalid request: {error}")))
}

pub(crate) fn build_effect_callback() -> BoxCallback {
    Box::new(|args| {
        let request: DurableEffectRequest = decode("host_durable_effect", args.first())?;
        use_durable_effect()(request)
            .map(HostValue::String)
            .map_err(|error| EngineError::Internal(error.to_string()))
    })
}

pub(crate) fn build_timer_schedule_callback() -> BoxCallback {
    Box::new(|args| {
        let request: DurableTimerRequest = decode("host_durable_timer_schedule", args.first())?;
        let timer_id = use_durable_timer()(DurableTimerCommand::Schedule(request))
            .map_err(|error| EngineError::Internal(error.to_string()))?
            .ok_or_else(|| {
                EngineError::Internal("durable timer schedule returned no timer id".into())
            })?;
        Ok(HostValue::String(timer_id))
    })
}

pub(crate) fn build_timer_cancel_callback() -> BoxCallback {
    Box::new(|args| {
        let timer_id = match args.first() {
            Some(HostValue::String(timer_id)) if !timer_id.trim().is_empty() => timer_id.clone(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_durable_timer_cancel: expected non-empty timer id, got {other:?}"
                )))
            }
        };
        use_durable_timer()(DurableTimerCommand::Cancel { timer_id })
            .map(|_| HostValue::Null)
            .map_err(|error| EngineError::Internal(error.to_string()))
    })
}

pub(crate) fn build_artifact_callback() -> BoxCallback {
    Box::new(|args| {
        let request: DurableArtifactRequest = decode("host_durable_artifact", args.first())?;
        use_durable_artifact()(request)
            .map(HostValue::String)
            .map_err(|error| EngineError::Internal(error.to_string()))
    })
}

pub(crate) fn build_cancellation_callback() -> BoxCallback {
    Box::new(|_| {
        use_durable_cancellation()()
            .map(|()| HostValue::Null)
            .map_err(|error| EngineError::Internal(error.to_string()))
    })
}
