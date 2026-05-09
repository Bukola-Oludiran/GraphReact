# GraphReact distribution wire protocol — M14

**Status:** living document. Last updated 2026-05-05 (M14.9 closeout).
**Wire version:** `WIRE_VERSION = 1`. **Min supported:** `1`.

This document describes the on-the-wire shape exchanged between
two `Cluster` instances over a TLS connection. The corresponding
Rust types live in
[`crates/graphreact-dist/src/wire/packet.rs`](../src/wire/packet.rs),
the codec lives in [`crates/graphreact-dist/src/wire/codec.rs`](../src/wire/codec.rs),
and the framing layer lives in
[`crates/graphreact-dist/src/wire/frame.rs`](../src/wire/frame.rs).

## Frame layout

Every frame is:

```
┌────────┬─────────────┬───────────────────────┐
│ Type:1 │ Len:4 (BE)  │  Payload: Len bytes    │
└────────┴─────────────┴───────────────────────┘
```

- `Type` — single-byte [`PacketType`](../src/wire/packet.rs)
  discriminant. Codes are stable, never re-numbered.
- `Len` — payload byte count (NOT whole-frame length). Big-endian
  per network convention. Capped at `MAX_FRAME_BYTES = 16 MiB`
  to prevent OOM amplification on malicious / malformed peers.
- `Payload` — postcard-encoded struct, one shape per `Type`.

`postcard` is `no_std`-friendly with compact varint encoding —
~30% smaller than bincode for our string-heavy packets, and audited
for safety-critical Rust use.

## Type code table

| Code | Name | Sub-phase | Direction |
|---|---|---|---|
| `0x01` | `Hello` | M14.3 (handshake) | client → server |
| `0x02` | `Challenge` | M14.3 (handshake) | server → client |
| `0x03` | `ChallengeReply` | M14.3 (handshake) | client → server |
| `0x04` | `ChallengeAck` | M14.3 (handshake) | server → client |
| `0x10` | `Message` | M14.4 (data plane) | bidirectional |
| `0x11` | `MonitorRequest` | M14.5 (supervision) | observer → target |
| `0x12` | `MonitorAck` | M14.5 | target → observer |
| `0x13` | `Demonitor` | M14.5 | observer → target |
| `0x14` | `Down` | M14.5 | target → observer |
| `0x15` | `LinkRequest` | M14.5 | bidirectional |
| `0x16` | `Unlink` | M14.5 | bidirectional |
| `0x17` | `Exit` | M14.5 | terminator → linked-peer |
| `0x20` | `PgJoin` | M14.6 (groups) | local-change → all peers |
| `0x21` | `PgLeave` | M14.6 | local-change → all peers |
| `0x22` | `PgSync` | M14.6 | reply to `PgDiscover` |
| `0x23` | `PgDiscover` | M14.6 | post-handshake initiation |
| `0x24` | `PgBroadcast` | M14.6 | originator → peers with members |
| `0x30` | `Tick` | M14.7 (liveness) | bidirectional |
| `0x31` | `TickAck` | M14.7 | bidirectional (today: not echoed; reserved) |
| `0xFF` | `Disconnect` | lifecycle | bidirectional |

Codes `0x05–0x0F`, `0x18–0x1F`, `0x25–0x2F`, `0x32–0xFE` are
reserved for future use.

## Handshake (M14.3)

The OTP-style four-packet handshake completes BEFORE any other
packet types are accepted. Cookie is verified via
HMAC-SHA256(cookie, challenge); the digest is compared via
`subtle::ConstantTimeEq` to defeat timing attacks.

```
┌─ client ────────────┐                  ┌─ server ────────────┐
│  Hello              │ ───────────────► │                     │
│                     │                  │  Challenge          │
│                     │ ◄─────────────── │   c_server: [u8;32] │
│  ChallengeReply     │ ───────────────► │                     │
│   c_client: [u8;32] │                  │   verify HMAC ─────►│
│   digest of c_server│                  │                     │
│                     │                  │  ChallengeAck       │
│                     │ ◄─────────────── │   digest of c_client│
│   verify HMAC       │                  │                     │
│   ─────────────────►│                  │                     │
└─────────────────────┘                  └─────────────────────┘
```

- `Hello.version` is rejected if `< MIN_SUPPORTED_VERSION`.
- `Hello.flags` carries bit-flag capabilities (bit 0:
  `tls_required`, bit 1: `pep684_aware`); reserved bits MUST be 0.
