//! Wire protocol — frame format, packet payloads, postcard codec.
//!
//! ## Frame layout (universal)
//!
//! ```text
//! ┌────────┬──────────────┬───────────────────┐
//! │ Type:1 │  Len:4 (BE)  │  Payload:Len bytes │
//! └────────┴──────────────┴───────────────────┘
//! ```
//!
//! - `Type` is a single-byte [`PacketType`] discriminant.
//! - `Len` is the **payload length only**, not the whole-frame length.
//!   Big-endian (network convention; matches OTP).
//! - `Payload` is postcard-encoded — one struct per packet type.
//!
//! Inbound frames with `Len > MAX_FRAME_BYTES` are rejected with
//! [`crate::DistError::FrameTooLarge`] to prevent OOM amplification.
//!
//! ## Payload schema versioning
//!
//! Every handshake packet carries a `version: u16`. Receivers reject
//! `version < MIN_SUPPORTED_VERSION` to prevent downgrade attacks. A
//! capability list ([`packet::Hello::capabilities`]) carries
//! finer-grained feature negotiation.

pub mod codec;
pub mod frame;
pub mod packet;

pub use codec::WireCodec;
pub use frame::{WireFrame, MAX_FRAME_BYTES};
pub use packet::{
    Challenge, ChallengeAck, ChallengeReply, Demonitor, Down, Exit, Hello, LinkRequest, Message,
    MonitorAck, MonitorRequest, Packet, PacketType, PgBroadcast, PgDiscover, PgJoin, PgLeave,
    PgSync, ReloadModulePacket, Tick, TickAck, Unlink, WireExitReason, MIN_SUPPORTED_VERSION,
    WIRE_VERSION,
};
