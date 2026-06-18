//! Packet payload types + the [`Packet`] dispatch enum.
//!
//! Every variant on [`PacketType`] has exactly one struct here that
//! is `#[derive(Serialize, Deserialize)]` postcard-compatible.
//!
//! Field order in each struct is the on-wire order.
//!
//! ## Capability surfaces (Phase β)
//!
//! Two parallel surfaces carry per-engine + per-feature capability
//! advertisement:
//!
//! - [`Hello::capabilities`] / [`ChallengeAck::server_capabilities`]
//!   — `Vec<String>` of per-feature opt-in flags
//!   (`"reload-v1"`, `"observer-otel-v1"`, `"ingress-call-v1"`,
//!   etc.). Open string set; each consumer owns its flag name.
//! - [`Hello::capabilities_bitmap`] /
//!   [`ChallengeAck::capabilities_bitmap`] — `u64` typed bitmap for
//!   substrate-level engine capabilities ([`CAP_MIXED_RUNTIME`],
//!   [`CAP_DELEGATED_RESTART`], [`CAP_AUDIT_CHAIN`]). Closed flag
//!   set governed by the dist crate.
//!
//! Future contributors: a new feature picks ONE surface — don't
//! double-declare. String-vec is for per-feature opt-in flags
//! (engines may not implement all of them); the bitmap is for
//! substrate-level capabilities that the dist crate or other
//! cross-engine layers structure dispatch around.

use serde::{Deserialize, Serialize};

/// Current wire protocol revision. Bumped on schema-breaking changes.
///
/// **History:**
/// - `1` — initial M14 substrate (handshake + Message + supervision +
///   pg + Tick + Disconnect packets).
/// - `2` — M15 Phase 10. Adds `PacketType::ReloadModule` for
///   cluster-wide hot reload propagation. `ChallengeAck` widens to
///   carry the server's capabilities so client-side connections
///   learn peer capabilities the same way server-side connections
///   learn them from `Hello`.
/// - `3` — **M16-Backend Phase 10.** Adds
///   [`Message::trace_context: Option<[u8; 24]>`] for cross-node OTel
///   span linkage (`TraceID(16) || SpanID(8)`). Peers advertising the
///   [`crate::OBSERVER_OTEL_V1_CAPABILITY`] flag in their `Hello` /
///   `ChallengeAck` capabilities populate the field on outbound
///   remote messages; receivers attach the context as an OTel `Link`
///   on the receiver-side `message.deliver` span. Peers without the
///   capability still receive v3 packets correctly (the field
///   deserializes as `None`); they simply don't populate the field
///   on their outbound side. **No backwards-compatible bridge to v2
///   peers** — the bumped [`MIN_SUPPORTED_VERSION`] makes this a hard
///   floor; M-Prod owns rolling-version negotiation. Capability-bit
///   mixing ("graceful mixed-version") happens *within* a v3 cluster
///   via the `Hello.capabilities` list, not across wire versions.
/// - `4` — **M-Ingress phase 2.** Adds
///   [`Message::correlation_id: Option<String>`] for end-to-end
///   correlation of synchronous HTTP calls (`POST /v1/nodes/:id/call`
///   in the management API) with their auto-tagged replies. Peers
///   advertising the [`crate::INGRESS_CALL_V1_CAPABILITY`] flag in
///   their `Hello` / `ChallengeAck` capabilities populate the field
///   when a local handler emits a reply tagged by
///   [`graphreact_core::Scheduler::deliver`]'s auto-propagation
///   (per the local manifest's `accepts_external_call` table); peers
///   without the capability strip the field on outbound and ignore
///   it on inbound (the wire bytes still parse — `Option<String>`
///   round-trips correctly under postcard either way — but a v4
///   peer treats correlation_id from a non-cap peer as "not part
///   of any call I'm tracking"). Same hard-floor invariant as the
///   `2 → 3` bump: v3 peers cannot interop with v4 peers; M-Prod
///   owns rolling-version negotiation. The capability flag handles
///   in-version graceful-mixed-version operation.
/// - `5` — **M-Prod sub-track #4 Phase β.** Rolling-version safe-slice.
///   Adds [`Hello::capabilities_bitmap: u64`] and
///   [`ChallengeAck::capabilities_bitmap: u64`] as a parallel typed
///   capability surface alongside the existing `Vec<String>`
///   per-feature flags. The floor [`MIN_SUPPORTED_VERSION`] no
///   longer tracks [`WIRE_VERSION`] one-for-one — it lags by one to
///   open a one-bump rolling-upgrade compat window so v4 daemons
///   can interop with v5 daemons during a cluster rollout. v4 peers
///   shipping a Hello/ChallengeAck WITHOUT the new field are
///   accepted via a dual-struct fallback in
///   [`crate::wire::WireCodec::decode`] that synthesises
///   `capabilities_bitmap = 0` (no substrate-bitmap capabilities
///   advertised). Future bumps either move BOTH constants together
///   (when no compat window is desired) OR move WIRE_VERSION only
///   (to widen the window). Planned consumers: #4 Phase γ
///   (cross-node delegated-restart, [`CAP_DELEGATED_RESTART`]) and
///   #1 Phase B (multi-token audit-chain, [`CAP_AUDIT_CHAIN`]).
pub const WIRE_VERSION: u16 = 5;
/// Minimum protocol version this build will accept on a Hello/Challenge.
///
/// Lags [`WIRE_VERSION`] by one for one-bump rolling-upgrade
/// compatibility as of Phase β (sub-track #4). Future bumps either
/// move both constants together (no compat window) OR move
/// [`WIRE_VERSION`] only (widen the window by one). The handshake
/// also rejects peers ABOVE [`WIRE_VERSION`] (future peers we don't
/// understand) — see [`crate::handshake::server_handshake`].
pub const MIN_SUPPORTED_VERSION: u16 = 4;

