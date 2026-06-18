# M-Prod sub-track #9 — Chaos + fuzz (safest slice)

**Status:** STARTED 2026-06-17 with the safest slice (libFuzzer target over the three ingress JSON body shapes + 10-min push-to-main CI cell). Tracks M-Prod sub-track #9 as defined in the [master plan](../../../../.claude/plans/graphreact-rust-later-milestones.md).

Lives alongside [`m-prod-observability.md`](m-prod-observability.md), [`m-prod-supply-chain.md`](m-prod-supply-chain.md), and [`m-prod-secrets.md`](m-prod-secrets.md). Full chaos coverage (partition-storm + cookie / cert / crash rotation soak + 7-day cumulative-corpus fuzz) is deferred to the CI-infra track — see [Open work](#open-work).

## Sub-track #9 scope recap

The M-Prod plan lists these chaos + fuzz deliverables:

1. **Continuous fuzzing** of the management-API JSON schemas via `cargo-fuzz`.
2. **24h CI corpus run** — accumulate a corpus over time, surface new inputs as CI artifacts.
3. **7-day soak run** under chaos injection: partition-storm, cookie rotation, cert rotation, crash-restart simulations.
4. **Reload-under-load** 7-day soak — `grx reload` against a running cluster while ingress is hot.

The **safest slice** lands item #1 (this commit). Items #2–#4 require CI infrastructure that doesn't ship in this repo today (long-running CI cells, 3-node cluster fixtures, OTel collector fixtures, Loki / promtool fixtures) — all routed to the CI-infra track per the M-Prod remainder plan.

## Shipped — `fuzz_management_api_bodies` libFuzzer target

### Substrate

New `fuzz/` directory at the repo root (NOT a workspace member — see [Why fuzz/ is its own workspace](#why-fuzz-is-its-own-workspace)).

| File | Role |
|---|---|
| [`fuzz/Cargo.toml`](../../fuzz/Cargo.toml) | Standalone-workspace manifest; pinned `libfuzzer-sys`; depends on workspace crates via `path = "../crates/…"` |
| [`fuzz/fuzz_targets/fuzz_management_api_bodies.rs`](../../fuzz/fuzz_targets/fuzz_management_api_bodies.rs) | `fuzz_target!` over `&[u8]` driving three `serde_json::from_slice::<T>` calls per iteration (one per ingress shape) |
| [`fuzz/corpus/fuzz_management_api_bodies/`](../../fuzz/corpus/fuzz_management_api_bodies/) | 16 hand-written seed corpus files covering each variant of payload (minimal, with-payload, unicode, deep-nesting, escapes, numeric-edge, unknown-fields, idempotency-key) |
| [`fuzz/README.md`](../../fuzz/README.md) | How to run locally (`cargo +nightly fuzz run fuzz_management_api_bodies`) |
| [`fuzz/.gitignore`](../../fuzz/.gitignore) | Ignores libFuzzer's runtime state (artifacts/, corpus/$other/, target/) |

### Target shape

The fuzz body feeds libFuzzer's `&[u8]` input directly into three deserialiser calls, mirroring what the live axum `Json<T>` extractor walks on each ingress POST:

```rust
fuzz_target!(|data: &[u8]| {
    // POST /v1/nodes/:id/messages       → InjectMessageBody
    let _: Result<InjectMessageBody, _> = serde_json::from_slice(data);
    // POST /v1/nodes/:id/call           → CallMessageBody
    let _: Result<CallMessageBody, _> = serde_json::from_slice(data);
    // POST /v1/nodes/:id/messages/bulk  → BulkLineBody (per-line)
    let _ = parse_bulk_line(data);
});
```

Coverage-guided fuzzing surfaces inputs good for any of the three shapes, so the single multi-call target dominates three separate single-shape targets at the same total CPU budget.

### Kill-gate semantics

`serde_json::from_slice` returning `Err(_)` on malformed input is **NOT** a fuzz failure (the deserialiser is supposed to refuse garbage). The job fails on:

- A panic / abort / unwind out of `from_slice` itself.
- An AddressSanitizer trap — `cargo-fuzz` enables ASan by default. `-rss_limit_mb=2048` caps per-iteration memory so unbounded-allocation bugs surface fast instead of OOMing the runner.
- A libFuzzer-detected timeout (default 1200s per input; typical input finishes in <10 ms — a per-input timeout means a deserialiser CPU bomb).

Successful `Ok(_)` parses pass silently. This target is a panic / OOM hunter, not a property test. Round-trip-stability and other property-test variants are tracked under [Open work](#open-work).

### CI cell (`.github/workflows/ci.yml::fuzz-management-api-bodies`)

| Knob | Value | Why |
|---|---|---|
| Trigger | `if: github.event_name == 'push'` | PRs would pay the 12–15-min nightly + libFuzzer cost on every commit; push-to-main + tags is the right cadence for a kill-gate fuzzer that surfaces real bugs over time, not per-PR feedback. |
| Toolchain | `dtolnay/rust-toolchain@nightly` pinned to `nightly-2026-06-01` | cargo-fuzz needs nightly (private libFuzzer compiler intrinsics + `-Z sanitizer=address`); pinning a specific date keeps a bad nightly from reddening the lane on unrelated pushes. |
| Cache | `Swatinem/rust-cache@v2` with `key: fuzz` + `workspaces: ".\nfuzz"` | Separate cache so the ASan-instrumented target dir doesn't bust the stable build cache of test / clippy / sbom. |
| `cargo-fuzz` install | `taiki-e/install-action@v2` with `tool: cargo-fuzz@0.12` | Prebuilt binary; mirrors the SBOM cell's pattern. Pinned version → upstream regressions surface as deliberate PR review, not silent CI drift. |
| Build-only sanity step | `cargo +nightly fuzz build fuzz_management_api_bodies` | Proves the fuzz substrate compiles before burning 10 min on a misconfiguration. |
| Runtime | `cargo +nightly fuzz run fuzz_management_api_bodies -- -max_total_time=600 -rss_limit_mb=2048` | 10-min wall-clock cap (soft; in-flight inputs finish so real wall-clock is ~600-650s). |
| `continue-on-error` | `true` | First-ship policy: the fuzzer is new substrate; failures surface as build statuses but don't gate downstream pipelines. Flip to `false` once a few weeks of stable green runs prove the substrate isn't flaking. |
| Artifact upload | `actions/upload-artifact@v4` on `if: failure()` only; uploads `fuzz/artifacts/` + `fuzz/corpus/` | Failure inputs feed downstream replay; success-run corpus growth is a 24h-cumulative-corpus CI-infra-track follow-on. |

### Cost budget

| Phase | Cost (cold cache) | Cost (warm) |
|---|---|---|
| Checkout + nightly toolchain install | ~30 s | ~10 s |
| `cargo-fuzz` install via taiki-e | ~10 s | ~5 s |
| `cargo fuzz build` (cold) | 2–3 min | ~15 s |
| `cargo fuzz run -max_total_time=600` | 10 min (soft cap) | 10 min |
| Artifact upload on failure | ≤30 s | ≤30 s |
| **Total** | **~13–15 min** | **~10.5 min** |

Well within typical GitHub-hosted runner ceilings; the lane runs in parallel with `fmt` / `clippy` / `test` / `deny` / `sbom` so it doesn't gate the critical path.

## Why `fuzz/` is its own workspace

`fuzz/Cargo.toml` carries `[workspace]` (empty table) to declare itself a standalone workspace. It is intentionally NOT in the root `[workspace] members` list. Three load-bearing reasons:

1. **libfuzzer-sys requires nightly Rust.** Uses `-Z sanitizer=address` style flags + private compiler intrinsics for the libFuzzer runtime. Every workspace member must build under the workspace toolchain (`rust-toolchain.toml` = stable); dragging libfuzzer into workspace members would force every `cargo check --workspace` invocation to require nightly.
2. **cargo-fuzz pollutes the build cache.** Injects `RUSTFLAGS='-Cpasses=sancov-module …'` which would bust the shared build cache for non-fuzz workspace members and reddener every `clippy` / `test` / `sbom` lane.
3. **Upstream convention.** `cargo-fuzz` upstream documents this pattern explicitly: "cargo-fuzz puts its files in a fuzz/ subdirectory and is meant to be used at the workspace root, NOT as a workspace member."

Workspace crates are referenced via `path = "../crates/…"` — the fuzz targets call the same `serde_json::from_slice::<T>` deser path the axum `Json<T>` extractor walks at the live HTTP boundary, so bugs found here ARE bugs the real ingress would hit.

## Exported helpers in `graphreact-observer`

The fuzz target lives outside `graphreact-observer`, so the body shapes had to surface a stable public API:

| Symbol | What | Why public |
|---|---|---|
| `InjectMessageBody` | Already `pub` in M-Ingress phase 1 | Used by ingress integration tests + now fuzz |
| `CallMessageBody` | Already `pub` in M-Ingress phase 2 | Same |
| `BulkLineBody` | Promoted from file-private to `pub` (sub-track #9 / chaos+fuzz) | Per-line bulk request shape; the live bulk handler parses it via `serde_json::from_slice`; the fuzz target needs the same surface |
| `parse_bulk_line(bytes: &[u8]) -> Result<BulkLineBody, serde_json::Error>` | New `pub` helper around `serde_json::from_slice::<BulkLineBody>` | Stable seam that future fuzz targets / replay tools depend on without inspecting `BulkLineBody`'s field shape (which is an HTTP wire-format detail) |

## Open work

Everything below depends on the CI-infra track (long-running cells, 3-node cluster fixtures, OTel-collector docker fixtures, Loki / promtool fixtures, OIDC IdP fixtures). Tracked here so the slice doesn't lose scope:

- **24h cumulative-corpus CI cell.** Today's lane re-seeds from `fuzz/corpus/fuzz_management_api_bodies/` on every run (no cross-run state). The 24h variant pulls the prior run's corpus from artifact storage, runs for 24h, pushes the grown corpus back. Surfaces drift in the schema's "interesting input" surface over time.
- **7-day soak under chaos.** Steady fuzz traffic + concurrent chaos arms (partition-storm, cookie rotation, cert rotation, crash-restart). The GREEN gate is `metrics endpoints unchanged + zero corruption + zero panic` across the 7-day window.
- **Partition-storm chaos arm.** Programmatic tcpkill / iptables-NFLOG on the cluster transport; needs a 3-node cluster fixture that the CI lane can drive.
- **Cookie / cert rotation chaos arms.** Depends on M-Prod sub-track #2 SIGHUP atomic-swap landing; until then the rotation primitive doesn't exist to exercise.
- **Crash-restart chaos arm.** SIGKILL `grx cluster daemon` mid-run; restart; assert state survives + ingress recovers. Needs sub-track #4 SIGTERM drain semantics to compose with the restart cycle without false-positiving on "graceful drain detected as crash."
- **Reload-under-load soak.** `grx reload` against the daemon while ingress is hot. The remote-reload wire protocol itself is a sub-track #4 deliverable; this soak rides on top of it.
- **Round-trip property tests.** A separate fuzz target (`fuzz_serialize_round_trip`) that asserts `from_slice(to_vec(x))` round-trips for every successful parse. Catches asymmetry between the serialiser + deserialiser. Lighter-weight than the panic-hunter target and could run on PRs.
- **`continue-on-error: true` flip.** Once a few weeks of stable green runs prove the substrate isn't flaking, flip the lane to a hard gate.

## Cross-track contracts locked here

- **Sub-track #2 (TLS / SIGHUP rotation):** Cert / cookie rotation chaos arms compose on top of #2's atomic-swap primitive. #9's fixture shape is locked at "drive the rotation primitive once per N seconds and assert no panic"; the primitive itself is owned by #2.
- **Sub-track #4 (SIGTERM drain + remote reload):** Crash-restart + reload-under-load chaos arms compose on top of #4's drain semantics + remote-reload wire. Same shape: #9 drives, #4 owns the surface.
- **Sub-track #6 (rate limiting):** Once #6 lands `tower_governor`, the fuzz target's per-iteration request rate may need to back off so the rate-limiter doesn't 429 every input. Concrete API: a `--no-rate-limit` daemon flag or an env-var bypass for the chaos+fuzz lane.
- **Sub-track #8 (packaging):** The fuzz target runs against the in-repo binary; once #8 ships an OCI image, a parallel "fuzz against the packaged binary" cell can validate the release artifact (not just the source). Out of scope for the safest slice.
- **`deny.toml`:** Unchanged — `libfuzzer-sys` is MIT/Apache-2.0 and lives in the standalone `fuzz/` workspace, NOT the main workspace, so it doesn't widen the workspace's license / advisory closure.

## Cross-references

- [`m-prod-observability.md`](m-prod-observability.md) — sibling M-Prod sub-track narrative.
- [`m-prod-supply-chain.md`](m-prod-supply-chain.md) — SBOM CI cell that the fuzz cell's `taiki-e/install-action@v2` + cache-key pattern mirrors.
- [`m-prod-secrets.md`](m-prod-secrets.md) — sibling Group 1 batch.
- [`m16-management-api.md`](m16-management-api.md) — the ingress surface the fuzz target probes.
- [`fuzz/README.md`](../../fuzz/README.md) — how to run the fuzz target locally.
