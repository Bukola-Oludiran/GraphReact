use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions};
use postgres_native_tls::MakeTlsConnector;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio_postgres::Client;

use crate::{
    ArtifactId, ArtifactMetadata, BoxFuture, CommitReceipt, DeploymentBundle, DeploymentDigest,
    DeploymentId, LeaseGrant, PersistenceEngine, PersistenceError, PersistenceStatus,
    RuntimeRecordV1, RuntimeSnapshotV1, MAX_ARTIFACT_BYTES, MAX_RECORD_BYTES,
};

const CLUSTER_BLOCK_VERSION: u16 = 1;

#[derive(Clone)]
pub struct ClusterConfig {
    pub deployment_id: DeploymentId,
    pub deployment_digest: DeploymentDigest,
    pub database_url: String,
    pub bucket: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub prefix: Option<String>,
    pub artifact_limit_bytes: u64,
}

impl std::fmt::Debug for ClusterConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClusterConfig")
            .field("deployment_id", &self.deployment_id)
            .field("deployment_digest", &self.deployment_digest)
            .field("database_url", &"[REDACTED]")
            .field("bucket", &self.bucket)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("prefix", &self.prefix)
            .field("artifact_limit_bytes", &self.artifact_limit_bytes)
            .finish()
    }
}

impl ClusterConfig {
    #[must_use]
    pub fn new(
        deployment_id: DeploymentId,
        deployment_digest: DeploymentDigest,
        database_url: impl Into<String>,
        bucket: impl Into<String>,
    ) -> Self {
        Self {
            deployment_id,
            deployment_digest,
            database_url: database_url.into(),
            bucket: bucket.into(),
            endpoint: None,
            region: None,
            prefix: None,
            artifact_limit_bytes: MAX_ARTIFACT_BYTES,
        }
    }
}

#[derive(Debug, Clone)]
struct RuntimeBlockRef {
    first_sequence: u64,
    last_sequence: u64,
    object_key: String,
    sha256: [u8; 32],
    byte_len: u64,
    fencing_token: u64,
}

#[derive(Debug, Clone)]
struct ClusterHead {
    fencing_token: u64,
    head_sequence: Option<u64>,
    snapshot_sequence: Option<u64>,
    snapshot_object: Option<String>,
    durable_bytes: u64,
    artifact_count: u64,
    lease_owner: Option<String>,
}

trait ClusterControlPlane: std::fmt::Debug + Send + Sync {
    fn head(&self) -> BoxFuture<'_, ClusterHead>;
    fn advance_head<'a>(
        &'a self,
        expected_head: Option<u64>,
        block: &'a RuntimeBlockRef,
    ) -> BoxFuture<'a, ()>;
    fn blocks(&self, from_sequence: u64) -> BoxFuture<'_, Vec<RuntimeBlockRef>>;
    fn advance_snapshot<'a>(
        &'a self,
        fencing_token: u64,
        through_sequence: u64,
        object_key: &'a str,
    ) -> BoxFuture<'a, ()>;
    fn put_artifact_metadata<'a>(
        &'a self,
        metadata: &'a ArtifactMetadata,
        object_key: &'a str,
    ) -> BoxFuture<'a, ()>;
    fn artifact_metadata<'a>(
        &'a self,
        artifact_id: &'a ArtifactId,
    ) -> BoxFuture<'a, Option<(ArtifactMetadata, String)>>;
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
}

#[derive(Debug)]
struct PostgresControlPlane {
    deployment_id: DeploymentId,
    deployment_digest: DeploymentDigest,
    client: Mutex<Client>,
}

impl PostgresControlPlane {
    async fn connect(config: &ClusterConfig) -> crate::Result<Arc<Self>> {
        let connector = native_tls::TlsConnector::builder()
            .build()
            .map_err(|error| PersistenceError::Database(error.to_string()))?;
        let (client, connection) =
            tokio_postgres::connect(&config.database_url, MakeTlsConnector::new(connector))
                .await
                .map_err(database_error)?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::error!(
                    target: "graphreact::persistence",
                    %error,
                    "cluster Postgres connection terminated"
                );
            }
        });
        let control = Arc::new(Self {
            deployment_id: config.deployment_id.clone(),
            deployment_digest: config.deployment_digest,
            client: Mutex::new(client),
        });
        control.initialize().await?;
        Ok(control)
    }

    async fn initialize(&self) -> crate::Result<()> {
        let client = self.client.lock().await;
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS grx_deployments (
                    deployment_id TEXT PRIMARY KEY,
                    deployment_digest BYTEA NOT NULL,
                    fencing_token BIGINT NOT NULL DEFAULT 0,
                    lease_owner TEXT,
                    lease_expires_at_ms BIGINT,
                    head_sequence BIGINT,
                    snapshot_sequence BIGINT,
                    snapshot_object TEXT,
                    durable_bytes BIGINT NOT NULL DEFAULT 0,
                    artifact_count BIGINT NOT NULL DEFAULT 0
                );
                CREATE TABLE IF NOT EXISTS grx_runtime_blocks (
                    deployment_id TEXT NOT NULL,
                    first_sequence BIGINT NOT NULL,
                    last_sequence BIGINT NOT NULL,
                    object_key TEXT NOT NULL,
                    sha256 BYTEA NOT NULL,
                    byte_len BIGINT NOT NULL,
                    fencing_token BIGINT NOT NULL,
                    PRIMARY KEY (deployment_id, first_sequence)
                );
                CREATE TABLE IF NOT EXISTS grx_artifacts (
                    deployment_id TEXT NOT NULL,
                    artifact_id TEXT NOT NULL,
                    metadata BYTEA NOT NULL,
                    object_key TEXT NOT NULL,
                    PRIMARY KEY (deployment_id, artifact_id)
                );",
            )
            .await
            .map_err(database_error)?;
        let inserted = client
            .execute(
                "INSERT INTO grx_deployments (deployment_id, deployment_digest)
                 VALUES ($1, $2) ON CONFLICT (deployment_id) DO NOTHING",
                &[&self.deployment_id.as_str(), &&self.deployment_digest.0[..]],
            )
            .await
            .map_err(database_error)?;
        if inserted == 0 {
            let row = client
                .query_one(
                    "SELECT deployment_digest FROM grx_deployments WHERE deployment_id = $1",
                    &[&self.deployment_id.as_str()],
                )
                .await
                .map_err(database_error)?;
            let digest: Vec<u8> = row.get(0);
            if digest.as_slice() != self.deployment_digest.0 {
                return Err(PersistenceError::DeploymentDigestMismatch);
            }
        }
        Ok(())
    }

    async fn head_inner(&self) -> crate::Result<ClusterHead> {
        let client = self.client.lock().await;
        let row = client
            .query_one(
                "SELECT fencing_token, head_sequence, snapshot_sequence, snapshot_object,
                        durable_bytes, artifact_count, lease_owner
                 FROM grx_deployments WHERE deployment_id = $1",
                &[&self.deployment_id.as_str()],
            )
            .await
            .map_err(database_error)?;
        Ok(ClusterHead {
            fencing_token: from_i64(row.get(0), "fencing_token")?,
            head_sequence: optional_from_i64(row.get(1), "head_sequence")?,
            snapshot_sequence: optional_from_i64(row.get(2), "snapshot_sequence")?,
            snapshot_object: row.get(3),
            durable_bytes: from_i64(row.get(4), "durable_bytes")?,
            artifact_count: from_i64(row.get(5), "artifact_count")?,
            lease_owner: row.get(6),
        })
    }
}

