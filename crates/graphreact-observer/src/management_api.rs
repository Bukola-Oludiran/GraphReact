//! **M16-Backend Phase 4.** Management API on `axum 0.7`. Per-route
//! GET endpoints under `/v1/` versioning; bearer-token auth
//! middleware; cross-thread bridge for the `!Send` Scheduler.
//!
//! ## Why axum, not bare hyper
//!
//! P2 shipped a hyper-1.x `service_fn` matched on `(method, path)` —
//! correct for one route. P4 has 6+ routes (`/v1/healthz`,
//! `/v1/identity`, `/v1/metrics`, `/v1/nodes`, `/v1/nodes/{id}`,
//! `/v1/supervisor/tree`, plus future `/observer` WebSocket in P7),
//! a bearer-token auth middleware that needs to short-circuit
//! before handlers run, JSON serialization with axum's `Json<T>`
//! extractor, and `axum::extract::Path<String>` for path params.
//! Hand-rolled hyper would be ~300 LOC of routing + auth + JSON
//! plumbing; axum gives us the same in ~120 LOC. See
//! [`docs/backend/m16-p4-framework-decision.md`](../../../docs/backend/m16-p4-framework-decision.md).
//!
//! ## Cross-thread bridge for the `!Send` Scheduler
//!
//! The `Scheduler` is `!Send` (holds `Rc<RefCell<...>>` interior
//! mutability — see [scheduler.rs:13]). axum runs on multi-threaded
//! tokio. Each handler:
//!
//! 1. Constructs a [`SnapshotRequest`] enum carrying a
//!    `oneshot::Sender<SnapshotResponse>`.
//! 2. Sends it through the bridge `mpsc::Sender<SnapshotRequest>`.
//! 3. Awaits the oneshot for the response.
//!
//! The scheduler thread (the binary's main task running
//! `drive_handle_until`) drains the `mpsc::Receiver` after each
//! `process_batch` round, builds the response from scheduler state
//! on its own thread, and sends through the oneshot. Worst-case
//! latency: one `process_batch` round (~1-10ms typical).
//!
//! ## Auth
//!
//! Per-route middleware checks `Authorization: Bearer <token>`
//! against the configured [`crate::BearerToken`]. Unauthenticated
//! requests get 401 + `WWW-Authenticate: Bearer realm=...`.
//! `/v1/healthz` bypasses auth (liveness probes shouldn't ship
//! the bearer token to every load balancer); `/v1/metrics` is
//! auth-checked because metrics include high-cardinality labels
//! that may leak topology information.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_rustls::TlsAcceptor;
use tower_http::cors::{Any, CorsLayer};

use crate::auth::BearerToken;
use crate::bind::BindConfig;
use crate::metrics::MetricsInner;
use crate::prometheus::encode_text;
use crate::ring_buffer::{MessageSnapshot, RingBufferTelemetry, DEFAULT_RING_CAPACITY};
use graphreact_core::telemetry::{NodeIntrospection, SupervisorIntrospection};

/// Request types posted from axum handlers onto the cross-thread
/// bridge. The scheduler-thread drain loop matches on the variant,
/// builds the response from scheduler state, and sends through the
/// embedded `oneshot`.
#[derive(Debug)]
pub enum SnapshotRequest {
    /// `GET /v1/nodes` — list every registered node.
    Nodes(oneshot::Sender<Vec<NodeIntrospection>>),
    /// `GET /v1/nodes/{id}` — look up a single node by ID. `None`
    /// in the response = 404.
    NodeDetail(String, oneshot::Sender<Option<NodeIntrospection>>),
    /// `GET /v1/supervisor/tree` — supervisor + child mapping.
    SupervisorTree(oneshot::Sender<Vec<SupervisorIntrospection>>),
    /// **M-Ingress phase 1.** `POST /v1/nodes/:id/messages` — inject
    /// an externally-originated message into a node's inbox. The
    /// drain handler validates node existence, then routes via
    /// `SchedulerHandle::send` (which prepends `from = "__external__"`
    /// so `Scheduler::deliver` enforces the manifest's
    /// `accepts_external` allowlist). `IdempotencyConflict` is
    /// surfaced *before* this variant ever reaches the bridge —
    /// the handler's `IdempotencyCache` short-circuits at HTTP
    /// boundary so the scheduler thread is never bothered with a
    /// duplicate retry.
    InjectMessage {
        node_id: String,
        message_type: String,
        payload: Value,
        oneshot_tx: oneshot::Sender<InjectResult>,
    },
    /// **M-Ingress phase 3.** `POST /v1/nodes/:id/messages/bulk` —
    /// per-line dispatch from a streaming NDJSON request body.
    /// The bulk handler issues ONE bridge request per parsed
    /// line; the drain handler validates node existence + the
    /// manifest's `accepts_external` allowlist (same gate as
    /// `Scheduler::deliver` enforces at delivery time, hoisted
    /// to drain so the per-line response can surface
    /// `RejectedByManifest` synchronously). Distinct from the
    /// Phase-1 `InjectMessage` variant — that one preserves the
    /// "202 = queued, manifest decided at delivery" contract for
    /// single-shot inject. For bulk we want richer per-line
    /// feedback so application-class clients (NewsCaster
    /// firehose, ComplyFlow audit-log) can detect failures
    /// mid-stream.
    InjectMessageBulkLine {
        node_id: String,
        message_type: String,
        payload: Value,
        oneshot_tx: oneshot::Sender<BulkInjectResult>,
    },
    /// **M-Ingress phase 2.** `POST /v1/nodes/:id/call` —
    /// synchronously inject a request and await a reply. The
    /// drain handler validates node existence + the manifest's
    /// `accepts_external_call` table for a matching
    /// `(request_type, reply_type)` pair, then dispatches via
    /// `SchedulerHandle::send_ingress_with_correlation`. The
    /// HTTP-side handler is responsible for awaiting the
    /// `oneshot::Receiver<Value>` it pre-registered in the
    /// `PendingCalls` registry under `correlation_id` (NOT routed
    /// through this `oneshot_tx`); this `oneshot_tx` carries
    /// only the disposition (`Dispatched` /
    /// `RejectedByManifest` / `NodeNotFound`) so the handler
    /// knows whether to await the reply or fail fast.
    InjectMessageWithReply {
        node_id: String,
        message_type: String,
        payload: Value,
        reply_type: String,
        correlation_id: String,
        oneshot_tx: oneshot::Sender<InjectMessageWithReplyDisposition>,
    },
}

/// **M-Ingress phase 2.** Initial drain disposition for a call —
/// distinct from [`CallResult`] because the call handler awaits
/// the reply (or timeout) on a separate `oneshot::Receiver<Value>`
/// pre-registered in the `PendingCalls` registry, while this
/// disposition tells the handler whether dispatch even succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectMessageWithReplyDisposition {
    /// Manifest gate passed and the message was injected via
    /// `SchedulerHandle::send_ingress_with_correlation`. The
    /// handler now awaits the `PendingCalls` oneshot for the
    /// reply payload (or the `tokio::time::timeout` for the
    /// 504 path).
    Dispatched,
    /// Target's `accepts_external_call` did not contain a matching
    /// `(request_type, reply_type)` pair → 403.
    RejectedByManifest,
    /// Target node id did not resolve → 404.
    NodeNotFound,
}

