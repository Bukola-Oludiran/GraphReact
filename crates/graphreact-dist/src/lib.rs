//! GraphReact distribution layer — M14.
//!
//! Erlang-cluster-equivalent multi-host clustering for the Backend
//! product. Lights up actor-shaped distribution: location-transparent
//! message addressing, distributed monitors and links, distributed
//! `pg` group registry, net-tick partition detection.
//!
//! ## Architectural position
//!
//! This crate is a **peer to the engine layer** — it depends on
//! [`graphreact_core`] for `Scheduler` / `Message` / `MonitorRef` /
//! `GroupRegistry`, but NOT on any specific engine
//! (`graphreact-engine-jsc`, `graphreact-engine-cpython`, etc.). The
//! same dist crate routes JSC and CPython component bodies because
//! the cross-node boundary lives at the scheduler layer, not at the
//! engine layer.
//!
//! ## Sub-staging (M14.1 → M14.9)
//!
//! - **M14.1 (this slice — shipped 2026-05-05)** — crate skeleton,
//!   `NodeId` / `Creation` / `RemoteNodeRef`, wire envelope types,
//!   postcard encode/decode, length-prefix codec on top of
//!   `tokio_util::codec::Framed`, `DistError` taxonomy. Round-trip
//!   tests for every packet variant + framed codec partial-read tests.
//! - **M14.2** — TLS transport (`Listener`, `Connection`, dial/accept
//!   loops via `tokio-rustls`).
//! - **M14.3** — OTP-style handshake state machine (Hello → Challenge
//!   → ChallengeReply → ChallengeAck) using HMAC-SHA256(cookie); peer
//!   registry; `Discovery` trait with `StaticDiscovery` and
//!   `DnsDiscovery` implementations.
//! - **M14.4** — `DistRouter` + cross-node message delivery with TTL
//!   inheritance preserved end-to-end. Adds a `RemoteSink` callback
//!   hook to `graphreact_core::Scheduler::make_emit`.
//! - **M14.5** — distributed monitors + links + disconnect synthesis
//!   (cross-node `__DOWN__` / `__EXIT__`).
//! - **M14.6** — distributed `pg` group registry with gossip protocol.
//! - **M14.7** — net-tick + `PartitionDetector` + `NetSplitPolicy`.
//! - **M14.8** — CLI integration (`grx daemon` + `grx connect`).
//! - **M14.9** — acceptance bench + soak + docs + plan/memory updates.
//!
//! ## Wire protocol (concrete)
//!
//! Every packet on the wire is `[Type:u8] [Len:u32 BE] [Payload]`.
//! Length is payload bytes only; cap at `MAX_FRAME_BYTES` (16 MiB)
//! to prevent OOM amplification. Payload is postcard-encoded — one
//! struct per type code in [`wire::packet`]. See
//! `docs/wire-protocol.md` for byte-level layout.
//!
//! Authentication: TLS (rustls) for confidentiality + transport
//! integrity; cookie verification via HMAC-SHA256 challenge-response
//! during the handshake. No application-layer HMAC on individual data
//! packets — TLS connection identity carries auth post-handshake
//! (matches OTP semantics).

pub mod cluster;
pub mod discovery;
pub mod error;
pub mod groups;
pub mod handshake;
pub mod net_tick;
pub mod node_id;
pub mod observability;
pub mod registry;
pub mod reload;
pub mod secret;
pub mod supervision;
pub mod telemetry;
pub mod tls_config;
pub mod transport;
pub mod wire;

#[cfg(test)]
pub(crate) mod dev_tls;

pub use cluster::{Cluster, ClusterConfig};
pub use discovery::{Discovery, DnsDiscovery, StaticDiscovery};
pub use error::DistError;
pub use handshake::{client_handshake, server_handshake, should_keep_outbound, HandshakeResult};
pub use node_id::{Creation, NodeId, RemoteNodeRef};
pub use groups::{DistGroups, MIN_GOSSIP_INTERVAL};
pub use net_tick::{NetSplitPolicy, NetTickConfig};
pub use observability::{
    WireTracingHook, WireTracingHookSlot, INGRESS_CALL_V1_CAPABILITY,
    OBSERVER_OTEL_V1_CAPABILITY,
};
pub use registry::{NodeRegistry, PeerHandle};
pub use reload::{
    ClusterReloadOutcome, PeerReloadOutcome, RemoteReloadCallback, RemoteReloadSink,
    RELOAD_V1_CAPABILITY,
};
pub use secret::SecretCookie;
pub use supervision::RemoteSupervision;
pub use telemetry::TelemetrySinkSlot;
pub use tls_config::TlsConfig;
pub use transport::{Connection, Listener};
pub use wire::{
    CAP_AUDIT_CHAIN, CAP_DELEGATED_RESTART, CAP_MIXED_RUNTIME, MAX_FRAME_BYTES,
    MIN_SUPPORTED_VERSION, Packet, PacketType, PgBroadcast, ReloadModulePacket, WireCodec,
    WireExitReason, WireFrame, WIRE_VERSION,
};

/// Result alias used throughout the dist crate.
pub type Result<T> = core::result::Result<T, DistError>;
