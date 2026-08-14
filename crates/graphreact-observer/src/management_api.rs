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
use crate::drain::{DrainFlag, IngressInFlight};
use crate::metrics::MetricsInner;
use crate::prometheus::encode_text;
use crate::ring_buffer::{MessageSnapshot, RingBufferTelemetry, DEFAULT_RING_CAPACITY};
use graphreact_core::telemetry::{NodeIntrospection, SupervisorIntrospection};
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::GovernorLayer;

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
    /// **M-Prod sub-track #4 Phase γ.next.** Admin-authorized request
    /// to issue a delegated restart through the daemon-owned Cluster.
    /// The observer crate deliberately carries only transport-neutral
    /// strings/result variants; graphreact-cli maps them to the dist
    /// crate's typed API on the daemon thread.
    DelegatedRestart {
        peer: String,
        child_node_id: String,
        token_id: String,
        oneshot_tx: oneshot::Sender<DelegatedRestartDispatchResult>,
    },
    StartDurableRun {
        entrypoint: String,
        payload: Value,
        idempotency_key: String,
        oneshot_tx: oneshot::Sender<DurableRunBridgeResult>,
    },
    GetDurableRun {
        run_id: String,
        oneshot_tx: oneshot::Sender<DurableRunBridgeResult>,
    },
    CancelDurableRun {
        run_id: String,
        oneshot_tx: oneshot::Sender<DurableRunBridgeResult>,
    },
    ListRunArtifacts {
        run_id: String,
        oneshot_tx: oneshot::Sender<DurableArtifactBridgeResult>,
    },
    GetArtifact {
        artifact_id: String,
        oneshot_tx: oneshot::Sender<DurableArtifactBridgeResult>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct DurableRunView {
    pub run_id: String,
    pub entrypoint: String,
    pub status: String,
    pub created_at_ms: u64,
    pub result: Option<Value>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum DurableRunBridgeResult {
    Ready {
        run: DurableRunView,
        created: Option<bool>,
    },
    NotFound,
    IdempotencyConflict,
    UnknownEntrypoint,
    Terminal,
    DurabilityDisabled,
    Failed(String),
}

#[derive(Debug, Deserialize)]
pub struct StartDurableRunBody {
    pub entrypoint: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct DurableArtifactView {
    pub artifact_id: String,
    pub run_id: String,
    pub logical_name: String,
    pub media_type: String,
    pub size: u64,
    pub created_at_ms: u64,
    pub sha256: String,
}

#[derive(Debug)]
pub enum DurableArtifactBridgeResult {
    List(Vec<DurableArtifactView>),
    Content {
        artifact: DurableArtifactView,
        bytes: Vec<u8>,
    },
    NotFound,
    DurabilityDisabled,
    Failed(String),
}

/// Immediate originator-side disposition for the delegated-restart
/// management endpoint. A successful response means the wire request
/// was queued; Ack/Result completion remains observable through the
/// delegated-restart audit stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegatedRestartDispatchResult {
    Accepted { correlation_id: u64, peer: String },
    Draining,
    UnknownPeer,
    CapabilityNotNegotiated,
    OutboundUnavailable,
}

/// JSON request accepted by `POST /v1/admin/cluster/delegated-restart`.
/// The endpoint always stamps the wire reason as `operator_request`;
/// callers cannot impersonate supervisor/reload/health-check causes.
#[derive(Debug, Clone, Deserialize)]
pub struct DelegatedRestartBody {
    pub peer: String,
    pub child_node_id: String,
}

#[derive(Debug, Clone)]
struct AuthenticatedPrincipal {
    token_id: String,
    is_admin: bool,
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
    /// **M-Prod sub-track #5.** Cluster-membership signal for
    /// `/v1/readyz`. `None` means the daemon was started without
    /// a cluster handle (test / engine-free path); the readiness
    /// predicate falls back to the bridge + first-batch check
    /// only. `Some(probe)` carries a cheap closure that returns
    /// `true` when the cluster is "ready" (no peers configured OR
    /// at least one peer dialed) so the same probe shape works for
    /// both single-node and multi-node deployments.
    pub cluster_readyz: Option<Arc<ClusterReadyzProbe>>,
    /// Present only for TLS-enabled listeners. Readiness returns
    /// `tls_rotation_in_progress` while any reload guard is alive.
    pub tls_readiness: Option<crate::TlsReadiness>,
    /// **M-Prod sub-track #1 Phase A.** Hash-chained tamper-evident
    /// audit sink. `None` for the default test + embedding path:
    /// the existing `target="graphreact::observer::audit"` tracing
    /// stream remains the sole audit surface and behavior is
    /// bit-for-bit identical to the pre-M-Prod baseline. `Some(sink)`
    /// fans every `audit::log_*` call to the sink in parallel, where
    /// the sink stamps `{seq, ts_ms, prev_hash, kind, fields,
    /// this_hash}` and writes one NDJSON line. Wired by
    /// `graphreact-cli` when the operator passes
    /// `--observer-audit-log <PATH>`. See `docs/backend/m-prod-auth.md`
    /// for the Phase B/C/D roadmap.
    pub audit_sink: Option<Arc<dyn crate::AuditSink>>,
    /// **M-Prod sub-track #4 Phase α.** Drain bit shared with the
    /// CLI's [`crate::DrainFlag::start`] orchestrator. `None` for
    /// legacy / test callers that spawned through [`spawn`] or
    /// [`spawn_with_cluster_readyz`] (drain remains a no-op on
    /// those paths — the daemon's normal `shutdown()` is the only
    /// teardown). `Some(flag)` enables: (a) `/v1/readyz` 503ing
    /// with body `draining` as soon as the flag flips, and (b)
    /// ingress POSTs (`/v1/nodes/:id/messages`,
    /// `/v1/nodes/:id/call`, `/v1/nodes/:id/messages/bulk`) 503ing
    /// with body `draining` BEFORE bumping
    /// [`Self::ingress_in_flight`]. Mirrors the
    /// [`Self::cluster_readyz`] Option pattern shipped in #5.
    pub draining: Option<DrainFlag>,
    /// **M-Prod sub-track #4 Phase α.** Lock-free count of
    /// in-flight ingress POSTs. Each entry into
    /// `inject_message_handler` / `inject_call_handler` /
    /// `bulk_inject_handler` bumps the counter via
    /// [`IngressInFlight::enter`], which returns an RAII guard
    /// that decrements on Drop (so axum future cancellation —
    /// HTTP client disconnect, drain-deadline-driven abort —
    /// keeps the count balanced). Read by the drain orchestrator
    /// to know when ingress quiesced. Clones share state.
    pub ingress_in_flight: IngressInFlight,
    /// **M-Prod sub-track #1 Phase B Part 1.** Multi-token RBAC
    /// registry behind an atomic-swap handle. `None` keeps the
    /// legacy single-token Phase A path (see [`Self::token`])
    /// bit-for-bit; `Some(rbac)` replaces it with the multi-token
    /// + per-route ACL flow in [`auth_layer`].
    ///
    /// The handle is `Clone`-cheap (`Arc<ArcSwap<TokenRegistry>>`);
    /// the auth middleware loads one snapshot per protected request
    /// and uses it for both `match_token` and `check_route` so a
    /// rotation between the match and the check can't tear the
    /// verdict.
    ///
    /// SIGHUP-driven reload is deferred to Phase B Part 2 (rides
    /// the same Group B-2 #2 SIGHUP topology the TLS rotation path
    /// uses).
    pub rbac: Option<crate::rbac::RotatableRegistry>,
    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1c).** JWT-OIDC
    /// bridge verifier behind an atomic-swap JWKS handle. `None`
    /// keeps Phase A / Phase B Part 1 auth bit-for-bit (no JWT
    /// path); `Some(verifier)` enables eyJ-prefixed bearers to
    /// route through the JWT verify → role mapping → existing
    /// per-route ACL flow in [`auth_layer_rbac`].
    ///
    /// The JWT bridge SITS INSIDE [`auth_layer_rbac`] — it does
    /// NOT add a new layer to the rate-limit / auth / bridge ladder.
    /// Static tokens (no `eyJ` prefix) short-circuit the JWT path
    /// and dispatch through the existing
    /// [`crate::rbac::TokenRegistry`] walk unchanged.
    ///
    /// SIGHUP-driven JWKS reload rides the Group 1ab fan-out via
    /// `DaemonHandle::reload_jwks_from_disk`.
    pub jwt_verifier: Option<Arc<crate::jwt_oidc::JwtVerifier>>,
    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Audit-log
    /// forwarder hub. `None` (the legacy default) keeps the audit
    /// sink writing only to the in-process file/tracing surfaces.
    /// `Some(hub)` enables a fan-out: every successful audit
    /// append is also delivered to every registered AuditForwarder
    /// via bounded mpsc channels. Shared with the
    /// [`crate::FileAuditSink`] via
    /// [`crate::FileAuditSink::with_forwarder_hub`]; surfaced on
    /// [`ApiState`] so the SIGHUP-driven daemon-side
    /// `rotate_audit_forwarder_spool` can reach the spool
    /// forwarder without re-investigating SpawnOptions.
    pub forwarder_hub: Option<Arc<crate::audit_forwarders::ForwarderHub>>,
}

/// **M-Prod sub-track #5.** Cluster-readiness probe shared by the
/// `/v1/readyz` handler. Held behind `Arc` so the management API
/// can clone it into every per-request `ApiState`. The closure
/// captures `(NodeRegistry, configured_peers: usize)` from the
/// caller; the call site (`commands.rs::start_daemon`) freezes the
/// configured-peer count at boot time so the predicate doesn't
/// false-positive on a partition that loses every peer.
///
/// The probe returns a boolean (rather than a richer reason
/// struct) because `/v1/readyz` only needs a binary go/no-go;
/// per-reason instrumentation is a sub-track #5 follow-on (open
/// work in [`m-prod-observability.md`](../../../../docs/backend/m-prod-observability.md)).
pub type ClusterReadyzProbe = dyn Fn() -> bool + Send + Sync;

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

/// **M-Prod sub-track #6 Phase A.** Default sustained request rate
/// per source IP for the protected `/v1/*` routes, in requests per
/// second. Tunable at boot via the `GRX_RATE_LIMIT_PER_IP_PER_SEC`
/// env var (positive integer). The picked baseline (50 req/s)
/// comfortably absorbs a single human-driven `curl` + a
/// modest-batch script while still pinning a malicious flood that
/// would otherwise burn auth CPU. NAT'd egress (corporate
/// gateway → many users) shares one bucket — see
/// [`m-prod-rate-limit.md`](../../../../docs/backend/m-prod-rate-limit.md)
/// for the Phase B per-token-key plan that lifts the NAT
/// limitation.
pub const DEFAULT_RATE_LIMIT_PER_IP_PER_SEC: u64 = 50;

/// **M-Prod sub-track #6 Phase A.** Default burst size — token
/// bucket capacity. 100 = 2× the sustained per-second rate, the
/// classic token-bucket convention that absorbs micro-bursts (a
/// page-load fanning ~30 parallel requests is well under cap)
/// while still rejecting sustained floods. Tunable at boot via
/// the `GRX_RATE_LIMIT_PER_IP_BURST` env var.
pub const DEFAULT_RATE_LIMIT_PER_IP_BURST: u32 = 100;

/// **M-Prod sub-track #6 Phase B Part 1.** Default sustained
/// request rate per AUTHENTICATED PRINCIPAL (per [`crate::rbac::TokenId`])
/// for the protected `/v1/*` routes, in requests per second. The
/// picked baseline (20 req/s) is TIGHTER than the per-IP cap because
/// per-token traffic is single-identity by construction — one
/// principal's normal operation rarely needs to sustain 50 req/s,
/// and a tighter cap catches stolen/leaked credentials sooner
/// without affecting legitimate operators. Tunable at boot via
/// the `GRX_RATE_LIMIT_PER_TOKEN_PER_SEC` env var.
pub const DEFAULT_RATE_LIMIT_PER_TOKEN_PER_SEC: u64 = 20;

/// **M-Prod sub-track #6 Phase B Part 1.** Default per-token burst
/// — token bucket capacity per principal. 40 = 2× the sustained
/// per-second rate, mirroring the per-IP convention. Tunable at
/// boot via the `GRX_RATE_LIMIT_PER_TOKEN_BURST` env var.
pub const DEFAULT_RATE_LIMIT_PER_TOKEN_BURST: u32 = 40;

/// **M-Prod sub-track #6 Phase A.** Static table of protected
/// route patterns that map raw URI paths back to bounded labels
/// for the `grx_rate_limit_blocked_total{route}` counter + the
/// `rate_limit_blocked` audit event's `route` field.
///
/// The rate-limit observer middleware sits OUTSIDE the axum
/// router (so it can fire on `tower_governor` 429s before any
/// route match happens), so `MatchedPath` is NOT in extensions on
/// the request path. Instead, the middleware uses
/// [`classify_protected_path`] which performs a small fixed-shape
/// match against the 17 known patterns + an `unknown` sentinel for
/// anything outside the table (e.g. an operator typoing `/v1/foo`).
///
/// Cardinality bound is exactly `17 + 1 = 18` for the rate-limit
/// metric family — well under the conventional Prometheus
/// per-family soft cap of ~10k.
pub const PROTECTED_ROUTE_LABELS: &[&str] = &[
    "/v1/admin/cluster/delegated-restart",
    "/v1/identity",
    "/v1/metrics",
    "/v1/runs",
    "/v1/runs/:run_id",
    "/v1/runs/:run_id/cancel",
    "/v1/runs/:run_id/artifacts",
    "/v1/artifacts/:artifact_id",
    "/v1/nodes",
    "/v1/nodes/:id",
    "/v1/nodes/:id/messages",
    "/v1/nodes/:id/messages/recent",
    "/v1/nodes/:id/messages/bulk",
    "/v1/nodes/:id/call",
    "/v1/supervisor/tree",
    "/v1/observer/events",
    "/v1/observer/events.sse",
];

/// **M-Prod sub-track #6 Phase A.** Classify a raw URI path to one
/// of the [`PROTECTED_ROUTE_LABELS`] patterns (or `"unknown"`).
///
/// Used by the rate-limit observer middleware to emit a bounded
/// `route` label on the `grx_rate_limit_blocked_total{route}`
/// counter + the `rate_limit_blocked` audit event. Bounded
/// cardinality is the operator contract — without this mapping,
/// labelling by the raw path would explode by the `:id` cardinality
/// (one entry per distinct node id ever rate-limited).
///
/// Returns the matching pattern as a `&'static str` so the metric
/// callsite can cheap-store it without per-call allocation.
#[must_use]
pub fn classify_protected_path(path: &str) -> &'static str {
    match path {
        "/v1/admin/cluster/delegated-restart" => "/v1/admin/cluster/delegated-restart",
        "/v1/identity" => "/v1/identity",
        "/v1/metrics" => "/v1/metrics",
        "/v1/runs" => "/v1/runs",
        "/v1/nodes" => "/v1/nodes",
        "/v1/supervisor/tree" => "/v1/supervisor/tree",
        "/v1/observer/events" => "/v1/observer/events",
        "/v1/observer/events.sse" => "/v1/observer/events.sse",
        p if p.starts_with("/v1/nodes/") => {
            let tail = &p["/v1/nodes/".len()..];
            // `tail` is `<id>` or `<id>/<subpath>`. Strip the id.
            if let Some(slash_idx) = tail.find('/') {
                let subpath = &tail[slash_idx + 1..];
                match subpath {
                    "messages" => "/v1/nodes/:id/messages",
                    "messages/recent" => "/v1/nodes/:id/messages/recent",
                    "messages/bulk" => "/v1/nodes/:id/messages/bulk",
                    "call" => "/v1/nodes/:id/call",
                    _ => "unknown",
                }
            } else if tail.is_empty() {
                // `/v1/nodes/` (trailing slash) — not a route we
                // registered; classify as unknown rather than
                // collapse to `/v1/nodes` which IS a registered
                // pattern.
                "unknown"
            } else {
                "/v1/nodes/:id"
            }
        }
        p if p.starts_with("/v1/runs/") => {
            let tail = &p["/v1/runs/".len()..];
            if !tail.is_empty() && !tail.contains('/') {
                "/v1/runs/:run_id"
            } else if tail
                .strip_suffix("/cancel")
                .is_some_and(|run_id| !run_id.is_empty() && !run_id.contains('/'))
            {
                "/v1/runs/:run_id/cancel"
            } else if tail
                .strip_suffix("/artifacts")
                .is_some_and(|run_id| !run_id.is_empty() && !run_id.contains('/'))
            {
                "/v1/runs/:run_id/artifacts"
            } else {
                "unknown"
            }
        }
        p if p.starts_with("/v1/artifacts/") => {
            let artifact_id = &p["/v1/artifacts/".len()..];
            if !artifact_id.is_empty() && !artifact_id.contains('/') {
                "/v1/artifacts/:artifact_id"
            } else {
                "unknown"
            }
        }
        _ => "unknown",
    }
}

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
    /// **M-Prod sub-track #2 Phase A.** Rotatable TLS handle shared
    /// with the accept loop. `None` when the observer was spawned
    /// without TLS at bind time (loopback default). Drives
    /// [`Self::reload_tls`] — operators (Phase B SIGHUP, Phase C
    /// ACME, today's tests) call that method to swap in a new
    /// cert without restarting the daemon.
    pub tls: Option<crate::RotatableServerConfig>,
    /// Shared TLS reload readiness state. `None` on plaintext
    /// loopback listeners, which have no certificate prerequisite.
    pub tls_readiness: Option<crate::TlsReadiness>,
    /// **M-Prod sub-track #1 Phase A.** Audit sink wired through
    /// [`SpawnOptions::audit_sink`]. Surfaced on the handle so
    /// [`Self::reload_tls`] can fan the
    /// `tls_reload_{attempt,success,failure}` records to the same
    /// sink the accept-loop bind event lands on. `None` keeps the
    /// legacy tracing-only audit surface bit-for-bit unchanged.
    pub audit_sink: Option<Arc<dyn crate::AuditSink>>,
    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Rotatable
    /// RBAC registry surfaced so the SIGHUP-driven daemon-side
    /// `reload_tokens_from_disk` can call `rbac.rotate(...)`
    /// symmetrically to how `reload_tls_from_disk` calls
    /// `tls.rotate(...)`. `None` when no RBAC was wired at
    /// `spawn_with_options` time — SIGHUP reload-tokens is then a
    /// silent no-op.
    pub rbac: Option<crate::rbac::RotatableRegistry>,
    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1c).** JWT-OIDC
    /// bridge verifier surfaced so the daemon-side
    /// `reload_jwks_from_disk` can reach the inner
    /// [`crate::jwt_oidc::RotatableJwks`] handle and call
    /// `jwks().rotate(...)` symmetrically to how
    /// `reload_tokens_from_disk` calls `rbac.rotate(...)`. `None`
    /// when no JWT bridge was wired at `spawn_with_options` time —
    /// SIGHUP JWKS reload is then a silent no-op.
    pub jwt_verifier: Option<Arc<crate::jwt_oidc::JwtVerifier>>,
    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Audit-log
    /// forwarder hub surfaced on the API handle so the SIGHUP-
    /// driven daemon-side `rotate_audit_forwarder_spool` can call
    /// `hub.spool_forwarder().rotate()` symmetrically to how
    /// `reload_tokens_from_disk` calls `rbac.rotate(...)`. `None`
    /// when no forwarder hub was wired at `spawn_with_options`
    /// time — SIGHUP spool rotation is then a silent no-op. Also
    /// drives drain composition inside
    /// `OBSERVER_SHUTDOWN_DEADLINE`.
    pub forwarder_hub: Option<Arc<crate::audit_forwarders::ForwarderHub>>,
    /// **M-Prod sub-track #4 Phase α.** Drain bit shared with the
    /// daemon's [`crate::DrainFlag::start`] orchestrator. `None`
    /// for spawn paths that didn't wire a drain bit
    /// ([`spawn`] / [`spawn_with_cluster_readyz`]); `Some(flag)`
    /// for [`spawn_with_drain`] (called by `start_daemon`). The
    /// orchestrator flips this flag, the management API
    /// `/v1/readyz` + ingress middlewares observe the flip and
    /// start 503ing with body `draining`.
    pub draining: Option<DrainFlag>,
    /// **M-Prod sub-track #4 Phase α.** In-flight ingress POST
    /// counter shared with the management API handlers. Always
    /// present; clones share the underlying `Arc<AtomicUsize>`.
    /// Read by the drain orchestrator to know when ingress
    /// quiesced.
    pub ingress_in_flight: IngressInFlight,
}

