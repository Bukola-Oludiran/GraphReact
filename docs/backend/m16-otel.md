# M16-Backend Phase 10 — OpenTelemetry traces

**Status:** SHIPPED 2026-05-08. Distributed tracing substrate via OTLP/gRPC, including cross-node span linkage over the M14 wire format. Feature-gated behind `--features otel` on `graphreact-cli` so non-OTel deployments don't pay the OTel SDK dep cost.

## TL;DR

Run `grx cluster daemon --observer-otel-endpoint http://collector:4317` to export OpenTelemetry spans to a stock OTel collector (Tempo / Jaeger / Honeycomb / etc.). Every `process_batch` cascade round becomes a `scheduler.tick` root span with child `node.evaluate` spans (one per evaluated node) and one-shot `message.deliver` spans (one per message). Cross-node messages carry their source-side `SpanContext` in the M14 wire packet, so `message.deliver` spans on the receiver render as `Link` attributes pointing back at the sender — the cross-cluster trace assembles into a connected DAG collector-side.

## Span shape

| Span | When | Attributes / Events |
|---|---|---|
| **`scheduler.tick`** (root) | Per `process_batch` round | `iterations`, `terminated_by_depth_cap` (set at end). Status: Error if `terminated_by_depth_cap`. Carries instant events for mount/unmount/topology/supervisor/remote/hot_reload/queue.depths (all events fire on the active root). |
| **`node.evaluate`** (child of root) | Per evaluated node | `node.id`. SpanKind: Internal. |
| **`message.deliver`** (child of root) | Per delivered message (one-shot, fire-and-end) | `from`, `to`, `type`, `ttl`, `direct`, `accepted`. SpanKind: Producer. Status: Error if `accepted=false` (post-policy rejection). Includes `Link` attribute pointing at the source `SpanContext` when `trace_context` was carried over the wire. |

Instant events on the root tick span:

| Event | Source TelemetrySink callback | Attributes |
|---|---|---|
| `node.mount` | `on_node_mounted` | `node.id` |
| `node.unmount` | `on_node_unmounted` | `node.id`, `reason`, optional `reason.detail` |
| `topology.event` | `on_topology_event` | `kind` + optional `node.id`/`edge.{u,v,key}` |
| `supervisor.decision` | `on_supervisor_decision` | `failed_id`, `reason`, `action` |
| `remote.message` | `on_remote_message` | `direction`, `peer`, `from`, `to`, `type`, `byte_size` |
| `hot.reload` | `on_hot_reload` | `nodes_replaced`, `nodes_remounted`, `edges_changed`, `rolled_back`, `error_count`, optional `source_path`/`origin_peer` |
| `queue.depths` | `on_queue_depths` | `depth`, `messages`, `dirty`, `pending_topology`, `external_inbox`, `pending_mutations` |

## Cross-node span linkage

The canonical-plan acceptance line: **"3-hop messages produce 3-link traces"**. Wire-format details:

1. **`WIRE_VERSION` bumped 2→3** (`graphreact-dist::wire::WIRE_VERSION`) to add `trace_context: Option<[u8; 24]>` to the wire `Message` struct. The 24 bytes encode `TraceID(16) || SpanID(8)` — W3C SpanContext minimum for `Link` attribute attachment. Hard floor: `MIN_SUPPORTED_VERSION == WIRE_VERSION` per the existing M15 P10 invariant; v3 peers cannot interop with v2 peers (M-Prod owns rolling-version negotiation).
2. **`OBSERVER_OTEL_V1_CAPABILITY = "observer-otel-v1"`** advertised in the M14 handshake's `Hello.capabilities` / `ChallengeAck.server_capabilities` when the daemon is configured with `--observer-otel-endpoint`. Receiving peers DON'T need the capability advertised to read the field (wire format is mandatorily v3) — the capability hints to senders that their outbound emissions should populate it.
3. **`Cluster::install_wire_tracing_hook`** — new slot on `graphreact_dist::Cluster` mirroring `install_telemetry_sink`. The CLI's `start_daemon` installs `OtelSink` as both `TelemetrySink` (for span emission) AND `WireTracingHook` (for outbound trace-context extraction).
4. **Outbound flow** — `route_outbound` queries `WireTracingHookSlot.extract_outbound()` which returns the current thread's active OTel span context (extracted from `Context::current().span()`). Bytes land in `wire::Message.trace_context`.
5. **Inbound flow** — `wire_to_local` propagates the field into `graphreact_core::Message.trace_context`. The scheduler's `deliver` path forwards it into the `MessageDelivered` event. `OtelSink::on_message_delivered` decodes the bytes back into a `SpanContext` via `decode_wire_context` and attaches as a `Link` on the receiver-side `message.deliver` span.

### Bytewidth reframing (P10 vs plan-locked)

