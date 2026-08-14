use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("persistence I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("deployment is already owned by another local process: {0}")]
    Locked(PathBuf),
    #[error("invalid durable log magic at sequence {sequence}")]
    InvalidMagic { sequence: u64 },
    #[error("unsupported durable format version {found}; supported version is {supported}")]
    UnsupportedVersion { found: u16, supported: u16 },
    #[error("durable record exceeds {cap} bytes: {actual}")]
    RecordTooLarge { actual: usize, cap: usize },
    #[error("artifact exceeds {cap} bytes: {actual}")]
    ArtifactTooLarge { actual: usize, cap: usize },
    #[error("checksum mismatch at durable sequence {sequence}")]
    ChecksumMismatch { sequence: u64 },
    #[error("non-monotonic durable sequence: expected {expected}, found {found}")]
    SequenceMismatch { expected: u64, found: u64 },
    #[error("deployment digest mismatch")]
    DeploymentDigestMismatch,
    #[error("stale fencing token: expected {expected}, found {found}")]
    StaleFence { expected: u64, found: u64 },
    #[error("durable codec error: {0}")]
    Codec(String),
    #[error("snapshot pointer is corrupt: {0}")]
    SnapshotPointer(String),
    #[error("artifact digest mismatch")]
    ArtifactDigestMismatch,
    #[error("lease is not owned by replica `{0}`")]
    LeaseNotOwned(String),
    #[error("invalid persistence configuration: {0}")]
    Configuration(String),
    #[error("Postgres persistence operation failed: {0}")]
    Database(String),
    #[error("object-store persistence operation failed: {0}")]
    ObjectStore(String),
}

impl PersistenceError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, PersistenceError>;