impl ApiServerHandle {
    /// Signal shutdown + wait for the accept-loop task to finish.
    pub async fn shutdown(self) {
        self.shutdown.notify_one();
        let _ = self.task.await;
    }

    /// **M-Prod sub-track #2 Phase A.** Atomic-swap rotation of the
    /// observer's TLS identity. Parses the supplied PEM-encoded
    /// cert + key, builds a fresh `rustls::ServerConfig`
    /// (preserving the prior ALPN list), and stores it through the
    /// rotatable handle. The NEXT accepted connection serves the
    /// new identity; in-flight handshakes complete against the old
    /// `Arc<ServerConfig>` they snapshotted at `acceptor.accept()`
    /// time — no torn handshake, no use-after-free.
    ///
    /// Failure modes (cert parse / key parse / rustls build / no TLS
    /// configured) are surfaced as [`crate::TlsRotationError`].
    /// **The rotation is gated on the new config building cleanly**:
    /// on failure, the old config keeps serving + an audit event of
    /// shape `event = "tls_reload_failure"` is emitted. On success
    /// the audit event is `event = "tls_reloaded"` with the SHA-256
    /// fingerprint of the new leaf cert.
    ///
    /// Cert + key bytes are NEVER written to the audit log (only
    /// `cert_bytes_len` / `key_bytes_len` on the attempt event +
    /// the SHA-256 fingerprint on success).
    pub fn reload_tls(
        &self,
        cert_pem: &[u8],
        key_pem: &[u8],
    ) -> Result<(), crate::TlsRotationError> {
        let _rotation = self.begin_tls_rotation();
        let sink = self.audit_sink.as_ref();
        crate::audit::log_tls_reload_attempt(cert_pem.len(), key_pem.len(), sink);
        let Some(handle) = self.tls.as_ref() else {
            crate::audit::log_tls_reload_failure(
                "not_configured",
                "observer was spawned without TLS at bind time",
                sink,
            );
            return Err(crate::TlsRotationError::NotConfigured);
        };
        // Compute the leaf-DER fingerprint FROM the supplied PEM
        // bytes before we build the rustls config — `rustls`'s
        // `ServerConfig` doesn't expose the cert chain after
        // construction (the cert resolver is opaque) so we have to
        // hash on the way in. Hashing the DER (not the PEM ASCII
        // armor) makes the digest match `openssl x509 -noout
        // -fingerprint -sha256 -in cert.pem` byte-for-byte.
        let leaf_fingerprint = leaf_cert_sha256_from_pem(cert_pem);
        let current = handle.current();
        let next = match crate::ServerTlsConfig::build_rotated_server_config(
            cert_pem, key_pem, &current,
        ) {
            Ok(cfg) => cfg,
            Err(err) => {
                let kind = match &err {
                    crate::TlsRotationError::CertParse { .. } => "cert_parse",
                    crate::TlsRotationError::EmptyCertChain => "empty_chain",
                    crate::TlsRotationError::KeyParse { .. } => "key_parse",
                    crate::TlsRotationError::Rustls { .. } => "rustls",
                    crate::TlsRotationError::NotConfigured => "not_configured",
                };
                crate::audit::log_tls_reload_failure(kind, &err.to_string(), sink);
                return Err(err);
            }
        };
        handle.rotate(next);
        // Fingerprint may be `None` if the leaf cert PEM parse
        // step inside `leaf_cert_sha256_from_pem` failed — but in
        // that case `build_rotated_server_config` would have
        // failed first and we'd never reach this branch. Still,
        // be defensive: surface a sentinel so operators see *some*
        // audit record on every successful rotation.
        let fp = leaf_fingerprint.as_deref().unwrap_or("unavailable");
        crate::audit::log_tls_reload_success(fp, sink);
        Ok(())
    }

    /// Mark the complete TLS reload window (including caller-side
    /// cert/key reads) as not-ready. Nested with `reload_tls` safely.
    #[must_use]
    pub fn begin_tls_rotation(&self) -> Option<crate::TlsRotationGuard> {
        self.tls_readiness
            .as_ref()
            .map(crate::TlsReadiness::begin_rotation)
    }
}

/// **M-Prod sub-track #2 Phase A.** Hex-encoded SHA-256 of the
/// leaf cert's DER bytes. Returns `None` when the PEM doesn't
/// parse — callers fall back to a sentinel string. Matches
/// `openssl x509 -fingerprint -sha256 -noout -in cert.pem` output
/// (lowercase, no colons).
fn leaf_cert_sha256_from_pem(cert_pem: &[u8]) -> Option<String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::CertificateDer;
    let mut iter = CertificateDer::pem_slice_iter(cert_pem);
    let first = iter.next()?.ok()?;
    Some(sha256_hex(first.as_ref()))
}

/// Lowercase hex SHA-256 digest of `bytes`. Uses `ring` directly
/// (already in the dep tree via `rustls`'s `ring` feature; declared
/// crate-locally in `Cargo.toml` so it's a documented direct dep).
/// Output matches `openssl x509 -fingerprint -sha256 -noout`
/// byte-for-byte for the leaf cert's DER bytes.
fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let d = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut s = String::with_capacity(64);
    for byte in d.as_ref() {
        write!(s, "{byte:02x}").expect("write to String never fails");
    }
    s
}

/// **M-Prod sub-track #6 Phase A.** Per-IP rate-limit knob struct
/// used by [`build_router_with_rate_limit`]. The legacy
/// [`build_router`] entry point falls through to
/// [`RateLimitConfig::from_env`] so production daemons (which
/// build the router via [`spawn_with_options`]) automatically pick
/// up `GRX_RATE_LIMIT_PER_IP_{PER_SEC,BURST}` without a callsite
/// change.
///
/// Tests use [`Self::for_tests`] to inject a deterministic
/// cap (low `per_second`, tiny `burst`) so a burst test reliably
/// trips the limiter inside a sub-second window — without
/// polluting process-global env state across parallel `cargo test`
/// runners.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitConfig {
    /// Sustained per-source-IP request rate (tokens/sec).
    pub per_second: u64,
    /// Per-source-IP burst size (token bucket capacity).
    pub burst: u32,
    /// **M-Prod sub-track #6 Phase B Part 1.** Sustained
    /// per-authenticated-principal request rate (tokens/sec). When
    /// RBAC is wired ([`SpawnOptions::rbac = Some(...)`]) each
    /// [`crate::rbac::TokenId`] gets its own bucket keyed by the
    /// custom [`crate::rate_limit_keys::TokenIdKeyExtractor`]. When
    /// RBAC is NOT wired the per-token layer is effectively dormant
    /// — every request shares the `NoMatch` bucket which is bounded
    /// by this rate as a coarse safety net.
    pub per_token_per_sec: u64,
    /// **M-Prod sub-track #6 Phase B Part 1.** Per-principal burst.
    pub per_token_burst: u32,
}

impl RateLimitConfig {
    /// Read defaults from env once at build time. Falls back to
    /// the in-code constants for unset / empty / unparseable /
    /// zero values. This is the production path —
    /// [`spawn_with_options`] threads `Self::from_env()` through
    /// [`build_router`].
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            per_second: parse_env_u64(
                "GRX_RATE_LIMIT_PER_IP_PER_SEC",
                DEFAULT_RATE_LIMIT_PER_IP_PER_SEC,
            ),
            burst: parse_env_u32(
                "GRX_RATE_LIMIT_PER_IP_BURST",
                DEFAULT_RATE_LIMIT_PER_IP_BURST,
            ),
            per_token_per_sec: parse_env_u64(
                "GRX_RATE_LIMIT_PER_TOKEN_PER_SEC",
                DEFAULT_RATE_LIMIT_PER_TOKEN_PER_SEC,
            ),
            per_token_burst: parse_env_u32(
                "GRX_RATE_LIMIT_PER_TOKEN_BURST",
                DEFAULT_RATE_LIMIT_PER_TOKEN_BURST,
            ),
        }
    }

    /// **For tests only.** Hand-pick a per-IP `(per_second, burst)`
    /// pair while leaving the per-token layer effectively dormant
    /// (`per_token_per_sec = 1`, `per_token_burst = 1_000_000` — the
    /// burst is huge enough no sub-second test ever exhausts the
    /// per-token bucket). Note: `per_second` on the governor builder
    /// is the PERIOD in seconds between token replenishes, NOT
    /// requests-per-second; we pick `1` so the refill rate is "1 token
    /// every second" — the bucket starts full at `burst` and the
    /// burst easily covers anything a sub-second integration test
    /// throws at it.
    ///
    /// Existing Phase A tests use this arity-2 helper unchanged;
    /// Phase B tests opt in via [`Self::for_tests_with_per_token`].
    ///
    /// Production callers should use [`Self::from_env`].
    #[must_use]
    pub const fn for_tests(per_second: u64, burst: u32) -> Self {
        Self {
            per_second,
            burst,
            per_token_per_sec: 1,
            per_token_burst: 1_000_000,
        }
    }

    /// **M-Prod sub-track #6 Phase B Part 1, for tests only.**
    /// Like [`Self::for_tests`] but pins ALL FOUR knobs — per-IP +
    /// per-token. The Phase B per-token integration tests use this
    /// to exercise the per-token bucket with a deterministic cap
    /// while leaving the per-IP cap loose.
    #[must_use]
    pub const fn for_tests_with_per_token(
        per_second: u64,
        burst: u32,
        per_token_per_sec: u64,
        per_token_burst: u32,
    ) -> Self {
        Self {
            per_second,
            burst,
            per_token_per_sec,
            per_token_burst,
        }
    }
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            per_second: DEFAULT_RATE_LIMIT_PER_IP_PER_SEC,
            burst: DEFAULT_RATE_LIMIT_PER_IP_BURST,
            per_token_per_sec: DEFAULT_RATE_LIMIT_PER_TOKEN_PER_SEC,
            per_token_burst: DEFAULT_RATE_LIMIT_PER_TOKEN_BURST,
        }
    }
}

/// Build the management API router. The router is the public seam
/// for tests + future P5+ extensions; spawning the listener is
/// a separate concern (see [`spawn`]).
///
/// **M-Prod sub-track #6 Phase A.** Defaults the rate limit
/// configuration to [`RateLimitConfig::from_env`] — the production
/// path. Tests that need a deterministic cap call
/// [`build_router_with_rate_limit`] directly.
pub fn build_router(state: ApiState) -> Router {
    build_router_with_rate_limit(state, RateLimitConfig::from_env())
}

/// **M-Prod sub-track #6 Phase A.** Variant of [`build_router`]
/// that accepts an explicit [`RateLimitConfig`]. Used by the
/// rate-limit integration tests to pin a deterministic cap
/// without polluting the env-var process state across parallel
/// test runners.
///
/// **Phase B Part 1 (Group B-4a).** This thunks through
/// [`build_router_with_rate_limit_and_proxies`] with
/// `trusted_proxies = None` so `X-Forwarded-For` is IGNORED —
/// bit-for-bit Phase A behavior. The new signature lives at
/// [`build_router_with_rate_limit_and_proxies`].
pub fn build_router_with_rate_limit(state: ApiState, rl: RateLimitConfig) -> Router {
    build_router_with_rate_limit_and_proxies(state, rl, None)
}

