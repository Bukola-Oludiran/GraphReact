# M14 — Distribution layer

**Status:** shipped 2026-05-05 (M14.1–M14.9 closed). First BEAM-class property.

[`crates/graphreact-dist/`](../../crates/graphreact-dist/) is the Erlang-cluster-equivalent
runtime layer for the Backend product: location-transparent message addressing across hosts,
distributed monitors / links, distributed `pg` group registry with gossip protocol, net-tick
partition detection, and a CLI surface (`grx cluster daemon` / `grx cluster connect`).

This document is the milestone narrative: what M14 ships, what each sub-phase added, the
acceptance numbers from the M14.9 closeout, and the known deferrals. For the wire format
itself see [`crates/graphreact-dist/docs/wire-protocol.md`](../../crates/graphreact-dist/docs/wire-protocol.md);
for the splendid-waffle sub-staging plan see [`~/.claude/plans/plan-the-implementation-for-splendid-waffle.md`](../../../../.claude/plans/plan-the-implementation-for-splendid-waffle.md).

## Why M14 lights up the application space

Every application class beyond the single-instance prototype shape needs distribution:

- **Thymus** self-healing across cluster nodes — supervisor decisions consume cross-node `__DOWN__` signals.
- **ComplyFlow** multi-region compliance — the policy engine spans hosts; ops broadcast policy updates.
- **NewsCaster** divine-council MMO — players + agents scatter across game-server hosts.
- **Generative Worlds**, **Adaptive Education at scale**, **Reputation networks**, **Knowledge graphs that reason** — all need actor-shaped multi-host clustering.

M14 is the first BEAM-class property that makes GraphReact a real distributed runtime
rather than a single-process reactive system. Distinct from M20 (DB-product distributed
graph), which is graph-shaped sharding with a client-visibility consistency model — both
can coexist in the same deployed system per
[decision 0003](../decisions/0003-graphreact-bifurcates-into-backend-and-db.md).

## Architectural shape

```
┌─────────────────────────────┐         ┌─────────────────────────────┐
│  graphreact-cli             │         │  graphreact-cli             │
│   `grx cluster daemon ...`  │         │   `grx cluster connect ...` │
└─────────────────────────────┘         └─────────────────────────────┘
              │                                       │
              ▼                                       ▼
     ┌────────────────────────────────────────────────────┐
     │  graphreact-dist                                   │
     │   Cluster (start/connect/shutdown)                 │
     │   ├─ NodeRegistry (peer table, last_tick AtomicU64)│
     │   ├─ RemoteSupervision (monitor + link tables)     │
     │   ├─ DistGroups (Lamport-versioned gossip)         │
     │   ├─ PartitionDetector (tick + miss-threshold)     │
     │   ├─ Wire codec (postcard + length-prefix frame)   │
     │   └─ TLS transport (rustls + tokio-rustls + mTLS)  │
     └────────────────────────────────────────────────────┘
              │                                       │
              ▼                                       ▼
     ┌────────────────────────────────────────────────────┐
     │  graphreact-core::Scheduler                        │
     │   M14 hooks: install_remote_sink, set_node_name,   │
     │   set_remote_sentinel, install_remote_monitor_sink,│
     │   install_remote_link_sink, install_remote_unlink_ │
     │   sink, install_unmount_hook, install_remote_group │
     │   _sink, install_remote_broadcast_sink             │
     │                                                    │
     │   M14.5 cleanup queues drained at process_batch:   │
     │   pending_remote_demonitors, pending_remote_unlinks│
     └────────────────────────────────────────────────────┘
```

`graphreact-dist` is a peer to the engine layer — it depends on `graphreact-core` for
`Scheduler` / `Message` / `MonitorRef` / `GroupRegistry` but NOT on any specific engine.
The same dist crate routes JSC and CPython component bodies because the cross-node
boundary lives at the scheduler layer, not at the engine layer.

## Foundational decisions (locked in)