/// Outcome of an inject-message bridge round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectResult {
    /// Message accepted onto the inbox. Note: this does NOT mean the
    /// node *handled* the message — only that it was queued. The
    /// telemetry sink (`MessageDelivered { accepted }`) reflects
    /// the manifest-level disposition asynchronously.
    Accepted,
    /// Node id did not resolve at the moment of delivery. Maps to
    /// HTTP 404.
    NodeNotFound,
}

/// **M-Ingress phase 3.** Per-line bulk inject disposition.
///
/// Distinct from [`InjectResult`] because bulk surfaces manifest
/// gating to the caller synchronously (per-line response in the
/// streaming NDJSON body), whereas Phase 1 single-shot defers
/// manifest gating to delivery and observes via the telemetry
/// stream. Substrate-not-MVP: ingestion-shaped applications
/// (NewsCaster firehose, ComplyFlow audit-log) need to know
/// per-line outcomes inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BulkInjectResult {
    /// Manifest's `accepts_external` allows this `(node_id,
    /// message_type)` pair AND the message has been dispatched
    /// via `SchedulerHandle::send_ingress`.
    Accepted,
    /// Manifest's `accepts_external` does NOT contain this
    /// `message_type`. Maps to per-line `"rejected_by_manifest"`
    /// response.
    RejectedByManifest,
    /// Node id did not resolve. Maps to per-line
    /// `"node_not_found"` response.
    NodeNotFound,
}

/// **M-Ingress phase 2.** Outcome of an HTTP-call bridge round-trip
/// (`POST /v1/nodes/:id/call`).
///
/// `Replied` is the success path; `Timeout` covers the case where
/// the handler never emitted a reply within `timeout_ms`;
/// `RejectedByManifest` surfaces server-side gating against the
/// target's `accepts_external_call` table; `NodeNotFound` mirrors
/// the inject path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallResult {
    /// Handler emitted a reply matching the declared `reply_type`;
    /// payload forwarded to the awaiting HTTP caller as 200 OK.
    Replied(Value),
    /// `timeout_ms` elapsed before any matching reply arrived.
    /// Maps to HTTP 504 Gateway Timeout.
    Timeout,
    /// Target's `accepts_external_call` did NOT contain a matching
    /// `(request_type, reply_type)` pair. Maps to HTTP 403.
    RejectedByManifest,
    /// Target node id did not resolve at the moment of dispatch.
    /// Maps to HTTP 404.
    NodeNotFound,
}

/// **M-Ingress phase 2.** Bounded pending-calls registry. Each
/// `POST /v1/nodes/:id/call` registers `(correlation_id →
/// oneshot::Sender<Value>)` here before crossing the bridge to
/// the scheduler thread; the registry is wired as a `TelemetrySink`
/// alongside Prometheus + ring buffer + WebSocket so the scheduler's
/// `on_call_reply` event resolves the awaiting oneshot directly,
/// without depending on actual graph delivery (leaf-node call
/// handlers still work).
///
/// **Bounded under load.** [`Self::HARD_CAP`] entries; once
/// reached, [`Self::register`] returns `false` so the call
/// handler can surface 503 Service Unavailable. Callers MUST
/// take responsibility for their oneshot half via the returned
/// `Drop` guard ([`PendingCallGuard`]) — when the call future
/// is dropped (HTTP client disconnect, axum future drop), the
/// guard evicts the registry entry so the registry doesn't leak
/// on cancelled requests.
pub struct PendingCalls {
    inner: Arc<parking_lot::Mutex<HashMap<String, oneshot::Sender<Value>>>>,
}

impl PendingCalls {
    /// Maximum concurrent in-flight calls before backpressure
    /// surfaces as 503 Service Unavailable.
    pub const HARD_CAP: usize = 10_000;

    pub fn new() -> Self {
        Self {
            inner: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }

    /// Try to register `correlation_id → oneshot_tx`. Returns
    /// `Some(guard)` on success — caller must hold the guard for
    /// the lifetime of the call so HTTP-client-disconnect
    /// (axum future drop) evicts the entry. Returns `None` if
    /// the registry is at [`Self::HARD_CAP`].
    pub fn register(
        &self,
        correlation_id: String,
        oneshot_tx: oneshot::Sender<Value>,
    ) -> Option<PendingCallGuard> {
        let mut inner = self.inner.lock();
        if inner.len() >= Self::HARD_CAP {
            return None;
        }
        inner.insert(correlation_id.clone(), oneshot_tx);
        Some(PendingCallGuard {
            inner: Arc::clone(&self.inner),
            correlation_id,
            disarmed: false,
        })
    }

    /// Current entry count. O(1) under the lock.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// `true` if no in-flight calls are tracked.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
}

impl Default for PendingCalls {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for PendingCalls {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// **M-Ingress phase 2.** TelemetrySink impl: resolve the awaiting
/// `oneshot::Sender` IMMEDIATELY at scheduler-emit-time. The
/// `Scheduler::deliver` auto-propagation logic constructs the
/// reply Message + fires `on_call_reply` from `make_emit` BEFORE
/// the reply Message enters the queue, so the HTTP call handler
/// observes the reply without needing the reply to actually be
/// delivered to a graph node.
impl graphreact_core::TelemetrySink for PendingCalls {
    #[inline]
    fn on_call_reply(&self, ev: &graphreact_core::CallReply<'_>) {
        let mut inner = self.inner.lock();
        if let Some(tx) = inner.remove(ev.correlation_id) {
            // The oneshot may already have been dropped (the
            // axum future was cancelled); ignore the send error
            // — the guard's Drop will have removed the entry
            // anyway, so this branch only fires for racey cases
            // where the disconnect arrives between `lock()` and
            // `tx.send()`.
            let _ = tx.send(ev.payload.clone());
        }
    }
}

/// **M-Ingress phase 2.** RAII guard returned by
/// [`PendingCalls::register`]. Evicts the registry entry on drop
/// so HTTP-client-disconnect (axum future cancellation) doesn't
/// leak the entry. Disarmed on graceful resolution by the
/// telemetry sink (i.e. the oneshot was successfully resolved).
pub struct PendingCallGuard {
    inner: Arc<parking_lot::Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    correlation_id: String,
    disarmed: bool,
}

impl PendingCallGuard {
    /// Mark this guard as disarmed — the call resolved gracefully
    /// or timed out; the entry has already been removed from the
    /// registry by the telemetry sink path (or by the timeout
    /// path), and the guard's `Drop` should NOT perform a second
    /// removal (which would otherwise be a no-op but races with
    /// other ids on the same lock).
    pub fn disarm(mut self) {
        self.disarmed = true;
    }
}

impl Drop for PendingCallGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            self.inner.lock().remove(&self.correlation_id);
        }
    }
}