/// **M-Prod sub-track #6 Phase B Part 1 (Group B-4a).** Variant of
/// [`build_router_with_rate_limit`] that ALSO accepts an explicit
/// [`crate::rate_limit_keys::TrustedProxyConfig`] for `X-Forwarded-For`
/// opt-in honoring.
///
/// `trusted_proxies = None` keeps the per-IP layer keyed on the TCP
/// peer (= Phase A behavior). `Some(cfg)` enables the injector to
/// resolve a trusted-proxy `X-Forwarded-For` leftmost IP into an
/// [`crate::rate_limit_keys::EffectivePeerIp`] request extension —
/// the per-IP extractor then keys on the resolved client IP rather
/// than the proxy's TCP IP. The trust gate is the load-bearing
/// silent-spoofing footgun guard.
///
/// **Layer ordering (outermost → innermost):**
///
/// 1. `rate_limit_observer` — captures peer + path classification
///    AFTER the chain runs so a 429 returned by either governor below
///    fires the audit event + metric. Reads the
///    `RateLimitDeniedBucket` response extension stamped by the
///    governor's error_handler to disambiguate per-IP vs per-token vs
///    per-token-no-match.
/// 2. `rate_limit_injector` — Phase B Part 1 state-injector.
///    Reads `ApiState.rbac` (snapshotted via `Arc::clone`) +
///    trusted-proxy config, matches the bearer token ONCE in
///    constant time, and inserts `PerRequestRateLimitCtx`
///    (precomputed `TokenIdKey` + bucket hint) plus
///    `EffectivePeerIp` (resolved client IP when `X-Forwarded-For`
///    is honored). Sits OUTSIDE both governor layers so the
///    extensions are present when their `extract` closures run.
/// 3. `token_governor_layer` — per-token bucket. Tighter cap; reads
///    [`crate::rate_limit_keys::TokenIdKeyExtractor`].
/// 4. `governor_layer` — per-IP bucket (now trusted-proxy aware).
///    Reads [`crate::rate_limit_keys::TrustedProxyAwarePeerIpExtractor`]
///    which falls back to `ConnectInfo<SocketAddr>` if no
///    `EffectivePeerIp` extension was set (= Phase A semantics).
/// 5. `auth_layer` — bearer-token check. Wrapped per-route via
///    `route_layer` so it composes with the merged router's CORS
///    layer without affecting the public branch.
///
/// **Denial precedence:** the OUTER layer fires its 429 FIRST, so
/// the order above means `per_token > per_ip`. A request blocked by
/// the per-token cap never reaches the per-IP layer. This matches
/// the operator contract — when a known principal exceeds their
/// tighter per-token quota, the 429 attributes the block to
/// `bucket="per_token"` so operators can find the noisy principal in
/// the audit log without sifting through IP-keyed buckets.
pub fn build_router_with_rate_limit_and_proxies(
    state: ApiState,
    rl: RateLimitConfig,
    trusted_proxies: Option<Arc<crate::rate_limit_keys::TrustedProxyConfig>>,
) -> Router {
    use axum::middleware;

    // /v1/healthz bypasses auth (liveness probe-friendly).
    // /v1/readyz also bypasses auth — readiness probes ride the same
    // path-bypass shape as liveness; the readiness predicate is
    // derived from MetricsInner (scheduler-batch progress) +
    // snapshot bridge capacity, so neither leaks secrets nor
    // requires per-probe authentication. **M-Prod sub-track #5
    // safest slice.**
    let public_routes = Router::new()
        .route("/v1/healthz", get(healthz))
        .route("/v1/readyz", get(readyz));

    // **M-Prod sub-track #6 Phase A → Phase B Part 1.** Per-IP rate
    // limiter for protected routes. Now keyed by
    // `TrustedProxyAwarePeerIpExtractor` (Phase B Part 1) instead of
    // the upstream `PeerIpKeyExtractor`. Behavior is bit-for-bit
    // identical when no `EffectivePeerIp` extension is present —
    // the new extractor's fallback path reads `ConnectInfo<SocketAddr>`
    // exactly the same way as the upstream.
    //
    // `use_headers()` enables the `x-ratelimit-*` family + `retry-after`;
    // our custom `error_handler` (below) re-emits a matching
    // `Retry-After` explicitly so the contract survives a future
    // tower_governor shape change, AND stamps the response extension
    // `RateLimitDeniedBucket("per_ip")` so the outer
    // `rate_limit_observer` can attribute the 429 to the per-IP bucket.
    let per_ip_governor_conf = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(rl.per_second)
            .burst_size(rl.burst)
            .key_extractor(crate::rate_limit_keys::TrustedProxyAwarePeerIpExtractor)
            .use_headers()
            .error_handler(|err: tower_governor::GovernorError| {
                rate_limited_response_from_error_with_bucket(err, "per_ip")
            })
            .finish()
            .expect("per-IP rate-limit config: per_second + burst_size both nonzero"),
    );
    let per_ip_governor_layer = GovernorLayer {
        config: per_ip_governor_conf,
    };

    // **M-Prod sub-track #6 Phase B Part 1.** Per-token rate
    // limiter. ONLY mounted when RBAC is wired (`state.rbac =
    // Some(...)`) — otherwise every request maps to the same
    // `NoMatch` sentinel and the per-token cap would degrade to a
    // global cap with the tighter per-token defaults (20 req/s + 40
    // burst), which would break Phase A-style legacy spawn paths
    // that only carry a single bearer token (pre-RBAC era). When
    // RBAC IS wired, this layer sits ABOVE the per-IP limiter so
    // its 429 fires FIRST — locks the `per_token > per_ip` denial
    // precedence. The custom `TokenIdKeyExtractor` reads the
    // precomputed `TokenIdKey` from the `PerRequestRateLimitCtx`
    // extension injected by `rate_limit_injector` below — so we
    // don't pay the registry walk twice (the auth_layer_rbac path
    // walks it again, but the cost is bounded by the registry size
    // <= 32 entries soft cap).
    let per_token_governor_layer = state.rbac.as_ref().map(|_| {
        let conf = Arc::new(
            GovernorConfigBuilder::default()
                .per_second(rl.per_token_per_sec)
                .burst_size(rl.per_token_burst)
                .key_extractor(crate::rate_limit_keys::TokenIdKeyExtractor)
                .use_headers()
                .error_handler(|err: tower_governor::GovernorError| {
                    rate_limited_response_from_error_with_bucket(err, "per_token")
                })
                .finish()
                .expect("per-token rate-limit config: per_second + burst_size both nonzero"),
        );
        GovernorLayer { config: conf }
    });

    // **M-Prod sub-track #6 Phase B Part 1.** State-injector
    // middleware. Sits OUTSIDE both governor layers so its
    // extensions are present when the extractors run. Snapshots
    // `state.rbac` once, walks the registry in constant time, and
    // stashes the precomputed `TokenIdKey` + bucket hint into the
    // request extensions. Also resolves the effective client IP
    // when `trusted_proxies` is wired AND the peer is in the
    // trusted set.
    let injector_state = RateLimitInjectorState {
        rbac: state.rbac.clone(),
        trusted_proxies,
        // **Verify-finding MED patch — M-Prod #1 Phase B Part 2
        // (Group 1c).** Thread the JWT verifier into the
        // rate-limit injector so per-token bucketing recognises
        // JWT principals by their verified `sub` claim. The
        // verified JWT is stashed in request extensions so
        // `auth_layer_rbac` does not re-verify.
        jwt_verifier: state.jwt_verifier.clone(),
    };

    let protected_routes = Router::new()
        .route(
            "/v1/admin/cluster/delegated-restart",
            post(delegated_restart_handler),
        )
        .route("/v1/identity", get(identity))
        .route("/v1/metrics", get(metrics))
        .route("/v1/runs", post(start_durable_run_handler))
        .route("/v1/runs/:run_id", get(get_durable_run_handler))
        .route("/v1/runs/:run_id/cancel", post(cancel_durable_run_handler))
        .route(
            "/v1/runs/:run_id/artifacts",
            get(list_run_artifacts_handler),
        )
        .route("/v1/artifacts/:artifact_id", get(get_artifact_handler))
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
        .route("/v1/nodes/:id/messages/bulk", post(bulk_inject_handler))
        .route("/v1/supervisor/tree", get(supervisor_tree))
        // M16-Backend P7 — live event stream (WebSocket primary,
        // SSE fallback). Both paths share the auth middleware +
        // `?node=` / `?type=` / `?since=` filter parsing.
        .route("/v1/observer/events", get(crate::websocket::ws_handler))
        .route(
            "/v1/observer/events.sse",
            get(crate::websocket::sse_handler),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_layer))
        // **Stack** (top-to-bottom in source = inner-to-outer in axum):
        //   per_ip_governor_layer  (deepest of the rate-limit layers)
        //   per_token_governor_layer (ONLY when RBAC is wired; above
        //                             per-IP; denial precedence
        //                             `per_token > per_ip`)
        //   rate_limit_injector    (provides extensions BEFORE either
        //                           extractor runs)
        //   rate_limit_observer    (outermost — fires audit/metric on
        //                           the final 429, reads the
        //                           `RateLimitDeniedBucket` response
        //                           extension stamped by each layer's
        //                           error_handler)
        .layer(per_ip_governor_layer);
    let protected_routes = if let Some(token_layer) = per_token_governor_layer {
        protected_routes.layer(token_layer)
    } else {
        protected_routes
    };
    let protected_routes = protected_routes
        .layer(middleware::from_fn_with_state(
            injector_state,
            rate_limit_injector,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            rate_limit_observer,
        ));

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

/// **M-Prod sub-track #6 Phase B Part 1.** State held by the
/// `rate_limit_injector` middleware. Cloned once per request via
/// `from_fn_with_state`; the inner `Option<RotatableRegistry>` is
/// already `Arc`-backed, and `Option<Arc<TrustedProxyConfig>>` is
/// just an `Arc` clone. No per-request allocation.
#[derive(Clone)]
struct RateLimitInjectorState {
    rbac: Option<crate::rbac::RotatableRegistry>,
    trusted_proxies: Option<Arc<crate::rate_limit_keys::TrustedProxyConfig>>,
    /// **Verify-finding MED patch — M-Prod #1 Phase B Part 2 (Group 1c).**
    /// JWT verifier handle so JWT-bearing requests get bucketed by
    /// the verified `sub` claim (= `TokenId`) instead of collapsing
    /// into `per_token_no_match`. The injector runs the verify
    /// FIRST, stashes the [`PreVerifiedJwt`] in request extensions
    /// for `auth_layer_rbac` to consume, and emits
    /// `TokenIdKey::Matched(TokenId(sub))` so the per-token
    /// governor enforces per-principal fairness for JWT principals
    /// the same way it does for static-token principals (B-4a
    /// contract). Single-verify cost preserved across the two
    /// middleware layers via the extension cache.
    jwt_verifier: Option<Arc<crate::jwt_oidc::JwtVerifier>>,
}

/// **Verify-finding MED patch — M-Prod #1 Phase B Part 2 (Group 1c).**
/// Request-extension marker: rate_limit_injector successfully ran
/// the JWT verify; auth_layer_rbac reads this BEFORE calling
/// `JwtVerifier::verify` and skips the second verify when present.
/// Single-verify cost preserved across the two middleware layers.
#[derive(Clone)]
pub(crate) struct PreVerifiedJwt(pub(crate) crate::jwt_oidc::VerifiedJwt);

/// **M-Prod sub-track #6 Phase B Part 1.** State-injector
/// middleware. Per-request work:
///
/// 1. Snapshot the RBAC registry once (`Arc::clone` is cheap).
/// 2. Constant-time match the `Authorization: Bearer <token>` against
///    every registry entry (mirrors [`crate::rbac::TokenRegistry::match_token`]).
/// 3. Stash a [`crate::rate_limit_keys::PerRequestRateLimitCtx`] in
///    request extensions — the per-token extractor reads it.
/// 4. Optionally resolve a trusted-proxy `X-Forwarded-For` leftmost IP
///    into [`crate::rate_limit_keys::EffectivePeerIp`] — the per-IP
///    extractor reads it.
///
/// Cost when neither RBAC nor trusted-proxies are wired: one
/// extension insert for the per-token sentinel (`NoMatch`) — the
/// per-token layer is dormant in this configuration. The per-IP
/// layer falls back to `ConnectInfo<SocketAddr>` (= Phase A).
async fn rate_limit_injector(
    State(s): State<RateLimitInjectorState>,
    mut req: axum::extract::Request,
    next: Next,
) -> Response {
    use crate::rate_limit_keys::{
        EffectivePeerIp, PerRequestRateLimitCtx, PerTokenBucketHintMarker, TokenIdKey,
    };

    // ----- per-token resolution -----
    // **Verify-finding MED patch — M-Prod #1 Phase B Part 2
    // (Group 1c).** JWT-first dispatch: when the bearer starts
    // with the JWT discriminator `eyJ` AND a verifier is wired,
    // run the verify here so per-token rate-limit bucketing
    // recognises the JWT principal (= sub claim) the same way it
    // recognises a static-token principal. The verified JWT is
    // stashed in [`PreVerifiedJwt`] for `auth_layer_rbac` to
    // consume — single-verify cost across the two middleware
    // layers. On verify failure / MissingKid / UnknownKid the
    // dispatch falls through to the static-token walk (graceful
    // coexistence during mid-rotation, matching
    // `auth_layer_rbac`'s discipline). A static-token bearer that
    // happens to start with `eyJ` would fail the JWT verify with
    // `MalformedJwt` and route through the same fall-through
    // path. Computed BEFORE the rbac fork so the JWT bridge works
    // even in a deploy that wires `jwt_verifier` without `rbac`
    // (boot WARN `jwt_config_without_rbac` already documents
    // this combo).
    let jwt_verified_token_ctx: Option<PerRequestRateLimitCtx> = {
        let header = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        let candidate = header.and_then(|v| v.strip_prefix("Bearer "));
        let mut out: Option<PerRequestRateLimitCtx> = None;
        if let (Some(c), Some(verifier)) = (candidate, s.jwt_verifier.as_ref()) {
            if c.starts_with("eyJ") {
                if let Ok(v) = verifier.verify(c) {
                    let token_id = crate::rbac::TokenId(v.sub.clone());
                    req.extensions_mut().insert(PreVerifiedJwt(v));
                    out = Some(PerRequestRateLimitCtx {
                        token_key: TokenIdKey::Matched(token_id),
                        bucket_hint: "per_token",
                    });
                }
            }
        }
        out
    };

    let token_ctx = if let Some(ctx) = jwt_verified_token_ctx {
        ctx
    } else if let Some(rbac) = s.rbac.as_ref() {
        let header = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        let candidate = header.and_then(|v| v.strip_prefix("Bearer "));
        match candidate {
            Some(c) => {
                let snap = rbac.current();
                // Constant-time walk — matches the auth-layer
                // discipline (no early-exit timing leak across
                // matched-vs-N-matched-vs-no-match).
                match snap.match_token(c.as_bytes()) {
                    Some(entry) => {
                        // **Verify-finding patch (LOW integration).**
                        // Symmetric with auth_layer_rbac: a leaked-and-
                        // expired credential MUST be attributed to the
                        // per_token_no_match bucket here so the 429
                        // bucket field is honest. Otherwise the expired
                        // credential would consume the matched
                        // principal's per-token bucket capacity AND
                        // attribute the 429 to `bucket=per_token`,
                        // even though auth_layer_rbac is about to
                        // reject it as auth_fail/reason=expired one
                        // layer down. The rate-limit gate is
                        // intentionally conservative — expired = no
                        // match, joins the global NoMatch sentinel
                        // bucket.
                        if snap.is_expired(entry, std::time::SystemTime::now()) {
                            PerRequestRateLimitCtx {
                                token_key: TokenIdKey::NoMatch,
                                bucket_hint: "per_token_no_match",
                            }
                        } else {
                            PerRequestRateLimitCtx {
                                token_key: TokenIdKey::Matched(entry.id.clone()),
                                bucket_hint: "per_token",
                            }
                        }
                    }
                    None => PerRequestRateLimitCtx {
                        token_key: TokenIdKey::NoMatch,
                        bucket_hint: "per_token_no_match",
                    },
                }
            }
            None => PerRequestRateLimitCtx {
                token_key: TokenIdKey::NoMatch,
                bucket_hint: "per_token_no_match",
            },
        }
    } else {
        // No RBAC wired — per-token layer is dormant. Every
        // request shares the NoMatch sentinel, which gets bounded
        // by the per-token cap as a global safety net. Bucket hint
        // is `per_token_no_match` so a 429 attribution is honest.
        PerRequestRateLimitCtx {
            token_key: TokenIdKey::NoMatch,
            bucket_hint: "per_token_no_match",
        }
    };
    let bucket_hint = token_ctx.bucket_hint;
    req.extensions_mut().insert(token_ctx);

    // ----- trusted-proxy X-Forwarded-For resolution -----
    if let Some(trusted) = s.trusted_proxies.as_ref() {
        let peer_ip = req
            .extensions()
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip());
        if let Some(peer_ip) = peer_ip {
            if trusted.contains(peer_ip) {
                // Read leftmost IP from X-Forwarded-For.
                if let Some(forwarded) = req
                    .headers()
                    .get("x-forwarded-for")
                    .and_then(|v| v.to_str().ok())
                {
                    if let Some(leftmost) = parse_xff_leftmost(forwarded) {
                        req.extensions_mut().insert(EffectivePeerIp(leftmost));
                    }
                }
            }
        }
    }

    let mut resp = next.run(req).await;
    // **Phase B Part 1.** Stamp the bucket-hint onto the response
    // so the outer `rate_limit_observer` can refine a per-token
    // 429 to `per_token_no_match` for unauthenticated / garbage-
    // token requests. The per-token governor's error_handler
    // closure only knows the layer-level discriminator (`"per_token"`);
    // we know the sub-discriminator because we just walked the
    // RBAC registry above.
    resp.extensions_mut()
        .insert(PerTokenBucketHintMarker(bucket_hint));
    resp
}

/// **M-Prod sub-track #6 Phase B Part 1.** Parse the leftmost IP
/// from an `X-Forwarded-For` header value. Returns `None` for
/// empty / malformed input.
///
/// Format per RFC 7239 / common usage: comma-separated list of
/// `client[, proxy1[, proxy2…]]`. Each entry is either `addr` or
/// `addr:port`. IPv6 addresses are wrapped in square brackets when a
/// port is present (`[::1]:80`); bare IPv6 is just `::1`. We try
/// parsing the raw token first, then fall back to stripping a final
/// `:port` suffix for IPv4 + bracketed IPv6.
fn parse_xff_leftmost(header_value: &str) -> Option<std::net::IpAddr> {
    let first = header_value.split(',').next()?.trim();
    if first.is_empty() {
        return None;
    }
    // Try the raw token first — covers bare IPv4 / bare IPv6.
    if let Ok(ip) = first.parse::<std::net::IpAddr>() {
        return Some(ip);
    }
    // Bracketed IPv6 with optional port: `[::1]` or `[::1]:80`.
    if let Some(rest) = first.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return rest[..end].parse::<std::net::IpAddr>().ok();
        }
    }
    // IPv4 with port: `1.2.3.4:80`.
    if let Some(idx) = first.rfind(':') {
        let addr = &first[..idx];
        return addr.parse::<std::net::IpAddr>().ok();
    }
    None
}

/// **M-Prod sub-track #6 Phase A.** Parse a positive-integer env
/// var to `u32`, falling back to `default` for unset / empty /
/// unparseable / zero. Mirrors the
/// `OBSERVER_SHUTDOWN_DEADLINE_SECS` shape from sub-track #5 so
/// operator scripts have one parsing contract across the M-Prod
/// surface.
fn parse_env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn parse_env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
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
    spawn_with_drain(bind, metrics, process_start_id, None, None).await
}

/// **M-Prod sub-track #5.** Variant of [`spawn`] that wires a
/// cluster-readiness probe through [`ApiState::cluster_readyz`].
/// Used by `start_daemon` so `/v1/readyz` reflects peer-dial
/// completion. Pass `None` for single-node / engine-free / test
/// callers (matching [`spawn`]'s legacy behaviour).
pub async fn spawn_with_cluster_readyz(
    bind: BindConfig,
    metrics: Arc<MetricsInner>,
    process_start_id: u64,
    cluster_readyz: Option<Arc<ClusterReadyzProbe>>,
) -> Result<ApiServerHandle, std::io::Error> {
    spawn_with_options(SpawnOptions {
        bind,
        metrics,
        process_start_id,
        cluster_readyz,
        audit_sink: None,
        draining: None,
        rate_limit: None,
        // **M-Prod sub-track #1 Phase B Part 1.** Legacy
        // `spawn_with_cluster_readyz` defaults rbac to `None`;
        // callers opt in via the unified `spawn_with_options`.
        rbac: None,
        // **M-Prod sub-track #6 Phase B Part 1.** Legacy spawn
        // path defaults trusted_proxies to `None` —
        // `spawn_with_options` falls back to reading
        // `GRX_TRUSTED_PROXIES` env var (or, if unset, ignores
        // `X-Forwarded-For` = Phase A behavior).
        trusted_proxies: None,
        // **M-Prod sub-track #1 Phase B Part 2 (Group 1c).**
        // Legacy spawn path defaults jwt_verifier to `None`;
        // callers opt in via the unified `spawn_with_options`.
        jwt_verifier: None,
        // **M-Prod sub-track #1 Phase B Part 2 (Group 1d).**
        // Legacy spawn path defaults forwarder_hub to `None`;
        // callers opt in via the unified `spawn_with_options`.
        forwarder_hub: None,
    })
    .await
}