/// **Phase β (sub-track #4).** Per-engine capability bitmap exchanged in
/// [`Hello::capabilities_bitmap`] / [`ChallengeAck::capabilities_bitmap`].
/// Distinct from the existing [`Hello::capabilities`] `Vec<String>` —
/// the bitmap is the new typed-flag surface for substrate features;
/// the string vec stays for per-feature opt-in flags
/// (`"reload-v1"`, `"observer-otel-v1"`, `"ingress-call-v1"`).
///
/// Both sides exchange their LOCAL bitmap; the negotiated
/// "effective" set is the bitwise AND of `our_bitmap & peer_bitmap`,
/// queryable from [`crate::Cluster::peer_effective_capabilities`].
///
/// Phase β only EXPOSES the bitmap — no dispatch path consumes it
/// yet. The first consumer ships in Phase γ (M14.11 cross-node
/// delegated-restart via [`CAP_DELEGATED_RESTART`]).
///
/// Reserved-but-unimplemented placeholder for M22 mixed-runtime
/// (cross-engine routing under one daemon). Listed here so the bit
/// position is reserved across the substrate before the M22 work
/// lands.
pub const CAP_MIXED_RUNTIME: u64 = 1 << 0;
/// **Phase β (sub-track #4).** Reserved for M14.11 cross-node
/// delegated-restart — supervisors on one node delegating restart
/// to another node's supervision tree. Phase γ consumer.
pub const CAP_DELEGATED_RESTART: u64 = 1 << 1;
/// **Phase β (sub-track #4).** Reserved for the M-Prod #1 multi-token
/// audit-chain feature — hash-chained audit-event sequences shipped
/// alongside cluster-wide reloads + cross-node delegated operations.
/// #1 Phase B consumer.
pub const CAP_AUDIT_CHAIN: u64 = 1 << 2;

/// Single-byte type discriminant for every wire packet.
///
/// Codes are stable — never re-numbered, never reused.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketType {
    Hello = 0x01,
    Challenge = 0x02,
    ChallengeReply = 0x03,
    ChallengeAck = 0x04,

    Message = 0x10,
    MonitorRequest = 0x11,
    MonitorAck = 0x12,
    Demonitor = 0x13,
    Down = 0x14,
    LinkRequest = 0x15,
    Unlink = 0x16,
    Exit = 0x17,

    PgJoin = 0x20,
    PgLeave = 0x21,
    PgSync = 0x22,
    PgDiscover = 0x23,
    PgBroadcast = 0x24,

    Tick = 0x30,
    TickAck = 0x31,

    /// **M15 Phase 10.** Cluster-wide hot reload. Origin ships
    /// origin-authoritative IR to every peer that advertised the
    /// `"reload-v1"` capability in its `Hello.capabilities` /
    /// `ChallengeAck.server_capabilities`. Peers without the
    /// capability are skipped at the origin (operator warning
    /// logged) — they never see this packet, so v1 peers don't
    /// trip on the unknown type code.
    ReloadModule = 0x40,

    Disconnect = 0xFF,
}