| Decision | Choice | Rationale |
|---|---|---|
| **Codec** | `postcard 1.0` | `no_std`-friendly, varint-compact (~30% smaller than bincode for our string-heavy packets), audited for safety-critical Rust. |
| **TLS** | `rustls 0.23` + `tokio-rustls 0.26` (ring backend) | Pure-Rust, audited (NCC Group 2020 + Trail of Bits 2023), no GPL/OpenSSL build complications, integrates with `tokio_util::codec::Framed`. mTLS by default. |
| **Auth** | TLS-only (no app-layer per-message HMAC) | mTLS handles confidentiality + authentication; cookie verification is once during the OTP-style handshake; subsequent packets rely on TLS connection identity. |
| **Handshake** | OTP-style 4-packet (Hello → Challenge → ChallengeReply → ChallengeAck), HMAC-SHA256(cookie) digest with `subtle::ConstantTimeEq` | OTP's challenge-response is well-tested; HMAC-SHA256 is the modern primitive (NIST SP 800-107). |
| **Framing** | `[Type:u8] [Len:u32 BE] [Payload]`, 16 MiB inbound cap | Prevents OOM amplification; BE matches OTP + general network convention. |
| **Discovery** | `Discovery` trait with `StaticDiscovery` + `DnsDiscovery` | DNS-SRV gets free TTL caching; integrates with k8s headless services without extra infra. EPMD-equivalent intentionally skipped (fixed-port scanning target + SPOF). |
| **Cookie storage** | `GRX_COOKIE` env var primary; `--cookie-file` fallback with mode-0600 enforcement | Mirrors OTP's `~/.erlang.cookie`. Refuses to start on world-/group-readable files. |
| **Sentinel design (M14.5)** | Single shared `__remote_sentinel__` `NodeKey` | Per-peer fan-out lives in `RemoteSupervision`'s `by_node` index; core's `MonitorRegistry` doesn't need per-peer disambiguation. Avoids polluting the slotmap with synthetic nodes per peer. |
| **Net-split policy default** | `PauseOnSplit` | Reactive semantics make stale-monitor-state user-visible; double-firing across heal corrupts user state. `ContinueWithStale` is opt-in. |

## Sub-phase log

### M14.1 — Crate skeleton + wire envelope + framing codec

**Shipped:** crate at `crates/graphreact-dist/`, workspace member,
`NodeId` / `Creation` / `RemoteNodeRef`, every packet struct in `wire/packet.rs`
(`Hello`, `Challenge`, `ChallengeReply`, `ChallengeAck`, `Message`, `MonitorRequest`,
`MonitorAck`, `Demonitor`, `Down`, `LinkRequest`, `Unlink`, `Exit`, `PgJoin`,
`PgLeave`, `PgSync`, `PgDiscover`, `Tick`, `TickAck`, `Disconnect`), postcard
`encode`/`decode` in `wire/codec.rs`, length-prefix codec in `wire/frame.rs`,
`DistError` taxonomy. Round-trip tests for every packet variant + framed
codec partial-read tests. `MAX_FRAME_BYTES = 16 MiB` enforced.

### M14.2 — TLS transport

`Listener::bind`, `Listener::accept_loop`, `Connection::dial`,
`Connection::send_packet` / `recv_packet`. `TlsConfig::from_pem_bytes` with
mTLS-default. `dev_tls` test helper (`#[cfg(test)] pub(crate)`) generates self-signed
certs via `rcgen` for two- and three-node fixtures. Tests cover:
TLS handshake completion over `tokio::io::duplex`, cert mismatch rejection,
unexpected-peer-name rejection, server `recv` termination on client drop.

### M14.3 — OTP-style handshake + node registry + cookie loading + discovery

`server_handshake` / `client_handshake` state machines. HMAC-SHA256(cookie, challenge)
digest via `subtle::ConstantTimeEq`. `NodeRegistry` (DashMap-backed peer table keyed
by node *name*; cluster names are unique by convention). `SecretCookie` (zeroize-on-drop)
with `from_env(GRX_COOKIE)` + `from_file(path)` + Unix mode-0600 enforcement.
`Discovery` trait + `StaticDiscovery` + `DnsDiscovery` (`tokio::net::lookup_host`).
Lex-tiebreak for simultaneous reciprocal connects (`should_keep_outbound`).

