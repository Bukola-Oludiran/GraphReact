# M-Prod — Packaging (sub-track #8)

**Status:** PHASE A SHIPPED 2026-06-17 (Dockerfile + multi-stage distroless image + release.yml scaffold + charts/grx helm chart + packaging/homebrew/grx.rb formula scaffold). Tracks M-Prod sub-track #8 as defined in the master plan.

This document lives alongside [`m-prod-supply-chain.md`](m-prod-supply-chain.md) (which Phase A links into for the SBOM + signature URL contract) and [`m-prod-lifecycle.md`](m-prod-lifecycle.md) (which Phase A's helm chart consumes via `terminationGracePeriodSeconds`).

## Sub-track #8 scope recap

The M-Prod plan lists these packaging deliverables:

1. **OCI Dockerfile** — multi-stage distroless image, per-CPython-minor ABI slices.
2. **release.yml** — tag-triggered release pipeline (reproducibility-recheck, SBOM pin, multi-arch image build, GitHub Release publish).
3. **Helm chart** (`charts/grx/`) — production-shaped Deployment + Service + ConfigMap with drain-budget-tuned `terminationGracePeriodSeconds`, distinct readiness vs liveness probes, default `runAsNonRoot` + `readOnlyRootFilesystem` security posture.
4. **Homebrew formula** (`packaging/homebrew/grx.rb`) — source-build recipe; Phase B submits to homebrew-tap, Phase C polishes for homebrew-core.
5. **Sigstore-keyless signing** + **SLSA build provenance attestation** + **ghcr.io push** (Phase B — deferred to follow-on slice once OIDC trust + ghcr.io repo trust is configured).
6. **macOS notarization** + **Windows MSI** (Phase C — out of scope for this slice).
7. **K8s operator** (Phase D — out of scope for this slice; ships once the multi-replica cluster substrate matures past the M14 baseline).

Phase A lands items #1–#4; items #5–#7 are tracked under [Open work in #8](#open-work-in-8).

## Shipped — Dockerfile

The `Dockerfile` at the repo root is a four-stage build:

| Stage | Base | Output |
|---|---|---|
| `zig-builder` | `rust:1.83-bookworm` + zig 0.16.0 + `python3.13-dev` | `zig/bridge/zig-out/lib/libgraphreact_*_zig.a` (frozen between Rust builds) |
| `rust-builder` | `rust:1.83-bookworm` + `python3.13-dev` | `target/release/grx` (linked against pkg-config python-3.13-embed) |
| `python-donor` | `python:3.13-slim-bookworm` | `libpython3.13.so.1.0` + `/usr/local/lib/python3.13` stdlib |
| `final` | `gcr.io/distroless/cc-debian12:nonroot` | the runtime image (~120MB) |

**Why distroless final + python donor:** `gcr.io/distroless/cc-debian12` ships glibc + libgcc_s + `/etc/ssl/certs/ca-certificates.crt` (everything `rustls-native-certs` needs) without a shell or package manager. But it has neither `libpython3.X.so.1.0` nor the `python3.X/` stdlib — both of which the cpython engine requires (the engine `build.rs` panics if the bridge `.a` is missing AND the absolute LC_RPATH baked by `crates/graphreact-cli/build.rs` must resolve `libpython` at runtime; Python's `Py_InitializeEx` further requires the stdlib at `/usr/local/lib/pythonX.Y`). We `COPY` both from the donor stage so the final image preserves the M-Prod attack-surface posture without rebuilding CPython from source.

**Per-CPython-minor ABI lock:** the cpython engine `build.rs` panics at link time on Python <3.12, and the engine emits `cargo:rustc-link-lib=dylib=python3.X` — bumping the minor is a per-image build. The `GRX_PYTHON_MINOR` build-arg (default 3.13) drives per-minor slices (`grx-py-3.12`, `grx-py-3.13`, `grx-py-3.14`); 3.14t (PEP 703 free-threaded) waits for `actions/setup-python` to ship free-threaded support.

**Reproducibility plumbing:** the `rust-builder` stage mirrors the `reproducible-build` CI cell at `.github/workflows/ci.yml` lines 367 + 383 — `SOURCE_DATE_EPOCH` passed as a build-arg + `RUSTFLAGS=--remap-path-prefix=/work=. --remap-path-prefix=/usr/local/cargo=/cargo`. Phase A scope: same-builder reproducibility only. Cross-host reproducibility is Phase B (see flip-to-hard-gate trigger in [`m-prod-supply-chain.md`](m-prod-supply-chain.md)).

**`GRX_SIGNATURE_URL` threading:** Phase A passes this as a build-arg into the `rust-builder` stage; release.yml's `docker-build` matrix sets it to `https://github.com/${REPO}/releases/download/${TAG}/grx.sig`. When Phase B ships the signing pipeline, that URL becomes operator-resolvable.

**Multi-arch:** Phase A relies on QEMU emulation via `docker buildx build --platform=linux/amd64,linux/arm64`. Native arm64 cross-compile needs an xtask `-Dtarget` knob (Phase B) — the slow `linux/arm64` emulation lane is acceptable for Phase A's once-per-tag cadence.

The `.dockerignore` trims the build context to the Rust source tree + zig source — without it the `COPY . .` in the zig-builder stage ships ~3-4GB of `target/` to the daemon. The exclusion list mirrors `.gitignore` plus `docs/`, `scripts/`, `.git/`, `.github/`, `fuzz/`, all `node_modules/`.

## Shipped — release.yml

`.github/workflows/release.yml` fires on `v*` tag push (same trigger as `ci.yml`). Four jobs:

| Job | Purpose | Gate posture |
|---|---|---|
| `reproducibility-recheck` | Twice-builds `target/release/grx` on a single runner, captures SHAs, hard-fails on divergence | **HARD GATE** (unlike ci.yml's `continue-on-error: true` repro cell) |
| `sbom-pin` | Polls the ci.yml run for the matching SHA's `sbom-cyclonedx-<tag>` artifact (15min budget), downloads it, generates a `SHA256SUMS.txt` manifest, re-uploads under `release-sbom-<tag>` | HARD (`exit 1` if ci.yml doesn't publish) |
| `docker-build` (`{amd64, arm64}` matrix) | `docker buildx build` per arch via QEMU, OUTPUT to local tar (NO push) | Soft (Phase A scaffold) |
| `publish-release` | `gh release create` attaches binary + SBOM bundle + OCI tars + the canonical `GRX_SIGNATURE_URL` placeholder | HARD |

**Defense against fork-tag-push:** every push job is gated by `if: github.repository == 'graphreact/graphreact'` so a fork pushing a `v*` tag does NOT run the release pipeline. Phase B widens this to also require `environment: production-release` (forces a maintainer-approval gate before any push runs); the environment must be configured in repo settings before Phase B's signing pipeline lands.

**ghcr.io push gating:** Phase A produces docker-load-able tars; Phase B flips to `push: true` once these three conditions are met:

1. `vars.PUSH_TO_GHCR == true` repo variable.
2. `environment: production-release` configured with a reviewer-approval gate.
3. Workflow-level `permissions: id-token: write` added for Sigstore-keyless signing (the only sub-track #8 deliverable that needs workflow-level permission widening — the existing `permissions: { security-events: write, contents: read }` in the Trivy cell stays job-scoped).

**Untrusted-input hygiene:** every shell step that touches `github.ref_name` / `github.repository` / `github.sha` threads the value through `env:` and quoted shell expansion. Tag refs are git-validated (refname(7) refuses shell metacharacters), but the env: posture is defense-in-depth against a future trigger broadening (`workflow_dispatch` with arbitrary inputs, `repository_dispatch.client_payload.*`).

## Shipped — charts/grx helm chart

| File | Role |
|---|---|
| `charts/grx/Chart.yaml` | apiVersion v2, version 0.1.0, appVersion 0.1.0 |
| `charts/grx/values.yaml` | All knobs documented inline; defaults match SHIPPED code |
| `charts/grx/templates/deployment.yaml` | `terminationGracePeriodSeconds` + probe split + emptyDir mounts |
| `charts/grx/templates/service.yaml` | ClusterIP on the observer port |
| `charts/grx/templates/configmap.yaml` | Effective runtime config; rollout trigger via `checksum/config` |
| `charts/grx/templates/serviceaccount.yaml` | Per-release SA when `serviceAccount.create=true` |
| `charts/grx/templates/_helpers.tpl` | Standard `fullname` / `labels` / `selectorLabels` helpers |
| `charts/grx/templates/NOTES.txt` | Post-install operator hints (curl /v1/readyz; kubectl logs) |
| `charts/grx/.helmignore` | VCS / editor cruft exclusion |

**Lifecycle budget math (canonical, m-prod-lifecycle.md):**

```
drainDeadlineSecs + observerShutdownDeadline + slack ≤ terminationGracePeriodSeconds
        20s      +              5s            +  5s  =          30s ✓
```

**Phase A vs orchestration-spec divergence:** the M-Prod #8 orchestration brief proposed `terminationGracePeriodSeconds: 35` (drain 25 + OTel 5 + slack 5). Raising the drain default from 20s to 25s requires a code change in `crates/graphreact-cli/src/commands.rs::DEFAULT_DRAIN_DEADLINE_SECS` AND a coupled doc update to the [`m-prod-lifecycle.md` budget math table](m-prod-lifecycle.md). That cross-track surgery is **out of scope for this packaging slice** (constraint: "DO NOT modify any crates/* code beyond what's required by the workspace-layout / docs changes"). Phase A ships the matching-current-code values (30 / 20 / 5 / 5). Phase B re-evaluates: either raise the CLI default in tandem with the helm default OR document the operator-facing override path.

**Probe split (canonical, m-prod-observability.md):**

| Probe | Path | Period | Tolerance | Auth |
|---|---|---|---|---|
| Liveness | `/v1/healthz` | 10s | failureThreshold 3 (= 30s detection) | bypassed |
| Readiness | `/v1/readyz` | 2s | failureThreshold 3 (= 6s deroute) | bypassed |

`/v1/readyz` is drain-aware: returns 503 `draining` ahead of `Retry-After: 1` during SIGTERM (sub-track #4 Phase α wiring).

**Security posture:** `podSecurityContext` + `containerSecurityContext` ship `runAsNonRoot: true`, `readOnlyRootFilesystem: true`, `allowPrivilegeEscalation: false`, `capabilities.drop: [ALL]`, `seccompProfile.type: RuntimeDefault`. The distroless image's `nonroot` UID 65532 matches. `readOnlyRootFilesystem: true` requires explicit emptyDir mounts at `/tmp` and `/home/nonroot`; the deployment template wires both.

**Replicas:** Phase A defaults to `replicas: 1`. The M14 distribution layer is intra-cluster-aware but the helm chart does NOT wire pod-to-pod gossip in Phase A — multi-replica clustering substrate (StatefulSet + headless Service + DNS-SRV discovery) lands Phase B.

## Shipped — Homebrew formula scaffold

`packaging/homebrew/grx.rb` is a build-instructions reference. Phase A scope: it documents the source-build recipe — `bottle :unneeded` (Rust-formula convention for early-tap phase), `depends_on "rust" => :build`, `depends_on "zig" => :build`, `depends_on "pkg-config" => :build`, `depends_on "python@3.13"` (runtime). Mirrors the CI repro cell's `SOURCE_DATE_EPOCH` + path-remap so `brew install --build-from-source` produces the same audit hash as the GitHub Release.

Phase B submits to a `graphreact/homebrew-tap` repo (with the `bottle` block populated by a macOS-{13,14,15} runner matrix). Phase C polishes for homebrew-core ingest (license review, dep-graph audit, naming review).

**Per-CPython-minor formula version:** the formula pins `python@3.13`. Bumping the Python pin is a deliberate formula-version bump — mirrors the per-CPython-minor image slices the Dockerfile documents.

## Cross-track contracts locked here

- **Sub-track #7 (supply chain):** the release pipeline owns the "where does the SBOM go" decision — Phase A pins the ci.yml SBOM artifact to the GitHub Release; Phase B adds the cosign `.sig` upload alongside (the `GRX_SIGNATURE_URL` baked into the binary by `crates/graphreact-cli/build.rs` already points at the Phase B URL). Cross-link: [`m-prod-supply-chain.md`](m-prod-supply-chain.md).
- **Sub-track #4 (lifecycle):** the helm chart's `terminationGracePeriodSeconds` IS the operator-facing k8s contract for the drain budget. Bumping either the CLI's `DEFAULT_DRAIN_DEADLINE_SECS` or the `OBSERVER_SHUTDOWN_DEADLINE` requires a coupled bump in `charts/grx/values.yaml` AND a coupled update to the `m-prod-lifecycle.md` budget math table. Cross-link: [`m-prod-lifecycle.md`](m-prod-lifecycle.md).
- **Sub-track #5 (observability):** the probes block in `charts/grx/values.yaml` was pasted verbatim from m-prod-observability.md's "k8s configuration" anchor — that doc explicitly says "paste into the helm chart when sub-track #8 lands". When the predicate set for `/v1/readyz` evolves (e.g. sub-track #2 TLS rotation adds a `tls_reload_pending` reason), the probe documentation in BOTH docs MUST stay in sync. Cross-link: [`m-prod-observability.md`](m-prod-observability.md).
- **Sub-track #1 (auth / audit):** `observer.audit.logPath` in `values.yaml` is the operator surface for the hash-chained audit log sink. When sub-track #1 graduates from Phase A (multi-token RBAC + batched-fsync + SIGHUP atomic-swap rotation) the helm chart MUST expose the new knobs. Cross-link: [`m-prod-auth.md`](m-prod-auth.md).

## Open work in #8

| Item | Phase | Blocker |
|---|---|---|
| Sigstore-keyless signing of binary + OCI image | B | OIDC trust setup + `vars.PUSH_TO_GHCR` repo variable + `environment: production-release` approval gate |
| SLSA build provenance attestation (`actions/attest-build-provenance@v1`) | B | Pairs with Sigstore-keyless — same blockers |
| ghcr.io push of multi-arch images | B | Same blockers |
| Cross-host reproducible-build (zig-build threads `SOURCE_DATE_EPOCH` + `--remap-path-prefix`) | B | xtask `-Dsource-date-epoch` knob, replacing absolute LC_RPATH with `@rpath`-style lookup |
| Native arm64 cross-compile (xtask `-Dtarget`) | B | xtask cross-compile substrate; multi-arch QEMU is acceptable for Phase A |
| StatefulSet substrate for multi-replica clustering | B | requires M14 distribution layer to grow a DNS-SRV discovery hook |
| `terminationGracePeriodSeconds=35` adoption (drain 25s default) | B | requires CLI change to `DEFAULT_DRAIN_DEADLINE_SECS` + coupled m-prod-lifecycle.md budget math update |
| Homebrew tap repo (`graphreact/homebrew-tap`) with bottle pipeline (macOS-13/14/15 runners) | B | bottle build infrastructure |
| Homebrew-core submission | C | tap-phase stability + license review |
| macOS notarization | C | Apple Developer ID + notarization-via-altool pipeline |
| Windows MSI | C | Windows build cell (currently excluded from ci.yml's test matrix) |
| K8s operator (CRD + reconcile loop for grx clusters) | D | requires multi-replica clustering substrate (Phase B item above) |

## Verifying the chart locally

Operators / contributors can sanity-check the chart before installing:

```sh
# Lint (catches templating + values schema bugs)
helm lint charts/grx

# Render to YAML without applying (catch indentation / missing-helper bugs)
helm template grx charts/grx --namespace grx-test

# Dry-run install against a cluster (catches admission-controller rejections)
helm install grx charts/grx --namespace grx-test --create-namespace --dry-run
```

## Verifying the Dockerfile locally

```sh
# Build (uses BuildKit; QEMU optional for cross-arch)
docker buildx build --platform=linux/amd64 -t grx:dev .

# Run --help (sanity)
docker run --rm grx:dev

# Run the daemon with a port-forwarded observer
docker run --rm -p 9090:9090 grx:dev cluster daemon --observer-bind ":9090"
curl http://localhost:9090/v1/readyz
```
