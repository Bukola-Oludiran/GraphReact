//! **M16-Backend Phase 10.** OpenTelemetry traces (feature-gated).
//!
//! [`OtelSink`] is the [`graphreact_core::TelemetrySink`] consumer
//! that converts the 12 hot-path scheduler events into OTel spans
//! exported via OTLP/gRPC to a stock OTel collector
//! (Tempo / Jaeger / Honeycomb / etc.).
//!
//! ## Span shape
//!
//! - **Root span per tick**: `scheduler.tick` opens at
//!   [`graphreact_core::TelemetrySink::on_scheduler_tick_start`] and
//!   closes at [`graphreact_core::TelemetrySink::on_scheduler_tick_end`].
//!   Attributes set at end: `iterations`, `terminated_by_depth_cap`.
//! - **Child spans**:
//!   - `node.evaluate` — one per
//!     [`graphreact_core::TelemetrySink::on_node_evaluate_start`] /
//!     `on_node_evaluate_end` pair. Attribute `node.id`.
//!   - `message.deliver` — one-shot span fired from
//!     [`graphreact_core::TelemetrySink::on_message_delivered`].
//!     Attributes: `from`, `to`, `type`, `ttl`, `direct`, `accepted`.
//!     If the scheduler's `Message.trace_context` is `Some`, the span
//!     includes an OTel `Link` pointing at the remote (TraceID, SpanID)
//!     so 3-hop messages produce 3-link traces in the collector view.
//! - **Span events on the active root span** (instant, zero-duration):
//!   - `node.mount` / `node.unmount` — from `on_node_mounted` /
//!     `on_node_unmounted`. Attributes carry `node.id` + (for unmount)
//!     the termination reason.
//!   - `topology.event` — from `on_topology_event`. `kind` + optional
//!     `node.id` / `edge.{u,v,key}`.
//!   - `supervisor.decision` — from `on_supervisor_decision`.
//!     `failed_id` + `reason` + `action`.
//!   - `remote.message` — from `on_remote_message`. `direction` +
//!     `peer` + `from` + `to` + `type` + `byte_size`.
//!   - `hot.reload` — from `on_hot_reload`. Counts + paths.
//!   - `queue.depths` — from `on_queue_depths`. Snapshot fields as
//!     attributes (sampled once per cascade round, NOT per message).
//!
//! ## Cross-node span links
//!
//! When [`graphreact_core::TelemetrySink::on_message_delivered`] fires
//! with [`graphreact_core::telemetry::MessageDelivered::trace_context`]
//! set to `Some([u8; 24])`, the bytes encode the remote
//! `TraceID(16) + SpanID(8)` (W3C SpanContext minimum for span linkage).
//! [`OtelSink`] decodes this and attaches an OTel `Link` to the
//! `message.deliver` span. The cross-cluster trace is then assembled
//! collector-side.
//!
//! Outbound propagation: when a body's `useEmit` resolves to a remote
//! node, the dist crate consults the [`WireTracingHook`] slot
//! installed via `Cluster::install_wire_tracing_hook`. The hook
//! returns the current OTel span context as `[u8; 24]`; dist embeds
//! it in the [`graphreact_dist::wire::Message::trace_context`] field.
//! Peers must advertise the `OBSERVER_OTEL_V1` capability + run
//! `WIRE_VERSION = 3` for trace context to propagate.
//!
//! ## Bytewidth reframing (P10 vs plan-locked)
//!
//! Plan-authored design specified `Option<[u8; 16]>` (TraceID only).
//! `[u8; 16]` is sufficient for trace-grouping but NOT for span
//! linkage — OTel's `Link` attribute requires a full SpanContext
//! (TraceID + SpanID = 24 bytes minimum). Shipping `Option<[u8; 24]>`
//! so receiver-side spans attach a real `Link { trace_id, span_id }`
//! and the cross-cluster trace renders as a connected DAG in the
//! collector. Same shape of plan reframing as P2's `{policy} →
//! {action}` Prometheus label rename.
//!
//! ## Hot-path discipline
//!
//! Span creation in opentelemetry_sdk is non-blocking by default —
//! the BatchSpanProcessor buffers spans on a background tokio task
//! and exports asynchronously. Per-event cost on the scheduler hot
//! path is one allocation (the span data) + a queue push.

