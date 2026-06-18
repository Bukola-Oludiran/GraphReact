# M-Prod — Operator runbook (draft scaffold)

**Status:** DRAFT scaffold — fills in as M-Prod sub-tracks land. Group 2 batch (2026-06-17) ships the "verify a release" section below. Group 4 batch (2026-06-17) extends it with steps 6 (Dockerfile build) and 7 (helm chart install) for the sub-track #8 Phase A packaging surface; other sections remain placeholders cross-linked to the owning sub-track's design doc.

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
   signature: https://github.com/<org>/<repo>/releases/download/v0.1.0/grx.sig
   ```

   If the `signature:` line is missing, the binary was NOT produced by the release pipeline — STOP and audit the provenance.

3. **Re-verify via `cosign verify-blob`.**

   The signature URL from step 2 points at the cosign `.sig` file alongside the binary on the GitHub Release. With cosign installed:

   ```sh
   # Download the binary AND the signature
   curl -L -O https://.../grx
   curl -L -O https://.../grx.sig

   # Verify (cosign keyless flow — uses Fulcio + Rekor)
   cosign verify-blob \
     --signature grx.sig \
     --certificate-identity-regexp 'https://github.com/<org>/<repo>/.*' \
     --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
     grx
   ```

   > **NOTE:** cosign + signing pipeline lands with sub-track #8. The runbook documents the CONTRACT today so operators have the verification flow locked in before the pipeline ships.

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

6. **Build the OCI image locally (optional — Phase A scaffold).**

   The release.yml `docker-build` matrix attaches `oci-image-{amd64,arm64}-<tag>.tar` to the GitHub Release (Phase A — Phase B replaces these with ghcr.io references once the keyless-signing push lane lands). Operators reproducing the image locally:

   ```sh
   # Mirror the release.yml docker-build inputs
   SOURCE_DATE_EPOCH=$(git log -1 --pretty=%ct)
   docker buildx build \
     --platform=linux/amd64 \
     --build-arg GRX_PYTHON_MINOR=3.13 \
     --build-arg SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH \
     --build-arg GRX_SIGNATURE_URL=https://github.com/graphreact/graphreact/releases/download/v0.1.0/grx.sig \
     -t grx:v0.1.0 .

   # Inspect the embedded signature URL
   docker run --rm grx:v0.1.0 version
   ```

   Cross-link: [`m-prod-packaging.md`](m-prod-packaging.md) documents the Dockerfile stage layout (zig-builder → rust-builder → python-donor → distroless final) and the per-CPython-minor ABI lock.

7. **Install the helm chart (optional — Phase A scaffold).**

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

> **PLACEHOLDER** — fills in when sub-track #4 lands.
>
> Cross-link: [`m-prod-observability.md`](m-prod-observability.md) (current `/v1/readyz` substrate); sub-track #4 owning doc (TBD).

---

## TLS rotation runbook (sub-track #2 — TLS)

> **PLACEHOLDER** — fills in when sub-track #2 lands.
>
> Cross-link: sub-track #2 owning doc (TBD).

---

## Secrets rotation runbook (sub-track #3 — secrets)

> **PLACEHOLDER** — fills in when sub-track #3 graduates from Phase A.
>
> Cross-link: [`m-prod-secrets.md`](m-prod-secrets.md) (Phase A — `grx secrets` CLI + OS keyring backend shipped 2026-06-17).

---

## Incident response (cross-cutting)

> **PLACEHOLDER** — fills in once the per-sub-track operator surfaces stabilise (auth incident, fuzz crash, supply-chain CVE, SBOM divergence, etc.).
>
> Each sub-track will own a "what to do when this signal fires" sub-section here, cross-linked to the substrate's source doc.

---

## Cross-track contracts

- **Sub-track #7 (this doc's Verify a release flow):** the SBOM + signature URL + repro-build hashes form a closed verification triangle. Drop any one of them and the operator can't fully audit a release.
- **Sub-track #8 (packaging, not yet started):** owns the release pipeline that injects `GRX_SIGNATURE_URL` at build time, runs `cosign sign`, attaches the `.sig` to the GitHub Release, and provides the `id-token: write` permission for keyless signing. Until #8 ships, the verification flow above runs against synthetic / dev-build inputs.
- **Sub-track #5 (observability) + #9 (chaos+fuzz):** the runbook's incident-response section depends on the observability surface (`/v1/readyz`, Prometheus metrics, OTel traces) and the fuzz-discovered failure modes both being stable; placeholder until both reach v1.