impl ClusterControlPlane for PostgresControlPlane {
    fn head(&self) -> BoxFuture<'_, ClusterHead> {
        Box::pin(self.head_inner())
    }

    fn advance_head<'a>(
        &'a self,
        expected_head: Option<u64>,
        block: &'a RuntimeBlockRef,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut client = self.client.lock().await;
            let transaction = client.transaction().await.map_err(database_error)?;
            let now = to_i64(unix_ms(), "now")?;
            let changed = transaction
                .execute(
                    "UPDATE grx_deployments
                     SET head_sequence = $1, durable_bytes = durable_bytes + $2
                     WHERE deployment_id = $3 AND fencing_token = $4
                       AND head_sequence IS NOT DISTINCT FROM $5
                       AND lease_owner IS NOT NULL AND lease_expires_at_ms >= $6",
                    &[
                        &to_i64(block.last_sequence, "last_sequence")?,
                        &to_i64(block.byte_len, "byte_len")?,
                        &self.deployment_id.as_str(),
                        &to_i64(block.fencing_token, "fencing_token")?,
                        &optional_to_i64(expected_head, "head_sequence")?,
                        &now,
                    ],
                )
                .await
                .map_err(database_error)?;
            if changed != 1 {
                transaction.rollback().await.map_err(database_error)?;
                let row = client
                    .query_one(
                        "SELECT fencing_token FROM grx_deployments WHERE deployment_id = $1",
                        &[&self.deployment_id.as_str()],
                    )
                    .await
                    .map_err(database_error)?;
                return Err(PersistenceError::StaleFence {
                    expected: from_i64(row.get(0), "fencing_token")?,
                    found: block.fencing_token,
                });
            }
            transaction
                .execute(
                    "INSERT INTO grx_runtime_blocks
                     (deployment_id, first_sequence, last_sequence, object_key, sha256,
                      byte_len, fencing_token)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    &[
                        &self.deployment_id.as_str(),
                        &to_i64(block.first_sequence, "first_sequence")?,
                        &to_i64(block.last_sequence, "last_sequence")?,
                        &block.object_key,
                        &&block.sha256[..],
                        &to_i64(block.byte_len, "byte_len")?,
                        &to_i64(block.fencing_token, "fencing_token")?,
                    ],
                )
                .await
                .map_err(database_error)?;
            transaction.commit().await.map_err(database_error)
        })
    }

    fn blocks(&self, from_sequence: u64) -> BoxFuture<'_, Vec<RuntimeBlockRef>> {
        Box::pin(async move {
            let client = self.client.lock().await;
            let rows = client
                .query(
                    "SELECT first_sequence, last_sequence, object_key, sha256, byte_len,
                            fencing_token
                     FROM grx_runtime_blocks
                     WHERE deployment_id = $1 AND last_sequence >= $2
                     ORDER BY first_sequence",
                    &[
                        &self.deployment_id.as_str(),
                        &to_i64(from_sequence, "from_sequence")?,
                    ],
                )
                .await
                .map_err(database_error)?;
            rows.into_iter()
                .map(|row| {
                    Ok(RuntimeBlockRef {
                        first_sequence: from_i64(row.get(0), "first_sequence")?,
                        last_sequence: from_i64(row.get(1), "last_sequence")?,
                        object_key: row.get(2),
                        sha256: digest_from_bytes(row.get::<_, Vec<u8>>(3))?,
                        byte_len: from_i64(row.get(4), "byte_len")?,
                        fencing_token: from_i64(row.get(5), "fencing_token")?,
                    })
                })
                .collect()
        })
    }

    fn advance_snapshot<'a>(
        &'a self,
        fencing_token: u64,
        through_sequence: u64,
        object_key: &'a str,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let client = self.client.lock().await;
            let now = to_i64(unix_ms(), "now")?;
            let changed = client
                .execute(
                    "UPDATE grx_deployments SET snapshot_sequence = $1, snapshot_object = $2
                     WHERE deployment_id = $3 AND fencing_token = $4
                       AND (head_sequence >= $1 OR (head_sequence IS NULL AND $1 = 0))
                       AND lease_owner IS NOT NULL AND lease_expires_at_ms >= $5",
                    &[
                        &to_i64(through_sequence, "snapshot_sequence")?,
                        &object_key,
                        &self.deployment_id.as_str(),
                        &to_i64(fencing_token, "fencing_token")?,
                        &now,
                    ],
                )
                .await
                .map_err(database_error)?;
            if changed == 1 {
                Ok(())
            } else {
                let row = client
                    .query_one(
                        "SELECT fencing_token FROM grx_deployments WHERE deployment_id = $1",
                        &[&self.deployment_id.as_str()],
                    )
                    .await
                    .map_err(database_error)?;
                Err(PersistenceError::StaleFence {
                    expected: from_i64(row.get(0), "fencing_token")?,
                    found: fencing_token,
                })
            }
        })
    }

    fn put_artifact_metadata<'a>(
        &'a self,
        metadata: &'a ArtifactMetadata,
        object_key: &'a str,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let encoded = postcard::to_stdvec(metadata)
                .map_err(|error| PersistenceError::Codec(error.to_string()))?;
            let mut client = self.client.lock().await;
            let transaction = client.transaction().await.map_err(database_error)?;
            let inserted = transaction
                .execute(
                    "INSERT INTO grx_artifacts (deployment_id, artifact_id, metadata, object_key)
                     VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
                    &[
                        &self.deployment_id.as_str(),
                        &metadata.id.as_str(),
                        &encoded,
                        &object_key,
                    ],
                )
                .await
                .map_err(database_error)?;
            if inserted == 1 {
                transaction
                    .execute(
                        "UPDATE grx_deployments SET artifact_count = artifact_count + 1
                         WHERE deployment_id = $1",
                        &[&self.deployment_id.as_str()],
                    )
                    .await
                    .map_err(database_error)?;
            }
            transaction.commit().await.map_err(database_error)
        })
    }

    fn artifact_metadata<'a>(
        &'a self,
        artifact_id: &'a ArtifactId,
    ) -> BoxFuture<'a, Option<(ArtifactMetadata, String)>> {
        Box::pin(async move {
            let client = self.client.lock().await;
            let row = client
                .query_opt(
                    "SELECT metadata, object_key FROM grx_artifacts
                     WHERE deployment_id = $1 AND artifact_id = $2",
                    &[&self.deployment_id.as_str(), &artifact_id.as_str()],
                )
                .await
                .map_err(database_error)?;
            row.map(|row| {
                let encoded: Vec<u8> = row.get(0);
                let metadata = postcard::from_bytes(&encoded)
                    .map_err(|error| PersistenceError::Codec(error.to_string()))?;
                Ok((metadata, row.get(1)))
            })
            .transpose()
        })
    }

    fn acquire_lease<'a>(
        &'a self,
        replica_id: &'a str,
        duration: Duration,
    ) -> BoxFuture<'a, LeaseGrant> {
        Box::pin(async move {
            let now = unix_ms();
            let expires = now.saturating_add(duration_ms(duration));
            let now_i64 = to_i64(now, "now")?;
            let mut client = self.client.lock().await;
            let transaction = client.transaction().await.map_err(database_error)?;
            let row = transaction
                .query_one(
                    "SELECT fencing_token, lease_owner, lease_expires_at_ms
                     FROM grx_deployments WHERE deployment_id = $1 FOR UPDATE",
                    &[&self.deployment_id.as_str()],
                )
                .await
                .map_err(database_error)?;
            let owner: Option<String> = row.get(1);
            let lease_expires: Option<i64> = row.get(2);
            if owner.as_deref().is_some_and(|owner| owner != replica_id)
                && lease_expires.is_some_and(|expiry| expiry > now_i64)
            {
                transaction.rollback().await.map_err(database_error)?;
                return Err(PersistenceError::LeaseNotOwned(replica_id.to_owned()));
            }
            let fencing_token = from_i64(row.get::<_, i64>(0), "fencing_token")?
                .checked_add(1)
                .ok_or_else(|| PersistenceError::Codec("fencing token overflow".into()))?;
            transaction
                .execute(
                    "UPDATE grx_deployments
                     SET fencing_token = $1, lease_owner = $2, lease_expires_at_ms = $3
                     WHERE deployment_id = $4",
                    &[
                        &to_i64(fencing_token, "fencing_token")?,
                        &replica_id,
                        &to_i64(expires, "lease_expires_at_ms")?,
                        &self.deployment_id.as_str(),
                    ],
                )
                .await
                .map_err(database_error)?;
            transaction.commit().await.map_err(database_error)?;
            Ok(LeaseGrant {
                deployment_id: self.deployment_id.clone(),
                replica_id: replica_id.to_owned(),
                fencing_token,
                expires_at_unix_ms: expires,
            })
        })
    }

    fn renew_lease<'a>(
        &'a self,
        grant: &'a LeaseGrant,
        duration: Duration,
    ) -> BoxFuture<'a, LeaseGrant> {
        Box::pin(async move {
            let now = unix_ms();
            let expires = now.saturating_add(duration_ms(duration));
            let client = self.client.lock().await;
            let changed = client
                .execute(
                    "UPDATE grx_deployments SET lease_expires_at_ms = $1
                     WHERE deployment_id = $2 AND lease_owner = $3 AND fencing_token = $4
                       AND lease_expires_at_ms >= $5",
                    &[
                        &to_i64(expires, "lease_expires_at_ms")?,
                        &self.deployment_id.as_str(),
                        &grant.replica_id,
                        &to_i64(grant.fencing_token, "fencing_token")?,
                        &to_i64(now, "now")?,
                    ],
                )
                .await
                .map_err(database_error)?;
            if changed != 1 {
                return Err(PersistenceError::LeaseNotOwned(grant.replica_id.clone()));
            }
            Ok(LeaseGrant {
                expires_at_unix_ms: expires,
                ..grant.clone()
            })
        })
    }

    fn release_lease<'a>(&'a self, grant: &'a LeaseGrant) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let client = self.client.lock().await;
            let changed = client
                .execute(
                    "UPDATE grx_deployments SET lease_owner = NULL, lease_expires_at_ms = NULL
                     WHERE deployment_id = $1 AND lease_owner = $2 AND fencing_token = $3",
                    &[
                        &self.deployment_id.as_str(),
                        &grant.replica_id,
                        &to_i64(grant.fencing_token, "fencing_token")?,
                    ],
                )
                .await
                .map_err(database_error)?;
            if changed == 1 {
                Ok(())
            } else {
                Err(PersistenceError::LeaseNotOwned(grant.replica_id.clone()))
            }
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ClusterLogBlockV1 {
    version: u16,
    deployment_digest: DeploymentDigest,
    fencing_token: u64,
    first_sequence: u64,
    last_sequence: u64,
    records: Vec<RuntimeRecordV1>,
}

