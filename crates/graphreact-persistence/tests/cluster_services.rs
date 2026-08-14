//! Real-service conformance for the Phase A cluster backend.
//!
//! The ordinary workspace suite does not require Docker. CI (or an operator)
//! opts into this test after providing a disposable Postgres database and
//! S3-compatible bucket through the environment variables read below.

use std::time::{Duration, Instant};

use graphreact_persistence::{
    ArtifactId, ArtifactMetadata, AttemptId, ClusterConfig, ClusterPersistence, DeploymentBundle,
    DeploymentDigest, DeploymentId, PersistenceEngine, PersistenceError, RunId, RunStatus,
    RuntimeRecordV1, RuntimeSnapshotV1,
};
use sha2::{Digest, Sha256};

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for cluster service tests"))
}

fn config(deployment_id: DeploymentId, deployment_digest: DeploymentDigest) -> ClusterConfig {
    let mut config = ClusterConfig::new(
        deployment_id.clone(),
        deployment_digest,
        required_env("GRX_CLUSTER_TEST_DATABASE_URL"),
        required_env("GRX_CLUSTER_TEST_S3_BUCKET"),
    );
    config.endpoint = Some(required_env("GRX_CLUSTER_TEST_S3_ENDPOINT"));
    config.region =
        Some(std::env::var("GRX_CLUSTER_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into()));
    config.prefix = Some(format!("service-conformance/{}", deployment_id.as_str()));
    config
}

fn attempt_started(started_at_ms: u64) -> RuntimeRecordV1 {
    RuntimeRecordV1::AttemptStarted {
        run_id: RunId::new(),
        attempt_id: AttemptId::new(),
        number: 1,
        started_at_ms,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires GRX_CLUSTER_TEST_* Postgres and S3-compatible services"]
async fn postgres_minio_backend_conforms_and_fences_takeover() {
    let deployment_id = DeploymentId::new();
    let bundle = DeploymentBundle::new(
        deployment_id.clone(),
        serde_json::json!({"graph": "service-conformance"}),
        b"immutable deployment payload".to_vec(),
    );
    let first = ClusterPersistence::connect(config(deployment_id.clone(), bundle.digest))
        .await
        .expect("connect first cluster backend");
    let second = ClusterPersistence::connect(config(deployment_id.clone(), bundle.digest))
        .await
        .expect("connect standby cluster backend");

    first.put_bundle(&bundle).await.expect("put bundle");
    first
        .put_bundle(&bundle)
        .await
        .expect("immutable bundle put is idempotent");
    assert_eq!(second.read_bundle().await.unwrap(), Some(bundle));

    let first_lease = first
        .acquire_lease("replica-a", Duration::from_secs(1))
        .await
        .expect("acquire first lease");
    assert!(matches!(
        second
            .acquire_lease("replica-b", Duration::from_secs(15))
            .await,
        Err(PersistenceError::LeaseNotOwned(owner)) if owner == "replica-b"
    ));

    let initial = attempt_started(1);
    let initial_receipt = first
        .commit(first_lease.fencing_token, std::slice::from_ref(&initial))
        .await
        .expect("commit initial block");
    assert_eq!(
        (
            initial_receipt.first_sequence,
            initial_receipt.last_sequence
        ),
        (0, 0)
    );

    // An expired writer still uploads its immutable block before the fenced
    // head CAS rejects it. That object is therefore an orphan, and replay must
    // continue to follow only Postgres-indexed blocks.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let orphaned = RuntimeRecordV1::RunStatusChanged {
        run_id: RunId::new(),
        status: RunStatus::Failed,
        result: None,
        error: Some("must remain unreferenced".into()),
        changed_at_ms: 2,
    };
    assert!(matches!(
        first
            .commit(first_lease.fencing_token, std::slice::from_ref(&orphaned))
            .await,
        Err(PersistenceError::StaleFence { .. })
    ));
    assert_eq!(second.replay(0).await.unwrap(), vec![initial.clone()]);

    let takeover_started = Instant::now();
    let second_lease = second
        .acquire_lease("replica-b", Duration::from_secs(15))
        .await
        .expect("standby takeover");
    assert!(takeover_started.elapsed() < Duration::from_secs(2));
    assert!(second_lease.fencing_token > first_lease.fencing_token);
    assert!(first
        .renew_lease(&first_lease, Duration::from_secs(15))
        .await
        .is_err());
    assert!(first.release_lease(&first_lease).await.is_err());
    assert!(matches!(
        first
            .commit(first_lease.fencing_token, std::slice::from_ref(&orphaned))
            .await,
        Err(PersistenceError::StaleFence { .. })
    ));

    let takeover_record = attempt_started(3);
    let takeover_receipt = second
        .commit(
            second_lease.fencing_token,
            std::slice::from_ref(&takeover_record),
        )
        .await
        .expect("new owner commit");
    assert_eq!(
        (
            takeover_receipt.first_sequence,
            takeover_receipt.last_sequence
        ),
        (1, 1)
    );
    assert_eq!(
        first.replay(0).await.unwrap(),
        vec![initial, takeover_record]
    );

    let snapshot = RuntimeSnapshotV1 {
        deployment_id: deployment_id.clone(),
        deployment_digest: second.deployment_digest(),
        through_sequence: 1,
        graph: serde_json::json!({"nodes": ["agent"]}),
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
    second
        .write_snapshot(second_lease.fencing_token, &snapshot)
        .await
        .expect("write snapshot");
    assert_eq!(first.read_snapshot().await.unwrap(), Some(snapshot));

    let artifact_bytes = b"service-level evidence";
    let artifact = ArtifactMetadata {
        id: ArtifactId::new(),
        run_id: RunId::new(),
        logical_name: "evidence.txt".into(),
        media_type: "text/plain".into(),
        size: artifact_bytes.len() as u64,
        sha256: Sha256::digest(artifact_bytes).into(),
        created_at_ms: 4,
    };
    second
        .put_artifact(&artifact, artifact_bytes)
        .await
        .expect("put artifact");
    second
        .put_artifact(&artifact, artifact_bytes)
        .await
        .expect("artifact metadata put is idempotent");
    assert_eq!(
        first.read_artifact(&artifact.id).await.unwrap(),
        Some(artifact_bytes.to_vec())
    );

    let verified = first.verify().await.expect("verify recovered deployment");
    assert_eq!(verified.head_sequence, Some(1));
    assert_eq!(verified.snapshot_sequence, Some(1));
    assert_eq!(verified.artifact_count, 1);
    assert_eq!(verified.lease_owner.as_deref(), Some("replica-b"));

    second
        .release_lease(&second_lease)
        .await
        .expect("release active lease");
    assert_eq!(first.status().await.unwrap().lease_owner, None);

    let mismatched = ClusterPersistence::connect(config(
        deployment_id,
        DeploymentDigest::of(b"different immutable deployment"),
    ))
    .await;
    assert!(matches!(
        mismatched,
        Err(PersistenceError::DeploymentDigestMismatch)
    ));
}
