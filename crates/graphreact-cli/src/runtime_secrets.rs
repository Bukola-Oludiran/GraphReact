//! Runtime secret access shared by the JSC, CPython, and Go hosts.
//!
//! M-Prod sub-track #3 Phase B Part 2 keeps guest-engine shims small:
//! each engine obtains the active node id + committed manifest from its
//! existing TLS, then delegates the policy verdict and store read here.
//! The store handle is rotatable so SIGHUP can replace it without
//! invalidating engine callbacks or guest-side references.

use std::sync::Arc;

use graphreact_observer::{AuditRecordInput, AuditSink};
use graphreact_secrets::{EnvStore, KeyringStore, SecretStore, SecretsError};

/// Runtime backend selected through `GRX_RUNTIME_SECRET_BACKEND`.
///
/// The daemon defaults to `env`: container and Kubernetes deployments
/// can inject `GRX_SECRET_<NAME>` without requiring a desktop keyring
/// agent. Developer workstations may opt into `keyring`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSecretBackend {
    Env,
    Keyring,
}

impl RuntimeSecretBackend {
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var("GRX_RUNTIME_SECRET_BACKEND") {
            Ok(value) if value.eq_ignore_ascii_case("keyring") => Self::Keyring,
            Ok(value) if value.eq_ignore_ascii_case("env") => Self::Env,
            Ok(value) => {
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "secret_store_backend_invalid",
                    backend = %value,
                    fallback = "env",
                    "invalid runtime secret backend; falling back to env"
                );
                Self::Env
            }
            Err(_) => Self::Env,
        }
    }

    fn build_store(self) -> Arc<dyn SecretStore> {
        match self {
            Self::Env => Arc::new(EnvStore),
            Self::Keyring => Arc::new(KeyringStore),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Keyring => "keyring",
        }
    }
}

/// Process-owned secret source plus the late-bound audit sink.
///
/// `ArcSwap` provides the same lock-free snapshot/rotation shape used
/// by TLS, tokens, and JWKS. Guest callbacks keep this outer Arc for
/// the daemon's lifetime and observe the replacement on their next call.
pub struct RuntimeSecretContext {
    // The outer Arc is ArcSwap's sized payload; the inner Arc is the
    // object-safe store handle. This mirrors ArcSwap<Arc<dyn Trait>>.
    store: arc_swap::ArcSwap<Arc<dyn SecretStore>>,
    backend: parking_lot::RwLock<RuntimeSecretBackend>,
    audit_sink: parking_lot::RwLock<Option<Arc<dyn AuditSink>>>,
    metrics: parking_lot::RwLock<Option<Arc<graphreact_observer::MetricsInner>>>,
}

impl std::fmt::Debug for RuntimeSecretContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeSecretContext")
            .field("backend", &*self.backend.read())
            .field("audit_sink", &self.audit_sink.read().is_some())
            .field("metrics", &self.metrics.read().is_some())
            .finish_non_exhaustive()
    }
}

impl RuntimeSecretContext {
    #[must_use]
    pub fn from_env() -> Arc<Self> {
        let backend = RuntimeSecretBackend::from_env();
        Arc::new(Self {
            store: arc_swap::ArcSwap::new(Arc::new(backend.build_store())),
            backend: parking_lot::RwLock::new(backend),
            audit_sink: parking_lot::RwLock::new(None),
            metrics: parking_lot::RwLock::new(None),
        })
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_store(store: Arc<dyn SecretStore>) -> Arc<Self> {
        Arc::new(Self {
            store: arc_swap::ArcSwap::new(Arc::new(store)),
            backend: parking_lot::RwLock::new(RuntimeSecretBackend::Env),
            audit_sink: parking_lot::RwLock::new(None),
            metrics: parking_lot::RwLock::new(None),
        })
    }

    /// Attach the observer's hash-chain sink after observer startup.
    /// Engine callbacks may already hold this context; no re-register
    /// operation is necessary.
    pub fn set_audit_sink(&self, sink: Option<Arc<dyn AuditSink>>) {
        *self.audit_sink.write() = sink;
    }

    pub fn set_metrics(&self, metrics: Option<Arc<graphreact_observer::MetricsInner>>) {
        *self.metrics.write() = metrics;
    }

    /// Re-resolve the configured backend and atomically replace the
    /// store handle. Existing callbacks see the replacement on their
    /// next invocation.
    pub fn reload_from_env(&self) {
        let backend = RuntimeSecretBackend::from_env();
        self.store.store(Arc::new(backend.build_store()));
        *self.backend.write() = backend;
        tracing::info!(
            target: "graphreact::observer::audit",
            event = "secret_store_reloaded",
            backend = backend.as_str(),
            "runtime secret store reloaded"
        );
        self.fan(AuditRecordInput {
            kind: "secret_store_reloaded",
            fields: serde_json::json!({ "backend": backend.as_str() }),
        });
    }

    pub fn log_reload_skipped(&self) {
        tracing::warn!(
            target: "graphreact::observer::audit",
            event = "secret_store_reload_skipped",
            reason = "draining",
            "runtime secret-store reload skipped"
        );
        self.fan(AuditRecordInput {
            kind: "secret_store_reload_skipped",
            fields: serde_json::json!({ "reason": "draining" }),
        });
    }

    /// Enforce the committed manifest allowlist and return a UTF-8
    /// value for guest runtimes. The value is never logged.
    #[must_use]
    pub fn read_utf8(&self, node_id: &str, name: &str, allowed: &[String]) -> Option<String> {
        if !allowed.iter().any(|candidate| candidate == name) {
            self.log_deny(node_id, name, "undeclared");
            return None;
        }

        let value = match self.store.load().get(name) {
            Ok(value) => value,
            Err(SecretsError::NotFound { .. }) => {
                self.log_deny(node_id, name, "missing");
                return None;
            }
            Err(error) => {
                self.count_failure("backend_error");
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "secret_read_deny",
                    node_id,
                    name,
                    reason = "backend_error",
                    error = %error,
                    "runtime secret read denied"
                );
                self.fan(AuditRecordInput {
                    kind: "secret_read_deny",
                    fields: serde_json::json!({
                        "node_id": node_id,
                        "name": name,
                        "reason": "backend_error",
                    }),
                });
                return None;
            }
        };

        let decoded = match std::str::from_utf8(value.expose_bytes()) {
            Ok(value) => value.to_owned(),
            Err(_) => {
                self.log_deny(node_id, name, "non_utf8");
                return None;
            }
        };

        tracing::info!(
            target: "graphreact::observer::audit",
            event = "secret_read_allow",
            node_id,
            name,
            "runtime secret read allowed"
        );
        self.fan(AuditRecordInput {
            kind: "secret_read_allow",
            fields: serde_json::json!({ "node_id": node_id, "name": name }),
        });
        Some(decoded)
    }