pub struct ClusterPersistence {
    config: ClusterConfig,
    control: Arc<dyn ClusterControlPlane>,
    objects: Arc<dyn ObjectStore>,
}

impl std::fmt::Debug for ClusterPersistence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClusterPersistence")
            .field("deployment_id", &self.config.deployment_id)
            .field("deployment_digest", &self.config.deployment_digest)
            .field("bucket", &self.config.bucket)
            .field("endpoint", &self.config.endpoint)
            .field("prefix", &self.config.prefix)
            .finish_non_exhaustive()
    }
}

impl ClusterPersistence {
    pub async fn connect(config: ClusterConfig) -> crate::Result<Arc<Self>> {
        if config.database_url.trim().is_empty() || config.bucket.trim().is_empty() {
            return Err(PersistenceError::Configuration(
                "cluster database URL and bucket must not be empty".into(),
            ));
        }
        let control = PostgresControlPlane::connect(&config).await?;
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(&config.bucket);
        if let Some(region) = &config.region {
            builder = builder.with_region(region);
        }
        if let Some(endpoint) = &config.endpoint {
            builder = builder
                .with_endpoint(endpoint)
                .with_allow_http(endpoint.starts_with("http://"));
        }
        let objects = Arc::new(
            builder
                .build()
                .map_err(|error| PersistenceError::Configuration(error.to_string()))?,
        );
        Ok(Arc::new(Self {
            config,
            control,
            objects,
        }))
    }

