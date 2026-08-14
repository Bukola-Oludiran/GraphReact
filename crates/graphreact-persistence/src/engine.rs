use std::time::Duration;

use crate::{
    ArtifactId, ArtifactMetadata, DeploymentBundle, DeploymentDigest, DeploymentId,
    RuntimeRecordV1, RuntimeSnapshotV1,
};

pub type BoxFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<T>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReceipt {
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub fencing_token: u64,
    pub durable_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseGrant {
    pub deployment_id: DeploymentId,
    pub replica_id: String,
    pub fencing_token: u64,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistenceStatus {
    pub deployment_id: DeploymentId,
    pub deployment_digest: DeploymentDigest,
    pub fencing_token: u64,
    pub head_sequence: Option<u64>,
    pub snapshot_sequence: Option<u64>,
    pub durable_bytes: u64,
    pub artifact_count: u64,
    pub lease_owner: Option<String>,
}

pub trait PersistenceEngine: std::fmt::Debug + Send + Sync {
    fn deployment_id(&self) -> &DeploymentId;
    fn deployment_digest(&self) -> DeploymentDigest;

    fn commit<'a>(
        &'a self,
        fencing_token: u64,
        records: &'a [RuntimeRecordV1],
    ) -> BoxFuture<'a, CommitReceipt>;

    fn replay(&self, from_sequence: u64) -> BoxFuture<'_, Vec<RuntimeRecordV1>>;

    fn write_snapshot<'a>(
        &'a self,
        fencing_token: u64,
        snapshot: &'a RuntimeSnapshotV1,
    ) -> BoxFuture<'a, [u8; 32]>;

    fn read_snapshot(&self) -> BoxFuture<'_, Option<RuntimeSnapshotV1>>;

    fn put_bundle<'a>(&'a self, bundle: &'a DeploymentBundle) -> BoxFuture<'a, ()>;

    fn read_bundle(&self) -> BoxFuture<'_, Option<DeploymentBundle>>;

    fn put_artifact<'a>(
        &'a self,
        metadata: &'a ArtifactMetadata,
        bytes: &'a [u8],
    ) -> BoxFuture<'a, ()>;

    fn read_artifact<'a>(&'a self, id: &'a ArtifactId) -> BoxFuture<'a, Option<Vec<u8>>>;

    fn acquire_lease<'a>(
        &'a self,
        replica_id: &'a str,
        duration: Duration,
    ) -> BoxFuture<'a, LeaseGrant>;

    fn renew_lease<'a>(
        &'a self,
        grant: &'a LeaseGrant,
        duration: Duration,
    ) -> BoxFuture<'a, LeaseGrant>;

    fn release_lease<'a>(&'a self, grant: &'a LeaseGrant) -> BoxFuture<'a, ()>;

    fn status(&self) -> BoxFuture<'_, PersistenceStatus>;

    fn verify(&self) -> BoxFuture<'_, PersistenceStatus>;
}
