use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use sha2::{Digest, Sha256};

use crate::{
    ArtifactId, ArtifactMetadata, BoxFuture, CommitReceipt, DeploymentBundle, DeploymentDigest,
    DeploymentId, LeaseGrant, PersistenceEngine, PersistenceError, PersistenceStatus,
    RuntimeRecordV1, RuntimeSnapshotV1, MAX_ARTIFACT_BYTES, MAX_RECORD_BYTES,
};

const MAGIC: [u8; 8] = *b"GRXLOG01";
const HEADER_BYTES: usize = 8 + 2 + 32 + 8 + 8 + 4 + 32;

/// A verified frame decoded from a local durable log segment.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedLocalFrame {
    pub fencing_token: u64,
    pub sequence: u64,
    pub record: RuntimeRecordV1,
    pub consumed_bytes: usize,
}

struct DecodedFrameHeader {
    fencing_token: u64,
    sequence: u64,
    payload_len: usize,
    checksum: [u8; 32],
}

/// Decode and verify the first framed record in `bytes`.
///
/// Trailing bytes are left unconsumed so fuzzers and offline tooling can walk
/// concatenated segment data using [`DecodedLocalFrame::consumed_bytes`].
pub fn decode_local_frame(
    bytes: &[u8],
    expected_digest: DeploymentDigest,
    expected_fencing_token: u64,
    expected_sequence: u64,
) -> crate::Result<DecodedLocalFrame> {
    if bytes.len() < HEADER_BYTES {
        return Err(PersistenceError::Codec("torn durable frame header".into()));
    }
    let header = decode_frame_header(
        &bytes[..HEADER_BYTES],
        expected_digest,
        expected_fencing_token,
        expected_sequence,
    )?;
    let frame_len =
        HEADER_BYTES
            .checked_add(header.payload_len)
            .ok_or(PersistenceError::RecordTooLarge {
                actual: usize::MAX,
                cap: MAX_RECORD_BYTES,
            })?;
    if bytes.len() < frame_len {
        return Err(PersistenceError::Codec("torn durable frame payload".into()));
    }
    let record = decode_frame_payload(&header, &bytes[HEADER_BYTES..frame_len])?;
    Ok(DecodedLocalFrame {
        fencing_token: header.fencing_token,
        sequence: header.sequence,
        record,
        consumed_bytes: frame_len,
    })
}

fn decode_frame_header(
    header: &[u8],
    expected_digest: DeploymentDigest,
    expected_fencing_token: u64,
    expected_sequence: u64,
) -> crate::Result<DecodedFrameHeader> {
    if header[..8] != MAGIC {
        return Err(PersistenceError::InvalidMagic {
            sequence: expected_sequence,
        });
    }
    let version = u16::from_be_bytes([header[8], header[9]]);
    if version != crate::FORMAT_VERSION {
        return Err(PersistenceError::UnsupportedVersion {
            found: version,
            supported: crate::FORMAT_VERSION,
        });
    }
    let digest = DeploymentDigest(header[10..42].try_into().expect("digest width"));
    if digest != expected_digest {
        return Err(PersistenceError::DeploymentDigestMismatch);
    }
    let fencing_token = u64::from_be_bytes(header[42..50].try_into().expect("fence width"));
    if fencing_token != expected_fencing_token {
        return Err(PersistenceError::StaleFence {
            expected: expected_fencing_token,
            found: fencing_token,
        });
    }
    let sequence = u64::from_be_bytes(header[50..58].try_into().expect("sequence width"));
    if sequence != expected_sequence {
        return Err(PersistenceError::SequenceMismatch {
            expected: expected_sequence,
            found: sequence,
        });
    }
    let payload_len =
        u32::from_be_bytes(header[58..62].try_into().expect("payload width")) as usize;
    if payload_len > MAX_RECORD_BYTES {
        return Err(PersistenceError::RecordTooLarge {
            actual: payload_len,
            cap: MAX_RECORD_BYTES,
        });
    }
    Ok(DecodedFrameHeader {
        fencing_token,
        sequence,
        payload_len,
        checksum: header[62..94].try_into().expect("checksum width"),
    })
}