/// **M-Prod sub-track #4 Phase α.** Variant of
/// [`spawn_with_cluster_readyz`] that ALSO wires the drain bit
/// through [`ApiState::draining`] (so `/v1/readyz` flips to 503
/// `draining` on drain-start) and the in-flight ingress counter
/// shared with the CLI orchestrator. Used by `start_daemon` once
/// SIGTERM-driven graceful drain is the canonical lifecycle.
///
/// Pass `None` for `draining` on legacy / test paths — drain
/// becomes a no-op on those (the daemon's normal `shutdown()` is
/// the only teardown). [`spawn`] and
/// [`spawn_with_cluster_readyz`] both default this to `None` for
/// back-compat with existing call sites.
pub async fn spawn_with_drain(
    bind: BindConfig,
    metrics: Arc<MetricsInner>,
    process_start_id: u64,
    cluster_readyz: Option<Arc<ClusterReadyzProbe>>,
    draining: Option<DrainFlag>,
) -> Result<ApiServerHandle, std::io::Error> {
    spawn_with_options(SpawnOptions {
        bind,
        metrics,
        process_start_id,
        cluster_readyz,
        audit_sink: None,
        draining,
        rate_limit: None,
        // **M-Prod sub-track #1 Phase B Part 1.** Legacy
        // `spawn_with_drain` defaults rbac to `None`; callers opt
        // in via the unified `spawn_with_options`.
        rbac: None,
        // **M-Prod sub-track #6 Phase B Part 1.** Legacy spawn
        // path defaults trusted_proxies to `None` —
        // `spawn_with_options` falls back to reading
        // `GRX_TRUSTED_PROXIES`.
        trusted_proxies: None,
        // **M-Prod sub-track #1 Phase B Part 2 (Group 1c).**
        // Legacy spawn path defaults jwt_verifier to `None`.
        jwt_verifier: None,
        // **M-Prod sub-track #1 Phase B Part 2 (Group 1d).**
        // Legacy spawn path defaults forwarder_hub to `None`.
        forwarder_hub: None,
    })
    .await
}

/// **M-Prod sub-track #1 Phase A + #4 Phase α.** Knob struct
/// passed to [`spawn_with_options`]. New options are additive —
/// existing callers either keep using [`spawn`] /
/// [`spawn_with_cluster_readyz`] / [`spawn_with_drain`] (which
/// default the new fields) or migrate to this struct when they
/// need the full capability set.
pub struct SpawnOptions {
    pub bind: BindConfig,
    pub metrics: Arc<MetricsInner>,
    pub process_start_id: u64,
    pub cluster_readyz: Option<Arc<ClusterReadyzProbe>>,
    /// **M-Prod sub-track #1 Phase A.** Hash-chained audit sink.
    /// `None` keeps the legacy tracing-only audit surface. `Some`
    /// fans every `audit::log_*` call to the sink in parallel.
    pub audit_sink: Option<Arc<dyn crate::AuditSink>>,
    /// **M-Prod sub-track #4 Phase α.** Drain bit. `None` keeps
    /// drain a no-op on the spawned daemon — appropriate for
    /// legacy and test paths. `Some(flag)` lets the CLI's
    /// [`crate::DrainFlag::start`] orchestrator flip the bit and
    /// have the management API observe it on the readyz + ingress
    /// hot paths.
    pub draining: Option<DrainFlag>,
    /// **M-Prod sub-track #6 Phase A.** Per-IP rate-limit override.
    /// `None` reads [`RateLimitConfig::from_env`] — production
    /// shape. `Some(cfg)` pins the cap explicitly (used by the
    /// rate-limit integration tests so parallel `cargo test`
    /// runners don't collide on `GRX_RATE_LIMIT_PER_IP_*` env
    /// vars).
    pub rate_limit: Option<RateLimitConfig>,
    /// **M-Prod sub-track #1 Phase B Part 1.** Multi-token RBAC
    /// registry behind an atomic-swap handle. `None` (the legacy
    /// default for [`spawn`] / [`spawn_with_cluster_readyz`] /
    /// [`spawn_with_drain`]) keeps the Phase A single-token path
    /// bit-for-bit. `Some(rbac)` replaces the [`auth_layer`]
    /// dispatch with the multi-token + per-route ACL flow.
    ///
    /// SIGHUP-driven reload defers to Phase B Part 2.
    pub rbac: Option<crate::rbac::RotatableRegistry>,
    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1c).** JWT-OIDC
    /// bridge verifier. `None` (the legacy default) keeps the JWT
    /// path off. `Some(verifier)` enables the bridge: eyJ-prefixed
    /// bearers dispatch through [`crate::jwt_oidc::JwtVerifier::verify`]
    /// before the static-token walk.
    ///
    /// The verifier itself owns the [`crate::jwt_oidc::RotatableJwks`]
    /// handle (so the daemon-side SIGHUP fan-out can rotate JWKS
    /// without re-investigating SpawnOptions).
    pub jwt_verifier: Option<Arc<crate::jwt_oidc::JwtVerifier>>,
    /// **M-Prod sub-track #6 Phase B Part 1 (Group B-4a).** Trusted-
    /// proxy CIDR set governing whether `X-Forwarded-For` is honored
    /// for the per-IP rate-limit key.
    ///
    /// `None` (the production default unless an operator wires
    /// `GRX_TRUSTED_PROXIES`) → `X-Forwarded-For` is IGNORED; the
    /// per-IP bucket keys on the TCP peer IP (`ConnectInfo<SocketAddr>`),
    /// bit-for-bit Phase A behavior.
    ///
    /// `Some(cfg)` → `X-Forwarded-For` is honored ONLY when the TCP
    /// peer is in `cfg`. This is the silent-spoofing footgun guard
    /// — a request arriving from an untrusted peer carrying
    /// `X-Forwarded-For: <any IP>` keys on the peer's IP (not the
    /// header), so an attacker can't bypass the per-IP cap by
    /// crafting the header.
    ///
    /// Production path: `spawn_with_options` defaults this to
    /// `TrustedProxyConfig::parse(env!("GRX_TRUSTED_PROXIES"))` when
    /// the caller passes `None`. Tests pin a deterministic config
    /// here (avoiding the env-var race across parallel runners).
    pub trusted_proxies: Option<Arc<crate::rate_limit_keys::TrustedProxyConfig>>,
    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Audit-log
    /// forwarder hub. `None` (the legacy default) keeps audit
    /// fan-out limited to the in-process file + tracing surfaces.
    /// `Some(hub)` enables per-forwarder bounded mpsc + worker
    /// fan-out for downstream sidecars (fluentd / vector /
    /// promtail / opentelemetry-collector / fluent-bit /
    /// aws-for-fluent-bit / google-cloud-ops-agent), which then
    /// ship to S3 / Loki / GCS / OpenSearch / Splunk / Datadog
    /// without the daemon ever needing its own outbound HTTP path.
    /// The hub itself owns the `Vec<Arc<dyn AuditForwarder>>` list
    /// + per-forwarder worker tasks.
    pub forwarder_hub: Option<Arc<crate::audit_forwarders::ForwarderHub>>,
}

/// **M-Prod sub-track #1 Phase A + #4 Phase α.** Full-knob
/// variant of [`spawn`] accepting a [`SpawnOptions`]. Mirrors
/// [`spawn_with_cluster_readyz`] plus the `audit_sink` and
/// `draining` fields; this is the call shape `graphreact-cli`
/// uses when `--observer-audit-log` and/or SIGTERM drain are
/// wired.
pub async fn spawn_with_options(opts: SpawnOptions) -> Result<ApiServerHandle, std::io::Error> {
    let SpawnOptions {
        bind,
        metrics,
        process_start_id,
        cluster_readyz,
        audit_sink,
        draining,
        rate_limit,
        rbac,
        trusted_proxies,
        jwt_verifier,
        forwarder_hub,
    } = opts;
    // **Verify-finding patch (LOW).** Surface the config foot-gun where
    // an operator wires BOTH a Phase A single-token (`bind.token =
    // Some(...)`) AND a Phase B RBAC registry (`rbac = Some(...)`).
    // `auth_layer` treats RBAC as precedence-winner so the single-token
    // becomes dead config — log a boot-time warning so the operator
    // knows their old `--observer-token-{env,file}` flag is silently
    // ignored after the flip to RBAC. The warning rides the same
    // `graphreact::observer::audit` target as the bind event so it
    // shows up in the operator dashboard adjacent to the spawn record.
    if bind.token.is_some() && rbac.is_some() {
        tracing::warn!(
            target: "graphreact::observer::audit",
            event = "auth_config_conflict",
            "both bind.token and rbac are wired — RBAC takes precedence \
             and the single-token is silently ignored; remove the \
             --observer-token-{{env,file}} flag once the operator has \
             migrated to multi-token RBAC"
        );
    }
    // **M-Prod sub-track #1 Phase B Part 2 (Group 1c).** Surface
    // the foot-gun where an operator wires the JWT-OIDC bridge but
    // forgot to wire RBAC. The JWT bridge dispatches INSIDE
    // `auth_layer_rbac` — without an `rbac` registry the static-
    // token path is the Phase A single-token flow which has NO
    // JWT awareness, so the JWT verifier becomes dead config.
    // A WARN-level audit event mirrors the
    // `auth_config_conflict` precedent.
    if jwt_verifier.is_some() && rbac.is_none() {
        tracing::warn!(
            target: "graphreact::observer::audit",
            event = "jwt_config_without_rbac",
            "JWT-OIDC bridge wired but RBAC is not — JWT path is \
             only reachable through auth_layer_rbac; pass \
             --observer-tokens to enable RBAC dispatch (the JWT \
             verifier is silently ignored otherwise)"
        );
    }
    let listener = TcpListener::bind(bind.addr).await?;
    let local_addr = listener.local_addr()?;
    crate::audit::log_bind(
        local_addr,
        bind.tls.is_some(),
        bind.token.is_some(),
        audit_sink.as_ref(),
    );

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
    let ingress_in_flight = IngressInFlight::new();
    let tls_readiness = bind.tls.as_ref().map(|_| crate::TlsReadiness::new());
    let state = ApiState {
        metrics,
        snapshot_tx,
        token: bind.token.map(Arc::new),
        identity,
        ring: ring.clone(),
        websocket: Some(websocket.clone()),
        idempotency_cache: idempotency_cache.clone(),
        pending_calls: pending_calls.clone(),
        // **M-Prod sub-track #5.** `spawn` defaults cluster_readyz
        // to `None` (test + engine-free path);
        // `spawn_with_cluster_readyz` (called by `start_daemon`)
        // threads the real probe through. `None` here keeps single-
        // node + no-cluster tests passing without a synthetic probe.
        cluster_readyz,
        tls_readiness: tls_readiness.clone(),
        // **M-Prod sub-track #1 Phase A.** Audit sink defaults to
        // `None` unless `spawn_with_options` was called with a
        // pre-constructed sink (typically `FileAuditSink` opened
        // by `graphreact-cli` when `--observer-audit-log` is set).
        audit_sink: audit_sink.clone(),
        // **M-Prod sub-track #4 Phase α.** `spawn` +
        // `spawn_with_cluster_readyz` default `draining` to `None`
        // (drain becomes a no-op on those paths). `spawn_with_drain`
        // threads the real flag from `start_daemon` so SIGTERM-
        // driven drain flips `/v1/readyz` to 503 `draining`.
        draining: draining.clone(),
        ingress_in_flight: ingress_in_flight.clone(),
        // **M-Prod sub-track #1 Phase B Part 1.** RBAC handle —
        // `None` keeps Phase A single-token auth bit-for-bit;
        // `Some` swaps `auth_layer` into the multi-token + per-
        // route ACL path.
        rbac: rbac.clone(),
        // **M-Prod sub-track #1 Phase B Part 2 (Group 1c).** JWT
        // bridge verifier — `None` keeps the Phase B Part 1
        // static-token-only path; `Some` enables eyJ-prefixed
        // bearer dispatch through JwtVerifier::verify inside
        // auth_layer_rbac.
        jwt_verifier: jwt_verifier.clone(),
        // **M-Prod sub-track #1 Phase B Part 2 (Group 1d).**
        // Forwarder hub — `None` keeps audit fan-out limited to
        // the in-process file; `Some` enables per-forwarder
        // bounded mpsc + worker fan-out for downstream sidecars.
        forwarder_hub: forwarder_hub.clone(),
    };
    let rl = rate_limit.unwrap_or_else(RateLimitConfig::from_env);
    // **M-Prod sub-track #6 Phase B Part 1.** Resolve the trusted-
    // proxy config. Callers (typically `start_daemon` going through
    // the legacy `spawn_with_drain` thunk) pass `None`; we read
    // `GRX_TRUSTED_PROXIES` here and fail-closed on parse error
    // (logged at audit target, then treated as "no trusted proxies").
    // Tests pin a deterministic config via `SpawnOptions::trusted_proxies`
    // to avoid the env-var race across parallel runners.
    let trusted = trusted_proxies.or_else(|| {
        std::env::var("GRX_TRUSTED_PROXIES").ok().and_then(|s| {
            if s.trim().is_empty() {
                return None;
            }
            match crate::rate_limit_keys::TrustedProxyConfig::parse(&s) {
                Ok(cfg) => Some(Arc::new(cfg)),
                Err(err) => {
                    tracing::warn!(
                        target: "graphreact::observer::audit",
                        event = "trusted_proxies_parse_failure",
                        detail = %err,
                        "failed to parse GRX_TRUSTED_PROXIES — X-Forwarded-For will be ignored"
                    );
                    None
                }
            }
        })
    });
    let router = build_router_with_rate_limit_and_proxies(state, rl, trusted);

    // **M-Prod sub-track #2 Phase A.** Hold the rotatable TLS
    // handle (NOT a pre-built `TlsAcceptor`) — the per-connection
    // task constructs `TlsAcceptor::from(handle.current())` per
    // accept so a `reload_tls(...)` call between accepts surfaces
    // the new cert on the next handshake. See
    // `crate::tls_rotation` module-level docs for why caching the
    // acceptor at spawn time would defeat rotation.
    let tls_handle: Option<crate::RotatableServerConfig> =
        bind.tls.as_ref().map(|tls| tls.server.clone());
    let shutdown = Arc::new(Notify::new());
    let shutdown_for_loop = shutdown.clone();
    let tls_handle_for_loop = tls_handle.clone();
    let audit_sink_for_loop = audit_sink.clone();

    let task = tokio::spawn(async move {
        accept_loop(
            listener,
            router,
            tls_handle_for_loop,
            shutdown_for_loop,
            audit_sink_for_loop,
        )
        .await;
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
        tls: tls_handle,
        tls_readiness,
        // **M-Prod sub-track #1 Phase A.** Surfaced on the handle so
        // `reload_tls` can fan TLS-rotation events to the same sink
        // the accept-loop bind event already lands on.
        audit_sink,
        // **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Park
        // the rotatable RBAC handle on the API handle so the
        // SIGHUP-driven daemon-side `reload_tokens_from_disk` can
        // call `rbac.rotate(...)` without re-investigating SpawnOptions.
        rbac,
        // **M-Prod sub-track #1 Phase B Part 2 (Group 1c).** Park
        // the JWT verifier on the API handle so the SIGHUP-driven
        // daemon-side `reload_jwks_from_disk` can reach the inner
        // `RotatableJwks` and call `jwks().rotate(...)` without
        // re-investigating SpawnOptions.
        jwt_verifier,
        // **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Park
        // the forwarder hub on the API handle so the SIGHUP-
        // driven daemon-side `rotate_audit_forwarder_spool` can
        // call `hub.spool_forwarder().rotate()` symmetrically to
        // how `reload_tokens_from_disk` calls `rbac.rotate(...)`.
        // Also drives drain composition inside
        // OBSERVER_SHUTDOWN_DEADLINE.
        forwarder_hub,
        draining,
        ingress_in_flight,
    })
}

