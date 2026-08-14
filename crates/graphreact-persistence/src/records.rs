use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use crate::{
    ArtifactId, AttemptId, CorrelationId, DeploymentId, EffectId, MessageId, RunId, TimerId,
};

pub const FORMAT_VERSION: u16 = 1;
pub const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
pub const SEGMENT_TARGET_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
pub const SNAPSHOT_ACTIVATION_INTERVAL: u64 = 10_000;

mod json_value {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &serde_json::Value, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes = serde_json::to_vec(value).map_err(serde::ser::Error::custom)?;
        serializer.serialize_bytes(&bytes)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<serde_json::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        serde_json::from_slice(&bytes).map_err(serde::de::Error::custom)
    }
}

mod json_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[allow(clippy::ref_option)]
    pub fn serialize<S>(value: &Option<serde_json::Value>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = value
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(serde::ser::Error::custom)?;
        encoded.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = Option::<Vec<u8>>::deserialize(deserializer)?;
        encoded
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeploymentDigest(pub [u8; 32]);

impl DeploymentDigest {
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    #[must_use]
    pub fn hex(&self) -> String {
        self.0
            .iter()
            .fold(String::with_capacity(64), |mut output, byte| {
                let _ = write!(output, "{byte:02x}");
                output
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentBundle {
    pub deployment_id: DeploymentId,
    pub digest: DeploymentDigest,
    #[serde(with = "json_value")]
    pub manifest: serde_json::Value,
    pub payload: Vec<u8>,
}

impl DeploymentBundle {
    #[must_use]
    pub fn new(deployment_id: DeploymentId, manifest: serde_json::Value, payload: Vec<u8>) -> Self {
        let mut canonical = canonical_json_bytes(&manifest);
        canonical.extend_from_slice(&payload);
        Self {
            deployment_id,
            digest: DeploymentDigest::of(&canonical),
            manifest,
            payload,
        }
    }

    #[must_use]
    pub fn verify_digest(&self) -> bool {
        let mut canonical = canonical_json_bytes(&self.manifest);
        canonical.extend_from_slice(&self.payload);
        self.digest == DeploymentDigest::of(&canonical)
    }
}

/// Deterministic JSON encoding with recursively sorted object keys.
#[must_use]
pub fn canonical_json_bytes(value: &serde_json::Value) -> Vec<u8> {
    fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(object) => {
                let sorted: std::collections::BTreeMap<_, _> = object
                    .iter()
                    .map(|(key, value)| (key.clone(), canonicalize(value)))
                    .collect();
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.iter().map(canonicalize).collect())
            }
            other => other.clone(),
        }
    }
    serde_json::to_vec(&canonicalize(value)).unwrap_or_default()
}

#[cfg(test)]
mod bundle_tests {
    use super::*;

    #[test]
    fn bundle_digest_is_independent_of_manifest_key_order() {
        let deployment_id = DeploymentId::new();
        let left = DeploymentBundle::new(
            deployment_id.clone(),
            serde_json::json!({"b": 2, "nested": {"z": 1, "a": 2}, "a": 1}),
            b"payload".to_vec(),
        );
        let right = DeploymentBundle::new(
            deployment_id,
            serde_json::json!({"a": 1, "nested": {"a": 2, "z": 1}, "b": 2}),
            b"payload".to_vec(),
        );
        assert_eq!(left.digest, right.digest);
        assert!(left.verify_digest());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    WaitingEffect,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    Indeterminate,
}

impl RunStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Indeterminate
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPolicy {
    Idempotent,
    NonReplayable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurableMessage {
    pub id: MessageId,
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub correlation_id: CorrelationId,
    pub from: String,
    pub to: String,
    pub message_type: String,
    #[serde(with = "json_value")]
    pub payload: serde_json::Value,
    pub ttl: u32,
    pub created_at_ms: u64,
    pub direct: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurableStateWrite {
    pub node_id: String,
    pub key: String,
    #[serde(with = "json_value")]
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimerIntent {
    pub id: TimerId,
    pub key: String,
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub node_id: String,
    pub deadline_unix_ms: u64,
    pub message_type: String,
    #[serde(with = "json_value")]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectIntent {
    pub id: EffectId,
    pub key: String,
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub node_id: String,
    pub adapter: String,
    pub operation: String,
    #[serde(with = "json_value")]
    pub input: serde_json::Value,
    pub result_type: String,
    pub error_type: String,
    pub replay_policy: ReplayPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EffectOutcome {
    Succeeded(#[serde(with = "json_value")] serde_json::Value),
    Failed { code: String, message: String },
    Indeterminate,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    pub id: ArtifactId,
    pub run_id: RunId,
    pub logical_name: String,
    pub media_type: String,
    pub size: u64,
    pub sha256: [u8; 32],
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivationCommit {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub input_message_id: MessageId,
    pub state_writes: Vec<DurableStateWrite>,
    pub emitted_messages: Vec<DurableMessage>,
    pub timers_scheduled: Vec<TimerIntent>,
    pub timers_cancelled: Vec<TimerId>,
    pub effect_intents: Vec<EffectIntent>,
    pub artifacts: Vec<ArtifactMetadata>,
    pub resulting_status: RunStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RuntimeRecordV1 {
    RunCreated {
        run_id: RunId,
        entrypoint: String,
        request_hash: [u8; 32],
        idempotency_key: String,
        created_at_ms: u64,
    },
    AttemptStarted {
        run_id: RunId,
        attempt_id: AttemptId,
        number: u32,
        started_at_ms: u64,
    },
    MessageEnqueued(DurableMessage),
    ActivationCommitted(ActivationCommit),
    EffectDispatched {
        effect_id: EffectId,
        dispatched_at_ms: u64,
    },
    EffectCompleted {
        effect_id: EffectId,
        outcome: EffectOutcome,
        /// Result/error delivery persisted in the same record. The scheduler
        /// makes it visible only after this record is durable.
        result_message: Option<DurableMessage>,
        completed_at_ms: u64,
    },
    TimerFired {
        timer_id: TimerId,
        result_message: DurableMessage,
        fired_at_ms: u64,
    },
    TimerCancelled {
        timer_id: TimerId,
        cancelled_at_ms: u64,
    },
    CancellationRequested {
        run_id: RunId,
        requested_at_ms: u64,
    },
    ArtifactCommitted(ArtifactMetadata),
    RunStatusChanged {
        run_id: RunId,
        status: RunStatus,
        #[serde(with = "json_option")]
        result: Option<serde_json::Value>,
        error: Option<String>,
        changed_at_ms: u64,
    },
    SnapshotInstalled {
        sequence: u64,
        digest: [u8; 32],
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSnapshotV1 {
    pub deployment_id: DeploymentId,
    pub deployment_digest: DeploymentDigest,
    pub through_sequence: u64,
    #[serde(with = "json_value")]
    pub graph: serde_json::Value,
    #[serde(with = "json_value")]
    pub manifests: serde_json::Value,
    pub durable_state: Vec<DurableStateWrite>,
    pub inbox: Vec<DurableMessage>,
    pub postponed: Vec<DurableMessage>,
    #[serde(with = "json_value")]
    pub supervision: serde_json::Value,
    pub timers: Vec<TimerIntent>,
    pub effects: Vec<EffectIntent>,
    pub artifacts: Vec<ArtifactMetadata>,
    #[serde(with = "json_value")]
    pub runs: serde_json::Value,
}