fn decode_frame_payload(
    header: &DecodedFrameHeader,
    payload: &[u8],
) -> crate::Result<RuntimeRecordV1> {
    let actual: [u8; 32] = Sha256::digest(payload).into();
    if header.checksum != actual {
        return Err(PersistenceError::ChecksumMismatch {
            sequence: header.sequence,
        });
    }
    crate::decode_runtime_record(payload)
}

#[derive(Debug, Clone)]
pub struct LocalConfig {
    pub root: PathBuf,
    pub deployment_id: DeploymentId,
    pub deployment_digest: DeploymentDigest,
    pub fencing_token: u64,
    pub artifact_limit_bytes: u64,
}

impl LocalConfig {
    #[must_use]
    pub fn new(
        root: impl Into<PathBuf>,
        deployment_id: DeploymentId,
        deployment_digest: DeploymentDigest,
    ) -> Self {
        Self {
            root: root.into(),
            deployment_id,
            deployment_digest,
            fencing_token: 1,
            artifact_limit_bytes: MAX_ARTIFACT_BYTES,
        }
    }

    /// Discover deployment identity from the immutable bundle in an existing
    /// local persistence directory. Offline tooling uses this so operators do
    /// not need to copy digests out of log headers by hand.
    pub fn discover(root: impl Into<PathBuf>) -> crate::Result<Self> {
        let root = root.into();
        let bundles_dir = root.join("bundles");
        let entries = std::fs::read_dir(&bundles_dir)
            .map_err(|error| PersistenceError::io(&bundles_dir, error))?;
        let mut bundle_paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| PersistenceError::io(&bundles_dir, error))?;
            let path = entry.path();
            if path.extension().is_some_and(|value| value == "grxbundle") {
                bundle_paths.push(path);
            }
        }
        let path = bundle_paths.first().ok_or_else(|| {
            PersistenceError::Configuration(format!(
                "no immutable deployment bundle found in {}",
                bundles_dir.display()
            ))
        })?;
        if bundle_paths.len() != 1 {
            return Err(PersistenceError::Configuration(format!(
                "multiple immutable deployment bundles found in {}",
                bundles_dir.display()
            )));
        }
        let bytes = std::fs::read(path).map_err(|error| PersistenceError::io(path, error))?;
        let bundle: DeploymentBundle = postcard::from_bytes(&bytes)
            .map_err(|error| PersistenceError::Codec(error.to_string()))?;
        if !bundle.verify_digest()
            || path.file_stem().and_then(std::ffi::OsStr::to_str)
                != Some(bundle.digest.hex().as_str())
        {
            return Err(PersistenceError::DeploymentDigestMismatch);
        }
        Ok(Self::new(root, bundle.deployment_id, bundle.digest))
    }
}

pub struct LocalPersistence {
    cfg: LocalConfig,
    _lock: File,
    state: Mutex<LocalState>,
}

struct LocalState {
    log: File,
    next_sequence: u64,
    durable_bytes: u64,
    snapshot_sequence: Option<u64>,
    artifact_count: u64,
    lease: Option<LeaseGrant>,
}

impl std::fmt::Debug for LocalPersistence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalPersistence")
            .field("root", &self.cfg.root)
            .field("deployment_id", &self.cfg.deployment_id)
            .finish_non_exhaustive()
    }
}