### M14.4 — Cross-node delivery + TTL inheritance

`Cluster` orchestrator. `route_outbound` installed as
`graphreact_core::Scheduler::install_remote_sink` consultant — `make_emit` consults
it whenever a target's `@<other>` suffix doesn't match `our_node_name`. Inbound
`run_connection` task converts wire `Message` → local `Message` (with authoritative
`from`-rewriting from the authenticated peer NodeId) and pushes via
`SchedulerHandle::send_raw`. **TTL invariant**: one scheduler-deliver hop = one
decrement, regardless of how many physical-network hops the message traversed.
Acceptance: two-node ping (100 messages, order preserved, TTL decrement = 1 across
the wire crossing).

### M14.5 — Distributed monitors + links + disconnect synthesis

`RemoteSupervision` orchestrator (cheap-clone `Arc<Inner>`). Outbound translation
tables (`outbound_monitors_by_node` for per-peer fan-out, `outbound_monitors_by_local_ref`
for inbound `Down` lookup). Per-monitor `synthesized: AtomicBool` for idempotent
disconnect fire. `LinkSyncer` mirrors local link/unlink to peers via wire packets;
per-observer link counter ensures the shared sentinel edge in `LinkRegistry` is
torn down only when ALL of an observer's remote links have unwound.

graphreact-core additions: `RemoteMonitorSink` / `RemoteLinkSink` / `RemoteUnlinkSink`
type aliases. `Scheduler::set_remote_sentinel` allocates the single shared
`__remote_sentinel__` `NodeKey`. `make_register_monitor` / `make_register_link` /
`make_unregister_link` refactored to string-taking closures that classify
local-vs-remote and route accordingly. `pending_remote_demonitors` /
`pending_remote_unlinks` queues drained at the head of every `process_batch` so
tokio worker threads can request scheduler-thread cleanup safely.
`Scheduler::install_unmount_hook` fires after the local DOWN/EXIT fan-out so the
dist crate ships wire `Down` / `Exit` for any inbound monitors / links subscribing
to the unmounting node id.

Acceptance: 3-node ring kill — alice and carol monitor `target@bob`;
`bob_cluster.shutdown()` triggers Eof on alice + carol's connection tasks;
`synthesize_disconnect` fires `__DOWN__{ref, target:"target@bob", reason:"noconnection"}`
on each observer; translation tables empty after fan-out.

### M14.6 — Distributed `pg` group registry

`DistGroups` orchestrator. Per-`(group, qualified_member)` Lamport-versioned
`MemberState` (present + lamport + origin). Local lamport monotonic counter.
Outbound: `record_local_group_op` ships `PgJoin` / `PgLeave` to every connected
peer when local membership transitions; `record_local_broadcast` deduplicates peers
by `(group, present, origin != us)` and ships one `PgBroadcast` per peer hub-and-spoke.
Inbound: `handle_pg_join` / `handle_pg_leave` Lamport-newest-wins;
`handle_pg_sync` correctly attributes each entry to the origin parsed from the
qualified member id (NOT the relay sender) and runs an implicit-leave sweep
scoped to entries `peer_name` actually authored; `handle_pg_discover` replies
with `PgSync`; `handle_pg_broadcast` fans to bare-local-id members via
`SchedulerHandle::send_raw`.

graphreact-core additions: `RemoteGroupSink` / `RemoteBroadcastSink` type aliases,
`GroupOp` enum, install methods. `make_group_join` / `make_group_leave` fire the
sink only on true membership transitions (not every render). `unmount_with_reason`
snapshots `groups_for_node(key)` BEFORE `leave_all` so the sink can fan a `Leave`
per group. `group_broadcast` qualifies `from` and fires the broadcast sink AFTER
local fan-out.

Acceptance: 3-node group broadcast — fully connected ring; all three views of
`workers` converge to 3 members within polling window; alice's
`Scheduler::group_broadcast` fans to bob and carol via `PgBroadcast`; all three
workers receive the message; bob's shutdown drops his member from alice + carol's
views.

### M14.7 — Net-tick + partition detection