/// **M-Ingress phase 1.** Bounded idempotency-key cache. Lookup-or-insert
/// at the HTTP boundary so duplicate retries (same `Idempotency-Key`
/// header) short-circuit with HTTP 409 Conflict before ever crossing
/// the cross-thread bridge.
///
/// Implementation: `parking_lot::Mutex<HashMap<String, Instant>>` plus
/// lazy pruning on every insert. When the map exceeds [`Self::HIGH_WATER`]
/// entries we sweep all expired entries (older than [`Self::TTL`]) in
/// one pass. Cap is [`Self::HARD_CAP`]; once reached we evict the
/// oldest half (Vec::sort by insert time) before accepting new keys.
/// Substrate-not-MVP: bounded under load; no unbounded LRU growth.
pub struct IdempotencyCache {
    inner: parking_lot::Mutex<HashMap<String, Instant>>,
}

impl IdempotencyCache {
    /// Maximum entries kept in the cache. Beyond this we evict the
    /// oldest 50%.
    pub const HARD_CAP: usize = 10_000;
    /// Above this, every insert triggers an opportunistic prune.
    pub const HIGH_WATER: usize = 8_000;
    /// Idempotency-key TTL. After this elapses, a duplicate POST
    /// with the same key is *not* a conflict.
    pub const TTL: Duration = Duration::from_secs(600);

    pub fn new() -> Self {
        Self {
            inner: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Try to insert `key` as a fresh idempotency entry. Returns
    /// `true` if accepted (caller proceeds with the inject), or
    /// `false` if a non-expired entry already exists (caller
    /// returns HTTP 409).
    pub fn try_insert(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        if let Some(prev) = inner.get(key) {
            if now.duration_since(*prev) < Self::TTL {
                return false;
            }
        }
        if inner.len() >= Self::HIGH_WATER {
            self.prune_locked(&mut inner, now);
        }
        if inner.len() >= Self::HARD_CAP {
            // Oldest-half eviction. Collect, sort by Instant, drop first half.
            let mut entries: Vec<(String, Instant)> = inner.drain().collect();
            entries.sort_by_key(|(_, t)| *t);
            let cutoff = entries.len() / 2;
            for (k, t) in entries.into_iter().skip(cutoff) {
                inner.insert(k, t);
            }
        }
        inner.insert(key.to_string(), now);
        true
    }

    fn prune_locked(&self, inner: &mut HashMap<String, Instant>, now: Instant) {
        inner.retain(|_, t| now.duration_since(*t) < Self::TTL);
    }

    /// Current entry count. Exposed for tests + ops-debug ad-hoc.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// `true` if no entries are tracked.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
}

impl Default for IdempotencyCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Identity payload returned by `GET /v1/identity`. Operators detect
/// daemon restarts by watching `process_start_id`; on change, ops
/// tooling clears any cached telemetry tape and reconnects. Matches
/// the field shape companion ecosystems will rely on across versions.
#[derive(Debug, Clone, Serialize)]
pub struct Identity {
    /// Random `u64` chosen at process start. Stable for the
    /// lifetime of one daemon; changes on every restart.
    pub process_start_id: u64,
    /// Crate version, sourced from `CARGO_PKG_VERSION` at compile
    /// time.
    pub version: &'static str,
    /// Wire-protocol version. Bumped on breaking schema changes
    /// to the management API; companion ecosystems pin against
    /// this. P4 ships v1.
    pub api_version: u32,
}

/// Per-server context shared between every handler. Held behind
/// `Arc` because axum extracts state by clone-from-`Arc`.
#[derive(Clone)]
pub struct ApiState {
    pub metrics: Arc<MetricsInner>,
    pub snapshot_tx: mpsc::Sender<SnapshotRequest>,
    pub token: Option<Arc<BearerToken>>,
    pub identity: Arc<Identity>,
    /// **M16-Backend P5.** Per-node recent-messages ring. Read
    /// directly by the `/v1/nodes/:id/messages/recent` handler —
    /// `Send + Sync` so no scheduler-thread bridge needed.
    pub ring: Arc<RingBufferTelemetry>,
    /// **M16-Backend P7.** Live event broadcast fanout backing
    /// `GET /v1/observer/events` (WebSocket) and
    /// `GET /v1/observer/events.sse` (SSE fallback). `None` when
    /// the daemon hasn't installed a [`crate::WebSocketSink`] (P7
    /// is opt-in; older deploys keep the WS-disabled error
    /// response — `503 Service Unavailable`).
    pub websocket: Option<crate::websocket::WebSocketSink>,
    /// **M-Ingress phase 1.** Bounded idempotency-key cache for
    /// `POST /v1/nodes/:id/messages`. Shared by every inject
    /// handler invocation; the dedup decision is made before the
    /// cross-thread bridge so duplicate retries never reach the
    /// scheduler.
    pub idempotency_cache: Arc<IdempotencyCache>,
    /// **M-Ingress phase 2.** In-flight HTTP-call registry. Each
    /// `POST /v1/nodes/:id/call` registers `(correlation_id →
    /// oneshot::Sender<Value>)` here before dispatch; the
    /// scheduler-side `on_call_reply` event resolves the awaiting
    /// oneshot. RAII guard returned from
    /// [`PendingCalls::register`] handles HTTP-client-disconnect
    /// eviction.
    pub pending_calls: PendingCalls,
}

/// Maximum number of in-flight `SnapshotRequest`s on the bridge
/// channel. Operators hammering the management API past this rate
/// see 503 Service Unavailable rather than unbounded memory growth.
/// Substrate-not-MVP: backpressure visible from day one.
pub const SNAPSHOT_CHANNEL_CAPACITY: usize = 256;

/// **M-Ingress phase 3.** Default line cap on
/// `POST /v1/nodes/:id/messages/bulk` requests. Beyond this
/// number of NDJSON lines the handler emits a final
/// `{"line": N, "result": "exceeded_line_cap"}` and closes the
/// response stream. Configurable via the `GRX_INGRESS_BULK_LINE_CAP`
/// env var (substrate-level safety bound; operators can tune up or
/// down for their workload).
pub const DEFAULT_BULK_LINE_CAP: usize = 10_000;

/// **M-Ingress phase 3.** Hard per-line byte cap. A single NDJSON
/// line larger than this surfaces as `{"line": N, "result":
/// "line_too_large"}` and the handler skips that line (the stream
/// continues for subsequent lines). Substrate-level safety —
/// operators can't override; if your application needs >1MB
/// per-line, switch to single-shot `POST /v1/nodes/:id/messages`
/// or chunk client-side.
pub const BULK_MAX_LINE_BYTES: usize = 1_048_576; // 1 MiB

/// **M-Ingress phase 3.** Pre-flight `Content-Length` cap. Requests
/// declaring a body larger than this are rejected with HTTP 413
/// before any streaming begins. Computed as `DEFAULT_BULK_LINE_CAP
/// × BULK_MAX_LINE_BYTES` so a perfectly-packed maximal request
/// fits but typo'd `Content-Length: 99999999999` is rejected
/// up-front.
pub const BULK_MAX_BODY_BYTES: u64 = (DEFAULT_BULK_LINE_CAP as u64) * (BULK_MAX_LINE_BYTES as u64);

/// Read the configured bulk line cap from env var, falling back
/// to [`DEFAULT_BULK_LINE_CAP`]. Parse failure (non-numeric or
/// zero) also falls back to the default.
fn bulk_line_cap() -> usize {
    std::env::var("GRX_INGRESS_BULK_LINE_CAP")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_BULK_LINE_CAP)
}

/// Outcome of a server bind, mirroring P2's `http_server::ServerHandle`.
pub struct ApiServerHandle {
    pub local_addr: SocketAddr,
    pub shutdown: Arc<Notify>,
    pub task: tokio::task::JoinHandle<()>,
    /// Receiver-side of the cross-thread bridge; the scheduler
    /// thread drains this after each `process_batch` round.
    pub snapshot_rx: mpsc::Receiver<SnapshotRequest>,
    /// **M16-Backend P5.** Per-node ring buffer that backs
    /// `/v1/nodes/:id/messages/recent`. The same `Arc` is held
    /// by the API state; the daemon caller installs this as a
    /// [`TelemetrySink`] consumer alongside the [`crate::PrometheusSink`].
    pub ring: Arc<RingBufferTelemetry>,
    /// **M16-Backend P7.** Optional live event broadcast — the
    /// [`crate::WebSocketSink`] held by the API state, surfaced
    /// here so the daemon caller can install it as a [`TelemetrySink`]
    /// consumer in the [`crate::MultiplexSink`] stack.
    pub websocket: Option<crate::websocket::WebSocketSink>,
    /// **M-Ingress phase 1.** Idempotency cache backing
    /// `POST /v1/nodes/:id/messages`. Shared between API state and
    /// the daemon caller; lifetime tied to the daemon process.
    pub idempotency_cache: Arc<IdempotencyCache>,
    /// **M-Ingress phase 2.** PendingCalls registry backing
    /// `POST /v1/nodes/:id/call`. The daemon caller installs this
    /// as a `TelemetrySink` consumer (typically inside its
    /// `MultiplexSink` stack alongside Prometheus + ring buffer +
    /// WebSocket) so the scheduler's `on_call_reply` event
    /// resolves awaiting oneshots.
    pub pending_calls: PendingCalls,
}

impl ApiServerHandle {
    /// Signal shutdown + wait for the accept-loop task to finish.
    pub async fn shutdown(self) {
        self.shutdown.notify_one();
        let _ = self.task.await;
    }
}

/// Build the management API router. The router is the public seam
/// for tests + future P5+ extensions; spawning the listener is
/// a separate concern (see [`spawn`]).
pub fn build_router(state: ApiState) -> Router {
    use axum::middleware;

    // /v1/healthz bypasses auth (liveness probe-friendly).
    let public_routes = Router::new().route("/v1/healthz", get(healthz));

    // Auth-protected routes. The middleware fires before each
    // handler; missing/wrong token short-circuits with 401.
    let protected_routes = Router::new()
        .route("/v1/identity", get(identity))
        .route("/v1/metrics", get(metrics))
        .route("/v1/nodes", get(nodes_list))
        .route("/v1/nodes/:id", get(node_detail))
        .route("/v1/nodes/:id/messages/recent", get(node_messages_recent))
        // **M-Ingress phase 1.** External-client message injection.
        // Default-deny enforced at delivery time via the target
        // node's `manifest.accepts_external` allowlist (see
        // `Scheduler::deliver`). Idempotency-Key + bridge backpressure
        // handled in `inject_message_handler`.
        .route("/v1/nodes/:id/messages", post(inject_message_handler))
        // **M-Ingress phase 2.** Synchronous request/response call.
        // Manifest gate: `accepts_external_call` must contain a
        // matching `(request_type, reply_type)` pair.
        // Auto-correlation: scheduler tags the first matching reply
        // emit with the inbound's `correlation_id`; the
        // `PendingCalls` registry resolves the awaiting oneshot.
        .route("/v1/nodes/:id/call", post(inject_call_handler))
        // **M-Ingress phase 3.** Bulk/streaming NDJSON inject.
        // Per-line manifest gating + per-line idempotency_key
        // + streaming per-line `{"line": N, "result": "..."}`
        // response. Body cap configurable via
        // `GRX_INGRESS_BULK_LINE_CAP` env var.
        .route(
            "/v1/nodes/:id/messages/bulk",
            post(bulk_inject_handler),
        )
        .route("/v1/supervisor/tree", get(supervisor_tree))
        // M16-Backend P7 — live event stream (WebSocket primary,
        // SSE fallback). Both paths share the auth middleware +
        // `?node=` / `?type=` / `?since=` filter parsing.
        .route(
            "/v1/observer/events",
            get(crate::websocket::ws_handler),
        )
        .route(
            "/v1/observer/events.sse",
            get(crate::websocket::sse_handler),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_layer));

