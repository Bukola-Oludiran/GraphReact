# graphreact-graspologic

*Graspologic-equivalent statistical graph analysis — Leiden partitioning, spectral embeddings (ASE/LSE/OMNI/MASE/CASE), GMM clustering, hypothesis tests, graph matching, vertex nomination.*

## Milestone

Ships as the statistical-analysis surface for **M8** (`partition` — Leiden, modularity), **M9** (`embed` — ASE/LSE/OMNI/MASE/CASE + Procrustes + dimension selection), and **M10** (`cluster` GMM, `inference` latent-position / latent-distribution, `match_` Hungarian + FAQ, `nominate` spectral VN). These three milestones are tracked in the [backend docs index](../../docs/backend/README.md) and the [workspace onboarding crate map](../../docs/onboarding.md); a dedicated narrative per milestone has not yet been carved out — the rustdoc per module currently serves that role.

## Role in the runtime

This crate sits in the *substrate* tier alongside [`graphreact-core`](../graphreact-core/) and [`graphreact-linalg`](../graphreact-linalg/), one rung below the engine and distribution layers. It is a pure-Rust port of the Python `graspologic` library, giving GraphReact a native statistical graph analysis surface without an out-of-process Python dependency. Engines and observers reach it transitively — the only direct downstream is [`graphreact-napi`](../graphreact-napi/), which exposes the embedding / partitioning / matching APIs to JavaScript bodies via N-API bindings. No engine (`graphreact-jsc-engine`, `graphreact-cpython-engine`) takes a build-time dependency on this crate; statistical analysis is an opt-in capability surfaced over Node-API.

## Key types

- **`fit_gmm` / `GmmModel`** — EM-fitted Gaussian mixture with `Full`/`Diag`/`Spherical`/`Tied` covariance variants ([`src/cluster/gmm.rs:83`](src/cluster/gmm.rs#L83)); `predict_proba` returns the `(n × k)` responsibility matrix.
- **`adjacency_spectral_embed` / `Embedding`** — ASE entry point and embedding container ([`src/embed/ase.rs:49`](src/embed/ase.rs#L49), [`src/embed/ase.rs:57`](src/embed/ase.rs#L57)).
- **`OmniEmbedding` / `MaseEmbedding` / `CaseEmbedding`** — multi-graph joint spectral embeddings (omnibus, multiple-ASE, covariate-assisted) at [`src/embed/omni.rs:50`](src/embed/omni.rs#L50), [`src/embed/mase.rs:43`](src/embed/mase.rs#L43), [`src/embed/case.rs:68`](src/embed/case.rs#L68).
- **`orthogonal_procrustes`** — Procrustes alignment of two embeddings via SVD ([`src/embed/procrustes.rs:23`](src/embed/procrustes.rs#L23)).
- **`select_dimension`** — Zhu & Ghodsi profile-likelihood elbow detection over a singular-value spectrum ([`src/embed/select.rs:37`](src/embed/select.rs#L37)).

## Composes with

| Crate | Role |
|---|---|
| [`graphreact-core`](../graphreact-core/) | Scheduler / `Message` / reactive substrate — consumed by the `reactive_consumer` integration test to validate memoization under signal change. |
| [`graphreact-linalg`](../graphreact-linalg/) | Pure-Rust SVD / eigendecomposition / GEMM on `faer` — the load-bearing numerical kernel under every embedding and Procrustes call. |
| [`graphreact-napi`](../graphreact-napi/) (downstream) | N-API bridge that re-exports embeddings + partitioning + matching to JS bodies. |

## Running the tests

```sh
cargo test -p graphreact-graspologic --release
```

101 unit tests in-crate plus 1 integration test (`tests/reactive_consumer.rs`) covering scheduler-driven ASE memoization — 102 total at the time this README was carved.

## See also

- [Workspace onboarding](../../docs/onboarding.md) — crate map + reading order; tags this crate under `M8 / M9 / M10`.
- [Backend docs index](../../docs/backend/README.md) — links the milestone log; M8/M9/M10 do not yet have standalone narrative pages.
- [Decision 0002 — GraphReact is backend-only](../../docs/decisions/0002-graphreact-is-backend-only.md) — scopes why a native statistical-analysis surface (vs an out-of-process Python dependency) belongs in the Rust workspace.
- [Decision 0003 — GraphReact bifurcates into Backend and DB](../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md) — both products share this substrate.