- `Hello.capabilities: Vec<String>` is the finer-grained negotiation
  surface (e.g. `["pg-v1", "monitor-v1", "ttl-inherit-v1"]`).
- 32-byte challenges are sourced from `OsRng`; never reused across
  handshakes.

### Hello

```rust
struct Hello {
    version: u16,                   // ≥ MIN_SUPPORTED_VERSION
    flags: u32,
    node_name: String,              // canonical "name@host"
    creation: u32,                  // boot epoch — increments on rejoin
    capabilities: Vec<String>,
}
```

### Challenge

```rust
struct Challenge {
    challenge: [u8; 32],            // server's nonce, HMAC'd by client
    server_name: String,
    creation: u32,
}
```

### ChallengeReply

```rust
struct ChallengeReply {
    challenge: [u8; 32],            // client's nonce
    digest: [u8; 32],               // HMAC-SHA256(cookie, server_challenge)
}
```

### ChallengeAck

```rust
struct ChallengeAck {
    digest: [u8; 32],               // HMAC-SHA256(cookie, client_challenge)
}
```

## Data plane (M14.4)

### Message

The cross-node form of `graphreact_core::Message`. The
authoritative `from`-rewriting on the receiving side prevents
forgery: the wire's `from`-field's `@<node>` suffix MUST match the
authenticated peer NodeId or the message is rejected.

```rust
struct Message {
    id: String,
    msg_type: String,
    from: String,                   // "local_id@from_node"
    to: String,                     // "local_id@to_node"
    payload: Vec<u8>,               // serde_json::to_vec(value)
    ttl: u32,                       // remaining hops; receiver decrements once
    timestamp: u64,                 // wall-clock ms (telemetry only)
    direct: bool,                   // bypasses neighbor-fan-out
    inherited_ttl: Option<u32>,     // reserved; today: routing uses `ttl` only
}
```

**TTL invariant**: one scheduler-deliver hop = one decrement.
Wire crossing does NOT decrement. A `DEFAULT_TTL = 10` message
emitted on alice and delivered on bob is observed as `ttl = 9` by
the handler (one decrement on bob's `deliver`).

## Supervision (M14.5)

### MonitorRequest / MonitorAck / Demonitor / Down

Distributed monitor protocol. The observer's stub `MonitorRef` is
allocated locally first; the wire packet exchanges the ref with
the target's peer. On disconnect (TCP close OR M14.7 partition
detection under `ContinueWithStale`), the observer's local
`__DOWN__` synthesis fires with `reason = "noconnection"`.

```rust
struct MonitorRequest {
    observer: String,               // qualified "local_id@observer_node"
    target: String,                  // qualified "local_id@target_node"
    remote_ref: u64,                // sender's local MonitorRef
    creation: u32,                  // sender's current Creation
}

struct MonitorAck {
    remote_ref: u64,
    target_alive: bool,             // optimistically true today
}

struct Demonitor { remote_ref: u64 }

struct Down {
    remote_ref: u64,
    target: String,                 // qualified
    reason: WireExitReason,
}
```

### LinkRequest / Unlink / Exit

Symmetric link protocol. `Exit` propagates abnormal terminations
to linked peers in both directions.

```rust
struct LinkRequest { a: String, b: String }   // both qualified
struct Unlink      { a: String, b: String }

struct Exit {
    from: String,                   // qualified
    reason: WireExitReason,
}
```

### WireExitReason

```rust
enum WireExitReason {
    Normal,
    Shutdown,
    Crashed { detail: String },
    NodeDown,                       // reserved for partition-shaped exits
}
```

The `__DOWN__` payload's `reason` field stringifies as:

| `WireExitReason` | Stringified |
|---|---|
| `Normal` | `"normal"` |
| `Shutdown` | `"shutdown"` |
| `Crashed { .. }` | `"crashed"` |
| `NodeDown` | `"noconnection"` |

Disconnect-synthesized `__DOWN__` (TCP close + `ContinueWithStale`
partition) uses `reason = "noconnection"` regardless of the
underlying cause. Inbound `Down` packets carry the remote target's
authoritative reason.

## Groups (M14.6)

### PgJoin / PgLeave

Delta gossip. Sent to every connected peer when a local node's
group membership transitions. The Lamport timestamp lets receivers
reconcile out-of-order delivery — older Lamports lose to newer
ones per `(group, member)`.

