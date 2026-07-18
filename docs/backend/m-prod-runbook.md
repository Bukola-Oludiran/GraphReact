# M-Prod — Operator runbook

**Status:** CORE OPERATIONS CLOSE-OUT 2026-07-18. Release verification, graceful drain, Unix SIGHUP rotation, runtime-secret handling, and first-response incident triage are documented. External release credentials, long-running chaos fixtures, Windows service control, and post-M-Prod platform integrations remain environment/roadmap owned.

This document is the production operator's index. Each sub-track owns one runbook section; the design docs in this directory own the "why we built it this way" narrative. When in doubt, the runbook is the source of truth for what the operator does; the design docs are the source of truth for what the substrate guarantees.

---

## Verify a release (sub-track #7 — supply chain)

Operators verifying a tagged `grx` release follow this flow. The contract under proof: every byte of the binary the operator runs is traceable back to a public, verifiable source.

1. **Download the SBOM artifact for the tag.**

   The `cyclonedx-sbom` CI job uploads `sbom-cyclonedx-<tag>` containing one `<crate>.cdx.json` per workspace member. Per-tag artifacts are stable + dereferenceable:

   ```sh
   # Latest CI run for the tag
   gh run list --workflow=ci.yml --branch=v0.1.0 --limit=1

   # Download the SBOM
   gh run download <run-id> --name sbom-cyclonedx-v0.1.0
   ```

   Each emitted `*.cdx.json` validates against the [CycloneDX 1.4 JSON schema](https://cyclonedx.org/docs/1.4/json/) — the `cyclonedx-sbom` job runs a `jq -e '.bomFormat == "CycloneDX" and .specVersion == "1.4"'` check inline before uploading, so a malformed file would have failed the CI run.

2. **Confirm `grx --version` advertises the signature URL.**

   Release builds set `GRX_SIGNATURE_URL` at build time (sub-track #8 release pipeline injects it). The CLI bakes the URL into the binary via `crates/graphreact-cli/build.rs`'s `cargo:rustc-env=GRX_SIGNATURE_URL=<url>` directive and renders it in `grx version`:

   ```
   $ grx version
   grx 0.1.0
   parser  0.1.0
   ...
   engine-cpython cpython-0.1.0-m13-phase3h (...)
   signature: https://github.com/<org>/<repo>/releases/download/v0.1.0/grx.cosign.bundle
   ```

   If the `signature:` line is missing, the binary was NOT produced by the release pipeline — STOP and audit the provenance.

3. **Re-verify via `cosign verify-blob`.**

   The signature URL from step 2 points at the cosign bundle alongside the binary on the GitHub Release. With cosign installed:

   ```sh
   # Download the binary AND the keyless bundle
   curl -L -O https://.../grx
   curl -L -O https://.../grx.cosign.bundle

   # Verify (cosign keyless flow — uses Fulcio + Rekor)
   cosign verify-blob \
     --bundle grx.cosign.bundle \
     --certificate-identity-regexp 'https://github.com/<org>/<repo>/.*' \
     --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
     grx
   ```

   The release workflow creates this bundle with keyless OIDC signing and attaches it to the GitHub Release. The protected release environment and repository OIDC permissions must be configured before running the tag workflow.

4. **Confirm the binary matches CI's reproducible-build output.**

   The `reproducible-build` CI job twice-builds `target/release/grx` on a single runner and uploads both SHA-256 hashes as `reproducible-build-sha256-<tag>`. Operators re-run the local build under the same Phase A constraints and compare:

   ```sh
   gh run download <run-id> --name reproducible-build-sha256-v0.1.0
   cat grx-sha256-1.txt grx-sha256-2.txt
   # CI's twice-build hashes — both should match each other AND…
   sha256sum /path/to/downloaded/grx
   # …the operator's locally-fetched binary.
   ```

   **Phase A scope:** twice-build on the SAME runner only. Cross-host reproducibility (different runner OS / libpython libdir / cargo home) is sub-track #7 Phase B — see flip-to-hard-gate trigger in [`m-prod-supply-chain.md`](m-prod-supply-chain.md). Today's contract: CI's two builds match each other; cross-host divergence is expected and gated.

5. **Audit the SBOM for advisories.**

   The `trivy-sbom-scan` CI job runs Trivy in `scan-type: sbom` mode (severity HIGH+CRITICAL) over the per-crate CycloneDX outputs and uploads SARIF to the GitHub Security tab. Operators inspect:

   - **GitHub Security tab** — filtered to `category: trivy-sbom-scan` — shows current advisories scoped to this binary's dep closure.
   - **`trivy-sarif-<tag>` workflow artifact** — the raw SARIF for offline analysis / ingest into the operator's own vulnerability tracker.

   The CycloneDX SBOM itself can also be re-scanned with the operator's tool of choice (Grype, Dependency-Track, Snyk) for a second-opinion signal.

6. **Build the OCI image locally (optional).**

   The release.yml `docker-build` matrix attaches signed `oci-image-{amd64,arm64}-<tag>.tar` artifacts. The separately gated GHCR job also publishes a signed multi-arch manifest when `PUSH_TO_GHCR=true` and the production release environment is approved.

   ```sh
   # Mirror the release.yml docker-build inputs
   SOURCE_DATE_EPOCH=$(git log -1 --pretty=%ct)
   docker buildx build \
     --platform=linux/amd64 \
     --build-arg GRX_PYTHON_MINOR=3.13 \
     --build-arg SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH \
     --build-arg GRX_SIGNATURE_URL=https://github.com/graphreact/graphreact/releases/download/v0.1.0/grx.cosign.bundle \
     -t grx:v0.1.0 .

   # Inspect the embedded signature URL
   docker run --rm grx:v0.1.0 version
   ```

   Cross-link: [`m-prod-packaging.md`](m-prod-packaging.md) documents the Dockerfile stage layout (zig-builder → rust-builder → python-donor → distroless final) and the per-CPython-minor ABI lock.

7. **Install the helm chart (optional).**

   `charts/grx/` is shipped in the source tree (Phase A — Phase B publishes to a chart repo). Operators install directly from the checkout:

   ```sh
   helm install grx ./charts/grx \
     --namespace grx \
     --create-namespace \
     --set image.repository=<your-registry>/grx \
     --set image.tag=v0.1.0

   # Wait for the readiness probe to flip green
   kubectl --namespace grx rollout status deployment/grx

   # Probe the observer endpoints (drain-aware /v1/readyz)
   kubectl --namespace grx port-forward svc/grx 9090:9090 &
   curl http://localhost:9090/v1/readyz
   ```

   Default values match SHIPPED code: `terminationGracePeriodSeconds=30` = `drainDeadlineSecs=20` + `observerShutdownDeadline=5` + 5s slack (matches the budget math in [`m-prod-lifecycle.md`](m-prod-lifecycle.md)). The Deployment runs as the distroless `nonroot` UID 65532 with `readOnlyRootFilesystem=true` and emptyDir mounts at `/tmp` + `/home/nonroot`.

   Cross-link: [`m-prod-packaging.md`](m-prod-packaging.md) documents every knob in `values.yaml`.

---

## Drain runbook (sub-track #4 — lifecycle)

1. Confirm the configured budget satisfies `drain deadline + observer shutdown deadline + 5s slack ≤ terminationGracePeriodSeconds`. Defaults are `20 + 5 + 5 = 30s`.
2. Send Unix `SIGTERM` to the daemon (`kill -TERM <pid>`). Kubernetes does this automatically during pod termination. There is no operator REST drain endpoint yet.
3. Confirm `/v1/readyz` changes to HTTP 503 with body `draining`. New ingress receives the canonical 503 JSON envelope; calls admitted before the signal may finish until the drain deadline.
4. During the drain, the daemon closes cluster handshake/delegated-restart admission, cancels outstanding delegated-restart results as `TimedOut`, and bounds admitted observer calls by the same deadline.
5. Inspect `graphreact::observer::audit` for exactly one `drain_started`, followed by either `drain_completed` or `drain_deadline_exceeded`. A deadline event includes the remaining ingress and pending-call counts.
6. If the process survives beyond the pod grace period, capture the audit tail and metrics before escalating. Repeated deadline events mean the configured budget or a handler/bridge is unhealthy; do not solve them by silently increasing SIGKILL delay.

Cross-link: [`m-prod-lifecycle.md`](m-prod-lifecycle.md) owns exact state transitions and [`m-prod-observability.md`](m-prod-observability.md) owns readiness semantics.

---

## TLS rotation runbook (sub-track #2 — TLS)

1. Start the daemon with absolute `--observer-cert` and `--observer-key` paths. Relative paths are accepted only for compatibility and emit `tls_paths_relative`.
2. Wait for `grx-sighup-ready <pid>` before sending the first signal. Firing SIGHUP earlier can hit the operating system's default action.
3. Write the replacement certificate/key to temporary files, validate the pair, then atomically rename them over the configured paths.
4. Send `kill -HUP <pid>`. The shared SIGHUP fan-out rotates the audit log/spool and reloads runtime secrets, tokens, JWKS, and TLS for whichever surfaces are configured.
5. Expect `/v1/readyz` to report `tls_rotation_in_progress` only during the guarded swap. Confirm `tls_reload_success`; on `tls_reload_failed`, the previous `ServerConfig` remains active.
6. Do not rotate during drain. SIGHUP is skipped with `tls_reload_skipped reason=draining` once SIGTERM drain has begun.

This is the observer HTTPS rotation path. ACME/cert-manager automation, session-ticket rotation, dist-cluster mTLS rotation, and Windows signaling are post-M-Prod work. See [`m-prod-tls-rotation.md`](m-prod-tls-rotation.md).

---

## Secrets rotation runbook (sub-track #3 — secrets)

1. Declare every guest-visible secret in the component's committed `needs_secret` manifest. Undeclared reads are denied even when the backing store contains the name.
2. Select `GRX_RUNTIME_SECRET_BACKEND=env|keyring` before daemon start (`env` is the runtime default). Provision values as `GRX_SECRET_<NAME>` or with `grx secrets set <NAME> --backend keyring`.
3. Send SIGHUP after changing a reloadable source. The daemon atomic-swaps the runtime store before token/JWKS/TLS reload. Reads already executing finish against their snapshot; subsequent reads see the replacement store.
4. Environment variables of an already-running process cannot normally be changed by editing its parent shell or Kubernetes Secret. For env-backed value rotation, roll the pod/process unless the deployment mechanism actually mutates the daemon's environment.
5. Watch `secret_read_allow` / `secret_read_deny` audit rows and `grx_secret_read_failures_total{reason}`. Names and node identity are recorded; values never are.
6. Treat `undeclared` as a manifest/configuration defect, `missing` as provisioning drift, `backend_error` as keyring/service health, and `non_utf8` as a guest-ABI mismatch.

See [`m-prod-secrets.md`](m-prod-secrets.md) and [ADR-0007](../decisions/0007-ctx-secret-runtime-accessor.md).

---

## Incident response (cross-cutting)

Use `/v1/healthz` to answer “is the process alive?” and `/v1/readyz` to answer “should traffic route here?”. For a readiness failure:

| Reason | First response |
|---|---|
| `draining` | Expected after SIGTERM; correlate `drain_started` with completion/deadline audit rows. |
| `audit_sink_failed` | De-route, repair storage/permissions, run `grx audit verify <PATH>`, and restart only after accounting for the ledger gap. |
| `tls_rotation_in_progress` | Wait briefly; if persistent, inspect `tls_reload_*` and the cert/key files. |
| `scheduler_first_batch_pending` | Inspect mount/runtime errors; the daemon has not completed its first consistent scheduler round. |
| `bridge_saturated` | Inspect scheduler batch latency and bridge capacity; retrying aggressively worsens saturation. |
| `cluster_no_peers_connected` | Inspect mTLS/cookie/compatibility handshake logs and peer reachability. |

For a suspected auth incident, preserve the hash-chained audit files before rotation and verify them read-only. For a supply-chain alert, stop promotion, verify the binary/OCI digest and provenance bundle, then rescan the exact SBOM artifact. For a fuzz or chaos crash, retain the corpus input, daemon audit tail, graph fixture, wire versions, and engine mix so the failure is reproducible.

---

## Cross-track contracts

- **Sub-track #7 (this doc's Verify a release flow):** the SBOM + signature URL + repro-build hashes form a closed verification triangle. Drop any one of them and the operator can't fully audit a release.
- **Sub-track #8 (packaging):** the shipped workflow injects the `grx.cosign.bundle` URL into both reproducibility builds and OCI builds, signs/attests the binary and per-arch tars, and provides job-scoped OIDC permissions. GHCR publication remains protected-environment gated.
- **Sub-track #5 (observability) + #9 (chaos+fuzz):** readiness, Prometheus, and audit signals drive the first-response table above. Long-running fuzz/chaos cells remain CI-infrastructure owned and should feed reproducible corpus/fixture evidence back into this runbook.