impl LocalPersistence {
    pub fn open(cfg: LocalConfig) -> crate::Result<Self> {
        std::fs::create_dir_all(cfg.root.join("snapshots"))
            .map_err(|e| PersistenceError::io(&cfg.root, e))?;
        std::fs::create_dir_all(cfg.root.join("artifacts"))
            .map_err(|e| PersistenceError::io(&cfg.root, e))?;
        std::fs::create_dir_all(cfg.root.join("bundles"))
            .map_err(|e| PersistenceError::io(&cfg.root, e))?;

        let lock_path = cfg.root.join("writer.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| PersistenceError::io(&lock_path, e))?;
        lock.try_lock_exclusive()
            .map_err(|_| PersistenceError::Locked(lock_path))?;

        let log_path = cfg.root.join("runtime.grxlog");
        let mut log = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| PersistenceError::io(&log_path, e))?;
        let scan = scan_log(
            &mut log,
            cfg.deployment_digest,
            cfg.fencing_token,
            true,
            &log_path,
        )?;
        let snapshot_sequence = read_snapshot_pointer(&cfg.root)?.map(|(seq, _)| seq);
        let artifact_count = std::fs::read_dir(cfg.root.join("artifacts"))
            .map(|it| it.filter_map(std::result::Result::ok).count() as u64)
            .unwrap_or(0);
        Ok(Self {
            cfg,
            _lock: lock,
            state: Mutex::new(LocalState {
                log,
                next_sequence: scan.next_sequence,
                durable_bytes: scan.valid_bytes,
                snapshot_sequence,
                artifact_count,
                lease: None,
            }),
        })
    }

    fn status_locked(&self, state: &LocalState) -> PersistenceStatus {
        PersistenceStatus {
            deployment_id: self.cfg.deployment_id.clone(),
            deployment_digest: self.cfg.deployment_digest,
            fencing_token: self.cfg.fencing_token,
            head_sequence: state.next_sequence.checked_sub(1),
            snapshot_sequence: state.snapshot_sequence,
            durable_bytes: state.durable_bytes,
            artifact_count: state.artifact_count,
            lease_owner: state.lease.as_ref().map(|g| g.replica_id.clone()),
        }
    }
}

impl PersistenceEngine for LocalPersistence {
    fn deployment_id(&self) -> &DeploymentId {
        &self.cfg.deployment_id
    }

    fn deployment_digest(&self) -> DeploymentDigest {
        self.cfg.deployment_digest
    }