impl PacketType {
    pub const fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0x01 => Self::Hello,
            0x02 => Self::Challenge,
            0x03 => Self::ChallengeReply,
            0x04 => Self::ChallengeAck,
            0x10 => Self::Message,
            0x11 => Self::MonitorRequest,
            0x12 => Self::MonitorAck,
            0x13 => Self::Demonitor,
            0x14 => Self::Down,
            0x15 => Self::LinkRequest,
            0x16 => Self::Unlink,
            0x17 => Self::Exit,
            0x20 => Self::PgJoin,
            0x21 => Self::PgLeave,
            0x22 => Self::PgSync,
            0x23 => Self::PgDiscover,
            0x24 => Self::PgBroadcast,
            0x30 => Self::Tick,
            0x31 => Self::TickAck,
            0x40 => Self::ReloadModule,
            0xFF => Self::Disconnect,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------
// Handshake (M14.3 wires these into a state machine)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u16,
    pub flags: u32,
    pub node_name: String,
    pub creation: u32,
    pub capabilities: Vec<String>,
    /// **Phase β (sub-track #4).** Per-engine capability bitmap.
    /// Substrate-level typed flag surface; see [`CAP_MIXED_RUNTIME`],
    /// [`CAP_DELEGATED_RESTART`], [`CAP_AUDIT_CHAIN`].
    ///
    /// **Compat (one-bump, bitmap-blindness model)**: v5 daemons
    /// receiving v4 Hellos decode via the [`HelloV4`] fallback path in
    /// [`crate::wire::WireCodec::decode`], synthesising
    /// `capabilities_bitmap = 0`. v4 daemons receiving v5 Hellos
    /// SILENTLY DROP the trailing u64 — postcard 1.x's `from_bytes`
    /// documents that the unused portion of the byte slice is not
    /// returned and does NOT error on trailing data (verify-finding
    /// correction 2026-06-18; prior comments here claimed postcard
    /// errored with `DeserializeUnexpectedEnd`, which is empirically
    /// wrong). So both directions decode at the WIRE layer, but the
    /// v4 side is BITMAP-BLIND: it sees the v4 shape with no
    /// `capabilities_bitmap` field at all and cannot advertise or
    /// consume any `CAP_*` bit. Operators rolling a cluster therefore
    /// still upgrade ALL daemons one bump up (v4→v5) before any
    /// feature that consumes the bitmap (Phase γ +) starts demanding
    /// `capabilities_bitmap != 0` — not because the wire breaks, but
    /// because v4 daemons cannot participate in bitmap-gated dispatch.
    pub capabilities_bitmap: u64,
}