    // **M-Ingress phase 1.** Browser is now a first-class consumer
    // of management-API + ingress endpoints. Baseline `*` origin
    // suits loopback bind defaults; production deploys behind a
    // header-injecting reverse proxy provide their own per-origin
    // policy. M-Prod refines (per-origin allowlists, credential
    // modes). Allowed methods cover ingress (POST), snapshots
    // (GET), and preflight (OPTIONS); allowed headers cover
    // bearer auth + JSON body + Idempotency-Key.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderName::from_static("idempotency-key"),
        ]);

    public_routes
        .merge(protected_routes)
        .with_state(state)
        .layer(cors)
}

/// Spawn the management API HTTP server. Returns an
/// [`ApiServerHandle`] the daemon owns + the receiver-side of the
/// snapshot bridge (which the scheduler thread drains).
///
/// `metrics` is shared with the [`crate::PrometheusSink`] so
/// `/v1/metrics` returns the same counter values the scheduler is
/// writing to.
pub async fn spawn(
    bind: BindConfig,
    metrics: Arc<MetricsInner>,
    process_start_id: u64,
) -> Result<ApiServerHandle, std::io::Error> {
    let listener = TcpListener::bind(bind.addr).await?;
    let local_addr = listener.local_addr()?;
    crate::audit::log_bind(local_addr, bind.tls.is_some(), bind.token.is_some());

    let (snapshot_tx, snapshot_rx) = mpsc::channel::<SnapshotRequest>(SNAPSHOT_CHANNEL_CAPACITY);

    let identity = Arc::new(Identity {
        process_start_id,
        version: env!("CARGO_PKG_VERSION"),
        api_version: 1,
    });
    let ring = Arc::new(RingBufferTelemetry::new());
    let websocket = crate::websocket::WebSocketSink::default();
    let idempotency_cache = Arc::new(IdempotencyCache::new());
    let pending_calls = PendingCalls::new();
    let state = ApiState {
        metrics,
        snapshot_tx,
        token: bind.token.map(Arc::new),
        identity,
        ring: ring.clone(),
        websocket: Some(websocket.clone()),
        idempotency_cache: idempotency_cache.clone(),
        pending_calls: pending_calls.clone(),
    };
    let router = build_router(state);

    let tls_acceptor = bind
        .tls
        .as_ref()
        .map(|tls| TlsAcceptor::from(tls.server.clone()));
    let shutdown = Arc::new(Notify::new());
    let shutdown_for_loop = shutdown.clone();

    let task = tokio::spawn(async move {
        accept_loop(listener, router, tls_acceptor, shutdown_for_loop).await;
    });

    Ok(ApiServerHandle {
        local_addr,
        shutdown,
        task,
        snapshot_rx,
        ring,
        websocket: Some(websocket),
        idempotency_cache,
        pending_calls,
    })
}