    fn commit<'a>(
        &'a self,
        fencing_token: u64,
        records: &'a [RuntimeRecordV1],
    ) -> BoxFuture<'a, CommitReceipt> {
        Box::pin(async move {
            if fencing_token != self.cfg.fencing_token {
                return Err(PersistenceError::StaleFence {
                    expected: self.cfg.fencing_token,
                    found: fencing_token,
                });
            }
            let mut state = self.state.lock().expect("local persistence lock poisoned");
            let first = state.next_sequence;
            let mut bytes_written = 0_u64;
            for record in records {
                let payload = postcard::to_stdvec(record)
                    .map_err(|e| PersistenceError::Codec(e.to_string()))?;
                if payload.len() > MAX_RECORD_BYTES {
                    return Err(PersistenceError::RecordTooLarge {
                        actual: payload.len(),
                        cap: MAX_RECORD_BYTES,
                    });
                }
                let sequence = state.next_sequence;
                let frame = encode_local_frame(
                    self.cfg.deployment_digest,
                    fencing_token,
                    sequence,
                    &payload,
                )?;
                state
                    .log
                    .write_all(&frame)
                    .map_err(|e| PersistenceError::io(self.cfg.root.join("runtime.grxlog"), e))?;
                state.next_sequence += 1;
                bytes_written += frame.len() as u64;
            }
            state
                .log
                .sync_data()
                .map_err(|e| PersistenceError::io(self.cfg.root.join("runtime.grxlog"), e))?;
            state.durable_bytes += bytes_written;
            Ok(CommitReceipt {
                first_sequence: first,
                last_sequence: state.next_sequence.saturating_sub(1),
                fencing_token,
                durable_bytes: bytes_written,
            })
        })
    }

    fn replay(&self, from_sequence: u64) -> BoxFuture<'_, Vec<RuntimeRecordV1>> {
        Box::pin(async move {
            let path = self.cfg.root.join("runtime.grxlog");
            let mut file = OpenOptions::new()
                .read(true)
                .open(&path)
                .map_err(|e| PersistenceError::io(&path, e))?;
            let scan = scan_log(
                &mut file,
                self.cfg.deployment_digest,
                self.cfg.fencing_token,
                false,
                &path,
            )?;
            Ok(scan
                .records
                .into_iter()
                .filter_map(|(seq, record)| (seq >= from_sequence).then_some(record))
                .collect())
        })
    }

    fn write_snapshot<'a>(
        &'a self,
        fencing_token: u64,
        snapshot: &'a RuntimeSnapshotV1,
    ) -> BoxFuture<'a, [u8; 32]> {
        Box::pin(async move {
            if fencing_token != self.cfg.fencing_token {
                return Err(PersistenceError::StaleFence {
                    expected: self.cfg.fencing_token,
                    found: fencing_token,
                });
            }
            if snapshot.deployment_digest != self.cfg.deployment_digest {
                return Err(PersistenceError::DeploymentDigestMismatch);
            }
            let bytes = postcard::to_stdvec(snapshot)
                .map_err(|e| PersistenceError::Codec(e.to_string()))?;
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            let name = format!("{}-{}.grxsnap", snapshot.through_sequence, hex(&digest));
            let final_path = self.cfg.root.join("snapshots").join(&name);
            atomic_write(&final_path, &bytes)?;
            let pointer = format!("{}\n{}\n", snapshot.through_sequence, name);
            atomic_write(&self.cfg.root.join("snapshot.current"), pointer.as_bytes())?;
            let mut state = self.state.lock().expect("local persistence lock poisoned");
            state.snapshot_sequence = Some(snapshot.through_sequence);
            Ok(digest)
        })
    }

    fn read_snapshot(&self) -> BoxFuture<'_, Option<RuntimeSnapshotV1>> {
        Box::pin(async move {
            let Some((_seq, name)) = read_snapshot_pointer(&self.cfg.root)? else {
                return Ok(None);
            };
            let path = self.cfg.root.join("snapshots").join(name);
            let bytes = std::fs::read(&path).map_err(|e| PersistenceError::io(&path, e))?;
            let snapshot = crate::decode_runtime_snapshot(&bytes)?;
            if snapshot.deployment_digest != self.cfg.deployment_digest {
                return Err(PersistenceError::DeploymentDigestMismatch);
            }
            Ok(Some(snapshot))
        })
    }

    fn put_bundle<'a>(&'a self, bundle: &'a DeploymentBundle) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if bundle.deployment_id != self.cfg.deployment_id
                || bundle.digest != self.cfg.deployment_digest
            {
                return Err(PersistenceError::DeploymentDigestMismatch);
            }
            let bytes =
                postcard::to_stdvec(bundle).map_err(|e| PersistenceError::Codec(e.to_string()))?;
            let path = self
                .cfg
                .root
                .join("bundles")
                .join(format!("{}.grxbundle", bundle.digest.hex()));
            atomic_write(&path, &bytes)
        })
    }

    fn read_bundle(&self) -> BoxFuture<'_, Option<DeploymentBundle>> {
        Box::pin(async move {
            let path = self
                .cfg
                .root
                .join("bundles")
                .join(format!("{}.grxbundle", self.cfg.deployment_digest.hex()));
            if !path.exists() {
                return Ok(None);
            }
            let bytes = std::fs::read(&path).map_err(|e| PersistenceError::io(&path, e))?;
            let bundle: DeploymentBundle =
                postcard::from_bytes(&bytes).map_err(|e| PersistenceError::Codec(e.to_string()))?;
            if bundle.digest != self.cfg.deployment_digest {
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
            if bytes.len() as u64 > self.cfg.artifact_limit_bytes {
                return Err(PersistenceError::ArtifactTooLarge {
                    actual: bytes.len(),
                    cap: usize::try_from(self.cfg.artifact_limit_bytes).unwrap_or(usize::MAX),
                });
            }
            let digest: [u8; 32] = Sha256::digest(bytes).into();
            if digest != metadata.sha256 || metadata.size != bytes.len() as u64 {
                return Err(PersistenceError::ArtifactDigestMismatch);
            }
            let blob_path = self.cfg.root.join("artifacts").join(hex(&digest));
            if !blob_path.exists() {
                atomic_write(&blob_path, bytes)?;
            }
            let meta_path = self
                .cfg
                .root
                .join("artifacts")
                .join(format!("{}.meta", metadata.id));
            let encoded = postcard::to_stdvec(metadata)
                .map_err(|e| PersistenceError::Codec(e.to_string()))?;
            atomic_write(&meta_path, &encoded)?;
            let mut state = self.state.lock().expect("local persistence lock poisoned");
            state.artifact_count += 1;
            Ok(())
        })
    }

    fn read_artifact<'a>(&'a self, id: &'a ArtifactId) -> BoxFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            let meta_path = self.cfg.root.join("artifacts").join(format!("{id}.meta"));
            if !meta_path.exists() {
                return Ok(None);
            }
            let meta_bytes =
                std::fs::read(&meta_path).map_err(|e| PersistenceError::io(&meta_path, e))?;
            let metadata: ArtifactMetadata = postcard::from_bytes(&meta_bytes)
                .map_err(|e| PersistenceError::Codec(e.to_string()))?;
            let blob_path = self.cfg.root.join("artifacts").join(hex(&metadata.sha256));
            let bytes =
                std::fs::read(&blob_path).map_err(|e| PersistenceError::io(&blob_path, e))?;
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            if digest != metadata.sha256 {
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
        Box::pin(async move {
            let mut state = self.state.lock().expect("local persistence lock poisoned");
            let now = unix_ms();
            if let Some(current) = state.lease.as_ref() {
                if current.expires_at_unix_ms > now && current.replica_id != replica_id {
                    return Err(PersistenceError::LeaseNotOwned(replica_id.to_string()));
                }
            }
            let grant = LeaseGrant {
                deployment_id: self.cfg.deployment_id.clone(),
                replica_id: replica_id.to_string(),
                fencing_token: self.cfg.fencing_token,
                expires_at_unix_ms: now.saturating_add(duration_ms(duration)),
            };
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
            let mut state = self.state.lock().expect("local persistence lock poisoned");
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
            let mut state = self.state.lock().expect("local persistence lock poisoned");
            if state.lease.as_ref().is_some_and(|current| {
                current.replica_id == grant.replica_id
                    && current.fencing_token == grant.fencing_token
            }) {
                state.lease = None;
                Ok(())
            } else {
                Err(PersistenceError::LeaseNotOwned(grant.replica_id.clone()))
            }
        })
    }

    fn status(&self) -> BoxFuture<'_, PersistenceStatus> {
        Box::pin(async move {
            let state = self.state.lock().expect("local persistence lock poisoned");
            Ok(self.status_locked(&state))
        })
    }

    fn verify(&self) -> BoxFuture<'_, PersistenceStatus> {
        Box::pin(async move {
            let path = self.cfg.root.join("runtime.grxlog");
            let mut file = OpenOptions::new()
                .read(true)
                .open(&path)
                .map_err(|e| PersistenceError::io(&path, e))?;
            let _ = scan_log(
                &mut file,
                self.cfg.deployment_digest,
                self.cfg.fencing_token,
                false,
                &path,
            )?;
            if let Some(snapshot) = self.read_snapshot().await? {
                if snapshot.deployment_digest != self.cfg.deployment_digest {
                    return Err(PersistenceError::DeploymentDigestMismatch);
                }
            }
            let state = self.state.lock().expect("local persistence lock poisoned");
            Ok(self.status_locked(&state))
        })
    }
}