/// **Phase β internal.** v4 wire shape of [`Hello`] (without
/// `capabilities_bitmap`). Used ONLY by the codec's
/// fallback-on-`DeserializeUnexpectedEnd` path in
/// [`crate::wire::WireCodec::decode`] so v5 daemons can parse v4
/// peers' Hello payloads. Never constructed by the handshake code
/// paths — they always emit the current [`Hello`] shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HelloV4 {
    pub version: u16,
    pub flags: u32,
    pub node_name: String,
    pub creation: u32,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Challenge {
    pub challenge: [u8; 32],
    pub server_name: String,
    pub creation: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChallengeReply {
    pub challenge: [u8; 32],
    /// HMAC-SHA256(cookie, server_challenge_bytes). Constant-time
    /// compared on the receiver side via `subtle::ConstantTimeEq`
    /// (lands in M14.3).
    pub digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChallengeAck {
    /// HMAC-SHA256(cookie, client_challenge_bytes).
    pub digest: [u8; 32],
    /// **M15 Phase 10 / wire v2.** The server's capabilities
    /// echoed back to the client. Closes the M14.3 substrate gap
    /// where client-side `client_handshake` returned
    /// `peer_capabilities: Vec::new()` because nothing in the
    /// flow carried them. With this field the client side learns
    /// the peer's capabilities the same way the server side
    /// learns them from `Hello.capabilities`, so cluster-wide
    /// hot reload (and any future capability-gated dispatch) can
    /// filter peers regardless of which side dialled.
    pub server_capabilities: Vec<String>,
    /// **Phase β (sub-track #4).** Server's capability bitmap (mirrors
    /// [`Hello::capabilities_bitmap`]). v5 receivers consuming v4
    /// ChallengeAcks decode via the [`ChallengeAckV4`] fallback path
    /// in [`crate::wire::WireCodec::decode`], synthesising
    /// `capabilities_bitmap = 0`. v4 receivers consuming v5
    /// ChallengeAcks silently drop the trailing u64 (postcard 1.x
    /// from_bytes ignores trailing data — verify-finding correction
    /// 2026-06-18). See [`Hello::capabilities_bitmap`] for the
    /// bitmap-blindness model + upgrade-ordering policy.
    pub capabilities_bitmap: u64,
}

/// **Phase β internal.** v4 wire shape of [`ChallengeAck`] (without
/// `capabilities_bitmap`). Used ONLY by the codec's
/// fallback-on-`DeserializeUnexpectedEnd` path in
/// [`crate::wire::WireCodec::decode`]. Never constructed by the
/// handshake code paths.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ChallengeAckV4 {
    pub digest: [u8; 32],
    pub server_capabilities: Vec<String>,
}

// ---------------------------------------------------------------------
// Data plane — Message routing across the wire
// ---------------------------------------------------------------------

/// On-wire form of `graphreact_core::Message`.
///
/// **TTL inheritance is preserved end-to-end** via the
/// `inherited_ttl` field. The receiving side installs it into the
/// scheduler's `current_delivery_ttl` for the duration of dispatch
/// so emits during re-render see the same TTL bound the local
/// scheduler would have applied. One hop = one decrement, regardless
/// of how many physical machines the message traversed.
///
/// **`from` is rewritten authoritatively** by the receiving side
/// from the authenticated peer NodeId — sender-supplied `from` is
/// never trusted (forgery prevention).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub msg_type: String,
    pub from: String,
    pub to: String,
    /// `serde_json::to_vec` output. Engines re-parse; cross-language
    /// payloads stay JSON for parity with the existing FFI marshalling.
    pub payload: Vec<u8>,
    pub ttl: u32,
    /// Wall-clock millisecond timestamp at creation (sender side).
    /// Telemetry only — Lamport-style ordering is not used yet
    /// (single TCP connection preserves order per peer pair).
    pub timestamp: u64,
    pub direct: bool,
    pub inherited_ttl: Option<u32>,
    /// **M16-Backend Phase 10.** Optional remote OTel span context
    /// (`TraceID(16) || SpanID(8)` = 24 bytes). Populated by the
    /// outbound peer's [`crate::Cluster::install_wire_tracing_hook`]
    /// installation when the local thread has an active OTel span
    /// (typically the `scheduler.tick` span emitted by
    /// `graphreact-observer::OtelSink`). The receiving peer's
    /// [`crate::dispatch_inbound`] propagates the context into the
    /// local
    /// [`graphreact_core::runtime::types::Message::trace_context`] so
    /// the scheduler's `MessageDelivered` event carries it; the
    /// receiver-side `OtelSink` consumes it and attaches as an OTel
    /// `Link` on the `message.deliver` span. `None` for peers without
    /// the [`crate::OBSERVER_OTEL_V1_CAPABILITY`] capability, or for
    /// messages with no active span at outbound time. Wire format
    /// landed at `WIRE_VERSION = 3`.
    pub trace_context: Option<[u8; 24]>,
    /// **M-Ingress phase 2.** Opaque correlation identifier paired
    /// with synchronous HTTP calls (`POST /v1/nodes/:id/call` in the
    /// management API). Generated server-side as a UUIDv4 by the
    /// call handler before crossing the cross-thread bridge to the
    /// scheduler; the auto-propagation logic in
    /// [`graphreact_core::Scheduler::deliver`] copies it onto the
    /// first outgoing emit whose `type` matches the call's declared
    /// `reply_type` (per the local manifest's
    /// `accepts_external_call` table). Cluster wire propagation is
    /// gated on the [`crate::INGRESS_CALL_V1_CAPABILITY`] flag — a
    /// sender targeting a peer without the capability serializes
    /// `None` here regardless of the local Message's value;
    /// receivers from peers without the capability ignore the field
    /// on inbound (a v4 peer without the capability still parses the
    /// wire bytes correctly — postcard deserializes `Option<String>`
    /// either way — but treats correlation_id from a non-cap peer as
    /// "not part of any call I'm tracking"). `None` for messages
    /// that aren't part of a call flow. Wire format landed at
    /// `WIRE_VERSION = 4`.
    pub correlation_id: Option<String>,
}