    fn object_key(&self, suffix: &str) -> String {
        let deployment = self.config.deployment_id.as_str();
        match self
            .config
            .prefix
            .as_deref()
            .map(|prefix| prefix.trim_matches('/'))
        {
            Some(prefix) if !prefix.is_empty() => format!("{prefix}/{deployment}/{suffix}"),
            _ => format!("{deployment}/{suffix}"),
        }
    }

    async fn put_immutable(&self, key: &str, bytes: Vec<u8>) -> crate::Result<()> {
        let result = self
            .objects
            .put_opts(
                &ObjectPath::from(key),
                Bytes::from(bytes).into(),
                PutOptions {
                    mode: PutMode::Create,
                    ..PutOptions::default()
                },
            )
            .await;
        match result {
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Ok(()),
            Err(error) => Err(object_error(error)),
        }
    }

    async fn get_object(&self, key: &str) -> crate::Result<Option<Vec<u8>>> {
        match self.objects.get(&ObjectPath::from(key)).await {
            Ok(result) => result
                .bytes()
                .await
                .map(|bytes| Some(bytes.to_vec()))
                .map_err(object_error),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(object_error(error)),
        }
    }
}

impl PersistenceEngine for ClusterPersistence {
    fn deployment_id(&self) -> &DeploymentId {
        &self.config.deployment_id
    }

    fn deployment_digest(&self) -> DeploymentDigest {
        self.config.deployment_digest
    }

