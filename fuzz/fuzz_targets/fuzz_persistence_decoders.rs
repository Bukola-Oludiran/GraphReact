#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let digest = graphreact_persistence::DeploymentDigest::of(b"fuzz-deployment");

    // Raw bytes exercise every framed-log header, length, checksum, and
    // postcard rejection path. Deriving the expected identity from the input
    // lets mutations progress beyond digest/sequence fencing when they happen
    // to contain a structurally plausible header.
    let expected_digest = data
        .get(10..42)
        .and_then(|bytes| bytes.try_into().ok())
        .map(graphreact_persistence::DeploymentDigest)
        .unwrap_or(digest);
    let expected_sequence = data
        .get(50..58)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_be_bytes)
        .unwrap_or(0);
    let expected_fencing_token = data
        .get(42..50)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_be_bytes)
        .unwrap_or(1);
    let _ = graphreact_persistence::decode_local_frame(
        data,
        expected_digest,
        expected_fencing_token,
        expected_sequence,
    );

    // A correctly framed arbitrary payload guarantees the mutator reaches the
    // production record decoder instead of spending the whole run rediscovering
    // magic bytes and SHA-256 checksums.
    if let Ok(frame) = graphreact_persistence::encode_local_frame(digest, 1, 0, data) {
        let _ = graphreact_persistence::decode_local_frame(&frame, digest, 1, 0);
    }

    let _ = graphreact_persistence::decode_runtime_record(data);
    let _ = graphreact_persistence::decode_runtime_snapshot(data);
});
