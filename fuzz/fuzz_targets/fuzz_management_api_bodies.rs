//! **M-Prod sub-track #9 (chaos + fuzz) — first fuzz target.**
//!
//! Fuzzes the three JSON ingress body shapes the management API
//! deserialises on the hot path:
//!
//! - `POST /v1/nodes/:id/messages`        → `InjectMessageBody`
//! - `POST /v1/nodes/:id/call`            → `CallMessageBody`
//! - `POST /v1/nodes/:id/messages/bulk`   → `BulkLineBody`
//!   (one per NDJSON line, via `parse_bulk_line`)
//!
//! The fuzz body feeds libFuzzer's `&[u8]` input directly into
//! `serde_json::from_slice::<T>` for each of the three types. That
//! is the same path the axum `Json<T>` extractor walks — by
//! targeting the deserialiser directly we avoid spinning up an
//! axum runtime per fuzz iteration (libFuzzer expects ~10k+ exec/s
//! for shallow targets; bringing up tokio + axum per iter would
//! drop to ~10 exec/s and the corpus would never converge).
//!
//! **Kill-gate semantics.** `serde_json::from_slice` is expected
//! to return `Err(_)` for malformed input; that is NOT a fuzz
//! failure. A fuzz failure is:
//!
//! - a panic / abort / unwind out of `from_slice` itself,
//! - an AddressSanitizer use-after-free / heap-overflow / OOM
//!   (cargo-fuzz defaults to ASan; `-rss_limit_mb=2048` catches
//!   unbounded-allocation bugs that would otherwise wedge CI),
//! - any libFuzzer-detected timeout (default 1200s per input;
//!   typical inputs finish in <10ms — a timeout means a
//!   deserialiser CPU bomb).
//!
//! Successful `Ok(_)` parses are accepted silently; this target is
//! a panic / OOM hunter, not a property test. (Property-test
//! variants — e.g. round-trip stability — are a follow-up slice
//! tracked under `docs/backend/m-prod-chaos-fuzz.md`.)

#![no_main]

use libfuzzer_sys::fuzz_target;

use graphreact_observer::{parse_bulk_line, CallMessageBody, InjectMessageBody};

fuzz_target!(|data: &[u8]| {
    // Drive all three ingress body deserialisers off the same
    // libFuzzer input. Coverage-guided fuzzing surfaces inputs
    // good for any of the three shapes, so a single target with
    // three calls is strictly better than three separate
    // single-shape targets at the same total CPU budget.

    // (1) InjectMessageBody — POST /v1/nodes/:id/messages.
    //     Two required fields (`type`) + a default-on-missing
    //     `payload`. Tolerates any JSON Value in payload.
    let _ = serde_json::from_slice::<InjectMessageBody>(data);

    // (2) CallMessageBody — POST /v1/nodes/:id/call.
    //     Required `type` + `reply_type`; optional `payload`,
    //     `timeout_ms` (the runtime clamps timeout_ms at
    //     MAX_CALL_TIMEOUT_MS at handler entry — fuzz tests the
    //     parse, not the clamp).
    let _ = serde_json::from_slice::<CallMessageBody>(data);

    // (3) BulkLineBody — one NDJSON line of
    //     POST /v1/nodes/:id/messages/bulk. Required `type`,
    //     optional `payload` + `idempotency_key`. Goes through
    //     the public `parse_bulk_line` shim so the wire-format
    //     surface stays narrow.
    let _ = parse_bulk_line(data);
});