`PeerHandle.last_tick: Arc<AtomicU64>` (Arc-wrapped so `run_connection` can hold
a clone WITHOUT keeping `send_tx` alive — load-bearing for clean shutdown).
`NetSplitPolicy` (`PauseOnSplit` default, `ContinueWithStale`); `NetTickConfig`
(5s tick / 3-miss default → 15s detection floor). `spawn_tick_loop` returns a
`JoinHandle<()>` driving a single cluster-wide tokio task: every `tick_interval`
it iterates the registry, ships one `Packet::Tick { seq, sent_at_ms }` to each
peer, and reads `last_tick_ms()`. Stale peers are removed under `ContinueWithStale`
(triggers connection-task exit + synthesize_disconnect for monitor / link / group
state) and held under `PauseOnSplit` (warn log, no action). `tick_pass` is `pub`
so unit tests can drive a single iteration without waiting on `tokio::time::interval`.

`run_connection` bumps `last_tick.store(now_ms, Relaxed)` at the top of every
inbound match arm — every packet is a liveness signal. `dispatch_inbound` no
longer needs the peer handle; Tick / TickAck arms are no-ops (the bump already
happened upstream). TickAck is intentionally not echoed — bidirectional liveness
is achieved by both sides running their own tick loop.

Acceptance: tick loop keeps idle connection alive — 100ms tick / 3-miss threshold
(300ms detection floor); two clusters connected, idle for 700ms; both sides remain
registered; both sides' `last_tick_ms()` views are within 300ms of `now`.

### M14.8 — CLI integration

`grx cluster daemon` and `grx cluster connect` subcommands under a namespaced
`Cluster { op: Box<ClusterOp> }` variant on `Command`. `DaemonOptions` /
`ConnectOptions` structured forms. `start_daemon(opts) -> CommandResult<DaemonHandle>`
is the test-friendly entry point (returns the live `Cluster` + `Box<Scheduler>`
so tests can introspect or drive batches). `run_daemon_blocking` /
`run_connect_blocking` are the binary-driven entry points. `drive_until` runs
`process_batch` on a 50ms cadence inside a `tokio::select!` racing the optional
deadline against `tokio::signal::ctrl_c()`. Pub helpers: `parse_peer_entry`,
`resolve_cookie`, `load_tls_from_paths`.

Engine-hosted `.grx` mounts under `grx cluster daemon` are explicitly deferred —
hooking JSC / CPython runtimes into `Cluster::start`'s `&mut Scheduler` requires
reordering `mount_with_scheduler`. `start_daemon` returns `CommandError::ClusterArg`
if `path: Some(_)` or `runtime != Null`. M14.8 ships the engine-free shape; engine
hosting lands in a follow-on slice.

Acceptance: 4 CLI integration tests — daemon accepts peer with matching cookie,
daemon rejects peer with cookie mismatch (printable error), world-readable cookie
file rejected with `mode`-bearing error, peer entry round-trip parsing.

### M14.9 — Closeout

Criterion bench at [`crates/graphreact-dist/benches/cross_node_throughput.rs`](../../crates/graphreact-dist/benches/cross_node_throughput.rs)
validating cross-node throughput on TLS loopback. Three-node ring kill +
continued-broadcast soak test in `cluster::tests`. Wire-protocol byte-layout
documentation at [`crates/graphreact-dist/docs/wire-protocol.md`](../../crates/graphreact-dist/docs/wire-protocol.md).
This narrative. M14 plan section updated to "shipped"; `project_graphreact_m14.md`
memory written.

## Acceptance numbers (M14.9 closeout)

### Throughput bench

`cargo bench -p graphreact-dist --bench cross_node_throughput` — 1KB JSON payload
over TLS loopback, M14.4 routing path:

| Batch size | Throughput (per criterion median) | Plan threshold |
|---|---|---|
| 1,000 | ~293k msg/sec | ≥50k msg/sec |
| 10,000 | ~325k msg/sec | ≥50k msg/sec |

5-6× over the M14 plan's `≥50k loopback` threshold. Bench machine: Apple M-series
loopback. The 1ms-RTT bench (the second M14 plan threshold of `≥10k @ 1ms RTT`)
is left as a follow-on — it needs an `AsyncRead`/`AsyncWrite` shim that injects
per-frame delay.