    fn commit<'a>(
        &'a self,
        fencing_token: u64,
        records: &'a [RuntimeRecordV1],
    ) -> BoxFuture<'a, CommitReceipt> {
        Box::pin(async move {
            if records.is_empty() {
                return Err(PersistenceError::Configuration(
                    "cluster commits must contain at least one record".into(),
                ));
            }
            for record in records {
                let size = postcard::to_stdvec(record)
                    .map_err(|error| PersistenceError::Codec(error.to_string()))?
                    .len();
                if size > MAX_RECORD_BYTES {
                    return Err(PersistenceError::RecordTooLarge {
                        actual: size,
                        cap: MAX_RECORD_BYTES,
                    });
                }
            }
            let head = self.control.head().await?;
            if fencing_token != head.fencing_token {
                return Err(PersistenceError::StaleFence {
                    expected: head.fencing_token,
                    found: fencing_token,
                });
            }
            let first_sequence = head.head_sequence.map_or(0, |sequence| sequence + 1);
            let last_sequence = first_sequence
                .checked_add(records.len() as u64 - 1)
                .ok_or_else(|| PersistenceError::Codec("sequence overflow".into()))?;
            let block = ClusterLogBlockV1 {
                version: CLUSTER_BLOCK_VERSION,
                deployment_digest: self.config.deployment_digest,
                fencing_token,
                first_sequence,
                last_sequence,
                records: records.to_vec(),
            };
            let encoded = postcard::to_stdvec(&block)
                .map_err(|error| PersistenceError::Codec(error.to_string()))?;
            let sha256: [u8; 32] = Sha256::digest(&encoded).into();
            let object_key = self.object_key(&format!(
                "log/{first_sequence:020}-{last_sequence:020}-{}.grxblock",
                hex(&sha256)
            ));
            self.put_immutable(&object_key, encoded.clone()).await?;
            let block_ref = RuntimeBlockRef {
                first_sequence,
                last_sequence,
                object_key,
                sha256,
                byte_len: encoded.len() as u64,
                fencing_token,
            };
            self.control
                .advance_head(head.head_sequence, &block_ref)
                .await?;
            Ok(CommitReceipt {
                first_sequence,
                last_sequence,
                fencing_token,
                durable_bytes: encoded.len() as u64,
            })
        })
    }

    fn replay(&self, from_sequence: u64) -> BoxFuture<'_, Vec<RuntimeRecordV1>> {
        Box::pin(async move {
            let blocks = self.control.blocks(from_sequence).await?;
            let mut expected = blocks.first().map(|block| block.first_sequence);
            let mut records = Vec::new();
            for reference in blocks {
                if expected.is_some_and(|expected| expected != reference.first_sequence) {
                    return Err(PersistenceError::SequenceMismatch {
                        expected: expected.unwrap_or_default(),
                        found: reference.first_sequence,
                    });
                }
                let bytes = self
                    .get_object(&reference.object_key)
                    .await?
                    .ok_or_else(|| PersistenceError::ObjectStore("runtime block missing".into()))?;
                if <[u8; 32]>::from(Sha256::digest(&bytes)) != reference.sha256 {
                    return Err(PersistenceError::ChecksumMismatch {
                        sequence: reference.first_sequence,
                    });
                }
                let block: ClusterLogBlockV1 = postcard::from_bytes(&bytes)
                    .map_err(|error| PersistenceError::Codec(error.to_string()))?;
                verify_block(&block, &reference, self.config.deployment_digest)?;
                for (offset, record) in block.records.into_iter().enumerate() {
                    let sequence = block.first_sequence + offset as u64;
                    if sequence >= from_sequence {
                        records.push(record);
                    }
                }
                expected = reference.last_sequence.checked_add(1);
            }
            Ok(records)
        })
    }

    fn write_snapshot<'a>(
        &'a self,
        fencing_token: u64,
        snapshot: &'a RuntimeSnapshotV1,
    ) -> BoxFuture<'a, [u8; 32]> {
        Box::pin(async move {
            if snapshot.deployment_digest != self.config.deployment_digest {
                return Err(PersistenceError::DeploymentDigestMismatch);
            }
            let bytes = postcard::to_stdvec(snapshot)
                .map_err(|error| PersistenceError::Codec(error.to_string()))?;
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            let key = self.object_key(&format!(
                "snapshots/{:020}-{}.grxsnap",
                snapshot.through_sequence,
                hex(&digest)
            ));
            self.put_immutable(&key, bytes).await?;
            self.control
                .advance_snapshot(fencing_token, snapshot.through_sequence, &key)
                .await?;
            Ok(digest)
        })
    }

    fn read_snapshot(&self) -> BoxFuture<'_, Option<RuntimeSnapshotV1>> {
        Box::pin(async move {
            let head = self.control.head().await?;
            let Some(key) = head.snapshot_object else {
                return Ok(None);
            };
            let bytes = self
                .get_object(&key)
                .await?
                .ok_or_else(|| PersistenceError::ObjectStore("snapshot object missing".into()))?;
            let snapshot = crate::decode_runtime_snapshot(&bytes)?;
            if snapshot.deployment_digest != self.config.deployment_digest
                || Some(snapshot.through_sequence) != head.snapshot_sequence
            {
                return Err(PersistenceError::DeploymentDigestMismatch);
            }
            Ok(Some(snapshot))
        })
    }

    fn put_bundle<'a>(&'a self, bundle: &'a DeploymentBundle) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if bundle.deployment_id != self.config.deployment_id
                || bundle.digest != self.config.deployment_digest
            {
                return Err(PersistenceError::DeploymentDigestMismatch);
            }
            let bytes = postcard::to_stdvec(bundle)
                .map_err(|error| PersistenceError::Codec(error.to_string()))?;
            self.put_immutable(
                &self.object_key(&format!("bundles/{}.grxbundle", bundle.digest.hex())),
                bytes,
            )
            .await
        })
    }

    fn read_bundle(&self) -> BoxFuture<'_, Option<DeploymentBundle>> {
        Box::pin(async move {
            let key = self.object_key(&format!(
                "bundles/{}.grxbundle",
                self.config.deployment_digest.hex()
            ));
            let Some(bytes) = self.get_object(&key).await? else {
                return Ok(None);
            };
            let bundle: DeploymentBundle = postcard::from_bytes(&bytes)
                .map_err(|error| PersistenceError::Codec(error.to_string()))?;
            if bundle.deployment_id != self.config.deployment_id
                || bundle.digest != self.config.deployment_digest
            {
                return Err(PersistenceError::DeploymentDigestMismatch);
            }
            Ok(Some(bundle))
        })
    }

    fn put_artifact<'a>(
        &'a self,
        metadata: &'a ArtifactMetadata,
        bytes: &'a [u8],
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if bytes.len() as u64 > self.config.artifact_limit_bytes {
                return Err(PersistenceError::ArtifactTooLarge {
                    actual: bytes.len(),
                    cap: usize::try_from(self.config.artifact_limit_bytes).unwrap_or(usize::MAX),
                });
            }
            let digest: [u8; 32] = Sha256::digest(bytes).into();
            if digest != metadata.sha256 || metadata.size != bytes.len() as u64 {
                return Err(PersistenceError::ArtifactDigestMismatch);
            }
            let key = self.object_key(&format!("artifacts/{}", hex(&digest)));
            self.put_immutable(&key, bytes.to_vec()).await?;
            self.control.put_artifact_metadata(metadata, &key).await
        })
    }

    fn read_artifact<'a>(&'a self, id: &'a ArtifactId) -> BoxFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            let Some((metadata, key)) = self.control.artifact_metadata(id).await? else {
                return Ok(None);
            };
            let bytes = self
                .get_object(&key)
                .await?
                .ok_or_else(|| PersistenceError::ObjectStore("artifact object missing".into()))?;
            if <[u8; 32]>::from(Sha256::digest(&bytes)) != metadata.sha256 {
                return Err(PersistenceError::ArtifactDigestMismatch);
            }
            Ok(Some(bytes))
        })
    }

    fn acquire_lease<'a>(
        &'a self,
        replica_id: &'a str,
        duration: Duration,
    ) -> BoxFuture<'a, LeaseGrant> {
        self.control.acquire_lease(replica_id, duration)
    }

    fn renew_lease<'a>(
        &'a self,
        grant: &'a LeaseGrant,
        duration: Duration,
    ) -> BoxFuture<'a, LeaseGrant> {
        self.control.renew_lease(grant, duration)
    }

    fn release_lease<'a>(&'a self, grant: &'a LeaseGrant) -> BoxFuture<'a, ()> {
        self.control.release_lease(grant)
    }

    fn status(&self) -> BoxFuture<'_, PersistenceStatus> {
        Box::pin(async move {
            let head = self.control.head().await?;
            Ok(PersistenceStatus {
                deployment_id: self.config.deployment_id.clone(),
                deployment_digest: self.config.deployment_digest,
                fencing_token: head.fencing_token,
                head_sequence: head.head_sequence,
                snapshot_sequence: head.snapshot_sequence,
                durable_bytes: head.durable_bytes,
                artifact_count: head.artifact_count,
                lease_owner: head.lease_owner,
            })
        })
    }

    fn verify(&self) -> BoxFuture<'_, PersistenceStatus> {
        Box::pin(async move {
            let _ = self.replay(0).await?;
            let _ = self.read_snapshot().await?;
            self.status().await
        })
    }
}