```rust
struct PgJoin {
    group: String,
    member: String,                 // "local_id@origin_node"
    vc: Vec<(String, u64)>,         // [(origin_node, lamport)]
}

struct PgLeave {
    group: String,
    member: String,
    vc: Vec<(String, u64)>,
}
```

### PgDiscover / PgSync

Post-handshake initiation. After handshake, the connector sends
`PgDiscover`; the receiver replies with `PgSync` carrying every
present member it knows about. Members are attributed to their
origin peer via the `@`-suffix of the qualified id, NOT the
relay sender — this lets a single `PgSync` from one peer carry
authoritative state about other peers without misattributing
membership origin.

```rust
struct PgDiscover {
    groups_known: Vec<String>,      // for diagnostics; today: informational
    vc: Vec<(String, u64)>,         // sender's known high-water per peer
}

struct PgSync {
    groups: Vec<(String, Vec<String>)>,   // (group, [qualified members])
    vc: Vec<(String, u64)>,
}
```

### PgBroadcast

Hub-and-spoke fan-out. Sent from the originating node to every
peer hosting at least one member of `group`. Receivers expand to
their local members via `SchedulerHandle::send_raw`. One wire
packet per peer regardless of member count — tree-based fan-out
is a future optimisation.

```rust
struct PgBroadcast {
    group: String,
    msg_type: String,
    payload: Vec<u8>,               // serde_json::to_vec(value)
    from_qualified: String,         // "local_id@origin_node"
    timestamp: u64,                 // wall-clock ms
}
```

## Liveness (M14.7)

### Tick / TickAck

```rust
struct Tick {
    seq: u64,
    sent_at_ms: u64,
}

struct TickAck { seq: u64 }
```

Both sides run a cluster-wide tick loop on `tick_interval`
cadence. Inbound `Tick` (or any other packet) bumps the peer's
`PeerHandle.last_tick`. The partition detector flags peers where
`now - last_tick > miss_threshold * tick_interval` and fires
disconnect synthesis under `NetSplitPolicy::ContinueWithStale` (or
warns under the default `PauseOnSplit`).

`TickAck` is reserved in the protocol but not echoed by today's
implementation — bidirectional liveness is achieved by both sides
running their own tick loop, so each side's outbound `Tick` is
the other side's inbound liveness signal. Echoing `TickAck` would
require the per-connection task to hold a `Sender` clone, which
would prevent registry-drop from closing the channel; today's
choice prioritises clean shutdown semantics over RTT measurement.

## Lifecycle

### Disconnect

```
0xFF (no payload)
```

Graceful shutdown announcement. The receiver breaks its
`select!` loop on receipt; the connection task exits and fires
the M14.5 / M14.6 disconnect-synthesis paths. Compatible with
abrupt TCP close — that arm of the read loop fires the same
synthesis.

## Authoritative `from`-rewriting

For every cross-node packet that carries a node-id field
(`Message.from`, `MonitorRequest.observer`, `Down.target`,
`Exit.from`, `PgJoin.member`, `PgBroadcast.from_qualified`, etc.):

- The `@<suffix>` MUST match the authenticated peer's NodeId
  name (verified at handshake time via mTLS + cookie).
- A mismatch is treated as a forgery attempt and the packet is
  rejected.

This applies to the *origin* of a packet, not relayed knowledge.
`PgSync.groups` carries members originated by other peers; the
`@`-suffix identifies the *origin*, not the relay sender.

## Caps and limits

| Limit | Value | Rationale |
|---|---|---|
| `MAX_FRAME_BYTES` | 16 MiB | Inbound OOM amplification floor |
| `OUTBOUND_CHANNEL_BUFFER` | 1024 packets | Per-peer mpsc backpressure |
| `DEFAULT_TTL` (graphreact-core) | 10 | Cycle protection |
| `tick_interval` (default) | 5 s | OTP-shaped |
| `miss_threshold` (default) | 3 | 15 s detection floor |

## See also

- [graphreact-dist Cargo.toml](../Cargo.toml) — protocol crate root.
- [docs/backend/m14-distribution.md](../../../docs/backend/m14-distribution.md) — milestone narrative + acceptance numbers.
- [otp/lib/kernel/src/dist_util.erl](../../../../otp/lib/kernel/src/dist_util.erl) — OTP's distribution handshake reference.
- [bun/src/bun.js/ipc.zig](../../../../bun/src/bun.js/ipc.zig) — length-prefixed framing reference.
