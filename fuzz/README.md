# `fuzz/` — GraphReact fuzz substrate (M-Prod sub-track #9)

**Status:** management-API and Phase A persistence-decoder targets shipped;
chaos arms + 24h corpus / 7-day soak / cert+cookie rotation soak deferred to
CI-infra (see [`../docs/backend/m-prod-chaos-fuzz.md`](../docs/backend/m-prod-chaos-fuzz.md)).

## What's in here

This directory is an out-of-workspace cargo-fuzz crate. The reasons it is *not* a member of the top-level `[workspace] members` list are written into [`Cargo.toml`](Cargo.toml); the short version is `libfuzzer-sys` requires nightly + ASan flags that would pollute the shared build cache for every other workspace member.

```
fuzz/
  Cargo.toml                        # standalone workspace; deps libfuzzer-sys + path-dep on graphreact-observer
  fuzz_targets/
    fuzz_management_api_bodies.rs   # InjectMessageBody + CallMessageBody + BulkLineBody
    fuzz_persistence_decoders.rs    # framed logs + record/snapshot postcard decoders
  corpus/
    fuzz_management_api_bodies/
      seed_*.json                   # hand-written seed inputs (committed; libFuzzer mutator output ignored)
  artifacts/                        # libFuzzer crash / leak / slow-input artifacts (gitignored)
  .gitignore                        # excludes target/ + Cargo.lock + corpus mutations + artifacts/
```

## Running locally

```sh
# One-time: install cargo-fuzz (requires nightly toolchain).
cargo install cargo-fuzz

# Run the management-API body fuzz target for 10 minutes (matches
# the CI lane budget at .github/workflows/ci.yml :: fuzz).
cd fuzz
cargo +nightly fuzz run fuzz_management_api_bodies -- -max_total_time=600

# Exercise framed-log integrity and record/snapshot decoding.
cargo +nightly fuzz run fuzz_persistence_decoders -- -max_total_time=600

# Or run forever until a crash / SIGINT.
cargo +nightly fuzz run fuzz_management_api_bodies

# Re-run a specific crash artifact discovered by CI / a previous
# local run. Artifacts land under `artifacts/<target>/crash-*`.
cargo +nightly fuzz run fuzz_management_api_bodies \
  artifacts/fuzz_management_api_bodies/crash-<hash>
```

If you don't have nightly:

```sh
rustup toolchain install nightly
rustup component add llvm-tools-preview --toolchain nightly
```

## What this target fuzzes

`fuzz_management_api_bodies` exercises the JSON deserialisation path used by the three HTTP-ingress entry points in the management API:

| Endpoint | Body shape | Re-exported as |
|---|---|---|
| `POST /v1/nodes/:id/messages` | `InjectMessageBody` | `graphreact_observer::InjectMessageBody` |
| `POST /v1/nodes/:id/call` | `CallMessageBody` | `graphreact_observer::CallMessageBody` |
| `POST /v1/nodes/:id/messages/bulk` (per NDJSON line) | `BulkLineBody` | via `graphreact_observer::parse_bulk_line` |

The target drives `serde_json::from_slice::<T>(data)` for each — the same call the axum `Json<T>` extractor (and the bulk handler's per-line parser) makes under the hood. No axum / tokio runtime is spun up per iteration, which keeps libFuzzer exec/s healthy.

## Kill-gate semantics

`serde_json::from_slice` returning `Err(_)` is **expected** for malformed input — that is NOT a fuzz failure. A fuzz failure is:

- a panic / abort / unwind out of `from_slice` itself,
- an ASan use-after-free / heap-overflow (cargo-fuzz enables AddressSanitizer by default),
- a libFuzzer-reported OOM (default `-rss_limit_mb=2048`),
- a libFuzzer-reported timeout (default 1200s per input).

Successful `Ok(_)` parses are accepted silently. This target is a panic / OOM hunter, not a property test. Round-trip / equivalence variants are tracked under `m-prod-chaos-fuzz.md`.

`fuzz_persistence_decoders` drives arbitrary bytes through the production
framed-log decoder and the shared Postcard record/snapshot decoders. It also
wraps each arbitrary payload in a valid production frame, ensuring mutations
reach checksum-verified payload parsing instead of stalling at the magic-byte
gate.

## Seed corpus

`corpus/fuzz_management_api_bodies/seed_*` is the hand-written seed corpus. The mutator-generated corpus (lots of tiny binary files libFuzzer writes during a run) is gitignored — only the seeds are version-controlled. Bootstrapping from the seeds gives a 10-min CI run honest coverage instead of finding shallow bugs only.

## CI

The `fuzz` job in [`../.github/workflows/ci.yml`](../.github/workflows/ci.yml) runs this target for 10 minutes on every push to `main` (not on PRs — the run is too slow). On failure it uploads `fuzz/artifacts/` as a workflow artifact named `fuzz-artifacts-<sha>` for triage. The job is marked `continue-on-error: true` for its first ship — the substrate is new and the first CI runs need to surface honest signal before blocking PRs.