### Workspace

699 release tests pass, 0 fail, 2 ignored. `cargo clippy --workspace --all-targets
--release -- -D warnings` clean; `cargo clippy --workspace --all-targets -- -D warnings`
(dev profile) clean.

Test counts by sub-phase:

- M14.1–M14.3: codec + handshake + transport unit tests (already in repo before M14.1 closeout).
- M14.4: `two_node_ping_100_messages_cross_wire`.
- M14.5: `three_node_ring_kill_fires_down_on_disconnect` + 4 `RemoteSupervision` unit tests.
- M14.6: `three_node_pg_workers_broadcast_and_disconnect` + 5 `DistGroups` unit tests.
- M14.7: `tick_loop_keeps_idle_connection_alive` + 5 `net_tick` unit tests.
- M14.8: 4 CLI integration tests in `crates/graphreact-cli/tests/cluster_cli.rs`.
- M14.9: 1 closeout soak test (`three_node_ring_soak_kill_and_continued_broadcast`).

## What's deferred

The M14.8.x-followon slice (2026-05-05) closed every M14.x deferral that
didn't depend on substrate that doesn't exist yet. The M14.10 slice
(2026-05-06) closed the supervisor-decision integration. Items remaining:

- **Cross-node remote restart.** "Supervisor on alice restarts a child
  on bob" requires a delegated-restart wire protocol that does not
  exist today. M14.10 adds local **deferral** of supervisor decisions
  whose dependencies span partitioned peers, but the actual restart
  still happens locally. Tracked as M14.11 follow-on.

### Closed by M14.10 (2026-05-06)

- ✅ **Partition-aware supervisor decisions.** New
  `Scheduler::install_supervise_decision_hook` consulted before
  `execute_supervisor_plan`. Hook returns `SuperviseDecision::Proceed`
  (run plan as today) or `SuperviseDecision::Defer { reason,
  retry_after }` (park decision in
  `Scheduler::pending_supervisor_decisions`; re-evaluate on every
  `process_batch`; escalate past `retry_after` deadline). The defer
  queue is drained by `Scheduler::drain_pending_supervisor_decisions`
  at the head of every `process_batch` round; defensive guard drops
  decisions whose supervisor was unmounted in the meantime. Tests
  in [crates/graphreact-core/tests/runtime_supervisor_decision.rs](../../crates/graphreact-core/tests/runtime_supervisor_decision.rs).
- ✅ **`NetSplitPolicy::PauseOnSplit` no longer warn-only forever.**
  New `NetTickConfig::partition_grace` (default 30s). `tick_pass`
  now tracks per-peer `PeerStatus` (`Healthy`, `Suspect{since}`,
  `Partitioned{since}`) in a process-shared
  `Arc<DashMap<String, PeerStatus>>` populated every tick interval.
  Under PauseOnSplit, the first threshold miss flips the peer to
  `Suspect`; once `partition_grace` elapses the peer escalates to
  `Partitioned` (drop registry + synthesize_disconnect) — the same
  effect as ContinueWithStale, just gated by a grace window. A peer
  that resumes traffic before grace expires reverts to `Healthy`
  cleanly.
- ✅ **Cluster wiring.** `Cluster::start` builds the per-peer state
  map, hands it to `spawn_tick_loop`, and installs a default
  decision hook: under PauseOnSplit, defer restart decisions while
  ANY peer is non-Healthy; under ContinueWithStale, always proceed.
  Per-child remote-dependency tracking (defer only when this child
  actually depends on the partitioned peer) is a refinement —
  today's conservative default parks all restart decisions until
  the cluster is healthy, which is correct but stops more work
  than needed during partitions.

### Closed by M14.8.x-followon (2026-05-05)

