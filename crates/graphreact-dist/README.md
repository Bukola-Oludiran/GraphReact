# graphreact-dist

_Distribution layer — Erlang-cluster-equivalent multi-host clustering: location-transparent addressing, distributed monitors/links/pg groups, net-tick partition detection over length-prefixed postcard on TLS._

## Milestone

Ships as part of [M14 — Distribution layer (first BEAM-class property) — SHIPPED 2026-05-05](../../docs/backend/m14-distribution.md). M14.1–M14.9 closed in that pass; the M14.8.x-followon and M14.10 slices closed the engine-hosted daemon, 1ms-RTT bench, partition-aware supervisor decisions, and the `PauseOnSplit` grace window. The byte-layout reference lives at [`docs/wire-protocol.md`](docs/wire-protocol.md).

## Role in the runtime

`graphreact-dist` is a peer to the engine layer in the dependency graph — it depends on `graphreact-core` for `Scheduler`, `Message`, `MonitorRef`, and `GroupRegistry`, but on no specific engine. The same crate routes JSC and CPython component bodies because the cross-node boundary lives at the scheduler layer, not at the engine layer. It sits between the CLI (`grx cluster daemon` / `grx cluster connect`) and core, installing remote sinks (`install_remote_sink`, `install_remote_monitor_sink`, `install_remote_link_sink`, `install_remote_group_sink`, `install_remote_broadcast_sink`, `install_unmount_hook`) so location-transparent semantics fall out of the existing reactive runtime instead of bolting on a parallel addressing scheme. Distinct from M20's graph-shaped sharding for GraphReactDB — both can coexist per [decision 0003](../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md).

## Key types

- [`NodeId` / `Creation` / `RemoteNodeRef`](src/node_id.rs) — `name@host` qualified identifiers, OTP-style `Creation` epoch counter, and the typed remote-target form used by the wire codec.
- [`Cluster`](src/cluster.rs) — top-level orchestrator: owns the listener, registry, supervision, dist-groups, tick loop, and the M14.4 `DistRouter` consultant installed on the scheduler.
- [`Discovery` / `StaticDiscovery` / `DnsDiscovery`](src/discovery.rs) — peer-list source; static for tests + k8s ConfigMaps, DNS-SRV via `tokio::net::lookup_host` for headless services.
- [`NetSplitPolicy` / `NetTickConfig`](src/net_tick.rs) — partition detector knobs: 5s tick / 3-miss default; `PauseOnSplit` (default, deferred restart decisions) vs `ContinueWithStale` (eager synthesize-disconnect).
- [`RemoteSupervision`](src/supervision.rs) — distributed monitor/link tables with shared `__remote_sentinel__` `NodeKey` and per-observer link counter; idempotent disconnect fan-out via `synthesized: AtomicBool`.

## Composes with

| Crate | Direction | Role |
|---|---|---|
| [`graphreact-core`](../graphreact-core/) | depends on | Scheduler hooks, `Message` envelope, `MonitorRegistry`, `GroupRegistry`, `pending_remote_*` cleanup queues. |
| [`graphreact-cli`](../graphreact-cli/) | depended on by | Exposes the runtime via `grx cluster daemon` / `grx cluster connect`; calls `start_daemon` / `run_connect_blocking`. |

## Running the tests

```sh
cargo test -p graphreact-dist --release
```

Roughly 50+ tests in this crate: codec / handshake / transport unit tests, plus integration tests covering the two-node ping (M14.4), three-node ring kill (M14.5), three-node pg broadcast (M14.6), idle-tick liveness (M14.7), and the M14.9 closeout soak. Workspace-wide: 699 release tests pass, 0 fail, 2 ignored.

## See also

- [M14 milestone narrative](../../docs/backend/m14-distribution.md) — sub-phase log, acceptance numbers, deferrals.
- [Wire protocol byte layout](docs/wire-protocol.md) — framing, packet types, handshake state machine.
- [Decision 0003 — backend/db bifurcation](../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md) — why actor-shaped clustering and graph-shaped sharding are separate products.
- [Workspace onboarding](../../docs/onboarding.md)
- [Architecture overview](../../docs/architecture.md)