fn verify_block(
    block: &ClusterLogBlockV1,
    reference: &RuntimeBlockRef,
    deployment_digest: DeploymentDigest,
) -> crate::Result<()> {
    if block.version != CLUSTER_BLOCK_VERSION {
        return Err(PersistenceError::UnsupportedVersion {
            found: block.version,
            supported: CLUSTER_BLOCK_VERSION,
        });
    }
    if block.deployment_digest != deployment_digest {
        return Err(PersistenceError::DeploymentDigestMismatch);
    }
    if block.fencing_token != reference.fencing_token {
        return Err(PersistenceError::StaleFence {
            expected: reference.fencing_token,
            found: block.fencing_token,
        });
    }
    if block.first_sequence != reference.first_sequence
        || block.last_sequence != reference.last_sequence
        || block.records.len() as u64
            != block.last_sequence.saturating_sub(block.first_sequence) + 1
    {
        return Err(PersistenceError::SequenceMismatch {
            expected: reference.first_sequence,
            found: block.first_sequence,
        });
    }
    Ok(())
}

fn database_error(error: tokio_postgres::Error) -> PersistenceError {
    PersistenceError::Database(error.to_string())
}

fn object_error(error: object_store::Error) -> PersistenceError {
    PersistenceError::ObjectStore(error.to_string())
}

fn to_i64(value: u64, field: &'static str) -> crate::Result<i64> {
    i64::try_from(value)
        .map_err(|_| PersistenceError::Codec(format!("{field} exceeds Postgres BIGINT")))
}

fn optional_to_i64(value: Option<u64>, field: &'static str) -> crate::Result<Option<i64>> {
    value.map(|value| to_i64(value, field)).transpose()
}

fn from_i64(value: i64, field: &'static str) -> crate::Result<u64> {
    u64::try_from(value).map_err(|_| PersistenceError::Codec(format!("negative {field}")))
}

fn optional_from_i64(value: Option<i64>, field: &'static str) -> crate::Result<Option<u64>> {
    value.map(|value| from_i64(value, field)).transpose()
}

fn digest_from_bytes(bytes: Vec<u8>) -> crate::Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| PersistenceError::Codec("invalid SHA-256 length".into()))
}