async fn accept_loop(
    listener: TcpListener,
    router: Router,
    tls_handle: Option<crate::RotatableServerConfig>,
    shutdown: Arc<Notify>,
    audit_sink: Option<Arc<dyn crate::AuditSink>>,
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
                        // **M-Prod sub-track #1 Phase A.** Sink fan
                        // for accept-loop accept failures. `detail`
                        // is the formatted error message so an
                        // operator triaging FD exhaustion sees the
                        // same shape in the chained record they see
                        // in tracing.
                        if let Some(sink) = audit_sink.as_ref() {
                            sink.append(crate::AuditRecordInput {
                                kind: "accept_failed",
                                fields: serde_json::json!({
                                    "detail": format!("{err}"),
                                }),
                            });
                        }
                        continue;
                    }
                };
                let router = router.clone();
                let tls_handle = tls_handle.clone();
                let audit_sink_for_handshake = audit_sink.clone();
                tokio::spawn(async move {
                    // axum's `Router` becomes a `tower::Service`
                    // once we feed it a single connection's IO.
                    // hyper-util's auto-builder negotiates HTTP/1
                    // (HTTP/2 is left off — Prometheus scrape
                    // clients all support HTTP/1.1; companion
                    // ecosystem clients negotiate the same).
                    let conn = ConnBuilder::new(hyper_util::rt::TokioExecutor::new());
                    let svc = hyper::service::service_fn(move |mut req| {
                        let router = router.clone();
                        // **M-Prod sub-track #6 Phase A.** Inject
                        // `ConnectInfo<SocketAddr>` into the request
                        // extensions so:
                        //   - tower_governor's `PeerIpKeyExtractor`
                        //     reads the peer IP for keying (without
                        //     this it would return
                        //     `UnableToExtractKey` and the layer
                        //     would 500 every request).
                        //   - the existing `auth_layer` audit-fail
                        //     line + the new `rate_limit_observer`
                        //     audit emit a real peer address rather
                        //     than the `<unknown>` sentinel that
                        //     latently masked the auth-fail
                        //     telemetry pre-#6.
                        //
                        // Also insert the raw `SocketAddr` so
                        // anything reading `req.extensions().get::<SocketAddr>()`
                        // (e.g. `auth_layer`) continues to work
                        // unchanged.
                        req.extensions_mut().insert(peer);
                        req.extensions_mut()
                            .insert(axum::extract::ConnectInfo(peer));
                        async move {
                            // Adapter: feed the request through the
                            // axum Router via ServiceExt::oneshot.
                            router.oneshot(req).await
                        }
                    });
                    if let Some(handle) = tls_handle {
                        // **M-Prod sub-track #2 Phase A.** Construct
                        // the `TlsAcceptor` PER ACCEPT off the
                        // current `Arc<ServerConfig>` snapshot.
                        // `TlsAcceptor::from` snapshots the Arc at
                        // construction; this in-flight handshake
                        // keeps using it until the connection drops,
                        // while a rotation between accepts surfaces
                        // the new cert on the next handshake.
                        let acceptor = TlsAcceptor::from(handle.current());
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
                                // **M-Prod sub-track #1 Phase A.**
                                // Sink fan for per-peer TLS
                                // handshake failures. Cert + key
                                // bytes are NEVER logged — only the
                                // peer + error detail string.
                                if let Some(sink) = audit_sink_for_handshake.as_ref() {
                                    sink.append(crate::AuditRecordInput {
                                        kind: "tls_handshake_failed",
                                        fields: serde_json::json!({
                                            "peer": peer.to_string(),
                                            "detail": format!("{err}"),
                                        }),
                                    });
                                }
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
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        "ok",
    )
        .into_response()
}

/// **M-Prod sub-track #5 (observability completeness) safest slice.**
///
/// Kubernetes-shaped readiness probe. Distinct from `/v1/healthz`
/// (which is liveness: process is up + can serve HTTP). Readiness
/// answers: "should the load balancer / k8s control plane route
/// traffic here YET?". A daemon that has bound its observer port
/// but whose scheduler has not yet drained the initial mount batch
/// is LIVE-but-NOT-READY — `/readyz` returns 503 + `Retry-After: 1`
/// until the first batch completes, then flips to 200 permanently.
///
/// ## Predicate
///
/// Readiness is the conjunction of:
///
/// 1. **Scheduler has completed ≥1 `process_batch` round.** Sourced
///    from `MetricsInner::process_batch_duration.snapshot().count`,
///    which is incremented exactly once per `tick_end` event by
///    the [`crate::PrometheusSink`]. A daemon that just bound its
///    port has count=0; after the daemon's first
///    [`drive_handle_until`] iteration emits a tick the count is
///    ≥1 and stays ≥1 for the rest of the process lifetime.
///
/// 2. **Snapshot bridge has capacity.** Sourced from
///    `snapshot_tx.capacity()`. A capacity-0 bridge means the
///    scheduler thread is wedged / blocked behind a long
///    `process_batch` round AND every channel slot is full —
///    inbound HTTP would 503 anyway, so we pre-emptively de-route
///    via readiness rather than serve a stale snapshot.
///
/// ## Failure modes documented for operators
///
/// - **503 with no batch tick yet**: daemon is still inside its
///   first mount cascade; expected at process startup, should
///   flip green within a few hundred ms on a healthy mount.
/// - **503 with bridge at 0 capacity**: scheduler thread is
///   wedged; LIVE probe still 200s (the axum task is alive) so
///   k8s won't restart, but `/readyz` 503 keeps new traffic off.
///
/// ## Future predicate extensions (deferred inside #5)
///
/// - TLS cert loaded / not-in-rotation — needs sub-track #2 SIGHUP
///   reload primitive.
/// - OTel exporter reachable — needs the BatchSpanProcessor connect
///   handle (currently the SDK absorbs the failure mode).
/// - Drain-in-progress flag — needs sub-track #4 SIGTERM drain
///   primitive; once shipped, readiness flips back to 503 at the
///   start of drain so the load balancer de-routes BEFORE we stop
///   accepting new calls.
async fn readyz(State(state): State<ApiState>) -> Response {
    // **M-Prod sub-track #4 Phase α.** Drain predicate wins over
    // all other not-ready reasons. A daemon mid-drain should NEVER
    // surface `scheduler_first_batch_pending` etc. to k8s — once
    // the drain bit flips, the canonical answer is `draining`, so
    // the load balancer de-routes immediately without re-checking
    // every other predicate. The check is a relaxed atomic load
    // (sub-µs hot-path cost).
    if state.draining.as_ref().is_some_and(DrainFlag::is_draining) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; charset=utf-8",
                ),
                (axum::http::header::RETRY_AFTER, "1"),
            ],
            "draining",
        )
            .into_response();
    }
    if !state.metrics.audit_sink_write_failures.is_empty() {
        return not_ready("audit_sink_failed");
    }
    if state
        .tls_readiness
        .as_ref()
        .is_some_and(crate::TlsReadiness::is_rotating)
    {
        return not_ready("tls_rotation_in_progress");
    }
    let batches = state.metrics.process_batch_duration.snapshot().count;
    let bridge_capacity = state.snapshot_tx.capacity();
    // **M-Prod sub-track #5.** Cluster-membership predicate.
    // When `cluster_readyz` is `None` (test / engine-free path)
    // we treat cluster as ready so single-node fixtures aren't
    // false-503'd. When wired, the probe returns `true` if no
    // peers were configured (single-node deploy) or at least one
    // peer dialed (multi-node convergence reached).
    let cluster_ready = state.cluster_readyz.as_ref().is_none_or(|probe| probe());
    let ready = batches > 0 && bridge_capacity > 0 && cluster_ready;
    if ready {
        (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            "ready",
        )
            .into_response()
    } else {
        let reason: &str = if batches == 0 {
            "scheduler_first_batch_pending"
        } else if bridge_capacity == 0 {
            "bridge_saturated"
        } else {
            // cluster_ready == false. Distinct reason so ops know
            // peer dialing is the holdup (not engine init, not
            // scheduler backpressure).
            "cluster_no_peers_connected"
        };
        not_ready(reason)
    }
}

fn not_ready(reason: &'static str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            ),
            (axum::http::header::RETRY_AFTER, "1"),
        ],
        reason,
    )
        .into_response()
}

/// **M-Prod sub-track #4 Phase α.** Backpressure-shaped 503 for
/// ingress endpoints that hit the drain bit. Mirrors
/// [`bridge_saturated_response`]'s shape — same status code, same
/// `Retry-After: 1` back-off contract, JSON body — but a distinct
/// `error = "draining"` discriminator so operators can grep their
/// LB / proxy logs apart. Body string matches the `/v1/readyz`
/// reason for operator-symmetry: every surface that surfaces a
/// drain-driven 503 uses the word `draining`.
fn drain_in_progress_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (axum::http::header::CONTENT_TYPE, "application/json"),
            (axum::http::header::RETRY_AFTER, "1"),
        ],
        serde_json::json!({
            "error": "draining",
            "retry_after_secs": 1,
        })
        .to_string(),
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

async fn start_durable_run_handler(
    State(state): State<ApiState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<StartDurableRunBody>,
) -> Response {
    if state.draining.as_ref().is_some_and(DrainFlag::is_draining) {
        return drain_in_progress_response();
    }
    let Some(idempotency_key) = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return durable_run_error(
            StatusCode::BAD_REQUEST,
            "missing_idempotency_key",
            "Idempotency-Key is required",
        );
    };
    if body.entrypoint.trim().is_empty() {
        return durable_run_error(
            StatusCode::BAD_REQUEST,
            "invalid_entrypoint",
            "entrypoint must not be empty",
        );
    }

    let _inflight = state.ingress_in_flight.enter();
    let (tx, rx) = oneshot::channel();
    let request = SnapshotRequest::StartDurableRun {
        entrypoint: body.entrypoint,
        payload: body.payload,
        idempotency_key: idempotency_key.to_owned(),
        oneshot_tx: tx,
    };
    match state.snapshot_tx.try_send(request) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => return bridge_saturated_response(),
        Err(mpsc::error::TrySendError::Closed(_)) => {
            return service_unavailable("scheduler bridge closed");
        }
    }
    match rx.await {
        Ok(DurableRunBridgeResult::Ready {
            run,
            created: Some(created),
        }) => {
            let status = if created {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            };
            (status, Json(run)).into_response()
        }
        Ok(DurableRunBridgeResult::IdempotencyConflict) => durable_run_error(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key was already used for different request content",
        ),
        Ok(DurableRunBridgeResult::UnknownEntrypoint) => durable_run_error(
            StatusCode::BAD_REQUEST,
            "unknown_entrypoint",
            "entrypoint does not exist in this deployment",
        ),
        Ok(DurableRunBridgeResult::DurabilityDisabled) => durable_run_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "durability_disabled",
            "durable run service is not enabled",
        ),
        Ok(DurableRunBridgeResult::Failed(message)) => durable_run_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "persistence_error",
            &message,
        ),
        Ok(_) => durable_run_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_bridge_response",
            "scheduler returned an invalid run response",
        ),
        Err(_) => service_unavailable("scheduler bridge dropped response"),
    }
}

async fn get_durable_run_handler(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Response {
    durable_run_lookup(&state, run_id, false).await
}

async fn cancel_durable_run_handler(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Response {
    if state.draining.as_ref().is_some_and(DrainFlag::is_draining) {
        return drain_in_progress_response();
    }
    let _inflight = state.ingress_in_flight.enter();
    durable_run_lookup(&state, run_id, true).await
}

async fn durable_run_lookup(state: &ApiState, run_id: String, cancel: bool) -> Response {
    let (tx, rx) = oneshot::channel();
    let request = if cancel {
        SnapshotRequest::CancelDurableRun {
            run_id,
            oneshot_tx: tx,
        }
    } else {
        SnapshotRequest::GetDurableRun {
            run_id,
            oneshot_tx: tx,
        }
    };
    let sent = if cancel {
        state
            .snapshot_tx
            .try_send(request)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => bridge_saturated_response(),
                mpsc::error::TrySendError::Closed(_) => {
                    service_unavailable("scheduler bridge closed")
                }
            })
    } else {
        state
            .snapshot_tx
            .send(request)
            .await
            .map_err(|_| service_unavailable("scheduler bridge closed"))
    };
    if let Err(response) = sent {
        return response;
    }
    match rx.await {
        Ok(DurableRunBridgeResult::Ready { run, .. }) => Json(run).into_response(),
        Ok(DurableRunBridgeResult::NotFound) => {
            durable_run_error(StatusCode::NOT_FOUND, "run_not_found", "run was not found")
        }
        Ok(DurableRunBridgeResult::Terminal) => durable_run_error(
            StatusCode::CONFLICT,
            "run_terminal",
            "terminal runs cannot be cancelled",
        ),
        Ok(DurableRunBridgeResult::DurabilityDisabled) => durable_run_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "durability_disabled",
            "durable run service is not enabled",
        ),
        Ok(DurableRunBridgeResult::Failed(message)) => durable_run_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "persistence_error",
            &message,
        ),
        Ok(_) => durable_run_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_bridge_response",
            "scheduler returned an invalid run response",
        ),
        Err(_) => service_unavailable("scheduler bridge dropped response"),
    }
}

fn durable_run_error(status: StatusCode, code: &'static str, message: &str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "error": code, "message": message }).to_string(),
    )
        .into_response()
}

async fn list_run_artifacts_handler(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Response {
    let (tx, rx) = oneshot::channel();
    if state
        .snapshot_tx
        .send(SnapshotRequest::ListRunArtifacts {
            run_id,
            oneshot_tx: tx,
        })
        .await
        .is_err()
    {
        return service_unavailable("scheduler bridge closed");
    }
    match rx.await {
        Ok(DurableArtifactBridgeResult::List(artifacts)) => Json(artifacts).into_response(),
        Ok(DurableArtifactBridgeResult::NotFound) => {
            durable_run_error(StatusCode::NOT_FOUND, "run_not_found", "run was not found")
        }
        Ok(DurableArtifactBridgeResult::DurabilityDisabled) => durable_run_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "durability_disabled",
            "durable artifact service is not enabled",
        ),
        Ok(DurableArtifactBridgeResult::Failed(message)) => durable_run_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "persistence_error",
            &message,
        ),
        Ok(_) => durable_run_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_bridge_response",
            "scheduler returned an invalid artifact response",
        ),
        Err(_) => service_unavailable("scheduler bridge dropped response"),
    }
}

async fn get_artifact_handler(
    State(state): State<ApiState>,
    Path(artifact_id): Path<String>,
) -> Response {
    let (tx, rx) = oneshot::channel();
    if state
        .snapshot_tx
        .send(SnapshotRequest::GetArtifact {
            artifact_id,
            oneshot_tx: tx,
        })
        .await
        .is_err()
    {
        return service_unavailable("scheduler bridge closed");
    }
    match rx.await {
        Ok(DurableArtifactBridgeResult::Content { artifact, bytes }) => {
            let content_type = HeaderValue::from_str(&artifact.media_type)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
            let mut response = bytes.into_response();
            response
                .headers_mut()
                .insert(axum::http::header::CONTENT_TYPE, content_type);
            response.headers_mut().insert(
                axum::http::header::CONTENT_LENGTH,
                HeaderValue::from_str(&artifact.size.to_string())
                    .unwrap_or_else(|_| HeaderValue::from_static("0")),
            );
            response.headers_mut().insert(
                axum::http::HeaderName::from_static("x-graphreact-artifact-id"),
                HeaderValue::from_str(&artifact.artifact_id)
                    .unwrap_or_else(|_| HeaderValue::from_static("invalid")),
            );
            response
        }
        Ok(DurableArtifactBridgeResult::NotFound) => durable_run_error(
            StatusCode::NOT_FOUND,
            "artifact_not_found",
            "artifact was not found",
        ),
        Ok(DurableArtifactBridgeResult::DurabilityDisabled) => durable_run_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "durability_disabled",
            "durable artifact service is not enabled",
        ),
        Ok(DurableArtifactBridgeResult::Failed(message)) => durable_run_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "persistence_error",
            &message,
        ),
        Ok(_) => durable_run_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_bridge_response",
            "scheduler returned an invalid artifact response",
        ),
        Err(_) => service_unavailable("scheduler bridge dropped response"),
    }
}

async fn nodes_list(
    State(state): State<ApiState>,
) -> Result<Json<Vec<NodeIntrospection>>, Response> {
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
    // **M-Prod sub-track #4 Phase α.** Drain-bit check fires
    // BEFORE the in-flight counter bumps so a request that
    // arrived 1ms after `start_drain` flipped the bit gets
    // refused symmetrically with the `/v1/readyz` 503 body.
    // Doing it before the `enter()` bump also keeps drain
    // quiescence racy-free — the orchestrator's
    // `ingress_in_flight.load() == 0` poll never sees a
    // late-arriving request bumping the counter post-drain-start.
    if state.draining.as_ref().is_some_and(DrainFlag::is_draining) {
        return drain_in_progress_response();
    }
    let _inflight = state.ingress_in_flight.enter();
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
    // **M-Prod sub-track #5.** Ingress endpoints use `try_send`
    // rather than `send().await` so a saturated bridge surfaces as
    // 503 + `Retry-After: 1` instead of silently parking the axum
    // task. Read-snapshot bridges (nodes_list / node_detail /
    // supervisor_tree) keep `send().await`: those are internal
    // observer→scheduler reads with bounded depth (scheduler is
    // single-threaded; drain fires after every batch round), and
    // blocking briefly on them is preferable to a 503 cascade
    // on the management API's snapshot routes. The bulk per-line
    // handler already uses `try_send` and renders the same shape
    // as a `backpressure_503` line.
    match state.snapshot_tx.try_send(req) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => return bridge_saturated_response(),
        Err(mpsc::error::TrySendError::Closed(_)) => {
            return service_unavailable("scheduler bridge closed");
        }
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

/// Issue an operator-driven cross-node delegated restart. The RBAC
/// middleware must attach an authenticated Admin principal; legacy
/// single-token auth and Custom roles are intentionally insufficient.
async fn delegated_restart_handler(
    State(state): State<ApiState>,
    principal: Option<axum::extract::Extension<AuthenticatedPrincipal>>,
    Json(body): Json<DelegatedRestartBody>,
) -> Response {
    if state.draining.as_ref().is_some_and(DrainFlag::is_draining) {
        return drain_in_progress_response();
    }
    let Some(axum::extract::Extension(principal)) = principal else {
        crate::audit::log_authz_deny(
            "<legacy>",
            "/v1/admin/cluster/delegated-restart",
            "legacy",
            "admin_role_required",
            state.audit_sink.as_ref(),
        );
        return forbidden_admin_response();
    };
    if !principal.is_admin {
        crate::audit::log_authz_deny(
            &principal.token_id,
            "/v1/admin/cluster/delegated-restart",
            "custom",
            "admin_role_required",
            state.audit_sink.as_ref(),
        );
        return forbidden_admin_response();
    }
    if body.peer.trim().is_empty() || body.child_node_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::json!({
                "error": "invalid_delegated_restart",
                "reason": "peer and child_node_id must be non-empty",
            })
            .to_string(),
        )
            .into_response();
    }

    let (tx, rx) = oneshot::channel();
    let request = SnapshotRequest::DelegatedRestart {
        peer: body.peer,
        child_node_id: body.child_node_id,
        token_id: principal.token_id,
        oneshot_tx: tx,
    };
    match state.snapshot_tx.try_send(request) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => return bridge_saturated_response(),
        Err(mpsc::error::TrySendError::Closed(_)) => {
            return service_unavailable("scheduler bridge closed");
        }
    }
    match rx.await {
        Ok(DelegatedRestartDispatchResult::Accepted {
            correlation_id,
            peer,
        }) => (
            StatusCode::ACCEPTED,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::json!({
                "status": "accepted",
                "correlation_id": correlation_id,
                "peer": peer,
            })
            .to_string(),
        )
            .into_response(),
        Ok(DelegatedRestartDispatchResult::Draining) => drain_in_progress_response(),
        Ok(DelegatedRestartDispatchResult::UnknownPeer) => {
            delegated_restart_error_response(StatusCode::NOT_FOUND, "unknown_peer")
        }
        Ok(DelegatedRestartDispatchResult::CapabilityNotNegotiated) => {
            delegated_restart_error_response(StatusCode::CONFLICT, "capability_not_negotiated")
        }
        Ok(DelegatedRestartDispatchResult::OutboundUnavailable) => {
            delegated_restart_outbound_unavailable_response()
        }
        Err(_) => service_unavailable("scheduler bridge dropped response"),
    }
}

