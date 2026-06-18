# M-Prod — Supply chain (sub-track #7)

**Status:** STARTED 2026-06-17 with the safest slice (CycloneDX SBOM in CI). Tracks M-Prod sub-track #7 as defined in the master plan.

This document lives alongside [`m-prod-observability.md`](m-prod-observability.md) and documents the M-Prod supply-chain hardening passes.

## Sub-track #7 scope recap

The M-Prod plan lists these supply-chain deliverables:

1. **CycloneDX SBOM** emitted on every release artifact build.
2. **Sigstore-keyless signing** of release artifacts via GitHub Actions OIDC (`cosign sign`).
3. **Reproducible builds** — byte-identical rebuild across host / timestamp; `cargo deny` enforcement floor (already shipped).
4. **`grx --version` advertises a verifiable signature URL** so operators can re-verify via `cosign verify-blob`.

The **safest slice** lands #1 (this commit). Items #2–#4 require the M-Prod #8 release pipeline (Dockerfile + multi-arch OCI + release.yml + tag triggers) to exist before they have actual release artifacts to sign or to point `grx --version` at. They're tracked under [Open work in #7](#open-work-in-7).

## Shipped — CycloneDX SBOM in CI

A new `sbom` job in [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) installs `cargo-cyclonedx` and generates a CycloneDX 1.4 JSON SBOM of every workspace member, uploaded as a CI artifact named `sbom-cyclonedx-<sha>`.

**Why CycloneDX, not SPDX:** the OWASP CycloneDX project ships first-class Rust support via `cargo-cyclonedx`, the schema is supply-chain-shaped (component → dependency → hash + license + reference), and the JSON format integrates directly with downstream vulnerability scanners (Grype / Trivy / Dependency-Track) without intermediate conversion. SPDX is equally valid as a standard but its Rust ecosystem support is thinner and the file shape is documentation-shaped rather than supply-chain-shaped.