fn hex(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttemptId, RunId, RunStatus};
    use object_store::memory::InMemory;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    #[derive(Debug)]
    struct MemoryControl {
        deployment_id: DeploymentId,
        state: StdMutex<MemoryControlState>,
    }

    #[derive(Debug)]
    struct MemoryControlState {
        head: ClusterHead,
        blocks: Vec<RuntimeBlockRef>,
        artifacts: HashMap<ArtifactId, (ArtifactMetadata, String)>,
        lease: Option<LeaseGrant>,
    }

    impl MemoryControl {
        fn new(deployment_id: DeploymentId) -> Self {
            Self {
                deployment_id,
                state: StdMutex::new(MemoryControlState {
                    head: ClusterHead {
                        fencing_token: 0,
                        head_sequence: None,
                        snapshot_sequence: None,
                        snapshot_object: None,
                        durable_bytes: 0,
                        artifact_count: 0,
                        lease_owner: None,
                    },
                    blocks: Vec::new(),
                    artifacts: HashMap::new(),
                    lease: None,
                }),
            }
        }
    }

    impl ClusterControlPlane for MemoryControl {
        fn head(&self) -> BoxFuture<'_, ClusterHead> {
            Box::pin(async move { Ok(self.state.lock().unwrap().head.clone()) })
        }

        fn advance_head<'a>(
            &'a self,
            expected_head: Option<u64>,
            block: &'a RuntimeBlockRef,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                let lease_active = state.lease.as_ref().is_some_and(|lease| {
                    lease.fencing_token == block.fencing_token
                        && lease.expires_at_unix_ms >= unix_ms()
                });
                if state.head.fencing_token != block.fencing_token
                    || state.head.head_sequence != expected_head
                    || !lease_active
                {
                    return Err(PersistenceError::StaleFence {
                        expected: state.head.fencing_token,
                        found: block.fencing_token,
                    });
                }
                state.head.head_sequence = Some(block.last_sequence);
                state.head.durable_bytes += block.byte_len;
                state.blocks.push(block.clone());
                Ok(())
            })
        }

        fn blocks(&self, from_sequence: u64) -> BoxFuture<'_, Vec<RuntimeBlockRef>> {
            Box::pin(async move {
                Ok(self
                    .state
                    .lock()
                    .unwrap()
                    .blocks
                    .iter()
                    .filter(|block| block.last_sequence >= from_sequence)
                    .cloned()
                    .collect())
            })
        }

        fn advance_snapshot<'a>(
            &'a self,
            fencing_token: u64,
            through_sequence: u64,
            object_key: &'a str,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                let lease_active = state.lease.as_ref().is_some_and(|lease| {
                    lease.fencing_token == fencing_token && lease.expires_at_unix_ms >= unix_ms()
                });
                let snapshot_boundary_exists = state
                    .head
                    .head_sequence
                    .is_some_and(|head| head >= through_sequence)
                    || (state.head.head_sequence.is_none() && through_sequence == 0);
                if state.head.fencing_token != fencing_token
                    || !snapshot_boundary_exists
                    || !lease_active
                {
                    return Err(PersistenceError::StaleFence {
                        expected: state.head.fencing_token,
                        found: fencing_token,
                    });
                }
                state.head.snapshot_sequence = Some(through_sequence);
                state.head.snapshot_object = Some(object_key.to_owned());
                Ok(())
            })
        }

        fn put_artifact_metadata<'a>(
            &'a self,
            metadata: &'a ArtifactMetadata,
            object_key: &'a str,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                if state
                    .artifacts
                    .insert(
                        metadata.id.clone(),
                        (metadata.clone(), object_key.to_owned()),
                    )
                    .is_none()
                {
                    state.head.artifact_count += 1;
                }
                Ok(())
            })
        }

        fn artifact_metadata<'a>(
            &'a self,
            artifact_id: &'a ArtifactId,
        ) -> BoxFuture<'a, Option<(ArtifactMetadata, String)>> {
            Box::pin(async move {
                Ok(self
                    .state
                    .lock()
                    .unwrap()
                    .artifacts
                    .get(artifact_id)
                    .cloned())
            })
        }

        fn acquire_lease<'a>(
            &'a self,
            replica_id: &'a str,
            duration: Duration,
        ) -> BoxFuture<'a, LeaseGrant> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                let now = unix_ms();
                if state.lease.as_ref().is_some_and(|lease| {
                    lease.expires_at_unix_ms > now && lease.replica_id != replica_id
                }) {
                    return Err(PersistenceError::LeaseNotOwned(replica_id.to_owned()));
                }
                state.head.fencing_token += 1;
                let grant = LeaseGrant {
                    deployment_id: self.deployment_id.clone(),
                    replica_id: replica_id.to_owned(),
                    fencing_token: state.head.fencing_token,
                    expires_at_unix_ms: now.saturating_add(duration_ms(duration)),
                };
                state.head.lease_owner = Some(replica_id.to_owned());
                state.lease = Some(grant.clone());
                Ok(grant)
            })
        }

        fn renew_lease<'a>(
            &'a self,
            grant: &'a LeaseGrant,
            duration: Duration,
        ) -> BoxFuture<'a, LeaseGrant> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                let Some(current) = state.lease.as_ref() else {
                    return Err(PersistenceError::LeaseNotOwned(grant.replica_id.clone()));
                };
                if current.replica_id != grant.replica_id
                    || current.fencing_token != grant.fencing_token
                {
                    return Err(PersistenceError::LeaseNotOwned(grant.replica_id.clone()));
                }
                let renewed = LeaseGrant {
                    expires_at_unix_ms: unix_ms().saturating_add(duration_ms(duration)),
                    ..grant.clone()
                };
                state.lease = Some(renewed.clone());
                Ok(renewed)
            })
        }

        fn release_lease<'a>(&'a self, grant: &'a LeaseGrant) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                if state.lease.as_ref().is_some_and(|lease| {
                    lease.replica_id == grant.replica_id
                        && lease.fencing_token == grant.fencing_token
                }) {
                    state.lease = None;
                    state.head.lease_owner = None;
                    Ok(())
                } else {
                    Err(PersistenceError::LeaseNotOwned(grant.replica_id.clone()))
                }
            })
        }
    }

    fn fixture() -> (ClusterPersistence, Arc<MemoryControl>) {
        let deployment_id = DeploymentId::new();
        let digest = DeploymentDigest::of(b"cluster fixture");
        let control = Arc::new(MemoryControl::new(deployment_id.clone()));
        let store = ClusterPersistence {
            config: ClusterConfig {
                deployment_id,
                deployment_digest: digest,
                database_url: "memory".into(),
                bucket: "memory".into(),
                endpoint: None,
                region: None,
                prefix: Some("tests".into()),
                artifact_limit_bytes: MAX_ARTIFACT_BYTES,
            },
            control: control.clone(),
            objects: Arc::new(InMemory::new()),
        };
        (store, control)
    }

    #[test]
    fn cluster_config_debug_redacts_database_credentials() {
        let config = ClusterConfig::new(
            DeploymentId::new(),
            DeploymentDigest::of(b"cluster"),
            "postgres://admin:secret@database/graphreact",
            "bucket",
        );
        let debug = format!("{config:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("admin"));
        assert!(!debug.contains("secret"));
    }

    #[test]
    fn block_round_trip_is_versioned_and_sequence_checked() {
        let digest = DeploymentDigest::of(b"cluster");
        let block = ClusterLogBlockV1 {
            version: CLUSTER_BLOCK_VERSION,
            deployment_digest: digest,
            fencing_token: 7,
            first_sequence: 4,
            last_sequence: 4,
            records: vec![RuntimeRecordV1::AttemptStarted {
                run_id: RunId::new(),
                attempt_id: AttemptId::new(),
                number: 1,
                started_at_ms: 1,
            }],
        };
        let encoded = postcard::to_stdvec(&block).unwrap();
        let decoded: ClusterLogBlockV1 = postcard::from_bytes(&encoded).unwrap();
        let reference = RuntimeBlockRef {
            first_sequence: 4,
            last_sequence: 4,
            object_key: "block".into(),
            sha256: Sha256::digest(&encoded).into(),
            byte_len: encoded.len() as u64,
            fencing_token: 7,
        };
        verify_block(&decoded, &reference, digest).unwrap();
    }

    #[test]
    fn block_rejects_digest_and_fencing_mismatch() {
        let digest = DeploymentDigest::of(b"cluster");
        let block = ClusterLogBlockV1 {
            version: CLUSTER_BLOCK_VERSION,
            deployment_digest: digest,
            fencing_token: 8,
            first_sequence: 0,
            last_sequence: 0,
            records: vec![RuntimeRecordV1::RunStatusChanged {
                run_id: RunId::new(),
                status: RunStatus::Running,
                result: None,
                error: None,
                changed_at_ms: 1,
            }],
        };
        let reference = RuntimeBlockRef {
            first_sequence: 0,
            last_sequence: 0,
            object_key: "block".into(),
            sha256: [0; 32],
            byte_len: 0,
            fencing_token: 7,
        };
        assert!(matches!(
            verify_block(&block, &reference, digest),
            Err(PersistenceError::StaleFence { .. })
        ));
        assert!(matches!(
            verify_block(&block, &reference, DeploymentDigest::of(b"other")),
            Err(PersistenceError::DeploymentDigestMismatch)
        ));
    }

    #[tokio::test]
    async fn cluster_commit_replay_and_stale_fence_conform() {
        let (store, _) = fixture();
        let lease = store
            .acquire_lease("replica-a", Duration::from_secs(15))
            .await
            .unwrap();
        let first = RuntimeRecordV1::AttemptStarted {
            run_id: RunId::new(),
            attempt_id: AttemptId::new(),
            number: 1,
            started_at_ms: 1,
        };
        let second = RuntimeRecordV1::RunStatusChanged {
            run_id: RunId::new(),
            status: RunStatus::Succeeded,
            result: None,
            error: None,
            changed_at_ms: 2,
        };
        let receipt = store
            .commit(lease.fencing_token, &[first.clone(), second.clone()])
            .await
            .unwrap();
        assert_eq!((receipt.first_sequence, receipt.last_sequence), (0, 1));
        assert_eq!(store.replay(0).await.unwrap(), vec![first, second.clone()]);
        assert_eq!(store.replay(1).await.unwrap(), vec![second]);
        assert!(matches!(
            store
                .commit(
                    lease.fencing_token + 1,
                    &[RuntimeRecordV1::RunStatusChanged {
                        run_id: RunId::new(),
                        status: RunStatus::Failed,
                        result: None,
                        error: None,
                        changed_at_ms: 3,
                    }]
                )
                .await,
            Err(PersistenceError::StaleFence { .. })
        ));
    }

    #[tokio::test]
    async fn cluster_snapshot_bundle_artifact_and_orphan_conform() {
        let (mut store, _) = fixture();
        let bundle = DeploymentBundle::new(
            store.config.deployment_id.clone(),
            serde_json::json!({"graph": "fixture"}),
            b"payload".to_vec(),
        );
        store.config.deployment_digest = bundle.digest;
        store.put_bundle(&bundle).await.unwrap();
        assert_eq!(store.read_bundle().await.unwrap(), Some(bundle));

        let lease = store
            .acquire_lease("replica-a", Duration::from_secs(15))
            .await
            .unwrap();
        store
            .commit(
                lease.fencing_token,
                &[RuntimeRecordV1::AttemptStarted {
                    run_id: RunId::new(),
                    attempt_id: AttemptId::new(),
                    number: 1,
                    started_at_ms: 1,
                }],
            )
            .await
            .unwrap();
        let snapshot = RuntimeSnapshotV1 {
            deployment_id: store.config.deployment_id.clone(),
            deployment_digest: store.config.deployment_digest,
            through_sequence: 0,
            graph: serde_json::json!({}),
            manifests: serde_json::json!({}),
            durable_state: vec![],
            inbox: vec![],
            postponed: vec![],
            supervision: serde_json::json!({}),
            timers: vec![],
            effects: vec![],
            artifacts: vec![],
            runs: serde_json::json!({}),
        };
        store
            .write_snapshot(lease.fencing_token, &snapshot)
            .await
            .unwrap();
        assert_eq!(store.read_snapshot().await.unwrap(), Some(snapshot));

        let bytes = b"evidence";
        let metadata = ArtifactMetadata {
            id: ArtifactId::new(),
            run_id: RunId::new(),
            logical_name: "evidence.txt".into(),
            media_type: "text/plain".into(),
            size: bytes.len() as u64,
            sha256: Sha256::digest(bytes).into(),
            created_at_ms: 1,
        };
        store.put_artifact(&metadata, bytes).await.unwrap();
        assert_eq!(
            store.read_artifact(&metadata.id).await.unwrap(),
            Some(bytes.to_vec())
        );

        store
            .put_immutable(&store.object_key("log/orphan.grxblock"), b"orphan".to_vec())
            .await
            .unwrap();
        assert_eq!(store.replay(0).await.unwrap().len(), 1);
        assert_eq!(store.verify().await.unwrap().artifact_count, 1);
    }

    #[tokio::test]
    async fn cluster_lease_takeover_increments_fence_and_rejects_former_owner() {
        let (store, control) = fixture();
        let first = store
            .acquire_lease("replica-a", Duration::from_millis(1))
            .await
            .unwrap();
        control
            .state
            .lock()
            .unwrap()
            .lease
            .as_mut()
            .unwrap()
            .expires_at_unix_ms = 0;
        assert!(matches!(
            store
                .commit(
                    first.fencing_token,
                    &[RuntimeRecordV1::RunStatusChanged {
                        run_id: RunId::new(),
                        status: RunStatus::Running,
                        result: None,
                        error: None,
                        changed_at_ms: 1,
                    }],
                )
                .await,
            Err(PersistenceError::StaleFence { .. })
        ));
        assert!(store.replay(0).await.unwrap().is_empty());
        let second = store
            .acquire_lease("replica-b", Duration::from_secs(15))
            .await
            .unwrap();
        assert!(second.fencing_token > first.fencing_token);
        assert!(store
            .renew_lease(&first, Duration::from_secs(15))
            .await
            .is_err());
        assert!(store.release_lease(&first).await.is_err());
        store
            .renew_lease(&second, Duration::from_secs(15))
            .await
            .unwrap();
    }
}