struct ScanResult {
    records: Vec<(u64, RuntimeRecordV1)>,
    next_sequence: u64,
    valid_bytes: u64,
}

fn scan_log(
    file: &mut File,
    expected_digest: DeploymentDigest,
    expected_fencing_token: u64,
    repair_tail: bool,
    path: &Path,
) -> crate::Result<ScanResult> {
    file.seek(SeekFrom::Start(0))
        .map_err(|e| PersistenceError::io(path, e))?;
    let total_len = file
        .metadata()
        .map_err(|e| PersistenceError::io(path, e))?
        .len();
    let mut offset = 0_u64;
    let mut expected_sequence = 0_u64;
    let mut records = Vec::new();
    while offset < total_len {
        let remaining = total_len - offset;
        if remaining < HEADER_BYTES as u64 {
            if repair_tail {
                file.set_len(offset)
                    .map_err(|e| PersistenceError::io(path, e))?;
                file.sync_data()
                    .map_err(|e| PersistenceError::io(path, e))?;
                break;
            }
            return Err(PersistenceError::Codec("torn durable frame header".into()));
        }
        let mut header = [0_u8; HEADER_BYTES];
        file.read_exact(&mut header)
            .map_err(|e| PersistenceError::io(path, e))?;
        let decoded_header = decode_frame_header(
            &header,
            expected_digest,
            expected_fencing_token,
            expected_sequence,
        )?;
        let payload_len = decoded_header.payload_len;
        let frame_end = offset + HEADER_BYTES as u64 + payload_len as u64;
        if frame_end > total_len {
            if repair_tail {
                file.set_len(offset)
                    .map_err(|e| PersistenceError::io(path, e))?;
                file.sync_data()
                    .map_err(|e| PersistenceError::io(path, e))?;
                break;
            }
            return Err(PersistenceError::Codec("torn durable frame payload".into()));
        }
        let mut payload = vec![0_u8; payload_len];
        file.read_exact(&mut payload)
            .map_err(|e| PersistenceError::io(path, e))?;
        let record = decode_frame_payload(&decoded_header, &payload)?;
        records.push((decoded_header.sequence, record));
        expected_sequence += 1;
        offset = frame_end;
    }
    file.seek(SeekFrom::End(0))
        .map_err(|e| PersistenceError::io(path, e))?;
    Ok(ScanResult {
        records,
        next_sequence: expected_sequence,
        valid_bytes: offset,
    })
}

