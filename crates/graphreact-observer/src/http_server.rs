//! **M16-Backend Phase 2.** Hyper-1.x HTTP server hosting the
//! observer's `/metrics` and `/healthz` endpoints.
//!
//! Single-route routing via a `service_fn` matched on `(method,
//! path)` — at one `/metrics` route + one `/healthz` route, full
//! Tower middleware infra is overkill. P4 lands axum + Tower for
//! the multi-route management API; P2 stays skinny.
//!
//! ## Connection lifecycle
//!
//! 1. Bind a tokio `TcpListener` on the configured address.
//! 2. `loop { listener.accept() }` — each accepted connection spawns
//!    a task that handles HTTP/1.1 over the bare TCP socket (or
//!    over a rustls-wrapped tokio stream when TLS is configured).
//! 3. Per-connection task runs
//!    `hyper_util::server::conn::auto::Builder::new(...).serve_connection(...)`
//!    to negotiate HTTP/1 (HTTP/2 is left off for P2 — Prometheus
//!    scrape clients all support HTTP/1.1).
//! 4. Per-request handler fans on path: `/metrics` (auth-checked,
//!    text-format render), `/healthz` (always 200), other paths 404.
//!
//! ## Shutdown
//!
//! The accept loop owns a `tokio::sync::Notify` clone. The daemon
//! drops it (or calls `notify_one`) at shutdown, the accept loop
//! sees it inside a `select!`, and the loop exits. In-flight
//! connections finish their current response before their tasks
//! drop — operators get a clean shutdown without truncated bodies.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_rustls::TlsAcceptor;

use crate::audit;
use crate::auth::BearerToken;
use crate::bind::BindConfig;
use crate::metrics::MetricsInner;
use crate::prometheus::encode_text;

/// Content-Type emitted for `/metrics`. Matches Prometheus's text
/// exposition format 0.0.4 — every Prometheus scraper accepts this.
const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Per-server context shared into every per-connection task.
struct ServerCtx {
    metrics: Arc<MetricsInner>,
    token: Option<BearerToken>,
}

/// Outcome of a server bind. Holds the listening address (which
/// may differ from the requested address when port=0 is used in
/// tests for ephemeral binding) + a [`Notify`] handle the daemon
/// uses to signal shutdown + a `JoinHandle` for the accept loop's
/// task.
pub struct ServerHandle {
    pub local_addr: SocketAddr,
    pub shutdown: Arc<Notify>,
    pub task: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    /// Signal shutdown + wait for the accept loop task to finish.
    /// In-flight connections finish their current response before
    /// the task drops.
    pub async fn shutdown(self) {
        self.shutdown.notify_one();
        let _ = self.task.await;
    }
}

/// Spawn the observer HTTP server on the bound address. Returns a
/// [`ServerHandle`] the daemon owns for lifetime management.
///
/// This is async because [`TcpListener::bind`] is async; the
/// returned handle's `task` keeps running until shutdown.
pub async fn spawn(
    bind: BindConfig,
    metrics: Arc<MetricsInner>,
) -> Result<ServerHandle, std::io::Error> {
    let listener = TcpListener::bind(bind.addr).await?;
    let local_addr = listener.local_addr()?;
    // **M-Prod sub-track #1 Phase A.** Legacy P2 `/metrics` server
    // is sink-free — pre-management-API embedders use the modern
    // `spawn_management_api` path to wire an `AuditSink`. Passing
    // `None` here preserves bit-for-bit identical tracing output
    // for the legacy server.
    audit::log_bind(local_addr, bind.tls.is_some(), bind.token.is_some(), None);

    let ctx = Arc::new(ServerCtx {
        metrics,
        token: bind.token,
    });
    // **M-Prod sub-track #2 Phase A.** Carry the rotatable handle,
    // NOT a pre-built `TlsAcceptor`. The legacy `/metrics` scrape
    // server picks up rotations driven through
    // `ApiServerHandle::reload_tls` because the rotation handle
    // (an `Arc<ArcSwap<_>>` under the hood) is shared across every
    // clone of `RotatableServerConfig`.
    let tls_handle: Option<crate::RotatableServerConfig> =
        bind.tls.as_ref().map(|tls| tls.server.clone());
    let shutdown = Arc::new(Notify::new());
    let shutdown_for_loop = shutdown.clone();

    let task = tokio::spawn(async move {
        accept_loop(listener, ctx, tls_handle, shutdown_for_loop).await;
    });

    Ok(ServerHandle {
        local_addr,
        shutdown,
        task,
    })
}

