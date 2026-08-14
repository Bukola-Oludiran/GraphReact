use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use graphreact_persistence::{EffectId, ReplayPolicy};

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
}