/// Encode one local durable-log frame using the production checksum and size
/// rules. Exposed for offline tooling and decoder fuzz corpus construction.
pub fn encode_local_frame(
    digest: DeploymentDigest,
    fence: u64,
    sequence: u64,
    payload: &[u8],
) -> crate::Result<Vec<u8>> {
    if payload.len() > MAX_RECORD_BYTES {
        return Err(PersistenceError::RecordTooLarge {
            actual: payload.len(),
            cap: MAX_RECORD_BYTES,
        });
    }
    let checksum: [u8; 32] = Sha256::digest(payload).into();
    let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&crate::FORMAT_VERSION.to_be_bytes());
    out.extend_from_slice(&digest.0);
    out.extend_from_slice(&fence.to_be_bytes());
    out.extend_from_slice(&sequence.to_be_bytes());
    let payload_len = u32::try_from(payload.len()).expect("record cap fits u32");
    out.extend_from_slice(&payload_len.to_be_bytes());
    out.extend_from_slice(&checksum);
    out.extend_from_slice(payload);
    Ok(out)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> crate::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::now_v7()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .map_err(|e| PersistenceError::io(&tmp, e))?;
    file.write_all(bytes)
        .map_err(|e| PersistenceError::io(&tmp, e))?;
    file.sync_all().map_err(|e| PersistenceError::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| PersistenceError::io(path, e))?;
    if let Some(parent) = path.parent() {
        let dir = File::open(parent).map_err(|e| PersistenceError::io(parent, e))?;
        dir.sync_all()
            .map_err(|e| PersistenceError::io(parent, e))?;
    }
    Ok(())
}