async fn accept_loop(
    listener: TcpListener,
    router: Router,
    tls_acceptor: Option<TlsAcceptor>,
    shutdown: Arc<Notify>,
) {
    use hyper_util::rt::TokioIo;
    use hyper_util::server::conn::auto::Builder as ConnBuilder;
    use tower::ServiceExt;

    loop {
        tokio::select! {
            biased;
            () = shutdown.notified() => break,
            res = listener.accept() => {
                let (socket, peer) = match res {
                    Ok(pair) => pair,
                    Err(err) => {
                        tracing::warn!(
                            target: "graphreact::observer::audit",
                            ?err,
                            "observer accept failed"
                        );
                        continue;
                    }
                };
                let router = router.clone();
                let acceptor = tls_acceptor.clone();
                tokio::spawn(async move {
                    // axum's `Router` becomes a `tower::Service`
                    // once we feed it a single connection's IO.
                    // hyper-util's auto-builder negotiates HTTP/1
                    // (HTTP/2 is left off — Prometheus scrape
                    // clients all support HTTP/1.1; companion
                    // ecosystem clients negotiate the same).
                    let conn = ConnBuilder::new(hyper_util::rt::TokioExecutor::new());
                    let svc = hyper::service::service_fn(move |req| {
                        let router = router.clone();
                        async move {
                            // Adapter: feed the request through the
                            // axum Router via ServiceExt::oneshot.
                            router.oneshot(req).await
                        }
                    });
                    if let Some(acceptor) = acceptor {
                        match acceptor.accept(socket).await {
                            Ok(tls_stream) => {
                                let _ = conn.serve_connection_with_upgrades(TokioIo::new(tls_stream), svc).await;
                            }
                            Err(err) => {
                                tracing::warn!(
                                    target: "graphreact::observer::audit",
                                    %peer,
                                    ?err,
                                    "observer TLS handshake failed"
                                );
                            }
                        }
                    } else {
                        let _ = conn.serve_connection_with_upgrades(TokioIo::new(socket), svc).await;
                    }
                });
            }
        }
    }
}

// ----- Handlers -----

async fn healthz() -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "ok",
    )
        .into_response()
}

async fn identity(State(state): State<ApiState>) -> Json<Identity> {
    Json((*state.identity).clone())
}

async fn metrics(State(state): State<ApiState>) -> Response {
    let mut body = encode_text(&state.metrics);
    // **M16-Backend P7.** Append observer subscriber metrics —
    // sourced from the WebSocketSink's atomics rather than a
    // MetricsInner field because the active count is pure-derive
    // (RAII guard counts) and we don't want to leak hot-path
    // counter mutations from the WS path back into Prometheus
    // bookkeeping.
    if let Some(ws) = state.websocket.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(
            body,
            "# HELP grx_observer_subscribers_active Number of currently-connected /v1/observer/events subscribers (WebSocket + SSE combined).\n\
             # TYPE grx_observer_subscribers_active gauge\n\
             grx_observer_subscribers_active {}\n\
             # HELP grx_observer_subscribers_kicked_total Total subscribers disconnected after broadcast::error::RecvError::Lagged. Increments once per kicked subscriber; the corresponding Resync event is also pushed before the connection closes.\n\
             # TYPE grx_observer_subscribers_kicked_total counter\n\
             grx_observer_subscribers_kicked_total {}\n",
            ws.active_subscribers(),
            ws.kicked_total(),
        );
    }
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

async fn nodes_list(State(state): State<ApiState>) -> Result<Json<Vec<NodeIntrospection>>, Response> {
    let (tx, rx) = oneshot::channel();
    state
        .snapshot_tx
        .send(SnapshotRequest::Nodes(tx))
        .await
        .map_err(|_| service_unavailable("scheduler bridge closed"))?;
    let nodes = rx
        .await
        .map_err(|_| service_unavailable("scheduler bridge dropped response"))?;
    Ok(Json(nodes))
}