// `as i64` casts in event-payload attribute construction: values
// are observability counters (queue depths, byte sizes, message
// counts, etc.) where overflow past i64::MAX (9.2 quintillion) is
// not a realistic production concern. OTel's `KeyValue::new`
// accepts i64 as the integer attribute type; `i64::try_from` would
// add ~10 LOC of `.unwrap_or(i64::MAX)` boilerplate per attribute
// for zero observable benefit.
#![allow(clippy::cast_possible_wrap)]

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use graphreact_core::telemetry::{
    HotReload, MessageDelivered, NodeEvaluateEnd, NodeEvaluateStart, NodeMounted, NodeUnmounted,
    QueueDepths, RemoteDirection, RemoteMessage, SchedulerTickEnd, SchedulerTickStart,
    SupervisorAction, SupervisorDecisionMade, TelemetrySink, TopologyEventOccurred,
};

use opentelemetry::trace::{
    Link, Span, SpanContext, SpanId, SpanKind, Status, TraceContextExt, TraceFlags, TraceId,
    TraceState, Tracer, TracerProvider as _,
};
use opentelemetry::{Context, ContextGuard, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use opentelemetry_sdk::Resource;

/// Add events to the currently-active span (root tick or innermost
/// node.evaluate). No-op if no span is active.
///
/// Macro instead of function because [`opentelemetry::trace::Span`]
/// has generic methods (`add_event<T>`, `set_attributes(impl IntoIterator)`,
/// `update_name<T>`) and is therefore NOT dyn-compatible — a closure
/// taking `&dyn Span` won't compile. The macro inlines the
/// `Context::current().span()` walk + validity check at each call site
/// so the generic call resolves against the concrete `SpanRef<'_>` type.
///
/// Defined at the top of the file because `macro_rules!` uses
/// text-position scoping; all call sites below this line.
macro_rules! with_active_span {
    ($span:ident => $body:block) => {{
        let __cx = Context::current();
        let $span = __cx.span();
        if $span.span_context().is_valid() {
            $body
        }
    }};
}

/// **M16-Backend Phase 10.** Capability flag advertised in the M14
/// handshake (`Hello.capabilities`) so peers know whether to populate
/// the [`graphreact_dist::wire::Message::trace_context`] field on
/// outbound remote messages. Mirrors `RELOAD_V1_CAPABILITY` from
/// M15 P10.
///
/// **Wire-format dependency**: `OBSERVER_OTEL_V1` requires
/// `WIRE_VERSION = 3` (the `Message.trace_context` field landed at
/// the same time as the version bump from M15's `WIRE_VERSION = 2`).
/// Peers without this capability still receive `WIRE_VERSION = 3`
/// packets correctly (the field deserializes as `None`); they simply
/// don't populate the field on their outbound side.
///
/// `graphreact-dist` defines a parallel constant
/// `OBSERVER_OTEL_V1_CAPABILITY` for use in capability negotiation
/// without forcing a `dist → observer` crate dependency. The two
/// must stay in sync; the dist-side test
/// `observer_otel_v1_capability_string_matches_observer_constant`
/// asserts the equality.
pub const OBSERVER_OTEL_V1: &str = "observer-otel-v1";

/// Default OTel resource attribute for spans emitted by GraphReact.
const DEFAULT_SERVICE_NAME: &str = "graphreact";

/// Default OTLP exporter timeout. Spans buffer behind a tokio
/// background task; the timeout bounds export retry latency, not
/// per-event hot-path latency.
const DEFAULT_EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

thread_local! {
    /// Active root tick span guard. `on_scheduler_tick_start` pushes
    /// the ContextGuard; `on_scheduler_tick_end` takes it, ends the
    /// active span via `Context::current().span().end()`, then drops
    /// the guard. Single slot — `process_batch` does not nest within
    /// a thread.
    ///
    /// **Why store only the ContextGuard (not the SdkSpan
    /// separately):** OTel's `SpanRef` (returned by `Context::span()`)
    /// uses interior mutability — `add_event` / `set_attribute` /
    /// `end` mutate the underlying span through the SpanRef. If we
    /// kept a separate owned `SdkSpan` outside Context, mutations
    /// via SpanRef and via the owned handle would diverge (events
    /// land on one, attributes on another). Storing ONLY the guard
    /// means there's a single span identity reachable via
    /// `Context::current().span()` for both event-firing and
    /// end-time attribute set.
    static TICK_GUARD: RefCell<Option<ContextGuard>> = const { RefCell::new(None) };

    /// Active node-evaluate context guards (LIFO stack). Today max
    /// depth 1 since `evaluate_node` does not nest re-entrantly, but
    /// kept as a stack for forward-compat with nested-component
    /// support.
    static NODE_GUARDS: RefCell<Vec<ContextGuard>> = const { RefCell::new(Vec::new()) };
}

/// **M16-Backend Phase 10.** OpenTelemetry traces consumer.
///
/// Construct via [`build_otel_sink`] (handles tracer-provider setup,
/// OTLP exporter wiring, and resource attributes) or directly via
/// [`OtelSink::with_tracer`] when an existing OTel SDK is in scope
/// (test fixtures using `InMemorySpanExporter`).
///
/// On Drop the underlying `SdkTracerProvider` is NOT shut down —
/// callers hold the provider via `build_otel_sink`'s returned guard
/// and call `provider.shutdown()` at process exit to flush buffered
/// spans. Same lifecycle pattern as
/// [`crate::tracing_layer::WriterHandle`].
pub struct OtelSink {
    tracer: SdkTracer,
}

impl OtelSink {
    /// Build an `OtelSink` from an existing `SdkTracer`. Test fixtures
    /// using `InMemorySpanExporter` instantiate this directly.
    #[must_use]
    pub fn with_tracer(tracer: SdkTracer) -> Self {
        Self { tracer }
    }

    /// Encode the active span context as `[u8; 24]` =
    /// `TraceID(16) || SpanID(8)`. Returns `None` if no span is active
    /// on the current thread (i.e. no `scheduler.tick` root span open).
    /// Used by [`WireTracingHook::extract_outbound`] to populate
    /// [`graphreact_dist::wire::Message::trace_context`] on outbound
    /// remote messages.
    #[must_use]
    pub fn current_wire_context() -> Option<[u8; 24]> {
        let cx = Context::current();
        let span = cx.span();
        let sc = span.span_context();
        if !sc.is_valid() {
            return None;
        }
        encode_wire_context(sc)
    }

    /// Internal helper: open a span, attach it to the OTel context
    /// (so child spans auto-parent + so SpanRef mutations from
    /// `with_active_span!` reach the same span), and return the
    /// guard that detaches it on drop.
    ///
    /// The owned `SdkSpan` MOVES into Context — subsequent access is
    /// via `Context::current().span()` returning a `SpanRef` that
    /// shares the underlying span (interior mutability).
    fn open_span(&self, name: &'static str, kind: SpanKind, attrs: Vec<KeyValue>) -> ContextGuard {
        let span = self
            .tracer
            .span_builder(name)
            .with_kind(kind)
            .with_attributes(attrs)
            .start_with_context(&self.tracer, &Context::current());
        Context::current_with_span(span).attach()
    }
}

/// Encode an OTel `SpanContext` into the 24-byte
/// `TraceID(16) || SpanID(8)` wire format used by
/// [`graphreact_dist::wire::Message::trace_context`].
#[must_use]
pub fn encode_wire_context(sc: &SpanContext) -> Option<[u8; 24]> {
    let trace_id = sc.trace_id().to_bytes();
    let span_id = sc.span_id().to_bytes();
    if trace_id == [0u8; 16] || span_id == [0u8; 8] {
        return None;
    }
    let mut out = [0u8; 24];
    out[..16].copy_from_slice(&trace_id);
    out[16..].copy_from_slice(&span_id);
    Some(out)
}

/// Decode a 24-byte wire-format context back into an OTel
/// `SpanContext` suitable for `Link` attribute attachment. Trace
/// flags are reset to default (sampled); trace state is empty.
#[must_use]
pub fn decode_wire_context(bytes: &[u8; 24]) -> SpanContext {
    let mut trace_id_bytes = [0u8; 16];
    trace_id_bytes.copy_from_slice(&bytes[..16]);
    let mut span_id_bytes = [0u8; 8];
    span_id_bytes.copy_from_slice(&bytes[16..]);
    SpanContext::new(
        TraceId::from_bytes(trace_id_bytes),
        SpanId::from_bytes(span_id_bytes),
        TraceFlags::SAMPLED,
        true,
        TraceState::default(),
    )
}

impl TelemetrySink for OtelSink {
    fn on_scheduler_tick_start(&self, _ev: &SchedulerTickStart) {
        let guard = self.open_span("scheduler.tick", SpanKind::Internal, Vec::new());
        TICK_GUARD.with(|cell| {
            cell.replace(Some(guard));
        });
    }

    fn on_scheduler_tick_end(&self, ev: &SchedulerTickEnd) {
        TICK_GUARD.with(|cell| {
            if let Some(guard) = cell.replace(None) {
                // Mutate + end the active span via SpanRef (interior
                // mutability shares the underlying SdkSpan with the
                // event-fire path).
                {
                    let cx = Context::current();
                    let span = cx.span();
                    span.set_attribute(KeyValue::new("iterations", i64::from(ev.iterations)));
                    span.set_attribute(KeyValue::new(
                        "terminated_by_depth_cap",
                        ev.terminated_by_depth_cap,
                    ));
                    if ev.terminated_by_depth_cap {
                        span.set_status(Status::error("scheduler hit MaxDepthExceeded cap"));
                    }
                    span.end();
                }
                drop(guard);
            }
        });
    }

    fn on_queue_depths(&self, ev: &QueueDepths) {
        with_active_span!(span => {
            span.add_event(
                "queue.depths",
                vec![
                    KeyValue::new("depth", i64::from(ev.depth)),
                    KeyValue::new("messages", ev.snapshot.messages as i64),
                    KeyValue::new("dirty", ev.snapshot.dirty as i64),
                    KeyValue::new("pending_topology", ev.snapshot.pending_topology as i64),
                    KeyValue::new("external_inbox", ev.snapshot.external_inbox as i64),
                    KeyValue::new("pending_mutations", ev.snapshot.pending_mutations as i64),
                ],
            );
        });
    }

    fn on_node_evaluate_start(&self, ev: &NodeEvaluateStart<'_>) {
        let guard = self.open_span(
            "node.evaluate",
            SpanKind::Internal,
            vec![KeyValue::new("node.id", ev.node_id.to_owned())],
        );
        NODE_GUARDS.with(|cell| {
            cell.borrow_mut().push(guard);
        });
    }

    fn on_node_evaluate_end(&self, _ev: &NodeEvaluateEnd<'_>) {
        NODE_GUARDS.with(|cell| {
            if let Some(guard) = cell.borrow_mut().pop() {
                {
                    let cx = Context::current();
                    let span = cx.span();
                    span.end();
                }
                drop(guard);
            }
        });
    }

    fn on_message_delivered(&self, ev: &MessageDelivered<'_>) {
        let mut builder = self
            .tracer
            .span_builder("message.deliver")
            .with_kind(SpanKind::Producer)
            .with_attributes(vec![
                KeyValue::new("from", ev.from.to_owned()),
                KeyValue::new("to", ev.to.to_owned()),
                KeyValue::new("type", ev.message_type.to_owned()),
                KeyValue::new("ttl", i64::from(ev.ttl)),
                KeyValue::new("direct", ev.direct),
                KeyValue::new("accepted", ev.accepted),
            ]);
        if let Some(bytes) = ev.trace_context.as_ref() {
            let remote = decode_wire_context(bytes);
            builder = builder.with_links(vec![Link::with_context(remote)]);
        }
        let mut span = builder.start_with_context(&self.tracer, &Context::current());
        if !ev.accepted {
            span.set_status(Status::error("message rejected post-policy"));
        }
        span.end();
    }

    fn on_node_mounted(&self, ev: &NodeMounted<'_>) {
        with_active_span!(span => {
            span.add_event(
                "node.mount",
                vec![KeyValue::new("node.id", ev.node_id.to_owned())],
            );
        });
    }

    fn on_node_unmounted(&self, ev: &NodeUnmounted<'_>) {
        let mut attrs = vec![
            KeyValue::new("node.id", ev.node_id.to_owned()),
            KeyValue::new("reason", ev.reason.to_owned()),
        ];
        if let Some(detail) = ev.reason_detail {
            attrs.push(KeyValue::new("reason.detail", detail.to_owned()));
        }
        with_active_span!(span => {
            span.add_event("node.unmount", attrs.clone());
        });
    }

    fn on_topology_event(&self, ev: &TopologyEventOccurred<'_>) {
        let mut attrs = vec![KeyValue::new("kind", ev.kind.to_owned())];
        if let Some(node_id) = ev.node_id {
            attrs.push(KeyValue::new("node.id", node_id.to_owned()));
        }
        if let Some(u) = ev.edge_u {
            attrs.push(KeyValue::new("edge.u", u.to_owned()));
        }
        if let Some(v) = ev.edge_v {
            attrs.push(KeyValue::new("edge.v", v.to_owned()));
        }
        if let Some(key) = ev.edge_key {
            attrs.push(KeyValue::new("edge.key", key.to_owned()));
        }
        with_active_span!(span => {
            span.add_event("topology.event", attrs.clone());
        });
    }

    fn on_supervisor_decision(&self, ev: &SupervisorDecisionMade<'_>) {
        let action_str = match ev.action {
            SupervisorAction::Proceeded => "proceeded",
            SupervisorAction::Deferred => "deferred",
            SupervisorAction::Escalated => "escalated",
        };
        with_active_span!(span => {
            span.add_event(
                "supervisor.decision",
                vec![
                    KeyValue::new("failed_id", ev.failed_id.to_owned()),
                    KeyValue::new("reason", ev.reason.to_owned()),
                    KeyValue::new("action", action_str),
                ],
            );
        });
    }

    fn on_remote_message(&self, ev: &RemoteMessage<'_>) {
        let dir_str = match ev.direction {
            RemoteDirection::Outbound => "outbound",
            RemoteDirection::Inbound => "inbound",
        };
        with_active_span!(span => {
            span.add_event(
                "remote.message",
                vec![
                    KeyValue::new("direction", dir_str),
                    KeyValue::new("peer", ev.peer.to_owned()),
                    KeyValue::new("from", ev.from.to_owned()),
                    KeyValue::new("to", ev.to.to_owned()),
                    KeyValue::new("type", ev.message_type.to_owned()),
                    KeyValue::new("byte_size", ev.byte_size as i64),
                ],
            );
        });
    }

    fn on_hot_reload(&self, ev: &HotReload<'_>) {
        let mut attrs = vec![
            KeyValue::new("nodes_replaced", ev.nodes_replaced as i64),
            KeyValue::new("nodes_remounted", ev.nodes_remounted as i64),
            KeyValue::new("edges_changed", ev.edges_changed as i64),
            KeyValue::new("rolled_back", ev.rolled_back),
            KeyValue::new("error_count", ev.error_count as i64),
        ];
        if let Some(path) = ev.source_path {
            attrs.push(KeyValue::new("source_path", path.to_owned()));
        }
        if let Some(peer) = ev.origin_peer {
            attrs.push(KeyValue::new("origin_peer", peer.to_owned()));
        }
        with_active_span!(span => {
            span.add_event("hot.reload", attrs.clone());
        });
    }
}

// `with_active_span!` macro defined at top of file (macro_rules uses
// text-position scoping; call sites in `impl TelemetrySink` reference
// it from above).

// **M16-Backend Phase 10.** `WireTracingHook` lives in
// `graphreact-dist::observability` where capability negotiation +
// wire serialization happen; the impl below lives here where OTel
// SpanContext extraction happens. `graphreact-dist` is an
// otel-feature-gated dep on this crate (default observer builds
// don't pull the dist crate or its M14 dep tree).

impl graphreact_dist::WireTracingHook for OtelSink {
    fn extract_outbound(&self) -> Option<[u8; 24]> {
        OtelSink::current_wire_context()
    }
}

/// Build an `OtelSink` + an `SdkTracerProvider` flush guard.
/// `OTEL_EXPORTER_OTLP_ENDPOINT` (env var) overrides `endpoint` per
/// the OTel SDK convention; `service_name` defaults to `"graphreact"`
/// and is overridable via `OTEL_SERVICE_NAME`.
///
/// Returns a tuple of (`Arc<OtelSink>`, `SdkTracerProvider`). The
/// caller MUST keep the provider alive for the daemon lifetime and
/// call `provider.shutdown()` at process exit so buffered spans
/// flush. `start_daemon` parks it on `DaemonHandle::otel_provider`.
pub fn build_otel_sink(
    endpoint: &str,
    service_name: Option<&str>,
) -> Result<(Arc<OtelSink>, SdkTracerProvider), opentelemetry_otlp::ExporterBuildError> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(DEFAULT_EXPORT_TIMEOUT)
        .build()?;

    let resource = Resource::builder()
        .with_service_name(service_name.unwrap_or(DEFAULT_SERVICE_NAME).to_owned())
        .build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("graphreact-observer");
    let sink = Arc::new(OtelSink::with_tracer(tracer));
    Ok((sink, provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `encode_wire_context` round-trips through `decode_wire_context`
    /// preserving TraceID + SpanID exactly.
    #[test]
    fn wire_context_round_trips() {
        let trace_id = TraceId::from_bytes([
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10,
        ]);
        let span_id = SpanId::from_bytes([0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8]);
        let sc = SpanContext::new(
            trace_id,
            span_id,
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        );
        let bytes = encode_wire_context(&sc).expect("non-zero ids encode");
        assert_eq!(&bytes[..16], &trace_id.to_bytes());
        assert_eq!(&bytes[16..], &span_id.to_bytes());
        let decoded = decode_wire_context(&bytes);
        assert_eq!(decoded.trace_id(), trace_id);
        assert_eq!(decoded.span_id(), span_id);
    }

    /// Zero-id (all-zero TraceID or all-zero SpanID) is invalid per
    /// W3C; `encode_wire_context` returns None.
    #[test]
    fn wire_context_rejects_zero_ids() {
        let zero_trace = SpanContext::new(
            TraceId::INVALID,
            SpanId::from_bytes([1, 2, 3, 4, 5, 6, 7, 8]),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        );
        assert!(encode_wire_context(&zero_trace).is_none());
        let zero_span = SpanContext::new(
            TraceId::from_bytes([1; 16]),
            SpanId::INVALID,
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        );
        assert!(encode_wire_context(&zero_span).is_none());
    }
}