Plan-authored design specified `Option<[u8; 16]>` (TraceID only). `[u8; 16]` is sufficient for trace-grouping but NOT for span linkage — OTel's `Link` attribute requires a full SpanContext (TraceID + SpanID = 24 bytes minimum). Shipping `Option<[u8; 24]>` so receiver-side spans attach a real `Link { trace_id, span_id }` and the cross-cluster trace renders as a connected DAG in the collector. Same shape of plan reframing as P2's `{policy} → {action}` Prometheus label rename. W3C TraceState (additional flags) is NOT carried; M-Prod follow-up if needed.

## Operator usage

### Feature requirement

OTel deps (`opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp` with `grpc-tonic`) are gated behind the `otel` Cargo feature on `graphreact-cli`. Build the daemon with the feature:

```sh
cargo build --release --bin grx --features otel
```

Without the feature, `--observer-otel-endpoint` returns:
```
--observer-otel-endpoint requires `--features otel` on graphreact-cli
```

### Local Tempo / Jaeger fixture

```sh
docker run -d --name tempo \
  -p 4317:4317 -p 3200:3200 \
  grafana/tempo:latest \
  -config.file=/etc/tempo.yaml

grx cluster daemon \
  --node-id local@127.0.0.1:5050 \
  --listen 127.0.0.1:5050 \
  --cookie-env GRX_COOKIE \
  --observer-bind 127.0.0.1:9090 \
  --observer-otel-endpoint http://localhost:4317 \
  --observer-otel-service-name grx-local \
  --runtime null
```

Open `http://localhost:3200/` (Tempo UI) → search by `service.name = "grx-local"` → drill into a `scheduler.tick` trace.

### Cross-host

OTLP/gRPC supports TLS via standard `OTEL_EXPORTER_OTLP_*` env vars (per the OTel SDK convention):

```sh
export OTEL_EXPORTER_OTLP_HEADERS="authorization=Bearer <token>"
export OTEL_EXPORTER_OTLP_CERTIFICATE=/path/to/ca.pem

grx cluster daemon \
  --observer-otel-endpoint https://collector.observability.example.com:4317 \
  ...
```

### Service name override

`--observer-otel-service-name <name>` sets the OTel resource attribute `service.name`. Defaults to `"graphreact"`. Per OTel SDK convention, can also be set via `OTEL_SERVICE_NAME` environment variable.

### Cross-node cluster

Both daemons need `--observer-otel-endpoint` set + `--features otel` built. Each daemon advertises `observer-otel-v1` in its handshake; cross-node messages between OTel-aware peers carry `trace_context` and assemble in the collector as connected traces.

```sh
# Peer A
grx cluster daemon --node-id alice@127.0.0.1:5050 --listen 127.0.0.1:5050 \
  --observer-otel-endpoint http://localhost:4317 \
  --observer-otel-service-name grx-alice ...

# Peer B
grx cluster daemon --node-id bob@127.0.0.1:5051 --listen 127.0.0.1:5051 \
  --observer-otel-endpoint http://localhost:4317 \
  --observer-otel-service-name grx-bob \
  --peers alice@127.0.0.1:5050 ...
```

In Tempo/Jaeger, pick a multi-hop trace; `message.deliver` spans on alice will have `Link` attributes pointing at bob's `scheduler.tick` spans (and vice versa). A 3-hop message (alice → bob → carol) produces a 3-span chain with 2 `Link` attributes connecting the spans across services.

## Implementation reference