async fn node_detail(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<NodeIntrospection>, Response> {
    let (tx, rx) = oneshot::channel();
    state
        .snapshot_tx
        .send(SnapshotRequest::NodeDetail(id, tx))
        .await
        .map_err(|_| service_unavailable("scheduler bridge closed"))?;
    let node = rx
        .await
        .map_err(|_| service_unavailable("scheduler bridge dropped response"))?;
    match node {
        Some(n) => Ok(Json(n)),
        None => Err((StatusCode::NOT_FOUND, "node not found").into_response()),
    }
}

/// `GET /v1/nodes/:id/messages/recent?limit=N` — last N messages
/// received by `id`. Reads directly from the `RingBufferTelemetry`
/// without going through the scheduler bridge — the ring is
/// `Send + Sync`. PII-safe: payload is never captured (see
/// [`crate::ring_buffer`] module docs).
async fn node_messages_recent(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<RecentParams>,
) -> Json<Vec<MessageSnapshot>> {
    let limit = params.limit.unwrap_or(DEFAULT_RING_CAPACITY);
    let limit = limit.min(DEFAULT_RING_CAPACITY * 4); // cap at 4x default
    Json(state.ring.recent(&id, limit))
}

#[derive(serde::Deserialize, Default)]
struct RecentParams {
    limit: Option<usize>,
}

/// Request body shape for `POST /v1/nodes/:id/messages`.
#[derive(Debug, Deserialize)]
pub struct InjectMessageBody {
    /// Message type — runtime-checked against the target's
    /// `manifest.accepts_external`.
    pub r#type: String,
    /// Arbitrary JSON payload, forwarded verbatim to the node's
    /// inbox. Runtime does not introspect this.
    #[serde(default)]
    pub payload: Value,
}

/// **M-Ingress phase 2.** Request body shape for
/// `POST /v1/nodes/:id/call`.
#[derive(Debug, Deserialize)]
pub struct CallMessageBody {
    /// Inbound message type — runtime-checked against the target's
    /// `manifest.accepts_external_call[*].request_type`.
    pub r#type: String,
    /// Expected reply message type — runtime-checked against the
    /// matching entry's `reply_type`. The runtime auto-tags the
    /// first outgoing emit with this `type` (in the receiving
    /// handler) with the correlation_id.
    pub reply_type: String,
    /// Arbitrary JSON payload, forwarded verbatim to the node's
    /// inbox. Runtime does not introspect this.
    #[serde(default)]
    pub payload: Value,
    /// Maximum time the call handler waits for the reply before
    /// returning 504 Gateway Timeout. Defaults to 30 000 (30s);
    /// server caps at 5 minutes (300 000) to bound resource
    /// consumption.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// **M-Ingress phase 1.** `POST /v1/nodes/:id/messages` handler.
///
/// Pipeline:
/// 1. (Optional) `Idempotency-Key` header dedup — duplicate retry → 409.
/// 2. Cross-thread bridge to the scheduler thread for node-existence
///    check + `SchedulerHandle::send` dispatch (origin = `__external__`).
/// 3. `Scheduler::deliver` enforces `manifest.accepts_external` at the
///    moment of delivery — types not on the allowlist are dropped with
///    `MessageDelivered { accepted: false }` for ops visibility, but
///    the HTTP response is still 202 Accepted (the semantics: "the
///    daemon received your inject; the manifest decides what happens
///    to it"). 403 is reserved for header-level rejection (which the
///    HTTP path doesn't currently use; future M-Prod auth tightens this).
async fn inject_message_handler(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<InjectMessageBody>,
) -> Response {
    // Idempotency-Key dedup before crossing the bridge.
    if let Some(key) = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    {
        if !state.idempotency_cache.try_insert(key) {
            return (
                StatusCode::CONFLICT,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                serde_json::json!({
                    "error": "idempotency_conflict",
                    "key": key,
                })
                .to_string(),
            )
                .into_response();
        }
    }

    let (tx, rx) = oneshot::channel();
    let req = SnapshotRequest::InjectMessage {
        node_id: id,
        message_type: body.r#type,
        payload: body.payload,
        oneshot_tx: tx,
    };
    if state.snapshot_tx.send(req).await.is_err() {
        return service_unavailable("scheduler bridge closed");
    }
    match rx.await {
        Ok(InjectResult::Accepted) => (
            StatusCode::ACCEPTED,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::json!({ "status": "accepted" }).to_string(),
        )
            .into_response(),
        Ok(InjectResult::NodeNotFound) => (
            StatusCode::NOT_FOUND,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::json!({ "error": "node_not_found" }).to_string(),
        )
            .into_response(),
        Err(_) => service_unavailable("scheduler bridge dropped response"),
    }
}

/// **M-Ingress phase 2.** Cap the per-call timeout at 5 minutes
/// to bound resource consumption (PendingCalls registry slots +
/// scheduler thread bridge entries). Operators wanting longer
/// waits should poll a status endpoint instead of hold-blocking
/// HTTP.
const MAX_CALL_TIMEOUT_MS: u64 = 5 * 60 * 1_000;
/// **M-Ingress phase 2.** Default per-call timeout when the
/// caller doesn't supply `timeout_ms`. 30s matches typical HTTP
/// client default-read timeouts so callers see a clean 504 rather
/// than a TCP-level hang on stuck handlers.
const DEFAULT_CALL_TIMEOUT_MS: u64 = 30_000;

/// **M-Ingress phase 2.** `POST /v1/nodes/:id/call` handler.
///
/// Pipeline:
/// 1. Generate `correlation_id` (UUIDv4) server-side.
/// 2. Pre-register `(correlation_id → oneshot::Sender<Value>)` in
///    the [`PendingCalls`] registry; if the registry is full,
///    return 503. The registration returns a [`PendingCallGuard`]
///    that evicts the entry on Drop — covers HTTP-client-disconnect
///    via axum future cancellation.
/// 3. Cross the bridge to the scheduler thread via
///    [`SnapshotRequest::InjectMessageWithReply`]. The drain
///    handler validates `accepts_external_call` for a matching
///    `(request_type, reply_type)` pair; mismatch → 403.
/// 4. Wrap the `oneshot::Receiver<Value>` in a `tokio::time::timeout`
///    capped at [`MAX_CALL_TIMEOUT_MS`]. On reply, return 200 +
///    payload. On timeout, return 504; the registry guard's Drop
///    handles eviction.
async fn inject_call_handler(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<CallMessageBody>,
) -> Response {
    // Server-side correlation_id mint. UUIDv4 keeps the space
    // wide enough that operator-driven `correlation_id` reuse is
    // ~impossible without explicit collusion.
    let correlation_id = uuid::Uuid::new_v4().to_string();

    // Server-side timeout cap. Callers asking for longer waits
    // get clamped to MAX_CALL_TIMEOUT_MS; smaller values pass
    // through verbatim.
    let timeout_ms = body
        .timeout_ms
        .unwrap_or(DEFAULT_CALL_TIMEOUT_MS)
        .min(MAX_CALL_TIMEOUT_MS);

    // Pre-register the awaiting oneshot BEFORE the bridge so the
    // sink can resolve even if dispatch + emit happen faster than
    // we can register (small but real race window).
    let (reply_tx, reply_rx) = oneshot::channel::<Value>();
    let Some(guard) = state.pending_calls.register(correlation_id.clone(), reply_tx) else {
        return service_unavailable("pending_calls registry full");
    };

    // Bridge dispatch.
    let (disp_tx, disp_rx) = oneshot::channel();
    let req = SnapshotRequest::InjectMessageWithReply {
        node_id: id,
        message_type: body.r#type,
        payload: body.payload,
        reply_type: body.reply_type.clone(),
        correlation_id: correlation_id.clone(),
        oneshot_tx: disp_tx,
    };
    if state.snapshot_tx.send(req).await.is_err() {
        // Drop `guard` evicts the registry entry.
        drop(guard);
        return service_unavailable("scheduler bridge closed");
    }
    let disposition = match disp_rx.await {
        Ok(d) => d,
        Err(_) => {
            drop(guard);
            return service_unavailable("scheduler bridge dropped response");
        }
    };

    match disposition {
        InjectMessageWithReplyDisposition::RejectedByManifest => {
            drop(guard);
            (
                StatusCode::FORBIDDEN,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                serde_json::json!({
                    "error": "rejected_by_manifest",
                    "reason": "no matching accepts_external_call entry",
                })
                .to_string(),
            )
                .into_response()
        }
        InjectMessageWithReplyDisposition::NodeNotFound => {
            drop(guard);
            (
                StatusCode::NOT_FOUND,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                serde_json::json!({ "error": "node_not_found" }).to_string(),
            )
                .into_response()
        }
        InjectMessageWithReplyDisposition::Dispatched => {
            // Now race the reply against the timeout. The
            // registry guard's Drop handles HTTP-disconnect
            // (axum cancels this future, the guard drops, the
            // registry entry evicts).
            let timeout_dur = Duration::from_millis(timeout_ms);
            match tokio::time::timeout(timeout_dur, reply_rx).await {
                Ok(Ok(payload)) => {
                    // Reply received — guard's slot was already
                    // evicted by the sink on resolution. Disarm
                    // so Drop doesn't double-evict.
                    guard.disarm();
                    let body = serde_json::json!({
                        "reply_type": body.reply_type,
                        "payload": payload,
                    });
                    (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body.to_string(),
                    )
                        .into_response()
                }
                Ok(Err(_)) => {
                    // The oneshot was dropped without sending —
                    // shouldn't happen on the live path because
                    // the sink either sends a value or doesn't
                    // touch the channel. Treat as 504.
                    drop(guard);
                    timeout_response(timeout_ms)
                }
                Err(_) => {
                    // tokio::time::timeout elapsed. Drop the
                    // guard so the entry evicts; the sink may
                    // still attempt to resolve (race), but its
                    // remove-then-send returns a closed channel
                    // which is silently ignored.
                    drop(guard);
                    timeout_response(timeout_ms)
                }
            }
        }
    }
}

fn timeout_response(timeout_ms: u64) -> Response {
    (
        StatusCode::GATEWAY_TIMEOUT,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({
            "error": "reply_timeout",
            "timeout_ms": timeout_ms,
        })
        .to_string(),
    )
        .into_response()
}

/// **M-Ingress phase 3.** Per-line shape parsed out of the
/// NDJSON request body for `POST /v1/nodes/:id/messages/bulk`.
/// `idempotency_key` lives per-line because the
/// `Idempotency-Key` HTTP header is single-valued and doesn't
/// apply to a bulk request that carries many distinct messages.
#[derive(Debug, Deserialize)]
struct BulkLineBody {
    r#type: String,
    #[serde(default)]
    payload: Value,
    #[serde(default)]
    idempotency_key: Option<String>,
}

/// **M-Ingress phase 3.** `POST /v1/nodes/:id/messages/bulk` —
/// streaming NDJSON dispatch.
///
/// Pipeline:
/// 1. Pre-flight `Content-Length` cap → 413 if the declared body
///    exceeds [`BULK_MAX_BODY_BYTES`].
/// 2. Spawn a producer task that:
///    - Reads the request body as a `Stream<Item = Result<Bytes>>`
///      and buffers a line at a time (split on `\n`).
///    - Per line: enforces [`BULK_MAX_LINE_BYTES`] (oversized →
///      `"line_too_large"`); parses JSON (malformed →
///      `"bad_json"`); checks `IdempotencyCache` (duplicate →
///      `"idempotency_conflict"`); then dispatches via the bridge
///      and awaits the `BulkInjectResult` oneshot.
///    - Streams a per-line `{"line": N, "result": "..."}` into
///      the response body channel as each disposition resolves.
/// 3. Stops at [`bulk_line_cap`] lines with a final
///    `"exceeded_line_cap"` line.
/// 4. mpsc-bridge full at any line → terminator
///    `{"line": N, "result": "backpressure_503"}` + close stream
///    (clients can retry from line N+1).
///
/// Status: 200 OK + `Transfer-Encoding: chunked` always (once the
/// pre-flight `Content-Length` check passes); per-line dispositions
/// surface as NDJSON in the streamed body. The 200 status is sent
/// at handler entry, so even a partial-failure run carries a 200
/// status — the per-line response shape is the source-of-truth for
/// client decision-making.
async fn bulk_inject_handler(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    use axum::body::Body;
    use bytes::{Bytes, BytesMut};
    use futures::StreamExt as _;

    // Pre-flight Content-Length cap. Browsers + curl always set
    // Content-Length on POST; chunked-transfer-encoded clients
    // bypass this gate (the per-line cap still applies).
    if let Some(cl) = request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
    {
        if cl > BULK_MAX_BODY_BYTES {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                serde_json::json!({
                    "error": "body_too_large",
                    "max_bytes": BULK_MAX_BODY_BYTES,
                })
                .to_string(),
            )
                .into_response();
        }
    }

    let line_cap = bulk_line_cap();
    let snapshot_tx = state.snapshot_tx.clone();
    let idem_cache = Arc::clone(&state.idempotency_cache);
    let body = request.into_body();

    // Bounded channel between the producer (parses + dispatches)
    // and the response-body stream (relays bytes to the client).
    // Capacity 64 lines of buffered output gives the producer
    // headroom without unbounded memory growth if the client is
    // slow.
    let (out_tx, out_rx) = mpsc::channel::<Bytes>(64);

    tokio::spawn(async move {
        let mut data_stream = body.into_data_stream();
        let mut buf: BytesMut = BytesMut::new();
        let mut line_num: u64 = 0;
        let mut eof = false;

        while !eof {
            // Ensure the buffer has at least one full line, OR EOF.
            while !buf.contains(&b'\n') && !eof {
                match data_stream.next().await {
                    Some(Ok(chunk)) => {
                        buf.extend_from_slice(&chunk);
                        // Per-line size cap: even before we see a
                        // `\n`, if the line buffer is already too
                        // large, eject this line and resync to the
                        // next `\n`.
                        if buf.len() > BULK_MAX_LINE_BYTES {
                            if write_result(&out_tx, line_num, "line_too_large")
                                .await
                                .is_err()
                            {
                                return;
                            }
                            line_num += 1;
                            if !drain_to_next_newline(&mut buf, &mut data_stream).await {
                                return;
                            }
                            if line_num >= line_cap as u64 {
                                let _ =
                                    write_result(&out_tx, line_num, "exceeded_line_cap")
                                        .await;
                                return;
                            }
                        }
                    }
                    Some(Err(_)) | None => {
                        eof = true;
                    }
                }
            }

            // Drain any complete lines from buf.
            while let Some(nl_pos) = buf.iter().position(|&b| b == b'\n') {
                let line = buf.split_to(nl_pos + 1);
                let line_text = &line[..line.len() - 1]; // drop the trailing '\n'
                // Strip optional CR from CRLF-terminated lines.
                let line_text = line_text
                    .strip_suffix(b"\r")
                    .unwrap_or(line_text);
                // Empty lines (NDJSON tolerates / ignores them):
                // skip without incrementing line_num. Real
                // application clients should not emit blanks
                // mid-stream, but trailing newlines after the
                // last record are common.
                if line_text.is_empty() {
                    continue;
                }
                if line_num >= line_cap as u64 {
                    let _ =
                        write_result(&out_tx, line_num, "exceeded_line_cap").await;
                    return;
                }
                let result = process_bulk_line(
                    &state_ref(&snapshot_tx, &idem_cache),
                    &id,
                    line_text,
                )
                .await;
                if write_line_result(&out_tx, line_num, &result).await.is_err() {
                    return;
                }
                line_num += 1;
                if matches!(result, BulkLineDisposition::BackpressureClosed) {
                    return;
                }
            }
        }

        // EOF with leftover bytes (no trailing newline) — treat
        // the remainder as the last line if non-empty.
        if !buf.is_empty() {
            let line_text = buf
                .strip_suffix(b"\r")
                .unwrap_or(&buf);
            if !line_text.is_empty() && line_num < line_cap as u64 {
                let result = process_bulk_line(
                    &state_ref(&snapshot_tx, &idem_cache),
                    &id,
                    line_text,
                )
                .await;
                let _ = write_line_result(&out_tx, line_num, &result).await;
            }
        }
    });

    let stream = futures::stream::unfold(out_rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk| (Ok::<_, std::io::Error>(chunk), rx))
    });
    let body = Body::from_stream(stream);
    Response::builder()
        .status(StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            "application/x-ndjson; charset=utf-8",
        )
        .body(body)
        .unwrap_or_else(|_| service_unavailable("response body assembly failed"))
}