    fn log_deny(&self, node_id: &str, name: &str, reason: &'static str) {
        self.count_failure(reason);
        tracing::warn!(
            target: "graphreact::observer::audit",
            event = "secret_read_deny",
            node_id,
            name,
            reason,
            "runtime secret read denied"
        );
        self.fan(AuditRecordInput {
            kind: "secret_read_deny",
            fields: serde_json::json!({
                "node_id": node_id,
                "name": name,
                "reason": reason,
            }),
        });
    }

    fn fan(&self, input: AuditRecordInput) {
        if let Some(sink) = self.audit_sink.read().as_ref() {
            sink.append(input);
        }
    }

    fn count_failure(&self, reason: &'static str) {
        if let Some(metrics) = self.metrics.read().as_ref() {
            metrics.secret_read_failures_inc(reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Debug)]
    struct MemoryStore(HashMap<String, Vec<u8>>);

    impl SecretStore for MemoryStore {
        fn set(
            &self,
            _name: &str,
            _value: graphreact_secrets::SecretValue,
        ) -> Result<(), SecretsError> {
            unreachable!("read-only test store")
        }

        fn get(&self, name: &str) -> Result<graphreact_secrets::SecretValue, SecretsError> {
            self.0
                .get(name)
                .cloned()
                .ok_or_else(|| SecretsError::NotFound { name: name.into() })
                .and_then(graphreact_secrets::SecretValue::from_bytes)
        }

        fn list(&self) -> Result<Vec<String>, SecretsError> {
            Ok(self.0.keys().cloned().collect())
        }

        fn remove(&self, _name: &str) -> Result<(), SecretsError> {
            unreachable!("read-only test store")
        }
    }

    #[derive(Debug, Default)]
    struct CaptureAudit(Mutex<Vec<AuditRecordInput>>);

    impl AuditSink for CaptureAudit {
        fn append(&self, record: AuditRecordInput) {
            self.0.lock().expect("audit lock").push(record);
        }
    }

    #[test]
    fn declared_reads_allow_and_undeclared_reads_deny_without_values_in_audit() {
        let store = MemoryStore(HashMap::from([
            ("TOKEN".into(), b"top-secret".to_vec()),
            ("BINARY".into(), vec![0xff]),
        ]));
        let context = RuntimeSecretContext::with_store(Arc::new(store));
        let audit = Arc::new(CaptureAudit::default());
        let metrics = Arc::new(graphreact_observer::MetricsInner::new());
        context.set_audit_sink(Some(audit.clone()));
        context.set_metrics(Some(metrics.clone()));

        assert_eq!(
            context.read_utf8("worker", "TOKEN", &["TOKEN".into()]),
            Some("top-secret".into())
        );
        assert_eq!(context.read_utf8("worker", "TOKEN", &[]), None);
        assert_eq!(
            context.read_utf8("worker", "MISSING", &["MISSING".into()]),
            None
        );
        assert_eq!(
            context.read_utf8("worker", "BINARY", &["BINARY".into()]),
            None
        );

        let records = audit.0.lock().expect("audit lock");
        assert_eq!(
            records.iter().map(|record| record.kind).collect::<Vec<_>>(),
            vec![
                "secret_read_allow",
                "secret_read_deny",
                "secret_read_deny",
                "secret_read_deny"
            ]
        );
        let serialized = serde_json::to_string(
            &records.iter().map(|record| &record.fields).collect::<Vec<_>>(),
        )
        .expect("serialize audit fields");
        assert!(!serialized.contains("top-secret"));
        assert!(serialized.contains("undeclared"));
        assert!(serialized.contains("missing"));
        assert!(serialized.contains("non_utf8"));
        assert_eq!(
            metrics
                .secret_read_failures
                .iter()
                .map(|entry| entry.counter.get())
                .sum::<u64>(),
            3
        );
        let prometheus = graphreact_observer::encode_text(&metrics);
        assert!(prometheus.contains("grx_secret_read_failures_total"));
        assert!(prometheus.contains("reason=\"undeclared\""));
        assert!(prometheus.contains("reason=\"missing\""));
        assert!(prometheus.contains("reason=\"non_utf8\""));
    }
}
