//! Durable execution substrate for GraphReact.
//!
//! The crate deliberately owns storage records and recovery semantics, but
//! does not depend on `graphreact-core`. Core integrates through the stable
//! types and [`PersistenceEngine`] trait exported here.

mod engine;
mod error;
mod ids;
mod local;
mod records;

pub use engine::{BoxFuture, CommitReceipt, LeaseGrant, PersistenceEngine, PersistenceStatus};
pub use error::{PersistenceError, Result};
pub use ids::{
    ArtifactId, AttemptId, CorrelationId, DeploymentId, EffectId, MessageId, RunId, TimerId,
};
pub use local::{LocalConfig, LocalPersistence};
pub use records::{
    canonical_json_bytes, ActivationCommit, ArtifactMetadata, DeploymentBundle, DeploymentDigest,
    DurableMessage, DurableStateWrite, EffectIntent, EffectOutcome, ReplayPolicy, RunStatus,
    RuntimeRecordV1, RuntimeSnapshotV1, TimerIntent, FORMAT_VERSION, MAX_ARTIFACT_BYTES,
    MAX_RECORD_BYTES, SEGMENT_TARGET_BYTES, SNAPSHOT_ACTIVATION_INTERVAL,
};
