# graphreact-linalg

*Linear algebra kernel — pure-Rust SVD, symmetric eigendecomposition, gemm, solve, row L2 normalization built on faer.*

## Milestone

Ships as part of **M8 substrate**. M8 doesn't have a dedicated milestone narrative; the substrate-tier framing is documented in [../../docs/architecture.md](../../docs/architecture.md) (this crate is named in the Substrate-tier table alongside `graphreact-core` and `graphreact-graspologic`), and the shared-substrate-across-products framing is pinned in [../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md](../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md).

## Role in the runtime

`graphreact-linalg` sits at the substrate tier with no upstream graphreact dependencies — it's the lowest-level numerical primitive in the workspace. Its sole consumer today is `graphreact-graspologic`, which builds spectral embeddings (ASE / LSE / OMNI / MASE / CASE), Leiden modularity passes, and GMM clustering on top of the five operations exposed here. The crate is a deliberately thin wrapper over [`faer`](https://docs.rs/faer): callers pass `faer::MatRef` / `Mat` and receive strongly-typed `SvdDecomp` / `EighDecomp` results, with no WASM boundary, no flat-array marshalling, and no numpy-shaped intermediate. Pure-Rust, `no_std`-friendly upstream, and engine-agnostic — the same kernel serves the Backend product runtime, the DB product analytics surface, and the NAPI / WASM foreign bindings without a per-consumer fork.

## Key types

- [`svd`](src/svd.rs) / `SvdDecomp` — truncated singular value decomposition; returns `U`, singular values, `V^T` in faer-typed matrices.
- [`eigh`](src/eigh.rs) / `EighDecomp` — symmetric eigendecomposition (largest-k); the spectral-embedding workhorse for graspologic.
- [`gemm`](src/gemm.rs) — general dense matrix multiply (`C = α·A·B + β·C`) over faer storage.
- [`solve`](src/solve.rs) — solve `Ax = b` for dense `A` via faer's factorization path.
- [`normalize_rows`](src/normalize.rs) — in-place L2 row normalization; the canonical pre-step before angle-based downstream embeddings.

## Composes with

| Crate | Direction | Role |
|---|---|---|
| [`faer`](https://docs.rs/faer) | depends on | Underlying pure-Rust dense linear algebra library — owns the `Mat` / `MatRef` types and the factorization routines this crate wraps. |
| [`graphreact-graspologic`](../graphreact-graspologic) | depends on this | Spectral embeddings, Leiden, GMM, hypothesis tests, graph matching, vertex nomination — every M8 / M9 / M10 analytics surface routes its numerical work through this crate. |

No `graphreact-*` upstream dependencies — this crate sits at the bottom of the substrate tier.

## Running the tests

```sh
cargo test -p graphreact-linalg --release
```

18 inline unit tests across the five modules (`svd`, `eigh`, `gemm`, `solve`, `normalize`) plus a Criterion bench at `benches/linalg.rs`.

## See also

- [../../docs/architecture.md](../../docs/architecture.md) — runtime architecture diagram; substrate-tier table names this crate alongside `graphreact-core` / `graphreact-graspologic`.
- [../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md](../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md) — product bifurcation; linalg is shared substrate consumed by both Backend and DB products.
- [../../docs/decisions/0002-graphreact-is-backend-only.md](../../docs/decisions/0002-graphreact-is-backend-only.md) — backend-tier framing; numerical kernels are reachable from CLI / protocol surfaces, never a frontend.
- [../../docs/onboarding.md](../../docs/onboarding.md) — workspace onboarding (crate tour + dev loop).