/// **M-Ingress phase 3.** Disposition of a single bulk-inject
/// line, mapped to the per-line response result string by
/// [`write_line_result`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum BulkLineDisposition {
    Accepted,
    NodeNotFound,
    RejectedByManifest,
    BadJson,
    IdempotencyConflict,
    /// mpsc bridge full — caller should retry from line N+1.
    /// Producer task closes the response stream after writing
    /// this line.
    BackpressureClosed,
    /// Bridge dropped its oneshot before responding (scheduler
    /// shutdown, race with daemon teardown). Stream continues
    /// for subsequent lines but operators should investigate.
    BridgeDropped,
}

/// State borrow shared between the producer task's per-line
/// processing — clones are cheap (Arc / mpsc::Sender).
struct BulkProducerState {
    snapshot_tx: mpsc::Sender<SnapshotRequest>,
    idem_cache: Arc<IdempotencyCache>,
}

fn state_ref(
    snapshot_tx: &mpsc::Sender<SnapshotRequest>,
    idem_cache: &Arc<IdempotencyCache>,
) -> BulkProducerState {
    BulkProducerState {
        snapshot_tx: snapshot_tx.clone(),
        idem_cache: Arc::clone(idem_cache),
    }
}

/// Process one NDJSON line: validate, dedupe, dispatch via bridge,
/// await the disposition.
async fn process_bulk_line(
    state: &BulkProducerState,
    node_id: &str,
    line_bytes: &[u8],
) -> BulkLineDisposition {
    let parsed: BulkLineBody = match serde_json::from_slice(line_bytes) {
        Ok(p) => p,
        Err(_) => return BulkLineDisposition::BadJson,
    };

    // Per-line idempotency check at the HTTP boundary.
    if let Some(key) = parsed.idempotency_key.as_deref() {
        if !key.is_empty() && !state.idem_cache.try_insert(key) {
            return BulkLineDisposition::IdempotencyConflict;
        }
    }

    let (tx, rx) = oneshot::channel::<BulkInjectResult>();
    let req = SnapshotRequest::InjectMessageBulkLine {
        node_id: node_id.to_string(),
        message_type: parsed.r#type,
        payload: parsed.payload,
        oneshot_tx: tx,
    };
    match state.snapshot_tx.try_send(req) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            return BulkLineDisposition::BackpressureClosed;
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            return BulkLineDisposition::BridgeDropped;
        }
    }
    match rx.await {
        Ok(BulkInjectResult::Accepted) => BulkLineDisposition::Accepted,
        Ok(BulkInjectResult::NodeNotFound) => BulkLineDisposition::NodeNotFound,
        Ok(BulkInjectResult::RejectedByManifest) => {
            BulkLineDisposition::RejectedByManifest
        }
        Err(_) => BulkLineDisposition::BridgeDropped,
    }
}

