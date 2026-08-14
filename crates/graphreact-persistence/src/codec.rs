use crate::{PersistenceError, RuntimeRecordV1, RuntimeSnapshotV1, MAX_RECORD_BYTES};

/// Decode one versioned runtime-record payload with the production size cap.
pub fn decode_runtime_record(bytes: &[u8]) -> crate::Result<RuntimeRecordV1> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(PersistenceError::RecordTooLarge {
            actual: bytes.len(),
            cap: MAX_RECORD_BYTES,
        });
    }
    postcard::from_bytes(bytes).map_err(|error| PersistenceError::Codec(error.to_string()))
}

/// Decode one versioned runtime snapshot through the same path used by both
/// persistence backends.
pub fn decode_runtime_snapshot(bytes: &[u8]) -> crate::Result<RuntimeSnapshotV1> {
    postcard::from_bytes(bytes).map_err(|error| PersistenceError::Codec(error.to_string()))
}
