//! Postcard encode/decode bridging [`Packet`] and `(PacketType, Vec<u8>)`.
//!
//! Pure logic — no I/O. The framing layer ([`super::frame`]) takes
//! the encoded payload bytes and wraps them with the length-prefix
//! header.

use crate::error::DistError;
use crate::Result;

use super::packet::{
    Challenge, ChallengeAck, ChallengeAckV4, ChallengeReply, Demonitor, Down, Exit, Hello, HelloV4,
    LinkRequest, Message, MonitorAck, MonitorRequest, Packet, PacketType, PgBroadcast, PgDiscover,
    PgJoin, PgLeave, PgSync, ReloadModulePacket, Tick, TickAck, Unlink,
};

pub struct WireCodec;

impl WireCodec {
    /// Serialise a [`Packet`] into `(PacketType, payload bytes)`.
    pub fn encode(packet: &Packet) -> Result<(PacketType, Vec<u8>)> {
        let bytes = match packet {
            Packet::Hello(p) => postcard::to_stdvec(p)?,
            Packet::Challenge(p) => postcard::to_stdvec(p)?,
            Packet::ChallengeReply(p) => postcard::to_stdvec(p)?,
            Packet::ChallengeAck(p) => postcard::to_stdvec(p)?,
            Packet::Message(p) => postcard::to_stdvec(p)?,
            Packet::MonitorRequest(p) => postcard::to_stdvec(p)?,
            Packet::MonitorAck(p) => postcard::to_stdvec(p)?,
            Packet::Demonitor(p) => postcard::to_stdvec(p)?,
            Packet::Down(p) => postcard::to_stdvec(p)?,
            Packet::LinkRequest(p) => postcard::to_stdvec(p)?,
            Packet::Unlink(p) => postcard::to_stdvec(p)?,
            Packet::Exit(p) => postcard::to_stdvec(p)?,
            Packet::PgJoin(p) => postcard::to_stdvec(p)?,
            Packet::PgLeave(p) => postcard::to_stdvec(p)?,
            Packet::PgSync(p) => postcard::to_stdvec(p)?,
            Packet::PgDiscover(p) => postcard::to_stdvec(p)?,
            Packet::PgBroadcast(p) => postcard::to_stdvec(p)?,
            Packet::Tick(p) => postcard::to_stdvec(p)?,
            Packet::TickAck(p) => postcard::to_stdvec(p)?,
            Packet::ReloadModule(p) => postcard::to_stdvec(p)?,
            Packet::Disconnect => Vec::new(),
        };
        Ok((packet.packet_type(), bytes))
    }

    /// Deserialise `(PacketType, payload bytes)` back into a [`Packet`].
    ///
    /// **Phase β (sub-track #4) — rolling-version compat.** `Hello`
    /// and `ChallengeAck` decode via a dual-struct fallback: the v5
    /// shape is tried first; on `DeserializeUnexpectedEnd` (the v4
    /// shape, without the trailing `capabilities_bitmap: u64`) the
    /// payload is re-decoded as [`HelloV4`] / [`ChallengeAckV4`]
    /// and the missing field synthesises to `0`. Any other postcard
    /// error propagates — so genuine wire corruption is NOT papered
    /// over as "v4 peer with zero bitmap".
    pub fn decode(ty: PacketType, payload: &[u8]) -> Result<Packet> {
        Ok(match ty {
            PacketType::Hello => match postcard::from_bytes::<Hello>(payload) {
                Ok(h) => Packet::Hello(h),
                Err(postcard::Error::DeserializeUnexpectedEnd) => {
                    // Phase β fallback: v4 peer sent the old shape
                    // without `capabilities_bitmap`. Decode as
                    // HelloV4 and synthesise `capabilities_bitmap = 0`.
                    let v4 = postcard::from_bytes::<HelloV4>(payload)?;
                    Packet::Hello(Hello {
                        version: v4.version,
                        flags: v4.flags,
                        node_name: v4.node_name,
                        creation: v4.creation,
                        capabilities: v4.capabilities,
                        capabilities_bitmap: 0,
                    })
                }
                Err(e) => return Err(e.into()),
            },
            PacketType::Challenge => Packet::Challenge(postcard::from_bytes::<Challenge>(payload)?),
            PacketType::ChallengeReply => {
                Packet::ChallengeReply(postcard::from_bytes::<ChallengeReply>(payload)?)
            }
            PacketType::ChallengeAck => match postcard::from_bytes::<ChallengeAck>(payload) {
                Ok(a) => Packet::ChallengeAck(a),
                Err(postcard::Error::DeserializeUnexpectedEnd) => {
                    // Phase β fallback (symmetric with Hello above).
                    let v4 = postcard::from_bytes::<ChallengeAckV4>(payload)?;
                    Packet::ChallengeAck(ChallengeAck {
                        digest: v4.digest,
                        server_capabilities: v4.server_capabilities,
                        capabilities_bitmap: 0,
                    })
                }
                Err(e) => return Err(e.into()),
            },
            PacketType::Message => Packet::Message(postcard::from_bytes::<Message>(payload)?),
            PacketType::MonitorRequest => {
                Packet::MonitorRequest(postcard::from_bytes::<MonitorRequest>(payload)?)
            }
            PacketType::MonitorAck => {
                Packet::MonitorAck(postcard::from_bytes::<MonitorAck>(payload)?)
            }
            PacketType::Demonitor => Packet::Demonitor(postcard::from_bytes::<Demonitor>(payload)?),
            PacketType::Down => Packet::Down(postcard::from_bytes::<Down>(payload)?),
            PacketType::LinkRequest => {
                Packet::LinkRequest(postcard::from_bytes::<LinkRequest>(payload)?)
            }
            PacketType::Unlink => Packet::Unlink(postcard::from_bytes::<Unlink>(payload)?),
            PacketType::Exit => Packet::Exit(postcard::from_bytes::<Exit>(payload)?),
            PacketType::PgJoin => Packet::PgJoin(postcard::from_bytes::<PgJoin>(payload)?),
            PacketType::PgLeave => Packet::PgLeave(postcard::from_bytes::<PgLeave>(payload)?),
            PacketType::PgSync => Packet::PgSync(postcard::from_bytes::<PgSync>(payload)?),
            PacketType::PgDiscover => {
                Packet::PgDiscover(postcard::from_bytes::<PgDiscover>(payload)?)
            }
            PacketType::PgBroadcast => {
                Packet::PgBroadcast(postcard::from_bytes::<PgBroadcast>(payload)?)
            }
            PacketType::Tick => Packet::Tick(postcard::from_bytes::<Tick>(payload)?),
            PacketType::TickAck => Packet::TickAck(postcard::from_bytes::<TickAck>(payload)?),
            PacketType::ReloadModule => {
                Packet::ReloadModule(postcard::from_bytes::<ReloadModulePacket>(payload)?)
            }
            PacketType::Disconnect => {
                if !payload.is_empty() {
                    // Strict: a Disconnect packet has no payload. A
                    // non-empty payload is a malformed peer.
                    return Err(DistError::Codec(postcard::Error::DeserializeUnexpectedEnd));
                }
                Packet::Disconnect
            }
        })
    }
}