/// Drain the body data stream up to (and including) the next
/// `\n`, then truncate the line buffer. Used after a
/// `line_too_large` rejection to resynchronize at the next
/// record boundary without buffering the rest of the oversized
/// line. Returns `false` if the stream ended without a `\n` —
/// which is fine, the caller drops out of the read loop.
async fn drain_to_next_newline<S>(buf: &mut bytes::BytesMut, data_stream: &mut S) -> bool
where
    S: futures::Stream<Item = Result<bytes::Bytes, axum::Error>> + Unpin,
{
    use futures::StreamExt as _;
    if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let _ = buf.split_to(pos + 1);
        return true;
    }
    buf.clear();
    while let Some(chunk) = data_stream.next().await {
        let Ok(chunk) = chunk else {
            return false;
        };
        if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&chunk[pos + 1..]);
            return true;
        }
    }
    false
}

/// Send a per-line response chunk into the response body
/// channel. Returns `Err` if the receiver has been dropped (e.g.
/// the client disconnected); callers MUST respect that and stop
/// dispatching to avoid wasted work.
async fn write_line_result(
    out_tx: &mpsc::Sender<bytes::Bytes>,
    line_num: u64,
    disposition: &BulkLineDisposition,
) -> Result<(), mpsc::error::SendError<bytes::Bytes>> {
    let result_str = match disposition {
        BulkLineDisposition::Accepted => "accepted",
        BulkLineDisposition::NodeNotFound => "node_not_found",
        BulkLineDisposition::RejectedByManifest => "rejected_by_manifest",
        BulkLineDisposition::BadJson => "bad_json",
        BulkLineDisposition::IdempotencyConflict => "idempotency_conflict",
        BulkLineDisposition::BackpressureClosed => "backpressure_503",
        BulkLineDisposition::BridgeDropped => "bridge_dropped",
    };
    write_result(out_tx, line_num, result_str).await
}

async fn write_result(
    out_tx: &mpsc::Sender<bytes::Bytes>,
    line_num: u64,
    result: &str,
) -> Result<(), mpsc::error::SendError<bytes::Bytes>> {
    let chunk = format!("{{\"line\":{line_num},\"result\":\"{result}\"}}\n");
    out_tx.send(bytes::Bytes::from(chunk)).await
}

async fn supervisor_tree(
    State(state): State<ApiState>,
) -> Result<Json<Vec<SupervisorIntrospection>>, Response> {
    let (tx, rx) = oneshot::channel();
    state
        .snapshot_tx
        .send(SnapshotRequest::SupervisorTree(tx))
        .await
        .map_err(|_| service_unavailable("scheduler bridge closed"))?;
    let tree = rx
        .await
        .map_err(|_| service_unavailable("scheduler bridge dropped response"))?;
    Ok(Json(tree))
}

fn service_unavailable(msg: &'static str) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, msg).into_response()
}

async fn auth_layer(
    State(state): State<ApiState>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(expected) = state.token.as_ref() else {
        return next.run(req).await;
    };
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let candidate = header.and_then(|v| v.strip_prefix("Bearer "));
    let peer_repr = req
        .extensions()
        .get::<SocketAddr>()
        .map(SocketAddr::to_string)
        .unwrap_or_else(|| "<unknown>".into());
    match candidate {
        None => {
            tracing::warn!(
                target: "graphreact::observer::audit",
                peer = %peer_repr,
                "observer auth fail (missing)"
            );
            challenge_response()
        }
        Some(c) if expected.matches(c.as_bytes()) => next.run(req).await,
        Some(_) => {
            tracing::warn!(
                target: "graphreact::observer::audit",
                peer = %peer_repr,
                "observer auth fail (wrong)"
            );
            challenge_response()
        }
    }
}

fn challenge_response() -> Response {
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "401 unauthorized",
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"graphreact-observer\""),
    );
    resp
}

/// Drain pending [`SnapshotRequest`]s from the bridge channel, calling
/// the supplied `respond` callback for each. The scheduler thread
/// invokes this after every `process_batch` round (in
/// `drive_handle_until`); `respond` is the only place scheduler
/// state is read on the scheduler thread, then sent through the
/// embedded `oneshot`.
///
/// Returns the number of requests drained — useful for instrumenting
/// management-API request rate.
pub fn drain_snapshot_requests(
    rx: &mut mpsc::Receiver<SnapshotRequest>,
    mut respond: impl FnMut(SnapshotRequest),
) -> usize {
    let mut count = 0;
    while let Ok(req) = rx.try_recv() {
        respond(req);
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_payload_is_json_serializable() {
        let id = Identity {
            process_start_id: 1234,
            version: "0.1.0",
            api_version: 1,
        };
        let v = serde_json::to_value(&id).unwrap();
        assert_eq!(v["process_start_id"], 1234);
        assert_eq!(v["version"], "0.1.0");
        assert_eq!(v["api_version"], 1);
    }
}