- ✅ **Engine-hosted `grx cluster daemon`** — `start_daemon` now mounts JSC
  (`SchedulerHost::Jsc(JsRuntime)`) and CPython (`SchedulerHost::Cpython(PyRuntime)`)
  via the new `build_scheduler_host` dispatcher. The mount-then-cluster ordering
  works because [`Scheduler::install_remote_group_sink`](../../crates/graphreact-core/src/runtime/scheduler.rs)
  retrofits `Join` events for pre-mounted memberships, and
  [`start_daemon`](../../crates/graphreact-cli/src/commands.rs) runs an initial
  `process_batch` after `Cluster::start` (so bodies fire with sinks installed)
  before dialing peers (so `PgSync` replies include local memberships).
- ✅ **Subprocess-driven CLI test** — [`tests/cluster_cli_subprocess.rs`](../../crates/graphreact-cli/tests/cluster_cli_subprocess.rs)
  spawns the real `grx` binary via `assert_cmd` and exercises clap parsing +
  the daemon's `grx-daemon-ready <addr>` readiness signal end-to-end.
- ✅ **1ms-RTT throughput bench** — [`benches/cross_node_throughput_rtt.rs`](../../crates/graphreact-dist/benches/cross_node_throughput_rtt.rs)
  uses a `DelayedDuplex` shim over `Cluster::accept_stream` /
  `Cluster::connect_with_stream` (Phase A transport injection). Reports
  ~573 msg/sec — well below the 10k target, **architecture-imposed ceiling**
  (single-writer-per-peer at the framed-codec layer; pipelining the writer is
  a real perf-optimization slice, not a substrate gap).
- ✅ **Net-split simulation via duplex packet-drop** —
  [`net_split_one_direction_drop_triggers_partition_detection`](../../crates/graphreact-dist/src/cluster.rs)
  wraps `tokio::io::duplex` in a `LossyDuplex` shim that consumes-and-discards
  one direction's bytes; the partition detector fires within ~300ms under
  `ContinueWithStale` policy.
- ✅ **TickAck echo + per-task oneshot abort signal** — `Cluster.abort_handles`
  table; `run_connection`'s `select!` gains an abort arm; shutdown/disconnect
  signal abort BEFORE dropping registry, letting the task hold an outbound
  `Sender<Packet>` clone for TickAck echo without trapping shutdown.
- ✅ **Tree-based broadcast fan-out (decentralized)** — `PgBroadcast` adds
  `visited_peers: Vec<String>`. Originator seeds with `[our_name]`; each
  receiver fans to local members + relays to peers not in `visited_peers`.
  Receiver-side dedup via FIFO-bounded `SeenBroadcasts` cache (`MAX_SEEN_BROADCASTS = 10_000`)
  drops duplicates that arrive via different relay paths.
- ✅ **Gossip debounce** — `MIN_GOSSIP_INTERVAL = 100ms` wired in
  [`groups.rs`](../../crates/graphreact-dist/src/groups.rs).
  `record_local_group_op` buffers ops in `Inner::pending_ops`; a one-shot
  tokio task sleeps 100ms then flushes via `flush_pending_ops`. Local
  upserts are immediate; only wire-side fan-out is delayed.
  `Cluster::shutdown` calls `flush_pending_ops` synchronously to avoid losing
  in-flight gossip on teardown.

### Closed M13 deferrals (M14.8.x-followon shared work)

- ✅ **JSC + CPython async CLI activation** — `grx run --runtime jsc` /
  `--runtime cpython` now drives `process_batch_async` via a
  `current_thread`+`LocalSet` hosted on a freshly-spawned OS thread (the
  binary's outer multi-thread runtime forbids nested `block_on`; the OS
  thread escapes that context). `useEffectAsync` + Python asyncio coroutines
  fire under both runtimes.
- ✅ **hyperfine benchmark** — `cargo run -p xtask -- hyperfine` shells
  hyperfine against the release `grx` binary running both engines. Graceful
  degrade if hyperfine isn't on PATH.
- ✅ **Per-OS × per-Python-minor CI matrix** —
  [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)'s test job is
  now a 6-cell matrix (macOS + Linux × Python 3.12 / 3.13 / 3.14). Free-threaded
  3.14t is excluded until `actions/setup-python` ships support; Windows is
  excluded pending end-to-end verification of the pkg-config probe path.

## Wire-version compatibility (Phase β safe-slice)