fn read_snapshot_pointer(root: &Path) -> crate::Result<Option<(u64, String)>> {
    let path = root.join("snapshot.current");
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| PersistenceError::io(&path, e))?;
    let mut lines = text.lines();
    let seq = lines
        .next()
        .ok_or_else(|| PersistenceError::SnapshotPointer("missing sequence".into()))?
        .parse::<u64>()
        .map_err(|_| PersistenceError::SnapshotPointer("invalid sequence".into()))?;
    let name = lines
        .next()
        .filter(|name| !name.contains('/') && !name.contains(".."))
        .ok_or_else(|| PersistenceError::SnapshotPointer("invalid snapshot name".into()))?;
    Ok(Some((seq, name.to_string())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        },
    )
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttemptId, RunId, RunStatus};

    fn test_config(root: &Path) -> LocalConfig {
        LocalConfig::new(
            root,
            DeploymentId::new(),
            DeploymentDigest::of(b"test-deployment"),
        )
    }

    fn record() -> RuntimeRecordV1 {
        RuntimeRecordV1::AttemptStarted {
            run_id: RunId::new(),
            attempt_id: AttemptId::new(),
            number: 1,
            started_at_ms: 7,
        }
    }

    #[tokio::test]
    async fn commit_reopen_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        {
            let store = LocalPersistence::open(cfg.clone()).unwrap();
            let receipt = store.commit(1, &[record(), record()]).await.unwrap();
            assert_eq!(receipt.first_sequence, 0);
            assert_eq!(receipt.last_sequence, 1);
        }
        let store = LocalPersistence::open(cfg).unwrap();
        assert_eq!(store.replay(0).await.unwrap().len(), 2);
        assert_eq!(store.status().await.unwrap().head_sequence, Some(1));
    }

    #[tokio::test]
    async fn config_discovers_identity_from_verified_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let deployment_id = DeploymentId::new();
        let bundle = DeploymentBundle::new(
            deployment_id.clone(),
            serde_json::json!({"format_version": 1}),
            b"deployment".to_vec(),
        );
        let cfg = LocalConfig::new(dir.path(), deployment_id.clone(), bundle.digest);
        {
            let store = LocalPersistence::open(cfg).unwrap();
            store.put_bundle(&bundle).await.unwrap();
        }

        let discovered = LocalConfig::discover(dir.path()).unwrap();
        assert_eq!(discovered.deployment_id, deployment_id);
        assert_eq!(discovered.deployment_digest, bundle.digest);
    }

    #[tokio::test]
    async fn torn_tail_is_repaired_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        {
            let store = LocalPersistence::open(cfg.clone()).unwrap();
            store.commit(1, &[record()]).await.unwrap();
        }
        let path = dir.path().join("runtime.grxlog");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"GRX")
            .unwrap();
        let store = LocalPersistence::open(cfg).unwrap();
        assert_eq!(store.replay(0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn checksum_corruption_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        {
            let store = LocalPersistence::open(cfg.clone()).unwrap();
            store.commit(1, &[record()]).await.unwrap();
        }
        let path = dir.path().join("runtime.grxlog");
        let mut bytes = std::fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 0xff;
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            LocalPersistence::open(cfg),
            Err(PersistenceError::ChecksumMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn fencing_token_corruption_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        {
            let store = LocalPersistence::open(cfg.clone()).unwrap();
            store.commit(1, &[record()]).await.unwrap();
        }
        let path = dir.path().join("runtime.grxlog");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[49] = 2;
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            LocalPersistence::open(cfg),
            Err(PersistenceError::StaleFence {
                expected: 1,
                found: 2,
            })
        ));
    }

    #[tokio::test]
    async fn snapshot_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let store = LocalPersistence::open(cfg.clone()).unwrap();
        let snapshot = RuntimeSnapshotV1 {
            deployment_id: cfg.deployment_id,
            deployment_digest: cfg.deployment_digest,
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
            runs: serde_json::json!({"status": RunStatus::Running}),
        };
        store.write_snapshot(1, &snapshot).await.unwrap();
        assert_eq!(store.read_snapshot().await.unwrap(), Some(snapshot));
    }

    #[tokio::test]
    async fn artifact_is_content_verified() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let store = LocalPersistence::open(cfg).unwrap();
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
            store.read_artifact(&metadata.id).await.unwrap().unwrap(),
            bytes
        );
    }
}