**Pinning:** `taiki-e/install-action@v2` with `tool: cargo-cyclonedx@0.5.9` — prebuilt-binary install (keeps the SBOM CI cell under the <1min M-Prod #5 budget; `cargo install`-from-source is multi-minute even with a warm registry cache). Version pinned so the SBOM schema is stable across CI runs; bump via deliberate PR rather than tracking latest. 0.5.9 was the latest release as of 2026-06-17 (Apache-2.0).

**Schema version pinned:** `cargo cyclonedx --format json --spec-version 1.4 --all`. cargo-cyclonedx 0.5.x defaults to spec version 1.3 if `--spec-version` is omitted; explicitly pinning to 1.4 keeps the documented contract (this doc + downstream Grype / Trivy / Dependency-Track consumers configured for 1.4) from drifting away from the emitted artifact.

**CI validation gate:** every emitted `*.cdx.json` is passed through `jq -e '.bomFormat == "CycloneDX" and .specVersion == "1.4"'` before the upload-artifact step. Catches a future cargo-cyclonedx regression (malformed JSON, schema drift, missing bomFormat) that the upload-artifact step alone cannot detect — `if-no-files-found: error` only fires on empty output.

**Not a workspace dependency:** `cargo-cyclonedx` is installed at CI time, not added to `[workspace.dev-dependencies]`. Three reasons:

1. It is a CI tool, not a runtime / test dependency.
2. Keeping it out of workspace deps means `deny.toml`'s license allowlist need not be updated for the cyclonedx transitive closure.
3. Future Sigstore-keyless signing (sub-track #7 v2) will sign the artifact path here without mutating any source-level Cargo manifest.

**Artifact retention:** 90 days (GitHub default). Once the M-Prod #8 release pipeline lands the SBOM will additionally upload to the GitHub Release for each tag, with cosign signature alongside.

**Trigger:** `push` to `main` and any `v*` tag (configured via the workflow's top-level `on: push: tags: ['v*']`). Tag pushes ship under `sbom-cyclonedx-<tag>` (e.g. `sbom-cyclonedx-v0.1.0`); main pushes fall back to `sbom-cyclonedx-<sha>`. Per-tag artifacts are stable + dereferenceable for downstream consumers.

**Artifact shape:**

```
sbom-cyclonedx-<sha-or-tag>/
  graphreact-core.cdx.json
  graphreact-observer.cdx.json
  graphreact-cli.cdx.json
  ... (one per workspace member)
```

Each `<crate>.cdx.json` describes the dependency closure of that workspace member. Downstream consumers can either inspect per-crate SBOMs (e.g. only the `grx` binary's closure) or run a CycloneDX merge tool to produce a single workspace-rooted SBOM.

## What `cargo-deny` already enforces (the floor)

The existing `deny` CI job (EmbarkStudios/cargo-deny-action@v2) runs `check advisories licenses bans sources` on every push to main and every PR. This is the supply-chain floor sub-track #7 inherits:

- **Advisories:** RustSec advisory database — any unpatched advisory in our dep tree fails CI.
- **Licenses:** `deny.toml` allowlist — MIT, Apache-2.0, BSD-2/3, ISC, MPL-2.0, Zlib, Unicode-3.0, CC0-1.0, Apache-WITH-LLVM-exception, Unicode-DFS-2016.
- **Bans:** workspace-deps yanked = warn, wildcards = deny.
- **Sources:** unknown-registry / unknown-git = warn; allow-registry pinned to crates.io.

The CycloneDX SBOM rides on top of this: the SBOM describes the dep tree, `cargo-deny` enforces what's allowed in the dep tree.

## Shipped — Group 2 batch (2026-06-17)

### `grx version` advertises `GRX_SIGNATURE_URL`

`crates/graphreact-cli/build.rs` reads `GRX_SIGNATURE_URL` from the build environment and emits `cargo:rustc-env=GRX_SIGNATURE_URL=<value>` (empty string when unset). `run_version()` reads the baked value via `option_env!()` and renders a `signature: <URL>\n` line ONLY when the value is non-empty.

**Operator contract:**

- absent at build time ⇒ `grx version` omits the signature line entirely (every dev build, every CI build that isn't a tagged release).
- set at build time ⇒ `grx version` appends `signature: <URL>` after the cpython engine line; operators re-verify via `cosign verify-blob --signature <URL>.sig <URL>` (cosign + signing pipeline lands with sub-track #8 — the runbook documents the contract today).

The legacy version output shape (`grx <ver>`, `parser`, `engine-trait`, `engine-null`, `engine-echo`, the long jsc + cpython lines) is preserved bit-for-bit — the signature line is purely additive. Unit test `commands::tests::version_omits_signature_when_unset` locks the absent contract; `version_shape_preserved` locks the legacy prefixes against regression. The env-var-set path is exercised by the same test via `option_env!()` branching, and end-to-end by the release pipeline (sub-track #8) when it ships.

### Reproducible-build verifier CI cell

A new `reproducible-build` job in `.github/workflows/ci.yml` twice-builds `target/release/grx` on a single runner and compares the SHA-256 hashes.

**Knobs:**

- `runs-on: ubuntu-latest`, `if: github.event_name == 'push'` (push-to-main + tags only — PRs would pay the ~6-8 min twice-build cost), `continue-on-error: true` (first-ship posture; see flip-to-hard-gate trigger below).
- Toolchain: `dtolnay/rust-toolchain@stable` (NOT nightly — reproducibility is a stable-toolchain gate; same SHA across both builds in one job is guaranteed).
- `SOURCE_DATE_EPOCH`: set from `$(git log -1 --pretty=%ct)` (HEAD's committer timestamp), surfaced via `$GITHUB_ENV` so both build steps see the same value.
- `RUSTFLAGS`: `--remap-path-prefix=${{ github.workspace }}=. --remap-path-prefix=${{ env.HOME }}/.cargo=/cargo` to strip the runner's homedir from debuginfo.
- Zig static libs (`libgraphreact_jsc_zig.a`, `libgraphreact_cpython_zig.a`, `libgraphreact_echo_zig.a`) produced ONCE via `cargo xtask zig-build --release` and frozen between the two Rust builds (see "Phase A scope" below).
- Compare: `sha256sum target/release/grx` after each build → if mismatch, `::warning::` + non-zero exit (so `continue-on-error: true` marks the job failed-but-non-blocking instead of falsely green).
- Upload both `/tmp/grx-sha256-{1,2}.txt` as `reproducible-build-sha256-<sha-or-tag>` workflow artifact (90-day retention) for audit.

**Phase A scope (today):** twice-build OF THE RUST TARGET ONLY. `SOURCE_DATE_EPOCH` + `--remap-path-prefix` do NOT propagate through `cargo xtask zig-build` (those would need separate zig-build env plumbing), so the `.a` files are produced once and frozen between the two Rust builds. Cross-host reproducibility is also out of scope: `crates/graphreact-cli/build.rs` bakes an absolute LC_RPATH to the libpython libdir (Homebrew on macOS, system on Linux) which is host-stable within a runner but host-specific across runners.

**Phase B flip-to-hard-gate trigger:** drop `continue-on-error: true` AND flip the divergence path from `::warning::` to a hard fail once (a) the zig-build lane threads `SOURCE_DATE_EPOCH` / `--remap-path-prefix` through `zig build`, OR (b) the LC_RPATH bake is replaced with a relative `@rpath`-style lookup. Tracked alongside SLSA + per-arch SBOMs in the deferred-to-#8 list below.

### Trivy SBOM vulnerability scan (SARIF → GitHub Security tab)

A new `trivy-sbom-scan` job in `.github/workflows/ci.yml` depends on the existing `sbom` job (YAML key — display name is `cyclonedx-sbom`), downloads the SBOM artifact, runs Trivy in `trivy sbom <file>` mode per workspace-member CDX (severity HIGH+CRITICAL), merges the per-crate SARIF runs into a single SARIF document, and uploads it to the GitHub Security tab via `github/codeql-action/upload-sarif@v3`.

**Knobs:**

- `needs: sbom`, `if: github.event_name == 'push'`, `continue-on-error: true`.
- `permissions: { security-events: write, contents: read }` scoped to THIS JOB ONLY — no workflow-level `permissions:` block is added; every other job continues to run under the default GITHUB_TOKEN posture. Matches the GitHub-Actions-hardening doc-comment at the top of `ci.yml`.
- CLI pinning: `taiki-e/install-action@v2` with `tool: trivy@0.53.0` (prebuilt binary; mirrors the cargo-cyclonedx@0.5.9 + cargo-fuzz@0.12 precedent). Bump via deliberate PR. We use the Trivy CLI directly rather than `aquasecurity/trivy-action` because the action wraps a single `trivy sbom <file>` invocation, and we emit per-workspace-member CDX files that need a per-file loop.
- `trivy sbom --exit-code 0` per file + `continue-on-error: true` at the job level → Phase A is non-gating. SARIF is the operator-facing signal; Phase B flips both knobs to hard-fail once the CVE backlog is triaged.
- Per-crate SBOM scan: the CycloneDX cell emits one `<crate>.cdx.json` per workspace member; the loop runs `trivy sbom` once per file, writes a per-crate SARIF, then merges every SARIF's `runs[]` array via `jq -s` into one SARIF 2.1.0 document. Trivy itself does NOT accept a directory of SBOMs — the loop is load-bearing.
- Empty-corpus path: if `sbom/*.cdx.json` matches nothing, the step emits a minimal valid SARIF (`{"version":"2.1.0","runs":[]}`) so `codeql-action/upload-sarif` doesn't fail on a missing file.
- Persists raw SARIF as `trivy-sarif-<sha-or-tag>` workflow artifact (90-day retention) in addition to the Security-tab upload, so operators can grab it via `gh run download` without going through the API.

## Shipped — Phase B Part 1 (2026-06-18)

Joint #7 + #8 safe-slice: Sigstore-keyless signing of release artifacts + SLSA build provenance attestation + (sub-track #8) Dockerfile base-stage SHA pinning + ghcr.io push gating. All changes are YAML + Dockerfile + docs only — no `crates/*` touched.

### Sigstore-keyless signing

`.github/workflows/release.yml`'s `reproducibility-recheck` job now installs cosign via `sigstore/cosign-installer@v3.5.0` and signs `target/release/grx` keyless via `cosign sign-blob --yes --bundle target/release/grx.cosign.bundle`. Bundle format (cert + signature + Rekor inclusion proof in one file) — operator-friendly verify flow.

The same flow is mirrored at every load-bearing artifact boundary:

| Job | Signed artifact | Bundle output |
|---|---|---|
| `reproducibility-recheck` | `target/release/grx` | `target/release/grx.cosign.bundle` |
| `docker-build` (per matrix shard) | `/tmp/grx-oci-{amd64,arm64}.tar` | `/tmp/grx-oci-{amd64,arm64}.tar.cosign.bundle` |
| `ghcr-push` | `ghcr.io/<repo>/grx@<digest>` (registry-side image) | Registry-side OCI signature attached to the manifest digest |

`sigstore/cosign-installer@v3.5.0` minor pin: ships cosign 2.2+ (required for the keyless v3 flow). Same pin discipline as `cargo-cyclonedx@0.5.9`, `trivy@0.53.0`, `cargo-fuzz@0.12` in `ci.yml`. Bumps are deliberate PRs.

### SLSA build provenance

`actions/attest-build-provenance@v1` runs in three places: `reproducibility-recheck` (binary), `docker-build` matrix (per-arch tar), `ghcr-push` (registry manifest with `push-to-registry: true`). The action emits GitHub-attested provenance queryable via `gh attestation verify <artifact> --repo graphreact/graphreact`. Output `.intoto.jsonl` bundles are captured via the action's `bundle-path` output, `cp`'d next to each signed artifact, and attached to the GitHub Release alongside the binary + OCI tars so operators can verify without the `gh` CLI.

The `ghcr-push` job uses `push-to-registry: true` so the GitHub-side SLSA attestation lands in the registry alongside the image. This coexists with `docker/build-push-action@v6`'s own `provenance: true` (one is buildkit-shaped, one is GitHub-attestations-shaped); both are standard.

### Verify a release (updated flow)

Operators verify a binary's signature + provenance via:

```sh
# Sigstore-keyless verify of the binary
cosign verify-blob --bundle grx.cosign.bundle \
  --certificate-identity-regexp 'https://github\.com/graphreact/graphreact/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  grx

# SLSA build provenance (either path):
gh attestation verify grx --repo graphreact/graphreact
cosign verify-blob-attestation --bundle grx.intoto.jsonl \
  --certificate-identity-regexp 'https://github\.com/graphreact/graphreact/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --type slsaprovenance grx

# Per-OCI-tar: same flow with grx-oci-{amd64,arm64}.tar +
# grx-oci-{amd64,arm64}.tar.cosign.bundle + .intoto.jsonl.

# Registry-side image (ghcr.io) once the operator approves the
# production-release environment gate:
cosign verify ghcr.io/graphreact/graphreact/grx:<tag> \
  --certificate-identity-regexp 'https://github\.com/graphreact/graphreact/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

### Cross-track contracts

- `id-token: write` is wired at the JOB scope in three jobs: `reproducibility-recheck`, `docker-build` (matrix), `ghcr-push`. Workflow-level permissions deliberately remain absent (default-deny). Any future Phase B job that needs OIDC MUST declare its own scope.
- `attestations: write` is the new permission used by `actions/attest-build-provenance@v1` — same job-scope-only discipline.
- `packages: write` is consumed only by `ghcr-push` (docker login + `docker/build-push-action@v6` with `push: true`). Other jobs do NOT receive this permission.
- The `ghcr-push` job is structurally a separate gate from `publish-release`: the GH Release ships with the binary + signed OCI tars regardless of whether the operator has approved the `production-release` environment. The release notes' container-images table explicitly flags the ghcr.io row as gated so operators reading the release are not surprised by a pull-error before approving.

## Open work in #7

- ~~**`grx --version` signature URL**~~ — SHIPPED 2026-06-17 (Group 2 batch above).
- ~~**Reproducible-build determinism verifier**~~ — SHIPPED 2026-06-17 (Group 2 batch above; Phase A scope: same-runner Rust-only twice-build, see flip-to-hard-gate trigger).
- ~~**Vulnerability scanning of the CycloneDX output (Trivy)**~~ — SHIPPED 2026-06-17 (Group 2 batch above; Phase A non-gating, flip-to-hard-gate after CVE backlog triage).
- ~~**Sigstore-keyless signing of release artifacts**~~ — SHIPPED 2026-06-18 (Phase B Part 1 above; jointly closed with sub-track #8 — single `release.yml` carries both `cosign sign-blob` of the binary + per-arch OCI tars and `cosign sign` of the ghcr.io registry manifest).
- ~~**SLSA build provenance attestation** (`actions/attest-build-provenance@v1`)~~ — SHIPPED 2026-06-18 (Phase B Part 1 above; runs in three jobs — binary, per-arch OCI tar, ghcr.io registry manifest with `push-to-registry: true`).
- **Per-arch SBOMs** when the multi-arch matrix lands (sub-track #8). The `docker-build` matrix is multi-arch but currently consumes the single workspace-rooted SBOM emitted by ci.yml; per-arch SBOMs require a separate CycloneDX invocation per target. Deferred to the next Phase B slice.
- **Cross-host / Phase B reproducible build** — threading `SOURCE_DATE_EPOCH` + `--remap-path-prefix` through `cargo xtask zig-build`, replacing the absolute LC_RPATH bake with `@rpath`-style lookup. Flip-to-hard-gate trigger for the repro cell.

## Verifying the SBOM locally

Operators / contributors can replicate CI's SBOM generation:

```sh
# One-time install (or upgrade) — CI uses taiki-e/install-action@v2
# (prebuilt binary). Locally, cargo install is fine; --locked keeps
# the resolver matching CI.
cargo install cargo-cyclonedx --version 0.5.9 --locked

# Generate per-workspace-member SBOMs, pinned to CycloneDX 1.4
cargo cyclonedx --format json --spec-version 1.4 --all

# Find them
find . -name '*.cdx.json' -not -path './target/*'
```

Each emitted `*.cdx.json` validates against the [CycloneDX 1.4 JSON schema](https://cyclonedx.org/docs/1.4/json/).

## Cross-track contracts locked here

- **Sub-track #8 (packaging):** the release pipeline owns the "where does the SBOM go" decision (CI artifact today, GitHub Release attachment + cosign signature tomorrow). #7 provides the SBOM-generating job + the future signature steps; #8 ships the release pipeline that invokes them.
- **`deny.toml`:** unchanged by this slice — `cargo-cyclonedx` is not a workspace dependency, so its transitive closure does not flow into the workspace license / advisory enforcement. Any future workspace dep added by #7 (e.g. an in-process SBOM generator) must clear the existing allowlist.
- **Cleanup-Pass gate:** per the master plan, M-Prod does not open until Cleanup-Pass closes. This slice is small enough to land as a pre-M-Prod scaffolding PR without violating that gate; the full sub-track waits.