**M-Prod sub-track #4 Phase β SHIPPED 2026-06-18.** The wire-version
floor lags `WIRE_VERSION` by one for **one-bump rolling-upgrade
compatibility** as of Phase β — daemons one bump back interop with
the current bump, but not two bumps back, and not future versions
above the local ceiling. Both ends of that range are policed by the
server-side handshake; a peer above ceiling is rejected with
`reason="wire_version_above_ceiling"` BEFORE the floor check.

Capability negotiation rides on TWO parallel surfaces:

- **`Vec<String>` per-feature opt-in flags** — `"reload-v1"`,
  `"observer-otel-v1"`, `"ingress-call-v1"`. Open set; each
  consumer owns its flag name. Used by capability-gated dispatch
  (e.g. `Cluster::reload_module` ships only to peers advertising
  `"reload-v1"`).
- **`u64` capabilities bitmap** — typed substrate-level flag set
  governed by the dist crate: `CAP_MIXED_RUNTIME`,
  `CAP_DELEGATED_RESTART`, `CAP_AUDIT_CHAIN`. Effective set =
  `our_local_bitmap & peer_advertised_bitmap`, queryable from
  `Cluster::peer_effective_capabilities(name) -> Option<u64>`.

v(N-1) peers that ship the old Hello/ChallengeAck shape (no
trailing `capabilities_bitmap: u64`) are decoded via a dual-struct
fallback path in `crate::wire::WireCodec::decode`; the missing
field synthesises to `0`. Genuine wire corruption is NOT papered
over — only `postcard::Error::DeserializeUnexpectedEnd` triggers the
fallback; any other postcard error propagates.

Pointers:

- Constants + struct shapes:
  [`crates/graphreact-dist/src/wire/packet.rs`](../../crates/graphreact-dist/src/wire/packet.rs)
- Dual-struct fallback decode:
  [`crates/graphreact-dist/src/wire/codec.rs`](../../crates/graphreact-dist/src/wire/codec.rs)
- Floor/ceiling checks + audit events:
  [`crates/graphreact-dist/src/handshake.rs`](../../crates/graphreact-dist/src/handshake.rs)
- Full Phase β scope + audit-row contract:
  [`docs/backend/m-prod-lifecycle.md#shipped--phase-β-rolling-version-handshake`](m-prod-lifecycle.md#shipped--phase-β-rolling-version-handshake)

## Cross-references

- **Wire protocol byte layout**: [`crates/graphreact-dist/docs/wire-protocol.md`](../../crates/graphreact-dist/docs/wire-protocol.md)
- **Strategic plan**: [`~/.claude/plans/graphreact-rust-later-milestones.md`](../../../../.claude/plans/graphreact-rust-later-milestones.md) (replaces the lost `toasty-crafting-lantern.md` 2026-06-15; M14 is referenced as the actor-cluster substrate distinct from M20's graph-shaped sharding, and as the M-Prod #4 lifecycle dependency)
- **Sub-staging plan**: [`~/.claude/plans/plan-the-implementation-for-splendid-waffle.md`](../../../../.claude/plans/plan-the-implementation-for-splendid-waffle.md)
- **OTP references**:
  - [`otp/lib/kernel/src/net_kernel.erl`](../../../../otp/lib/kernel/src/net_kernel.erl) — node discovery + connection establishment
  - [`otp/lib/kernel/src/dist_util.erl`](../../../../otp/lib/kernel/src/dist_util.erl) — handshake + ticker
  - [`otp/lib/kernel/src/pg.erl`](../../../../otp/lib/kernel/src/pg.erl) — gossip pattern (discover/sync/update)
- **Bun reference**: [`bun/src/bun.js/ipc.zig`](../../../../bun/src/bun.js/ipc.zig) — length-prefixed framing
- **Substrate-not-MVP rule**: every primitive shipped in M14 (no phase-1 MVP / phase-2 deferred). Sub-staging was execution-order convenience.
- **Decision 0003 (product bifurcation)**: M14 is actor-shaped multi-host clustering for the Backend product; M20 is graph-shaped sharding with client-visibility consistency for GraphReactDB. Both can coexist.
