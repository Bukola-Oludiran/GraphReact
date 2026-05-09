# M16-Backend P4 — HTTP framework decision

**Status:** decided 2026-05-07.
**Decision:** **axum 0.7** (`axum = { version = "0.7", default-features = false, features = ["http1", "json", "tokio", "matched-path"] }`).
**Alternatives considered:** bare `hyper 1.x` (P2's path), `actix-web 4`, `warp 0.3`, `poem 3`.

## Context

Phase 2 of M16-Backend shipped a single-route `/metrics` + `/healthz` HTTP server on bare `hyper 1.x` `service_fn` matched on `(method, path)`. Phase 4 expands that to a multi-route management API with auth middleware, JSON request/response serialization, path parameters, and a cross-thread bridge to the `!Send` `Scheduler`. The framework choice was deferred to P4 specifically so the route count + middleware + state-sharing requirements would be visible as evidence.

## Requirements

What P4+ needs:

1. **6+ routes today**, growing to 10+ with P5 (`/v1/nodes/{id}/messages/recent`) and P7 (`/v1/observer` WebSocket upgrade).
2. **Per-route auth middleware** — `/v1/healthz` bypasses auth; everything else requires a Bearer token.
3. **Path parameter extraction** — `/v1/nodes/{id}` needs `id` extracted as a typed `Path<String>`.
4. **JSON serialization** — every non-`/metrics` route returns `Json<T>` where `T: Serialize`.
5. **Cross-thread state sharing** — every handler needs `Arc<MetricsInner>` + `mpsc::Sender<SnapshotRequest>` + `Option<BearerToken>`. axum's `State<T>` extractor with `T: Clone` handles this.
6. **WebSocket upgrade in P7** — same router needs to support `/v1/observer` upgrading from HTTP/1.1 to a WebSocket frame stream.
7. **Compatibility with `tower` middleware** — P5+ may add request-rate limiting (`tower_governor` or hand-rolled), distributed tracing (`tower-http::trace`), and CORS (`tower-http::cors`) without a framework swap.

## Evaluation

### Bare `hyper 1.x` (P2's existing path)

**Pros:** zero-dep beyond what we already have. Hand-rolled `service_fn` matching `(method, path)` is what we shipped in P2.

**Cons:**
- Hand-rolled router for 10+ routes is ~150 LOC of `match` arms growing as routes are added; future-incompatible with the WebSocket upgrade in P7 (hyper's `upgrade::on()` is usable but every route handler needs to know about it).
- No path parameter extraction — a `match` against `req.uri().path().split('/')` works but moves complexity into every handler.
- No state injection — every handler has to thread `Arc<...>` through closures manually; auth becomes a boilerplate prefix on every handler.
- JSON serialization is hand-rolled `serde_json::to_string` + `Response::new(Full::new(...))` per handler — about 15 LOC per route.

**Estimated cost at P4 scope:** ~350 LOC of routing + auth + JSON plumbing for 6 routes.

### axum 0.7

**Pros:**
- `Router::new().route("/v1/path", get(handler))` is the canonical idiom — one line per route.
- `extract::Path<String>` for path params; `Json<T>` extractor for request bodies; `Json<T>` for typed responses.
- `State<T: Clone>` for shared application state — ApiState is `Arc`-clone-cheap.
- `middleware::from_fn_with_state` for auth — runs before the handler, can short-circuit with 401.
- WebSocket upgrade via `axum::extract::ws::WebSocketUpgrade` — first-class support, no per-handler hyper plumbing.
- `tower::Layer` integration — `tower-http::trace`, `tower-http::cors`, `tower_governor` all drop in as `.layer(...)` calls.
- Mature: the dominant Rust HTTP framework as of 2026-05; widespread documentation; large dependency footprint already proven.

**Cons:**
- Pulls `tower`, `tower-layer`, `tower-service`, `matchit`, `serde_path_to_error`, `sync_wrapper`, `axum-core`, `mime`, `percent-encoding` (most are tiny; total transitive impact ~12 crates we don't otherwise have).
- Pulls a `tracing` dep for its own `tracing` integration — already in our workspace, no net add.
- `default-features = false` mode requires explicit feature selection; we want `http1 + json + tokio + matched-path`.
- WebSocket support requires the `ws` feature in P7 — easy to add then.

**Estimated cost at P4 scope:** ~150 LOC of router + handlers + middleware for 6 routes.

### actix-web 4

**Pros:** mature, fast, full-featured.

**Cons:**
- Pulls `actix-runtime` which is its own actor system with a separate scheduler — incompatible with the existing tokio runtime that hosts dist + observer + (via NAPI) future engine integrations. Single-runtime substrate is a hard constraint.
- Larger transitive dep footprint than axum (~40 crates vs ~12).
- We don't get to share `tower` middleware across the dist crate's future surfaces.

### warp 0.3

**Pros:** filter-based composition, very light.

**Cons:**
- Filter-based routing has worse error messages for typed handler signatures than axum's extractor model — substrate-not-MVP needs operators to read clear errors.
- Maintenance: warp's release cadence has slowed; `axum` is now the de-facto Rust HTTP framework for new projects.
- WebSocket support via `warp::ws::ws()` is fine but again worse-documented than axum's path.

### poem 3

**Pros:** clean syntax, strong OpenAPI integration.

**Cons:**
- Smaller community + documentation surface than axum.
- OpenAPI integration is ahead-of-need — M16-Backend doesn't need OpenAPI generation in P4; companion ecosystems can derive schemas from the JSON shapes once stable.
- We'd be picking poem because of a feature (OpenAPI) we don't need today, paying maintenance cost for the smaller community in exchange.

## Decision: axum 0.7

axum wins on:

1. **LOC efficiency** — ~150 LOC vs hyper's ~350 LOC at P4 scope.
2. **Future-compatibility** — WebSocket upgrade for P7 is already supported; `tower-http` middleware drops in for P5+ rate limiting / CORS / distributed tracing.
3. **Single-runtime substrate** — actix-web's separate runtime is a hard no.
4. **Maturity** — axum is the canonical Rust HTTP framework in 2026; documentation + community + LTS-shaped releases.
5. **State-sharing model** — `State<T: Clone>` directly fits our `ApiState` shape.

The transitive dep cost (~12 small crates) is well-understood and within the substrate-not-MVP framing.

## Cross-thread bridge for `!Send` Scheduler — separate from framework choice

axum runs on multi-threaded tokio; `Scheduler` is `!Send` (holds `Rc<RefCell<...>>` interior mutability — see `crates/graphreact-core/src/runtime/scheduler.rs:13`). The framework choice doesn't affect this — every framework would face the same constraint.

The bridge ships in P4 as:

- **`mpsc::channel<SnapshotRequest>(SNAPSHOT_CHANNEL_CAPACITY=256)`** — bounded channel from axum handlers (any tokio worker thread) to the scheduler thread.
- **`oneshot::Sender<Response>` embedded in each `SnapshotRequest`** — handler awaits the response.
- **Scheduler-thread drain in `commands.rs::drive_handle_until`** — after each `process_batch` round, calls `drain_snapshot_requests(rx, |req| ...)`. The closure runs `scheduler.introspect_*()` on the scheduler thread + replies via the embedded oneshot.
- **Worst-case latency:** one process_batch round (~50ms tick cadence + drain). For active workloads where `process_batch` runs faster than 50ms, latency is bounded by the tokio scheduler's awakening of the drain task.

Backpressure: a 256-slot bounded channel + 503 Service Unavailable when full. Substrate-not-MVP — the rate-limit response is visible from day one.

## Reversibility

Re-evaluate axum if any of these become true:
- Transitive dep audit flags an unmaintained crate in the axum dependency tree (M-Prod #4 will catch this).
- Companion ecosystems request OpenAPI schemas — `utoipa` plugs into axum cleanly, no swap needed.
- A tower middleware we want is genuinely incompatible with axum's `Service` trait wiring (none currently anticipated).

A swap to a different framework would be ~150 LOC on the management_api side — bounded blast radius. The cross-thread bridge (`SnapshotRequest` enum + bridge channels) is framework-agnostic and stays.

## References

- `crates/graphreact-observer/src/management_api.rs` — the implementation.
- `~/.claude/plans/plan-the-implementation-of-nested-minsky.md` — Phase 4 brief.
- [axum 0.7 documentation](https://docs.rs/axum/0.7).
- [tower 0.5 documentation](https://docs.rs/tower/0.5).
