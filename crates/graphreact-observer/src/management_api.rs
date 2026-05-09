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

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_rustls::TlsAcceptor;

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
}

/// Maximum number of in-flight `SnapshotRequest`s on the bridge
/// channel. Operators hammering the management API past this rate
/// see 503 Service Unavailable rather than unbounded memory growth.
/// Substrate-not-MVP: backpressure visible from day one.
pub const SNAPSHOT_CHANNEL_CAPACITY: usize = 256;

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

    public_routes.merge(protected_routes).with_state(state)
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
    let state = ApiState {
        metrics,
        snapshot_tx,
        token: bind.token.map(Arc::new),
        identity,
        ring: ring.clone(),
        websocket: Some(websocket.clone()),
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