| Layer | File | Surface |
|---|---|---|
| Trait + event extension | [`crates/graphreact-core/src/telemetry.rs`](../../crates/graphreact-core/src/telemetry.rs) | `MessageDelivered.trace_context: Option<[u8; 24]>` (`#[serde(skip_serializing_if = "Option::is_none")]`). |
| Local message extension | [`crates/graphreact-core/src/runtime/types.rs`](../../crates/graphreact-core/src/runtime/types.rs) | `Message.trace_context: Option<[u8; 24]>`. Threaded by scheduler `deliver` into `MessageDelivered` event. |
| Wire format | [`crates/graphreact-dist/src/wire/packet.rs`](../../crates/graphreact-dist/src/wire/packet.rs) | `Message.trace_context: Option<[u8; 24]>`; `WIRE_VERSION = 3`. |
| Capability + hook trait | [`crates/graphreact-dist/src/observability.rs`](../../crates/graphreact-dist/src/observability.rs) | `OBSERVER_OTEL_V1_CAPABILITY = "observer-otel-v1"`; `WireTracingHook` trait + `WireTracingHookSlot` synchronization slot. |
| Outbound integration | [`crates/graphreact-dist/src/cluster.rs`](../../crates/graphreact-dist/src/cluster.rs) | `Cluster::install_wire_tracing_hook` slot; `route_outbound` queries the slot before serializing wire `Message`. |
| Inbound integration | [`crates/graphreact-dist/src/cluster.rs`](../../crates/graphreact-dist/src/cluster.rs) | `wire_to_local` propagates `wire.trace_context` into `local::Message.trace_context`. |
| OtelSink + helpers | [`crates/graphreact-observer/src/otel.rs`](../../crates/graphreact-observer/src/otel.rs) | `OtelSink: TelemetrySink + WireTracingHook`; `build_otel_sink(endpoint, service_name)`; `encode_wire_context`/`decode_wire_context` for the 24-byte format; TLS span-guard storage via `Context::current_with_span().attach()`. |
| CLI flags | [`crates/graphreact-cli/src/cli.rs`](../../crates/graphreact-cli/src/cli.rs) | `--observer-otel-endpoint <url>` + `--observer-otel-service-name <name>` on `cluster daemon`. |
| start_daemon wiring | [`crates/graphreact-cli/src/commands.rs`](../../crates/graphreact-cli/src/commands.rs) | Constructs `OtelSink` + installs as MultiplexSink consumer + `WireTracingHook` + parks `SdkTracerProvider` on `DaemonHandle`. Conditionally advertises `OBSERVER_OTEL_V1_CAPABILITY`. |
| Shutdown flush | [`crates/graphreact-cli/src/commands.rs`](../../crates/graphreact-cli/src/commands.rs) | `DaemonHandle::shutdown` calls `provider.shutdown()` via `spawn_blocking` BEFORE the cluster teardown so buffered spans hit the collector. |

## Test gate

| Layer | Test | Pin |
|---|---|---|
| Wire round-trip | `graphreact-dist::wire::frame::tests::roundtrip_message_with_trace_context` | postcard encode/decode preserves `trace_context: Some([...; 24])`. |
| Wire round-trip | `graphreact-dist::wire::frame::tests::min_supported_version_matches_wire_version` | `MIN_SUPPORTED_VERSION == WIRE_VERSION == 3` invariant. |
| Capability slot | `graphreact-dist::observability::tests::*` | `OBSERVER_OTEL_V1_CAPABILITY` string + `WireTracingHookSlot` install/extract round-trip + reinstall replaces + None propagates. |
| OtelSink span shape (root) | `graphreact-observer::otel_sink::scheduler_tick_emits_root_span_with_attributes` | `scheduler.tick` span emitted with `iterations` + `terminated_by_depth_cap` attributes. |
| OtelSink span status | `graphreact-observer::otel_sink::scheduler_tick_with_depth_cap_sets_error_status` | Error status when `terminated_by_depth_cap=true`. |
| OtelSink parent-child | `graphreact-observer::otel_sink::node_evaluate_creates_child_span_under_tick` | `node.evaluate` parents to `scheduler.tick` via `parent_span_id`. |
| OtelSink message span | `graphreact-observer::otel_sink::message_delivered_emits_one_shot_span_with_attributes` | `message.deliver` span carries from/to/type/ttl/direct/accepted attributes. |
| **Cross-node link (acceptance)** | `graphreact-observer::otel_sink::cross_node_message_delivered_produces_link_attribute` | `MessageDelivered.trace_context: Some(_)` produces a `Link` on the local span pointing at the remote (TraceID, SpanID). **The canonical-plan "3-hop messages produce 3-link traces" gate.** |
| OtelSink instant events | `graphreact-observer::otel_sink::instant_events_fire_on_root_tick_span` | All 7 non-span events (mount/unmount/topology/supervisor/remote/hot_reload/queue.depths) fire as instant events on the root tick span. |
| OtelSink wire context | `graphreact-observer::otel_sink::current_wire_context_returns_active_span_id` | `OtelSink::current_wire_context()` returns Some during an active tick, None otherwise. |
| Wire codec round-trip | `graphreact-observer::otel_sink::wire_context_decode_round_trips_through_link` | encode → decode → encode produces identical bytes; the wire format is stable. |
| Wire codec edge | `graphreact-observer::otel::tests::wire_context_round_trips` + `wire_context_rejects_zero_ids` | Zero-ID SpanContexts (W3C invalid) are rejected by `encode_wire_context`. |

**Manual gate:** stock OTel collector (Tempo/Jaeger/Honeycomb) ingests `scheduler.tick` traces with full child-span DAG; cross-host cluster traces show `Link` attributes connecting peers' message-flow spans. Documented above with Tempo fixture.

## Hot-path discipline