async fn accept_loop(
    listener: TcpListener,
    ctx: Arc<ServerCtx>,
    tls_handle: Option<crate::RotatableServerConfig>,
    shutdown: Arc<Notify>,
) {
    loop {
        let accept = listener.accept();
        tokio::select! {
            biased;
            () = shutdown.notified() => {
                // Operator asked us to stop — break the accept loop.
                // In-flight per-connection tasks finish on their own.
                break;
            }
            res = accept => {
                let (socket, peer) = match res {
                    Ok(pair) => pair,
                    Err(err) => {
                        // Accept errors are typically transient
                        // (out-of-fd, etc.); log + continue.
                        tracing::warn!(
                            target: "graphreact::observer::audit",
                            ?err,
                            "observer accept failed"
                        );
                        continue;
                    }
                };
                let ctx = ctx.clone();
                // **M-Prod sub-track #2 Phase A.** Build the
                // `TlsAcceptor` PER ACCEPT off the current
                // rotation snapshot — see `crate::tls_rotation`
                // module docs for why this can't be cached at
                // spawn time.
                let acceptor = tls_handle
                    .as_ref()
                    .map(|handle| TlsAcceptor::from(handle.current()));
                tokio::spawn(async move {
                    handle_connection(socket, peer, ctx, acceptor).await;
                });
            }
        }
    }
}

async fn handle_connection(
    socket: tokio::net::TcpStream,
    peer: SocketAddr,
    ctx: Arc<ServerCtx>,
    tls_acceptor: Option<TlsAcceptor>,
) {
    // Resolve to either a plaintext or TLS-wrapped stream, then
    // hand off to hyper. The two branches diverge in stream type
    // so we fan out the `serve_connection` call into each branch.
    if let Some(acceptor) = tls_acceptor {
        match acceptor.accept(socket).await {
            Ok(tls_stream) => {
                serve_one_connection(TokioIo::new(tls_stream), peer, ctx).await;
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
        serve_one_connection(TokioIo::new(socket), peer, ctx).await;
    }
}

async fn serve_one_connection<I>(io: I, peer: SocketAddr, ctx: Arc<ServerCtx>)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let svc = service_fn(move |req: Request<Incoming>| {
        let ctx = ctx.clone();
        async move { Ok::<_, Infallible>(handle_request(&req, peer, &ctx)) }
    });
    if let Err(err) = ConnBuilder::new(TokioExecutor::new())
        .serve_connection(io, svc)
        .await
    {
        // Connection-level errors include client-side disconnects
        // mid-response; log at debug to avoid spamming on every
        // scrape that closes early.
        tracing::debug!(
            target: "graphreact::observer::audit",
            %peer,
            ?err,
            "observer connection terminated"
        );
    }
}

fn handle_request(
    req: &Request<Incoming>,
    peer: SocketAddr,
    ctx: &ServerCtx,
) -> Response<Full<Bytes>> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    match (method, path.as_str()) {
        (Method::GET, "/healthz") => {
            // Health check intentionally bypasses auth — operators
            // need a way to liveness-check the daemon without
            // shipping the bearer token to every load balancer.
            // P2 is liveness-only; readiness ("can serve metrics
            // for a known scheduler?") lands at M-Prod.
            // Legacy P2 server keeps a sink-free fan: see comment
            // on `audit::log_bind` above.
            audit::log_response(peer, "/healthz", 200, 2, None);
            response(StatusCode::OK, "text/plain; charset=utf-8", "ok")
        }
        (Method::GET, "/metrics") => match check_auth(req, ctx) {
            AuthCheck::Ok => {
                let body = encode_text(&ctx.metrics);
                let bytes = body.len();
                audit::log_response(peer, "/metrics", 200, bytes, None);
                response(StatusCode::OK, METRICS_CONTENT_TYPE, body)
            }
            AuthCheck::Missing => {
                audit::log_auth_fail(peer, "missing", None);
                challenge_response()
            }
            AuthCheck::Wrong => {
                audit::log_auth_fail(peer, "wrong", None);
                challenge_response()
            }
        },
        (_, _) => {
            audit::log_response(peer, &path, 404, 0, None);
            response(
                StatusCode::NOT_FOUND,
                "text/plain; charset=utf-8",
                "not found",
            )
        }
    }
}

enum AuthCheck {
    Ok,
    Missing,
    Wrong,
}

fn check_auth(req: &Request<Incoming>, ctx: &ServerCtx) -> AuthCheck {
    let Some(expected) = &ctx.token else {
        // No token configured = no auth required (loopback case;
        // BindConfig::from_flags already enforced this is safe).
        return AuthCheck::Ok;
    };
    let Some(header) = req.headers().get(AUTHORIZATION) else {
        return AuthCheck::Missing;
    };
    let Ok(value) = header.to_str() else {
        // Non-UTF-8 Authorization header — treat as missing rather
        // than letting a panic from the parser surface.
        return AuthCheck::Missing;
    };
    let Some(rest) = value.strip_prefix("Bearer ") else {
        return AuthCheck::Missing;
    };
    if expected.matches(rest.as_bytes()) {
        AuthCheck::Ok
    } else {
        AuthCheck::Wrong
    }
}

fn response(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<Bytes>,
) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(body.into()));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

fn challenge_response() -> Response<Full<Bytes>> {
    let mut resp = response(
        StatusCode::UNAUTHORIZED,
        "text/plain; charset=utf-8",
        "401 unauthorized",
    );
    resp.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"graphreact-observer\""),
    );
    resp
}
