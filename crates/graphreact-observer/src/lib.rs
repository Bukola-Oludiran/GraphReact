//! **M16-Backend Phase 1.** Observer + telemetry surfaces for the
//! GraphReact (Backend) product.
//!
//! The [`graphreact_core::TelemetrySink`] trait is defined in core so
//! the scheduler can call it without a crate-graph dependency on this
//! crate. Rich consumer impls live here:
//!
//! - [`MultiplexSink`] fans events to multiple consumers behind a
//!   single sink installation point. Phase 2 onward attaches a stack
//!   of sinks (Prometheus + JSON logs + WebSocket fan-out + OTel)
//!   through one `MultiplexSink`; the scheduler still sees a single
//!   `Arc<dyn TelemetrySink>`.
//! - [`JsonLogTelemetry`] writes a single-line JSON record per event
//!   to stderr (default) or to the path named by the
//!   `GRX_OBSERVER_LOG` environment variable. Phase 1's first
//!   consumer; validates the trait shape end-to-end.
//!
//! Phase 2 adds Prometheus exporter + bind-defaults + auth substrate;
//! Phase 3 adds tracing-subscriber JSON-log integration; Phase 4+ add
//! the management API, terminal UI, WebSocket stream, and per-body
//! debug protocols.
//!
//! ## Hot-path discipline (re-stated)
//!
//! Every method on a [`graphreact_core::TelemetrySink`] runs from the
//! scheduler hot path while it holds internal `RefCell` borrows.
//! Sinks defined here:
//!
//! - MUST NOT call back into the scheduler.
//! - MUST NOT block.
//! - MUST keep allocations bounded — Phase-2 Prometheus sinks use
//!   atomic counters (no per-event `String` allocations); Phase-1
//!   `JsonLogTelemetry` uses one `serde_json::to_writer` call per
//!   event behind a `Mutex<Box<dyn Write + Send>>` (acceptable here
//!   because Phase 1's bench gate already accounts for it; production
//!   consumers should prefer non-blocking emit).

pub mod audit;
pub mod auth;
pub mod bind;
pub mod events;
pub mod http_server;
pub mod json_escape;
pub mod json_logs;
pub mod management_api;
pub mod metrics;
pub mod multiplex;
#[cfg(feature = "otel")]
pub mod otel;
pub mod prometheus;
pub mod prometheus_sink;
pub mod ring_buffer;
pub mod tls;
pub mod tracing_layer;
pub mod websocket;

// **M16-Backend Phase 10.** OTel re-exports gated behind the `otel`
// feature; default builds neither pull the OTel SDK deps nor see the
// types. The `WireTracingHook` trait lives in `graphreact-dist`
// (closer to the wire serialization callsite + capability
// negotiation); `OtelSink` impls it but the trait is exported by
// dist. See `docs/backend/m16-otel.md` for the span shape.
#[cfg(feature = "otel")]
pub use otel::{
    build_otel_sink, decode_wire_context, encode_wire_context, OtelSink, OBSERVER_OTEL_V1,
};

/// **M16-Backend Phase 10.** Re-export of the OTel SDK's
/// `SdkTracerProvider` so callers (e.g. `graphreact-cli`'s daemon
/// handle) can park a provider for shutdown-time span flush WITHOUT
/// taking a direct dep on `opentelemetry_sdk`. Available only under
/// the `otel` feature.
#[cfg(feature = "otel")]
pub use opentelemetry_sdk::trace::SdkTracerProvider;

pub use auth::{AuthError, BearerToken};
pub use bind::{BindConfig, BindConfigError, BindFlags, DEFAULT_BIND};
pub use events::TelemetryEvent;
pub use graphreact_core::TelemetrySink;
pub use http_server::{spawn as spawn_http_server, ServerHandle};
pub use json_logs::JsonLogTelemetry;
pub use management_api::{
    build_router, drain_snapshot_requests, parse_bulk_line, spawn as spawn_management_api,
    spawn_with_cluster_readyz as spawn_management_api_with_cluster_readyz, ApiServerHandle,
    ApiState, BulkInjectResult, BulkLineBody, CallMessageBody, CallResult, ClusterReadyzProbe,
    IdempotencyCache, Identity, InjectMessageBody, InjectMessageWithReplyDisposition, InjectResult,
    PendingCallGuard, PendingCalls, SnapshotRequest, BULK_MAX_BODY_BYTES, BULK_MAX_LINE_BYTES,
    DEFAULT_BULK_LINE_CAP, SNAPSHOT_CHANNEL_CAPACITY,
};
pub use metrics::{Counter, Gauge, Histogram, HistogramSnapshot, LabeledCounter, MetricsInner};
pub use multiplex::MultiplexSink;
pub use prometheus::encode_text;
pub use prometheus_sink::PrometheusSink;
pub use ring_buffer::{MessageSnapshot, RingBufferTelemetry, DEFAULT_RING_CAPACITY};
pub use tls::{ServerTlsConfig, TlsError};
pub use tracing_layer::{stderr_writer, TracingJsonLayer, WriterHandle};
pub use websocket::{EventFilter, WebSocketSink, BROADCAST_CAPACITY};

/// **M16-Backend Phase 3.** Install a `tracing_subscriber::Registry`
/// with our [`TracingJsonLayer`] attached, writing single-line JSON
/// to the supplied writer. Returns the resulting subscriber so the
/// caller can chain additional layers (e.g. an `EnvFilter`) before
/// calling `.init()` or `.set_global_default(...)`.
///
/// Opt-in installation pattern: callers choose whether to set the
/// subscriber as global default (process-wide) or per-thread (via
/// `tracing::subscriber::with_default`). The crate itself NEVER
/// touches the global default — that's the application's choice.
///
/// ## Typical use
///
/// ```ignore
/// use graphreact_observer::install_json_logs_subscriber;
/// use std::fs::OpenOptions;
///
/// let log = OpenOptions::new()
///     .create(true)
///     .append(true)
///     .open("/var/log/grx-observer.log")
///     .expect("open log");
/// let subscriber = install_json_logs_subscriber(Box::new(log));
/// tracing::subscriber::set_global_default(subscriber).expect("install");
/// ```
#[must_use]
pub fn install_json_logs_subscriber(
    writer: Box<dyn std::io::Write + Send>,
) -> impl tracing::Subscriber + Send + Sync {
    use tracing_subscriber::layer::SubscriberExt;
    tracing_subscriber::Registry::default().with(TracingJsonLayer::new(writer))
}