fn forbidden_admin_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "error": "admin_role_required" }).to_string(),
    )
        .into_response()
}

fn delegated_restart_error_response(status: StatusCode, error: &'static str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "error": error }).to_string(),
    )
        .into_response()
}

fn delegated_restart_outbound_unavailable_response() -> Response {
    let mut response =
        delegated_restart_error_response(StatusCode::SERVICE_UNAVAILABLE, "outbound_unavailable");
    response.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        HeaderValue::from_static("1"),
    );
    response
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

async fn wait_for_drain_deadline(
    receiver: &mut Option<tokio::sync::watch::Receiver<Option<crate::drain::DrainDeadline>>>,
) -> u64 {
    let Some(receiver) = receiver.as_mut() else {
        return std::future::pending::<u64>().await;
    };
    loop {
        let deadline = { *receiver.borrow_and_update() };
        if let Some(deadline) = deadline {
            tokio::time::sleep_until(deadline.at).await;
            return deadline.budget_ms;
        }
        if receiver.changed().await.is_err() {
            return std::future::pending::<u64>().await;
        }
    }
}

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
/// 4. Race scheduler disposition and reply waiting against the
///    published drain deadline; also cap reply waiting at
///    [`MAX_CALL_TIMEOUT_MS`]. On reply, return 200 + payload. On
///    either timeout, return 504; the registry guard's Drop evicts.
async fn inject_call_handler(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<CallMessageBody>,
) -> Response {
    // **M-Prod sub-track #4 Phase α.** Drain check fires before
    // the in-flight bump + PendingCalls register so a request
    // that arrived after drain-start gets refused symmetrically
    // with `/v1/readyz`'s 503 body. The orchestrator's
    // quiescence loop polls `ingress_in_flight.load() == 0 &&
    // pending_calls.is_empty()`; keeping the drain check before
    // both counter bumps prevents a race where a late-arriving
    // request bumps either counter post-drain-start.
    if state.draining.as_ref().is_some_and(DrainFlag::is_draining) {
        return drain_in_progress_response();
    }
    // Subscribe immediately after the admission check. If drain
    // races this point, watch subscription snapshots the published
    // deadline and the admitted call is still bounded by it.
    let mut drain_deadline = state.draining.as_ref().map(DrainFlag::subscribe_deadline);
    let _inflight = state.ingress_in_flight.enter();
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
    let Some(guard) = state
        .pending_calls
        .register(correlation_id.clone(), reply_tx)
    else {
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
    // **M-Prod sub-track #5.** Same try_send→503+Retry-After
    // policy as `inject_message_handler` (see comment there for
    // the ingress-vs-internal-bridge split). On a full bridge,
    // drop the guard FIRST so the registry entry evicts before
    // the 503 returns — leaving a dangling slot would shrink the
    // [`PendingCalls`] capacity until the TTL prune sweeps it.
    match state.snapshot_tx.try_send(req) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            drop(guard);
            return bridge_saturated_response();
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            drop(guard);
            return service_unavailable("scheduler bridge closed");
        }
    }
    let disposition = tokio::select! {
        result = disp_rx => match result {
            Ok(disposition) => disposition,
            Err(_) => {
                drop(guard);
                return service_unavailable("scheduler bridge dropped response");
            }
        },
        drain_timeout_ms = wait_for_drain_deadline(&mut drain_deadline) => {
            drop(guard);
            return drain_call_timeout_response(drain_timeout_ms);
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
            let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
            tokio::pin!(timeout);
            tokio::select! {
                reply = reply_rx => match reply {
                    Ok(payload) => {
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
                    Err(_) => {
                        // The oneshot was dropped without sending —
                        // shouldn't happen on the live path because
                        // the sink either sends a value or doesn't
                        // touch the channel. Treat as 504.
                        drop(guard);
                        timeout_response(timeout_ms)
                    }
                },
                () = &mut timeout => {
                    // tokio::time::timeout elapsed. Drop the
                    // guard so the entry evicts; the sink may
                    // still attempt to resolve (race), but its
                    // remove-then-send returns a closed channel
                    // which is silently ignored.
                    drop(guard);
                    timeout_response(timeout_ms)
                }
                drain_timeout_ms = wait_for_drain_deadline(&mut drain_deadline) => {
                    drop(guard);
                    drain_call_timeout_response(drain_timeout_ms)
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

fn drain_call_timeout_response(timeout_ms: u64) -> Response {
    (
        StatusCode::GATEWAY_TIMEOUT,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({
            "error": "reply_timeout",
            "reason": "drain_started",
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
///
/// Promoted to `pub` (M-Prod sub-track #9 / chaos+fuzz) so the
/// out-of-workspace `fuzz/` substrate can target the same
/// `serde_json::from_slice` path the live bulk handler walks
/// without re-declaring the schema. Non-fuzz consumers should
/// reach for [`parse_bulk_line`] instead of inspecting fields
/// directly — the type's field shape is an HTTP wire-format
/// detail, NOT a public API the runtime cares to stabilise.
#[derive(Debug, Deserialize)]
pub struct BulkLineBody {
    /// Inbound message type. Same allowlist semantics as
    /// [`InjectMessageBody::type`]: runtime-checked against the
    /// target's `manifest.accepts_external` at delivery time.
    pub r#type: String,
    /// Arbitrary JSON payload forwarded verbatim to the node's
    /// inbox.
    #[serde(default)]
    pub payload: Value,
    /// Per-line idempotency token. Header-level `Idempotency-Key`
    /// does not apply to bulk requests; clients carry the token
    /// inline per record instead.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// **M-Prod sub-track #9 (chaos + fuzz).** Thin, allocation-free
/// wrapper around `serde_json::from_slice::<BulkLineBody>` that
/// the out-of-workspace `fuzz/` substrate calls. Keeps the public
/// surface narrow — fuzz targets do not need (or want) the rest
/// of the management-API runtime, just the deserialisation path
/// the bulk handler walks per NDJSON line.
///
/// This is the third ingress JSON layer (alongside
/// [`InjectMessageBody`] and [`CallMessageBody`]) and is the only
/// one whose body type is otherwise file-private — exposing this
/// shim is the lower-blast-radius alternative to widening the
/// struct itself to `pub(crate)` re-export.
pub fn parse_bulk_line(bytes: &[u8]) -> Result<BulkLineBody, serde_json::Error> {
    serde_json::from_slice::<BulkLineBody>(bytes)
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

    // **M-Prod sub-track #4 Phase α.** Drain-bit check fires
    // BEFORE the streaming response body is constructed so a
    // bulk POST that arrived after drain-start gets refused with
    // the same 503 `draining` body the single-shot endpoints
    // surface. The streaming producer task carries an
    // `InflightGuard`; if drain fires AFTER the bulk request
    // entered, the producer's existing per-line bridge `try_send`
    // path will start failing once the daemon's snapshot bridge
    // closes — the producer's `BulkLineDisposition::BridgeDropped`
    // arm emits the per-line 503 row + closes the stream.
    if state.draining.as_ref().is_some_and(DrainFlag::is_draining) {
        return drain_in_progress_response();
    }
    let inflight_guard = state.ingress_in_flight.enter();

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
        // **M-Prod sub-track #4 Phase α.** Hold the in-flight
        // guard for the lifetime of the producer task so the
        // drain orchestrator's `ingress_in_flight.load() == 0`
        // poll only flips to zero once the bulk producer has
        // drained EVERY line of the request. If the producer
        // panics (no recover branch), the guard's Drop still
        // decrements via the panic-unwinding path.
        let _inflight_guard = inflight_guard;
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
                                let _ = write_result(&out_tx, line_num, "exceeded_line_cap").await;
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
                let line_text = line_text.strip_suffix(b"\r").unwrap_or(line_text);
                // Empty lines (NDJSON tolerates / ignores them):
                // skip without incrementing line_num. Real
                // application clients should not emit blanks
                // mid-stream, but trailing newlines after the
                // last record are common.
                if line_text.is_empty() {
                    continue;
                }
                if line_num >= line_cap as u64 {
                    let _ = write_result(&out_tx, line_num, "exceeded_line_cap").await;
                    return;
                }
                let result =
                    process_bulk_line(&state_ref(&snapshot_tx, &idem_cache), &id, line_text).await;
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
            let line_text = buf.strip_suffix(b"\r").unwrap_or(&buf);
            if !line_text.is_empty() && line_num < line_cap as u64 {
                let result =
                    process_bulk_line(&state_ref(&snapshot_tx, &idem_cache), &id, line_text).await;
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
        Ok(BulkInjectResult::RejectedByManifest) => BulkLineDisposition::RejectedByManifest,
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

/// **M-Prod sub-track #5.** Backpressure-shaped 503 for ingress
/// endpoints that fail `snapshot_tx.try_send` because every slot
/// of the scheduler bridge (capacity = [`SNAPSHOT_CHANNEL_CAPACITY`])
/// is occupied. The shape matches the `/v1/readyz` bridge-saturation
/// reply: 503 + `Retry-After: 1` + a stable plain-text reason —
/// load balancers + k8s probes see the same back-off contract on the
/// liveness/readiness pair and on individual ingress POSTs, so a
/// caller hammering a wedged daemon gets uniform behaviour across
/// the surface.
///
/// `Retry-After: 1` is intentional: the bridge drains in milliseconds
/// once the scheduler-thread loop runs again, so a one-second back-off
/// is short enough to recover quickly but long enough to avoid
/// thundering-herd retries from many in-flight clients (see
/// [`m-prod-observability.md`](../../../../docs/backend/m-prod-observability.md)
/// for the trade-off rationale).
fn bridge_saturated_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (axum::http::header::CONTENT_TYPE, "application/json"),
            (axum::http::header::RETRY_AFTER, "1"),
        ],
        serde_json::json!({
            "error": "bridge_saturated",
            "retry_after_secs": 1,
        })
        .to_string(),
    )
        .into_response()
}

/// **M-Prod sub-track #6 Phase A.** Build the canonical 429 reply
/// the per-IP rate limiter returns when a client trips the cap.
/// Shape-symmetric with [`bridge_saturated_response`]: same JSON
/// envelope, same `Retry-After` header semantics, only the `error`
/// discriminator changes. Operator scripts grepping for
/// `bridge_saturated` / `rate_limited` / `draining` /
/// `scheduler_first_batch_pending` / `cluster_no_peers_connected`
/// find every back-pressure-class reply in the same place.
///
/// `retry_after_secs` is `wait_time` from `tower_governor` (seconds
/// until the next token becomes available). We re-emit the header
/// explicitly rather than relying on tower_governor's default
/// `Retry-After` so a future shape change inside the layer cannot
/// silently downgrade the operator-visible back-off contract.
#[cfg(test)]
fn rate_limited_response_from_error(err: tower_governor::GovernorError) -> Response {
    // Phase A back-compat wrapper. Defaults the bucket discriminator
    // to `"per_ip"` so the existing in-module unit-test surface stays
    // bit-for-bit (those tests don't pass through the per-layer
    // error_handler closures + don't read the bucket extension).
    rate_limited_response_from_error_with_bucket(err, "per_ip")
}

/// **M-Prod sub-track #6 Phase B Part 1 (Group B-4a).** Identical to
/// [`rate_limited_response_from_error`] except it ALSO stamps the
/// response with the [`crate::rate_limit_keys::RateLimitDeniedBucket`]
/// extension carrying the `bucket` discriminator. The outer
/// `rate_limit_observer` middleware reads that extension to attribute
/// the 429 to the right bucket on both the audit record + the
/// `grx_rate_limit_blocked_total{route,bucket}` Prometheus counter.
///
/// `bucket` MUST be one of the closed-over `&'static str`s
/// `{"per_ip", "per_token", "per_token_no_match"}`. Each per-layer
/// `error_handler` closure passes its own discriminator.
fn rate_limited_response_from_error_with_bucket(
    err: tower_governor::GovernorError,
    bucket: &'static str,
) -> Response {
    use tower_governor::GovernorError;
    let mut resp = match err {
        GovernorError::TooManyRequests { wait_time, headers } => {
            // `wait_time` is duration-until-next-token in whole
            // seconds (matches RFC 7231 §7.1.3 delta-seconds and
            // bridge_saturated's `Retry-After: 1`). Floor at 1 to
            // keep client retry logic that polls on `Retry-After`
            // from spinning when the value is technically 0.
            let retry_after = wait_time.max(1);
            let body = serde_json::json!({
                "error": "rate_limited",
                "retry_after_secs": retry_after,
            })
            .to_string();
            let mut resp = (
                StatusCode::TOO_MANY_REQUESTS,
                [
                    (axum::http::header::CONTENT_TYPE, "application/json"),
                    (axum::http::header::RETRY_AFTER, "1"),
                ],
                body,
            )
                .into_response();
            // Fold in any `x-ratelimit-*` headers tower_governor
            // attached via `use_headers()` FIRST so its raw
            // `retry-after: <wait_time>` value (which is the
            // truncated `Duration::as_secs()` floor and can be 0
            // for sub-second token-refill windows) doesn't
            // silently overwrite the clamped Retry-After below.
            // The order is load-bearing: tower_governor populates
            // `extra` with `retry-after` before calling our
            // error_handler, so without this ordering the JSON
            // body would say `retry_after_secs: 1` while the HTTP
            // header would say `Retry-After: 0` — well-behaved
            // clients honoring `Retry-After: 0` would spin-retry
            // immediately, defeating the limiter's protective
            // purpose and breaking operator triage assumptions.
            if let Some(extra) = headers {
                for (k, v) in &extra {
                    resp.headers_mut().insert(k, v.clone());
                }
            }
            // Overwrite the (possibly-folded-in) Retry-After with
            // the clamped value LAST so the header matches the
            // documented 1-second-minimum contract.
            if let Ok(value) = axum::http::HeaderValue::from_str(&retry_after.to_string()) {
                resp.headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, value);
            }
            resp
        }
        GovernorError::UnableToExtractKey => {
            // Should be impossible under our wiring — the
            // `accept_loop` injects `ConnectInfo<SocketAddr>` per
            // accept, AND the Phase B Part 1 injector middleware
            // installs `PerRequestRateLimitCtx` for the per-token
            // extractor. If we ever hit this branch it means an
            // injection point regressed; surface a 500 with the
            // operator-facing discriminator so the regression is
            // loud in audit logs.
            tracing::error!(
                target: "graphreact::observer::audit",
                bucket = %bucket,
                "rate-limit key extraction failed — required request extension missing"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                serde_json::json!({
                    "error": "rate_limit_key_unavailable",
                })
                .to_string(),
            )
                .into_response()
        }
        GovernorError::Other { code, msg, headers } => {
            // Reserved for custom key extractors that return their
            // own errors — neither of our extractors uses this
            // branch (both return either a key or
            // UnableToExtractKey), but keep the structured shape
            // for future-proofing.
            let body = serde_json::json!({
                "error": "rate_limit_other",
                "detail": msg.unwrap_or_default(),
            })
            .to_string();
            let mut resp = (
                code,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response();
            if let Some(extra) = headers {
                for (k, v) in &extra {
                    resp.headers_mut().insert(k, v.clone());
                }
            }
            resp
        }
    };
    // **Phase B Part 1.** Stamp the bucket discriminator on the
    // response so the outer `rate_limit_observer` can attribute
    // the 429 to the right bucket. Defaults to `"per_ip"` for
    // back-compat at the observer site if the extension is missing.
    resp.extensions_mut()
        .insert(crate::rate_limit_keys::RateLimitDeniedBucket(bucket));
    resp
}

/// **M-Prod sub-track #6 Phase A.** Observer middleware that wraps
/// the protected-route chain OUTSIDE the `GovernorLayer`. Captures
/// peer + classified route on entry; awaits the chain; if the
/// response is `429 Too Many Requests` (the discriminator the
/// governor emits via [`rate_limited_response_from_error`]),
/// fires:
///
/// 1. `tracing::warn!(target = "graphreact::observer::audit",
///    event = "rate_limit_blocked", peer, route)` — same target
///    convention the existing `auth_fail` / `bind` / `tls_*` events
///    ride, so the JSON-log + audit-sink fan-out works without a
///    callsite change.
/// 2. `state.audit_sink.append(...)` — hash-chained tamper-evident
///    record if a sink is wired (the M-Prod #1 substrate).
/// 3. `state.metrics.rate_limit_blocked_inc(route)` — bumps the
///    `grx_rate_limit_blocked_total{route}` counter under a bounded
///    label vocabulary (see [`classify_protected_path`]).
///
/// `peer` resolution mirrors `auth_layer`: read
/// `req.extensions().get::<SocketAddr>()`, fall back to
/// `"<unknown>"` on the (theoretically impossible) miss path. The
/// `accept_loop` injects both `SocketAddr` AND
/// `ConnectInfo<SocketAddr>` per accept so the miss path stays
/// dead under normal operation.
async fn rate_limit_observer(
    State(state): State<ApiState>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    // Snapshot peer + route classification BEFORE forwarding, so
    // we don't depend on extensions surviving the chain (they
    // shouldn't be mutated by inner layers but defensive snapshot
    // is cheap and decouples this observer from inner-layer
    // changes).
    let peer = req
        .extensions()
        .get::<SocketAddr>()
        .map(SocketAddr::to_string)
        .unwrap_or_else(|| "<unknown>".into());
    let route = classify_protected_path(req.uri().path());

    let resp = next.run(req).await;
    if resp.status() == StatusCode::TOO_MANY_REQUESTS {
        // **Phase B Part 1.** Read the bucket discriminator stamped
        // by the per-layer error_handler. Defaults to `"per_ip"` —
        // back-compat in case a regression strips the extension.
        // When the per-token bucket fires, the injector also stamps
        // its bucket_hint onto the response so `"per_token"` can
        // refine to `"per_token_no_match"` for unauthenticated /
        // garbage-token requests.
        let raw_bucket = resp
            .extensions()
            .get::<crate::rate_limit_keys::RateLimitDeniedBucket>()
            .map(|b| b.0)
            .unwrap_or("per_ip");
        let hinted_no_match = resp
            .extensions()
            .get::<crate::rate_limit_keys::PerTokenBucketHintMarker>()
            .map(|m| m.0)
            .unwrap_or("per_token_no_match");
        let bucket = if raw_bucket == "per_token" {
            hinted_no_match
        } else {
            raw_bucket
        };
        // Audit fan + metric bump. Token / cert / payload bytes
        // are NEVER included — only peer + route + bucket + the
        // literal event name. Substrate-not-MVP: the tracing line
        // is the canonical surface, the sink is a parallel fan,
        // the metric is the cumulative aggregate.
        tracing::warn!(
            target: "graphreact::observer::audit",
            peer = %peer,
            route = %route,
            bucket = %bucket,
            "rate_limit_blocked"
        );
        if let Some(sink) = state.audit_sink.as_ref() {
            sink.append(crate::AuditRecordInput {
                kind: "rate_limit_blocked",
                fields: serde_json::json!({
                    "peer": peer,
                    "route": route,
                    "bucket": bucket,
                }),
            });
        }
        state.metrics.rate_limit_blocked_inc(route, bucket);
    }
    resp
}

async fn auth_layer(
    State(state): State<ApiState>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    // **M-Prod sub-track #1 Phase B Part 1.** RBAC path takes
    // precedence when wired. `state.rbac = None` (legacy spawn
    // paths default) falls through to the Phase A single-token
    // flow, bit-for-bit unchanged. `state.rbac = Some(...)`
    // replaces `state.token` as the auth source — operators wire
    // ONE or the OTHER, not both (Part 2's JWT-OIDC bridge folds
    // both into the same registry).
    if let Some(rbac) = state.rbac.as_ref() {
        return auth_layer_rbac(state.clone(), rbac.clone(), req, next).await;
    }
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
            // **M-Prod sub-track #1 Phase A.** Fan to the sink in
            // parallel. The tracing line above is unchanged; the
            // sink record carries the same `peer` + `reason`
            // fields the audit::log_auth_fail helper would emit
            // from an accept-loop callsite. Phase B's RBAC verdict
            // fields ride here without bumping the line format.
            if let Some(sink) = state.audit_sink.as_ref() {
                sink.append(crate::AuditRecordInput {
                    kind: "auth_fail",
                    fields: serde_json::json!({
                        "peer": peer_repr,
                        "reason": "missing",
                    }),
                });
            }
            challenge_response()
        }
        Some(c) if expected.matches(c.as_bytes()) => next.run(req).await,
        Some(_) => {
            tracing::warn!(
                target: "graphreact::observer::audit",
                peer = %peer_repr,
                "observer auth fail (wrong)"
            );
            if let Some(sink) = state.audit_sink.as_ref() {
                sink.append(crate::AuditRecordInput {
                    kind: "auth_fail",
                    fields: serde_json::json!({
                        "peer": peer_repr,
                        "reason": "wrong",
                    }),
                });
            }
            challenge_response()
        }
    }
}

/// **M-Prod sub-track #1 Phase B Part 1.** Multi-token RBAC dispatch.
/// Invoked from [`auth_layer`] when `state.rbac` is `Some(...)`.
///
/// Flow:
/// 1. Classify the request URI to a `&'static str` route label.
/// 2. Extract the bearer candidate from `Authorization: Bearer ...`.
/// 3. Snapshot the registry (one `Arc::clone`).
/// 4. Match the candidate against ALL entries — no early exit
///    (timing-attack safety; see [`crate::rbac::TokenRegistry::match_token`]).
/// 5. Check expiry against `SystemTime::now()`.
/// 6. Dispatch via [`crate::rbac::TokenRegistry::check_route`] →
///    [`crate::audit::log_auth_success`] on permit, or
///    [`crate::audit::log_authz_deny`] + 403 on deny.
///
/// The auth-fail audit emissions (`missing` / `wrong` / `expired`)
/// match the existing inline-tracing-line shape in [`auth_layer`]
/// (using `peer_repr: String` rather than [`SocketAddr`]) so the
/// audit record schema stays consistent with Phase A.
async fn auth_layer_rbac(
    state: ApiState,
    rbac: crate::rbac::RotatableRegistry,
    mut req: axum::extract::Request,
    next: Next,
) -> Response {
    let route = classify_protected_path(req.uri().path());
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

    let snap = rbac.current();

    let Some(candidate_bytes) = candidate else {
        tracing::warn!(
            target: "graphreact::observer::audit",
            peer = %peer_repr,
            reason = "missing",
            "observer auth fail (missing)"
        );
        if let Some(sink) = state.audit_sink.as_ref() {
            sink.append(crate::AuditRecordInput {
                kind: "auth_fail",
                fields: serde_json::json!({
                    "peer": peer_repr,
                    "reason": "missing",
                }),
            });
        }
        return challenge_response();
    };

    // **M-Prod sub-track #1 Phase B Part 2 (Group 1c).** JWT-OIDC
    // bridge dispatch. eyJ-prefixed bearers route through
    // JwtVerifier::verify FIRST. The static-token walk below stays
    // bit-for-bit unchanged for non-eyJ bearers.
    //
    // Verify-failure semantics: emit `jwt_auth_fail` with the
    // closed-vocabulary reason tag from JwtVerifyError::reason_str
    // and return challenge_response — do NOT fall through to the
    // static-token walk (would double-emit auth_fail for the same
    // request). EXCEPTION: a MissingKid failure could legitimately
    // be a static-token coincidence that happens to start with
    // `eyJ`; in that case fall through silently and let the
    // existing match_token / auth_fail path handle it. UnknownKid
    // also falls through (operator may be mid-rotation; the static
    // token might still be the right credential during the
    // transition).
    if candidate_bytes.starts_with("eyJ") {
        if let Some(verifier) = state.jwt_verifier.as_ref() {
            // **Verify-finding MED patch — M-Prod #1 Phase B Part 2
            // (Group 1c).** `rate_limit_injector` runs JWT verify
            // before us and stashes the result in `PreVerifiedJwt`;
            // consume it here and skip the second verify to keep
            // single-verify cost across the rate-limit + auth
            // middleware layers. If the cache is absent (e.g. a
            // route bypasses the rate-limit middleware), fall back
            // to running the verify directly.
            let cached: Option<crate::jwt_oidc::VerifiedJwt> = req
                .extensions()
                .get::<PreVerifiedJwt>()
                .map(|p| p.0.clone());
            let verify_outcome: Result<
                crate::jwt_oidc::VerifiedJwt,
                crate::jwt_oidc::JwtVerifyError,
            > = match cached {
                Some(v) => Ok(v),
                None => verifier.verify(candidate_bytes),
            };
            match verify_outcome {
                Ok(verified) => {
                    match crate::jwt_oidc::derive_role(&verified, verifier.role_mapping()) {
                        Ok(role) => {
                            let role_str: &str = match &role {
                                crate::rbac::Role::Admin => "admin",
                                crate::rbac::Role::ReadOnly => "read_only",
                                crate::rbac::Role::Custom(_) => "custom",
                            };
                            if crate::jwt_oidc::check_route_for_role(&role, route) {
                                crate::audit::log_jwt_auth_success(
                                    &peer_repr,
                                    &verified.sub,
                                    &verified.iss,
                                    route,
                                    role_str,
                                    Some(verified.kid.as_str()),
                                    verified.exp_unix_ms,
                                    state.audit_sink.as_ref(),
                                );
                                req.extensions_mut().insert(AuthenticatedPrincipal {
                                    token_id: verified.sub.clone(),
                                    is_admin: matches!(role, crate::rbac::Role::Admin),
                                });
                                return next.run(req).await;
                            }
                            crate::audit::log_jwt_auth_fail(
                                &peer_repr,
                                "authz_deny",
                                Some(verified.iss.as_str()),
                                Some(verified.kid.as_str()),
                                state.audit_sink.as_ref(),
                            );
                            return forbidden_response();
                        }
                        Err(err) => {
                            crate::audit::log_jwt_auth_fail(
                                &peer_repr,
                                err.reason_str(),
                                Some(verified.iss.as_str()),
                                Some(verified.kid.as_str()),
                                state.audit_sink.as_ref(),
                            );
                            return challenge_response();
                        }
                    }
                }
                Err(
                    crate::jwt_oidc::JwtVerifyError::MissingKid
                    | crate::jwt_oidc::JwtVerifyError::UnknownKid,
                ) => {
                    // Could be a static token that coincidentally
                    // starts with eyJ, or a token signed by a key
                    // we don't have yet (operator mid-rotation).
                    // Fall through to the static-token walk; if
                    // the static path also fails, the standard
                    // auth_fail emits there.
                }
                Err(err) => {
                    crate::audit::log_jwt_auth_fail(
                        &peer_repr,
                        err.reason_str(),
                        None,
                        None,
                        state.audit_sink.as_ref(),
                    );
                    return challenge_response();
                }
            }
        }
    }

    let Some(entry) = snap.match_token(candidate_bytes.as_bytes()) else {
        tracing::warn!(
            target: "graphreact::observer::audit",
            peer = %peer_repr,
            reason = "wrong",
            "observer auth fail (wrong)"
        );
        if let Some(sink) = state.audit_sink.as_ref() {
            sink.append(crate::AuditRecordInput {
                kind: "auth_fail",
                fields: serde_json::json!({
                    "peer": peer_repr,
                    "reason": "wrong",
                }),
            });
        }
        return challenge_response();
    };

    if snap.is_expired(entry, std::time::SystemTime::now()) {
        // **Verify-finding patch (MED).** The expired path KNOWS the
        // matched principal — emit `token_id` alongside `reason` so
        // SOC pipelines can distinguish "known principal whose
        // credential aged out" from "unknown wire-side junk that was
        // expired-shaped" (= the missing/wrong arms above, which
        // legitimately have no token_id to record).
        let token_id_str = entry.id.0.as_str();
        tracing::warn!(
            target: "graphreact::observer::audit",
            peer = %peer_repr,
            reason = "expired",
            token_id = token_id_str,
            "observer auth fail (expired)"
        );
        if let Some(sink) = state.audit_sink.as_ref() {
            sink.append(crate::AuditRecordInput {
                kind: "auth_fail",
                fields: serde_json::json!({
                    "peer": peer_repr,
                    "reason": "expired",
                    "token_id": token_id_str,
                }),
            });
        }
        return challenge_response();
    }

    let role_str = match &entry.role {
        crate::rbac::Role::Admin => "admin",
        crate::rbac::Role::ReadOnly => "read_only",
        crate::rbac::Role::Custom(_) => "custom",
    };

    if snap.check_route(entry, route) {
        crate::audit::log_auth_success(&entry.id.0, route, role_str, state.audit_sink.as_ref());
        req.extensions_mut().insert(AuthenticatedPrincipal {
            token_id: entry.id.0.clone(),
            is_admin: matches!(entry.role, crate::rbac::Role::Admin),
        });
        next.run(req).await
    } else {
        crate::audit::log_authz_deny(
            &entry.id.0,
            route,
            role_str,
            "rbac",
            state.audit_sink.as_ref(),
        );
        forbidden_response()
    }
}

fn challenge_response() -> Response {
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        "401 unauthorized",
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"graphreact-observer\""),
    );
    resp
}

/// **M-Prod sub-track #1 Phase B Part 1.** 403 Forbidden response
/// for an authenticated principal that lacks authorization for the
/// requested route (RBAC deny).
///
/// 403 is semantically distinct from [`challenge_response`]'s 401:
/// the bearer authenticated cleanly + the principal is known; the
/// role doesn't permit this route. No `WWW-Authenticate` header
/// (the client doesn't have a different credential that would
/// succeed — that's an operator-side RBAC config change). No
/// `Retry-After` (not a backpressure response — retrying with the
/// same credentials will deterministically deny again).
fn forbidden_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        "403 forbidden",
    )
        .into_response()
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

    fn delegated_restart_test_state(
        role: crate::rbac::Role,
    ) -> (ApiState, mpsc::Receiver<SnapshotRequest>) {
        let entry = crate::rbac::TokenEntry {
            id: crate::rbac::TokenId("operator-1".into()),
            token: BearerToken::from_bytes(b"delegated-restart-token".to_vec()).unwrap(),
            role,
            expires_at: None,
        };
        let registry = crate::rbac::TokenRegistry::new(vec![entry]).unwrap();
        let (snapshot_tx, snapshot_rx) = mpsc::channel(SNAPSHOT_CHANNEL_CAPACITY);
        (
            ApiState {
                metrics: Arc::new(MetricsInner::new()),
                snapshot_tx,
                token: None,
                identity: Arc::new(Identity {
                    process_start_id: 1,
                    version: "test",
                    api_version: 1,
                }),
                ring: Arc::new(RingBufferTelemetry::new()),
                websocket: None,
                idempotency_cache: Arc::new(IdempotencyCache::new()),
                pending_calls: PendingCalls::new(),
                cluster_readyz: None,
                tls_readiness: None,
                audit_sink: None,
                draining: None,
                ingress_in_flight: IngressInFlight::new(),
                rbac: Some(crate::rbac::RotatableRegistry::new(Arc::new(registry))),
                jwt_verifier: None,
                forwarder_hub: None,
            },
            snapshot_rx,
        )
    }

    fn delegated_restart_http_request() -> axum::http::Request<axum::body::Body> {
        use axum::extract::ConnectInfo;
        let peer: SocketAddr = "127.0.0.1:34567".parse().unwrap();
        let mut request = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/admin/cluster/delegated-restart")
            .header(
                axum::http::header::AUTHORIZATION,
                "Bearer delegated-restart-token",
            )
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                r#"{"peer":"worker","child_node_id":"child-a"}"#,
            ))
            .unwrap();
        request.extensions_mut().insert(peer);
        request.extensions_mut().insert(ConnectInfo(peer));
        request
    }

    fn durable_run_http_request(
        method: Method,
        uri: &str,
        body: &'static str,
        idempotency_key: Option<&'static str>,
    ) -> axum::http::Request<axum::body::Body> {
        use axum::extract::ConnectInfo;
        let peer: SocketAddr = "127.0.0.1:34568".parse().unwrap();
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(
                axum::http::header::AUTHORIZATION,
                "Bearer delegated-restart-token",
            )
            .header(axum::http::header::CONTENT_TYPE, "application/json");
        if let Some(key) = idempotency_key {
            builder = builder.header("idempotency-key", key);
        }
        let mut request = builder.body(axum::body::Body::from(body)).unwrap();
        request.extensions_mut().insert(peer);
        request.extensions_mut().insert(ConnectInfo(peer));
        request
    }

    #[tokio::test]
    async fn durable_run_creation_requires_idempotency_key() {
        use tower::ServiceExt;
        let (state, _snapshot_rx) = delegated_restart_test_state(crate::rbac::Role::Admin);
        let app = build_router_with_rate_limit(state, RateLimitConfig::for_tests(1_000, 1_000));
        let response = app
            .oneshot(durable_run_http_request(
                Method::POST,
                "/v1/runs",
                r#"{"entrypoint":"summarize","payload":{}}"#,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn durable_run_creation_dispatches_and_returns_run() {
        use tower::ServiceExt;
        let (state, mut snapshot_rx) = delegated_restart_test_state(crate::rbac::Role::Admin);
        let app = build_router_with_rate_limit(state, RateLimitConfig::for_tests(1_000, 1_000));
        let responder = tokio::spawn(async move {
            match snapshot_rx.recv().await.expect("durable run request") {
                SnapshotRequest::StartDurableRun {
                    entrypoint,
                    idempotency_key,
                    oneshot_tx,
                    ..
                } => {
                    assert_eq!(entrypoint, "summarize");
                    assert_eq!(idempotency_key, "request-1");
                    let _ = oneshot_tx.send(DurableRunBridgeResult::Ready {
                        run: DurableRunView {
                            run_id: "01900000-0000-7000-8000-000000000001".into(),
                            entrypoint,
                            status: "queued".into(),
                            created_at_ms: 1,
                            result: None,
                            error: None,
                        },
                        created: Some(true),
                    });
                }
                other => panic!("unexpected request: {other:?}"),
            }
        });
        let response = app
            .oneshot(durable_run_http_request(
                Method::POST,
                "/v1/runs",
                r#"{"entrypoint":"summarize","payload":{"prompt":"hello"}}"#,
                Some("request-1"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "queued");
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn durable_artifact_download_streams_stored_media_type() {
        use tower::ServiceExt;
        let (state, mut snapshot_rx) = delegated_restart_test_state(crate::rbac::Role::Admin);
        let app = build_router_with_rate_limit(state, RateLimitConfig::for_tests(1_000, 1_000));
        let responder = tokio::spawn(async move {
            match snapshot_rx.recv().await.expect("artifact request") {
                SnapshotRequest::GetArtifact {
                    artifact_id,
                    oneshot_tx,
                } => {
                    let _ = oneshot_tx.send(DurableArtifactBridgeResult::Content {
                        artifact: DurableArtifactView {
                            artifact_id,
                            run_id: "01900000-0000-7000-8000-000000000001".into(),
                            logical_name: "evidence.txt".into(),
                            media_type: "text/plain".into(),
                            size: 7,
                            created_at_ms: 1,
                            sha256: "00".repeat(32),
                        },
                        bytes: b"durable".to_vec(),
                    });
                }
                other => panic!("unexpected request: {other:?}"),
            }
        });
        let response = app
            .oneshot(durable_run_http_request(
                Method::GET,
                "/v1/artifacts/01900000-0000-7000-8000-000000000002",
                "",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "text/plain"
        );
        assert_eq!(response.headers()[axum::http::header::CONTENT_LENGTH], "7");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"durable");
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn delegated_restart_admin_dispatches_token_id_and_returns_correlation() {
        use tower::ServiceExt;
        let (state, mut snapshot_rx) = delegated_restart_test_state(crate::rbac::Role::Admin);
        let app = build_router_with_rate_limit(state, RateLimitConfig::for_tests(1_000, 1_000));
        let responder = tokio::spawn(async move {
            match snapshot_rx.recv().await.expect("delegated restart request") {
                SnapshotRequest::DelegatedRestart {
                    peer,
                    child_node_id,
                    token_id,
                    oneshot_tx,
                } => {
                    assert_eq!(peer, "worker");
                    assert_eq!(child_node_id, "child-a");
                    assert_eq!(token_id, "operator-1");
                    let _ = oneshot_tx.send(DelegatedRestartDispatchResult::Accepted {
                        correlation_id: 42,
                        peer,
                    });
                }
                other => panic!("unexpected request: {other:?}"),
            }
        });

        let response = app.oneshot(delegated_restart_http_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["correlation_id"], 42);
        assert_eq!(json["status"], "accepted");
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn delegated_restart_custom_role_is_denied_even_when_route_allowlisted() {
        use std::collections::BTreeSet;
        use tower::ServiceExt;
        let role = crate::rbac::Role::Custom(BTreeSet::from([
            "/v1/admin/cluster/delegated-restart".to_string(),
        ]));
        let (state, _snapshot_rx) = delegated_restart_test_state(role);
        let app = build_router_with_rate_limit(state, RateLimitConfig::for_tests(1_000, 1_000));

        let response = app.oneshot(delegated_restart_http_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"],
            "admin_role_required"
        );
    }

    #[tokio::test]
    async fn delegated_restart_bridge_saturation_returns_retryable_503() {
        use tower::ServiceExt;
        let (mut state, _snapshot_rx) = delegated_restart_test_state(crate::rbac::Role::Admin);
        let (full_tx, _full_rx) = mpsc::channel(1);
        let (unused_tx, _unused_rx) = oneshot::channel();
        full_tx.try_send(SnapshotRequest::Nodes(unused_tx)).unwrap();
        state.snapshot_tx = full_tx;
        let app = build_router_with_rate_limit(state, RateLimitConfig::for_tests(1_000, 1_000));

        let response = app.oneshot(delegated_restart_http_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[axum::http::header::RETRY_AFTER], "1");
    }

    #[tokio::test]
    async fn delegated_restart_unknown_peer_maps_to_404() {
        use tower::ServiceExt;
        let (state, mut snapshot_rx) = delegated_restart_test_state(crate::rbac::Role::Admin);
        let app = build_router_with_rate_limit(state, RateLimitConfig::for_tests(1_000, 1_000));
        let responder = tokio::spawn(async move {
            match snapshot_rx.recv().await.expect("delegated restart request") {
                SnapshotRequest::DelegatedRestart { oneshot_tx, .. } => {
                    let _ = oneshot_tx.send(DelegatedRestartDispatchResult::UnknownPeer);
                }
                other => panic!("unexpected request: {other:?}"),
            }
        });

        let response = app.oneshot(delegated_restart_http_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"],
            "unknown_peer"
        );
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn delegated_restart_dispatch_race_with_drain_maps_to_503() {
        use tower::ServiceExt;
        let (state, mut snapshot_rx) = delegated_restart_test_state(crate::rbac::Role::Admin);
        let app = build_router_with_rate_limit(state, RateLimitConfig::for_tests(1_000, 1_000));
        let responder = tokio::spawn(async move {
            match snapshot_rx.recv().await.expect("delegated restart request") {
                SnapshotRequest::DelegatedRestart { oneshot_tx, .. } => {
                    let _ = oneshot_tx.send(DelegatedRestartDispatchResult::Draining);
                }
                other => panic!("unexpected request: {other:?}"),
            }
        });

        let response = app.oneshot(delegated_restart_http_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"],
            "draining"
        );
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn delegated_restart_refuses_dispatch_while_draining() {
        use tower::ServiceExt;
        let (mut state, _snapshot_rx) = delegated_restart_test_state(crate::rbac::Role::Admin);
        let drain = DrainFlag::new();
        assert!(drain.start(20_000));
        state.draining = Some(drain);
        let app = build_router_with_rate_limit(state, RateLimitConfig::for_tests(1_000, 1_000));

        let response = app.oneshot(delegated_restart_http_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"],
            "draining"
        );
    }
}

#[cfg(test)]
mod rate_limit_tests {
    //! **M-Prod sub-track #6 Phase A** — fast in-process unit
    //! tests for the rate-limit classifier + the
    //! `rate_limited_response_from_error` shape. End-to-end (HTTP
    //! over a real listener with burst behavior + per-source-IP
    //! discrimination) lives in `tests/rate_limit.rs`; the unit
    //! coverage here keeps the cheap shape checks adjacent to the
    //! code they exercise.

    use super::*;

    #[test]
    fn classify_known_protected_routes_round_trip() {
        // Every registered protected-route pattern in
        // PROTECTED_ROUTE_LABELS must classify to itself when the
        // path is its concrete instantiation. Without this,
        // grx_rate_limit_blocked_total{route} would emit labels
        // for the raw-path variant (`/v1/nodes/abc`) instead of
        // the bounded pattern (`/v1/nodes/:id`).
        assert_eq!(classify_protected_path("/v1/identity"), "/v1/identity");
        assert_eq!(classify_protected_path("/v1/metrics"), "/v1/metrics");
        assert_eq!(classify_protected_path("/v1/nodes"), "/v1/nodes");
        assert_eq!(
            classify_protected_path("/v1/supervisor/tree"),
            "/v1/supervisor/tree"
        );
        assert_eq!(
            classify_protected_path("/v1/observer/events"),
            "/v1/observer/events"
        );
        assert_eq!(
            classify_protected_path("/v1/observer/events.sse"),
            "/v1/observer/events.sse"
        );
        assert_eq!(classify_protected_path("/v1/nodes/abc"), "/v1/nodes/:id");
        assert_eq!(
            classify_protected_path("/v1/nodes/abc/messages"),
            "/v1/nodes/:id/messages"
        );
        assert_eq!(
            classify_protected_path("/v1/nodes/abc/messages/recent"),
            "/v1/nodes/:id/messages/recent"
        );
        assert_eq!(
            classify_protected_path("/v1/nodes/abc/messages/bulk"),
            "/v1/nodes/:id/messages/bulk"
        );
        assert_eq!(
            classify_protected_path("/v1/nodes/abc/call"),
            "/v1/nodes/:id/call"
        );
    }

    #[test]
    fn classify_unknown_paths_collapse_to_sentinel() {
        // Routes outside the registered table — typos,
        // adversarial paths, anything not in
        // `PROTECTED_ROUTE_LABELS` — collapse to a single
        // `"unknown"` label so an attacker can't inflate the
        // metric's cardinality by hammering random URIs.
        assert_eq!(classify_protected_path("/v1/foo"), "unknown");
        assert_eq!(classify_protected_path("/v1/nodes/"), "unknown");
        assert_eq!(classify_protected_path("/v1/nodes/abc/weird"), "unknown");
        assert_eq!(classify_protected_path("/"), "unknown");
        assert_eq!(classify_protected_path(""), "unknown");
    }

    #[test]
    fn classify_returns_static_str_so_metric_label_is_zero_alloc() {
        // The metric callsite stores the label as `&'static str`
        // — verify the return type at compile time by passing it
        // through a function bound. If the classifier ever
        // returned an owned `String` (or `Cow`), this would
        // refuse to compile, surfacing the cardinality
        // regression at build time rather than at scrape time.
        fn static_label_only(_s: &'static str) {}
        static_label_only(classify_protected_path("/v1/identity"));
        static_label_only(classify_protected_path("/v1/nodes/abc"));
        static_label_only(classify_protected_path("/random"));
    }

    #[test]
    fn rate_limited_response_shape_matches_bridge_saturated_envelope() {
        // Operator contract: every back-pressure-class response
        // ships the SAME JSON envelope + Retry-After header
        // semantics; only the `error` discriminator changes.
        // Drift here breaks operator grep scripts that index on
        // `error` to triage `{rate_limited, bridge_saturated,
        // draining, scheduler_first_batch_pending,
        // cluster_no_peers_connected}`.
        let resp =
            rate_limited_response_from_error(tower_governor::GovernorError::TooManyRequests {
                wait_time: 7,
                headers: None,
            });
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry_after = resp
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .expect("Retry-After header required on rate_limited")
            .to_str()
            .unwrap();
        assert_eq!(retry_after, "7");
        let content_type = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .expect("Content-Type required")
            .to_str()
            .unwrap();
        assert_eq!(content_type, "application/json");
    }

    #[test]
    fn rate_limited_response_clamps_zero_wait_to_one_second_minimum() {
        // A `wait_time` of 0 would tell a client to retry
        // immediately, which spins. Clamp at 1s to match
        // bridge_saturated_response's `Retry-After: 1` baseline.
        let resp =
            rate_limited_response_from_error(tower_governor::GovernorError::TooManyRequests {
                wait_time: 0,
                headers: None,
            });
        let retry_after = resp
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(retry_after, "1");
    }

    #[test]
    fn rate_limited_response_clamp_survives_tower_governor_header_fold() {
        // **Verify-finding regression lock.** tower_governor's
        // `use_headers()` populates `extra` with a literal
        // `retry-after: <wait_time>` BEFORE calling our
        // error_handler. The fold MUST run before the clamp
        // overwrite — otherwise an under-1-second wait_time (the
        // default 50/sec config has a 20ms refill, so this hits
        // production on every blocked request) would silently
        // overwrite `Retry-After: 1` back to `0`, the body would
        // say `retry_after_secs: 1`, and a well-behaved client
        // honoring `Retry-After: 0` would spin-retry into a
        // tight loop, defeating the limiter's protective purpose.
        let mut sabotage = axum::http::HeaderMap::new();
        sabotage.insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_static("0"),
        );
        let resp =
            rate_limited_response_from_error(tower_governor::GovernorError::TooManyRequests {
                wait_time: 0,
                headers: Some(sabotage),
            });
        let retry_after = resp
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .expect("Retry-After present")
            .to_str()
            .expect("Retry-After valid ascii");
        assert_eq!(
            retry_after, "1",
            "Retry-After must survive the tower_governor header fold + match \
             the clamped body value (`retry_after_secs: 1`); got {retry_after}"
        );
    }

    #[test]
    fn rate_limited_response_unable_to_extract_key_surfaces_500() {
        // Should be unreachable under the Phase A wiring (the
        // accept loop injects ConnectInfo per accept) but lock
        // the shape so a regression that drops the inject site
        // surfaces as a loud 500 in audit logs rather than a
        // silent 200.
        let resp =
            rate_limited_response_from_error(tower_governor::GovernorError::UnableToExtractKey);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn protected_route_labels_table_matches_registered_routes() {
        // `PROTECTED_ROUTE_LABELS` must list exactly the 17
        // protected paths the router registers. If a new
        // protected route lands without updating the table, the
        // classifier collapses it to `"unknown"` and operators
        // lose per-route observability — surface that drift here
        // at unit-test time.
        let expected: std::collections::HashSet<&'static str> = [
            "/v1/admin/cluster/delegated-restart",
            "/v1/identity",
            "/v1/metrics",
            "/v1/runs",
            "/v1/runs/:run_id",
            "/v1/runs/:run_id/cancel",
            "/v1/runs/:run_id/artifacts",
            "/v1/artifacts/:artifact_id",
            "/v1/nodes",
            "/v1/nodes/:id",
            "/v1/nodes/:id/messages",
            "/v1/nodes/:id/messages/recent",
            "/v1/nodes/:id/messages/bulk",
            "/v1/nodes/:id/call",
            "/v1/supervisor/tree",
            "/v1/observer/events",
            "/v1/observer/events.sse",
        ]
        .into_iter()
        .collect();
        let actual: std::collections::HashSet<&'static str> =
            PROTECTED_ROUTE_LABELS.iter().copied().collect();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn rate_limit_is_per_source_ip() {
        // **Per-IP key extractor invariant.** Two distinct source
        // IPs each driving below-cap traffic should NEVER produce
        // 429 — the limiter is per-IP, not global. This test can't
        // be written as an HTTP integration test on a single host
        // (every loopback request comes from 127.0.0.1, so they
        // all share one bucket), so it pokes the router directly
        // via `tower::Service::oneshot` and crafts requests with
        // hand-rolled `ConnectInfo<SocketAddr>` extensions —
        // exactly what the `accept_loop` does per accept under
        // real traffic.
        use axum::body::Body;
        use axum::extract::ConnectInfo;
        use axum::http::Request;
        use std::net::{Ipv4Addr, SocketAddr};
        use tower::ServiceExt;

        // Tight cap: per_second=1, burst=2 — three requests from
        // one IP would 429 on the third. Sending TWO from each of
        // TWO different IPs must yield 0 429s.
        let metrics = Arc::new(MetricsInner::new());
        let snapshot_tx = mpsc::channel(1).0;
        let state = ApiState {
            metrics: metrics.clone(),
            snapshot_tx,
            token: None,
            identity: Arc::new(Identity {
                process_start_id: 1,
                version: "test",
                api_version: 1,
            }),
            ring: Arc::new(RingBufferTelemetry::new()),
            websocket: None,
            idempotency_cache: Arc::new(IdempotencyCache::new()),
            pending_calls: PendingCalls::new(),
            cluster_readyz: None,
            tls_readiness: None,
            audit_sink: None,
            draining: None,
            ingress_in_flight: IngressInFlight::new(),
            // M-Prod #1 Phase B Part 1 — None preserves the
            // Phase A single-token path that this rate-limit
            // test exercises (token is also None → no auth at
            // all on this synthetic state).
            rbac: None,
            // M-Prod #1 Phase B Part 2 (Group 1c) — JWT bridge
            // is unwired for this rate-limit substrate test.
            jwt_verifier: None,
            // M-Prod #1 Phase B Part 2 (Group 1d) — forwarder hub
            // unwired for this rate-limit substrate test.
            forwarder_hub: None,
        };
        let app = build_router_with_rate_limit(state, RateLimitConfig::for_tests(1, 2));

        let alice: SocketAddr = (Ipv4Addr::new(10, 0, 0, 1), 30_000).into();
        let bob: SocketAddr = (Ipv4Addr::new(10, 0, 0, 2), 30_001).into();

        let mut alice_statuses = Vec::new();
        let mut bob_statuses = Vec::new();
        for _ in 0..2 {
            // Alice's request
            let mut req = Request::builder()
                .uri("/v1/identity")
                .body(Body::empty())
                .unwrap();
            req.extensions_mut().insert(alice);
            req.extensions_mut().insert(ConnectInfo(alice));
            let resp = app.clone().oneshot(req).await.unwrap();
            alice_statuses.push(resp.status().as_u16());

            // Bob's request
            let mut req = Request::builder()
                .uri("/v1/identity")
                .body(Body::empty())
                .unwrap();
            req.extensions_mut().insert(bob);
            req.extensions_mut().insert(ConnectInfo(bob));
            let resp = app.clone().oneshot(req).await.unwrap();
            bob_statuses.push(resp.status().as_u16());
        }
        let alice_429 = alice_statuses.iter().filter(|s| **s == 429).count();
        let bob_429 = bob_statuses.iter().filter(|s| **s == 429).count();
        assert_eq!(
            alice_429, 0,
            "alice (10.0.0.1) under cap, got {alice_statuses:?}"
        );
        assert_eq!(bob_429, 0, "bob (10.0.0.2) under cap, got {bob_statuses:?}");
        // Now a 3rd request from alice — same IP, over burst —
        // MUST 429. This locks the per-IP discrimination: if the
        // limiter were global, the 4 already-spent tokens (2
        // alice + 2 bob) would have exhausted a burst=2 bucket
        // long ago.
        let mut req = Request::builder()
            .uri("/v1/identity")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(alice);
        req.extensions_mut().insert(ConnectInfo(alice));
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status().as_u16(),
            429,
            "alice's 3rd request over burst MUST 429"
        );
    }

    #[test]
    fn rate_limit_blocked_metric_uses_static_route_label() {
        // Mechanically verify the metric increment + bounded
        // label: feed a known route through the classifier into
        // the counter, then walk the DashMap to confirm exactly
        // one series exists with the expected label value.
        let m = MetricsInner::new();
        assert!(m.rate_limit_blocked.is_empty());

        let route = classify_protected_path("/v1/nodes/abc/messages");
        m.rate_limit_blocked_inc(route, "per_ip");
        m.rate_limit_blocked_inc(route, "per_ip");
        // Same route, different bucket — must produce a SEPARATE
        // series under the 2-label vocabulary.
        m.rate_limit_blocked_inc(route, "per_token");

        assert_eq!(m.rate_limit_blocked.len(), 2);
        let entries: Vec<_> = m
            .rate_limit_blocked
            .iter()
            .map(|e| {
                let v = e.value();
                let bucket = v
                    .labels
                    .iter()
                    .find(|(k, _)| *k == "bucket")
                    .map(|(_, v)| v.clone())
                    .unwrap();
                let route_lv = v
                    .labels
                    .iter()
                    .find(|(k, _)| *k == "route")
                    .map(|(_, v)| v.clone())
                    .unwrap();
                (route_lv, bucket, v.counter.get())
            })
            .collect();
        let per_ip = entries
            .iter()
            .find(|(_, b, _)| b == "per_ip")
            .expect("per_ip series present");
        let per_token = entries
            .iter()
            .find(|(_, b, _)| b == "per_token")
            .expect("per_token series present");
        assert_eq!(per_ip.0, "/v1/nodes/:id/messages");
        assert_eq!(per_ip.2, 2);
        assert_eq!(per_token.0, "/v1/nodes/:id/messages");
        assert_eq!(per_token.2, 1);
    }
}