// ---------------------------------------------------------------------
// Supervision — distributed monitors, links, exits
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorRequest {
    pub observer: String,
    pub target: String,
    pub remote_ref: u64,
    pub creation: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorAck {
    pub remote_ref: u64,
    pub target_alive: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Demonitor {
    pub remote_ref: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Down {
    pub remote_ref: u64,
    pub target: String,
    pub reason: WireExitReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkRequest {
    pub a: String,
    pub b: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unlink {
    pub a: String,
    pub b: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exit {
    pub from: String,
    pub reason: WireExitReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WireExitReason {
    Normal,
    Shutdown,
    Crashed { detail: String },
    /// Synthesized by the dist crate when a peer connection drops or
    /// the tick threshold is exceeded. Maps to OTP's `noconnection`
    /// reason string.
    NodeDown,
}

// ---------------------------------------------------------------------
// pg — distributed group registry (gossip protocol)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PgJoin {
    pub group: String,
    /// `"local_id@node"` — fully qualified.
    pub member: String,
    /// Vector clock entries `(node_name, lamport)` so receivers can
    /// reconcile concurrent joins/leaves.
    pub vc: Vec<(String, u64)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PgLeave {
    pub group: String,
    pub member: String,
    pub vc: Vec<(String, u64)>,
}

/// Full snapshot — sent on initial discovery and after partition heal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PgSync {
    pub groups: Vec<(String, Vec<String>)>,
    pub vc: Vec<(String, u64)>,
}

/// Initial peer interrogation. Receiver responds with [`PgSync`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PgDiscover {
    pub groups_known: Vec<String>,
    pub vc: Vec<(String, u64)>,
}

/// Decentralized tree-based broadcast — M14.6 hub-and-spoke
/// updated to M14.8.x-followon (Phase F) tree fan-out. The
/// originating node populates `visited_peers = [our_name]` and
/// sends to every peer hosting ≥1 member of `group`. Each
/// receiver fans to its local members AND relays to every peer
/// in `peers_with_members(group)` not already in `visited_peers`,
/// pushing its own name onto `visited_peers` first. Loop
/// prevention: the visited list grows monotonically; each peer
/// is reached at most once. **Wire size grows linearly with
/// cluster size** — packets carry a list of peer names, ~30
/// bytes each, capped well under `MAX_FRAME_BYTES = 16 MiB`.
///
/// Pre-Phase-F clusters that send `PgBroadcast` without
/// `visited_peers` populated are still correctly handled (the
/// field is parsed as empty by the receiver, who treats itself
/// as the only visited peer when relaying).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PgBroadcast {
    pub group: String,
    pub msg_type: String,
    /// `serde_json::to_vec` output. Engines re-parse on receipt;
    /// payload encoding mirrors [`Message::payload`].
    pub payload: Vec<u8>,
    /// Already-qualified `from_local_id@origin_node` form. The
    /// receiver enqueues local Messages with this as `from`
    /// verbatim so handlers see the same authoritative source as
    /// any other cross-node Message.
    pub from_qualified: String,
    /// Wall-clock millisecond timestamp, telemetry only.
    pub timestamp: u64,
    /// **M14.8.x-followon (Phase F).** Peer names that have
    /// already received this broadcast. Originator populates with
    /// `[our_name]`; each relay appends its own name before
    /// shipping downstream. Loop prevention: receivers only
    /// relay to peers not in this list.
    #[serde(default)]
    pub visited_peers: Vec<String>,
}

// ---------------------------------------------------------------------
// Hot reload — M15 Phase 10
// ---------------------------------------------------------------------

/// Origin-authoritative `.grx` reload payload shipped to every
/// peer that advertised the `"reload-v1"` capability.
///
/// Peers decode `ir_payload` as a JSON-encoded
/// `graphreact_grx_parser::ir::GrxIr` (mirrors `Message.payload`'s
/// outer-postcard / inner-JSON split — postcard itself can't
/// serialise `AttrMap`'s `serde_json::Value` field) and apply
/// via the same classifier + orchestrator pipeline used for
/// local reloads — no re-parse from local disk in v1 (peer-side
/// re-parse + hash verification is M-Prod's third-party security
/// audit deliverable). `ir_hash` is the SHA-256 over `ir_payload`,
/// shipped alongside so peers can surface a tracing warning on
/// mismatch (mismatch is non-fatal in v1; see [`crate::cluster`]
/// inbound dispatch).
///
/// `version` is a monotonic per-origin reload counter intended
/// for replay defence + simple operator visibility (e.g. "alice
/// shipped reload #7"). Per-origin uniqueness only — peers
/// receiving multiple origins' reloads track them independently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReloadModulePacket {
    /// Bare origin-node name (e.g. `alice`, not `alice@127.0.0.1`).
    /// Receivers compare directly against the authenticated peer
    /// name from the handshake, mirroring the `Message` forgery
    /// rejection pattern.
    pub origin: String,
    /// Canonical path on the origin's filesystem. Informational
    /// plus tracing-friendly; peers treat it as opaque and do
    /// not re-read from local disk in v1.
    pub file_path: String,
    /// SHA-256 of `ir_payload`. Lets peers detect transit
    /// corruption over the encoded bytes (TLS already guards
    /// against that, so a non-matching hash post-decode is
    /// substrate evidence of an encoder bug or a hash
    /// misconfiguration; we log + continue rather than reject).
    pub ir_hash: [u8; 32],
    /// `postcard::to_stdvec(&GrxIr)`. Decoded by the peer side
    /// of the reload pipeline (no parser invocation; pure
    /// deserialisation of the origin-authoritative IR shape).
    pub ir_payload: Vec<u8>,
    /// Per-origin monotonic reload counter. M-Prod will harden
    /// this with signed packets + replay defence beyond the
    /// counter; v1 just logs the version for ops visibility.
    pub version: u64,
}

// ---------------------------------------------------------------------
// Liveness — net-tick (M14.7)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tick {
    pub seq: u64,
    pub sent_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TickAck {
    pub seq: u64,
}

// ---------------------------------------------------------------------
// Top-level dispatch enum
// ---------------------------------------------------------------------

/// Tagged union of every wire-protocol packet. Decoded from
/// `(PacketType, &[u8])` via [`crate::wire::WireCodec`].
#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    Hello(Hello),
    Challenge(Challenge),
    ChallengeReply(ChallengeReply),
    ChallengeAck(ChallengeAck),

    Message(Message),
    MonitorRequest(MonitorRequest),
    MonitorAck(MonitorAck),
    Demonitor(Demonitor),
    Down(Down),
    LinkRequest(LinkRequest),
    Unlink(Unlink),
    Exit(Exit),

    PgJoin(PgJoin),
    PgLeave(PgLeave),
    PgSync(PgSync),
    PgDiscover(PgDiscover),
    PgBroadcast(PgBroadcast),

    Tick(Tick),
    TickAck(TickAck),

    /// **M15 Phase 10.** Cluster-wide hot reload payload.
    ReloadModule(ReloadModulePacket),

    /// Graceful disconnect — peer announces it's leaving the cluster.
    /// Carries no payload; the type code is sufficient.
    Disconnect,
}

impl Packet {
    pub fn packet_type(&self) -> PacketType {
        match self {
            Self::Hello(_) => PacketType::Hello,
            Self::Challenge(_) => PacketType::Challenge,
            Self::ChallengeReply(_) => PacketType::ChallengeReply,
            Self::ChallengeAck(_) => PacketType::ChallengeAck,
            Self::Message(_) => PacketType::Message,
            Self::MonitorRequest(_) => PacketType::MonitorRequest,
            Self::MonitorAck(_) => PacketType::MonitorAck,
            Self::Demonitor(_) => PacketType::Demonitor,
            Self::Down(_) => PacketType::Down,
            Self::LinkRequest(_) => PacketType::LinkRequest,
            Self::Unlink(_) => PacketType::Unlink,
            Self::Exit(_) => PacketType::Exit,
            Self::PgJoin(_) => PacketType::PgJoin,
            Self::PgLeave(_) => PacketType::PgLeave,
            Self::PgSync(_) => PacketType::PgSync,
            Self::PgDiscover(_) => PacketType::PgDiscover,
            Self::PgBroadcast(_) => PacketType::PgBroadcast,
            Self::Tick(_) => PacketType::Tick,
            Self::TickAck(_) => PacketType::TickAck,
            Self::ReloadModule(_) => PacketType::ReloadModule,
            Self::Disconnect => PacketType::Disconnect,
        }
    }
}
