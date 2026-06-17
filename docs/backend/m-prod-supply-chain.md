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

**Pinning:** `cargo install cargo-cyclonedx --version 0.5.9 --locked` — version pinned so SBOM schema is stable across CI runs; bump via deliberate PR rather than tracking latest. 0.5.9 was the latest release as of 2026-06-17 (Apache-2.0).

**Not a workspace dependency:** `cargo-cyclonedx` is installed at CI time, not added to `[workspace.dev-dependencies]`. Three reasons:

1. It is a CI tool, not a runtime / test dependency.
2. Keeping it out of workspace deps means `deny.toml`'s license allowlist need not be updated for the cyclonedx transitive closure.
3. Future Sigstore-keyless signing (sub-track #7 v2) will sign the artifact path here without mutating any source-level Cargo manifest.

**Artifact retention:** 90 days (GitHub default). Once the M-Prod #8 release pipeline lands the SBOM will additionally upload to the GitHub Release for each tag, with cosign signature alongside.

**Artifact shape:**

```
sbom-cyclonedx-<sha>/
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

## Open work in #7

- **Sigstore-keyless signing of release artifacts** — blocked on sub-track #8 (Dockerfile + release.yml + tag triggers). Once #8 ships, add a `cosign sign` step to the release pipeline + GitHub Actions `permissions: id-token: write`.
- **`grx --version` signature URL** — needs a `build.rs` env-var passthrough (`GRX_SIGNATURE_URL`) + one line in `run_version()` printing it when non-empty.
- **Reproducible-build determinism verifier** — twice-build + diff CI cell. `[profile.release]` already pins `codegen-units = 1`; needs `SOURCE_DATE_EPOCH` + `--remap-path-prefix` RUSTFLAGS injection + an actual diff cell on a different host / timestamp.
- **SLSA build provenance attestation** (`actions/attest-build-provenance@v1`) — pairs naturally with Sigstore-keyless. Same blocker: needs the release pipeline.
- **Per-arch SBOMs** when the multi-arch matrix lands (sub-track #8).
- **Vulnerability scanning** of the CycloneDX output (Grype / Trivy) — soft addition; not in the explicit acceptance gate but conventional pair.

## Verifying the SBOM locally

Operators / contributors can replicate CI's SBOM generation:

```sh
# One-time install (or upgrade)
cargo install cargo-cyclonedx --version 0.5.9 --locked

# Generate per-workspace-member SBOMs
cargo cyclonedx --format json --all

# Find them
find . -name '*.cdx.json' -not -path './target/*'
```

Each emitted `*.cdx.json` validates against the [CycloneDX 1.4 JSON schema](https://cyclonedx.org/docs/1.4/json/).

## Cross-track contracts locked here

- **Sub-track #8 (packaging):** the release pipeline owns the "where does the SBOM go" decision (CI artifact today, GitHub Release attachment + cosign signature tomorrow). #7 provides the SBOM-generating job + the future signature steps; #8 ships the release pipeline that invokes them.
- **`deny.toml`:** unchanged by this slice — `cargo-cyclonedx` is not a workspace dependency, so its transitive closure does not flow into the workspace license / advisory enforcement. Any future workspace dep added by #7 (e.g. an in-process SBOM generator) must clear the existing allowlist.
- **Cleanup-Pass gate:** per the master plan, M-Prod does not open until Cleanup-Pass closes. This slice is small enough to land as a pre-M-Prod scaffolding PR without violating that gate; the full sub-track waits.