OTel SDK's `BatchSpanProcessor` buffers spans on a background tokio task and exports asynchronously. Per-event cost on the scheduler hot path is one allocation (the span data) + a queue push. The bench gate (`scheduler_overhead`) measured P10 at `case_b/case_a +1.14%` (within macOS dev-machine noise band, consistent with P5-P9's ±2% trend). **P10 added zero hot-path code on the no-OTel path** — the new `trace_context: Option<[u8; 24]>` field on `Message` adds 1 byte (Option discriminant) when `None`, and the wire-format change adds 1 byte to outbound packets when `trace_context: None`. The `WireTracingHookSlot.extract_outbound()` is a single atomic load when no hook is installed.

When OTel IS configured, expect:
- Per `scheduler.tick`: 1 OTel span allocation + 1 ContextGuard.
- Per `node.evaluate`: 1 OTel span allocation + 1 ContextGuard.
- Per `message.deliver`: 1 one-shot OTel span allocation (no guard).
- Per cross-node message outbound: 1 atomic load (slot acquire) + 1 `OtelSink::current_wire_context()` call (which does `Context::current().span().span_context().clone()` — cheap copy of 24 bytes).
- Per cross-node message inbound: forwards 24 bytes through `wire_to_local` → `Message.trace_context` → `MessageDelivered.trace_context` → `OtelSink::on_message_delivered` → `Link::with_context(decode_wire_context(bytes))`.

## Footguns

- **`WIRE_VERSION 2→3 bump invalidates v2 peers.** Every existing M14/M15 cluster deployment on `WIRE_VERSION = 2` cannot interop with v3 peers. Rolling-version upgrades require either coordinated full-cluster restart OR the wire-version negotiation work in M-Prod. Document operationally for every M16 upgrade.
- **OTel dep tree is heavy** (~30 transitive crates including tonic, prost, h2, rand 0.9 et al). Default `cargo build` doesn't pull them — the `otel` feature is opt-in. Production builds with `--features otel` add ~30s of cold compile time.
- **`SdkTracerProvider::shutdown()` is sync + blocking.** `DaemonHandle::shutdown` wraps it in `tokio::task::spawn_blocking` to keep the async runtime cooperative. If the OTel collector is unreachable, the shutdown can block for up to `DEFAULT_EXPORT_TIMEOUT` (10s) waiting for the export retry to give up. M-Prod could add a configurable shutdown deadline.
- **Cross-node spans require BOTH peers configured with `--observer-otel-endpoint`.** A unilaterally-configured peer's outbound messages carry `trace_context` but the receiver doesn't have an OtelSink to read it; the link information is silently lost. Documented.
- **No payload-aware span filtering.** Every message produces a `message.deliver` span, even messages with sensitive payloads. Per the existing P5 PII-safe-by-default design (`MessageSnapshot.payload` is omitted from the ring buffer), spans NEVER include `payload` either. For payload visibility, the operator has the P3 JSON-log stream which IS payload-aware (and PII-aware); spans stay safe by default.
- **OTel `Context::current()` is thread-local.** OtelSink relies on the scheduler being single-threaded (which it is — `Scheduler` is `!Send`). If a future scheduler design fans body evaluation across threads, the TLS-based context propagation needs revisiting.
- **`OBSERVER_OTEL_V1` constant duplicated in observer + dist.** `graphreact-observer::OBSERVER_OTEL_V1 = "observer-otel-v1"` and `graphreact-dist::OBSERVER_OTEL_V1_CAPABILITY = "observer-otel-v1"` are two separate definitions with the same string value (dist cannot depend on observer due to layering). Keep them in sync; the test gate `observer_otel_v1_capability_string_is_observer_otel_v1` in dist's observability module pins the dist-side value.
- **Bench fixture doesn't install OtelSink.** `case_g_otel_sink` is P11 closeout work — measure scheduler tick time when OtelSink is configured + the BatchSpanProcessor is buffering. Expected: small per-event allocation overhead (<5%), much like P2's PrometheusSink. Lower priority than the existing `case_b/case_a` baseline gate.

## Cross-references

- [`m16-backend-observer.md`](m16-backend-observer.md) — full M16-Backend narrative + architecture (P11 closeout).
- [`m16-debug-cpython.md`](m16-debug-cpython.md) — P8's CPython debugpy story; symmetric Axis B framing.
- [`m16-debug-jsc.md`](m16-debug-jsc.md) — P9's JSC RemoteInspector story; symmetric Axis B framing.
- [decision 0002](../decisions/0002-graphreact-is-backend-only.md) — backend-only architectural commitment; OTel via OTLP is a backend protocol consumed by stock observability tooling (Tempo/Jaeger/Honeycomb), not a frontend.
- `~/.claude/projects/-Users-damioludiran-jays-workspace/memory/feedback_complete_substrate_not_mvp.md` — substrate-not-MVP rule; P10 ships the complete primitive set including cross-node link propagation, not a local-only MVP.
- OTel SDK reference: <https://opentelemetry.io/docs/languages/rust/> + <https://docs.rs/opentelemetry_sdk/0.31/opentelemetry_sdk/trace/>.
