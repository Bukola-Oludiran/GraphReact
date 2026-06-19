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
    PgJoin, PgLeave, PgSync, ReloadModulePacket, RestartDelegationAck, RestartDelegationRequest,
    RestartDelegationResult, Tick, TickAck, Unlink,
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
            Packet::RestartDelegationRequest(p) => postcard::to_stdvec(p)?,
            Packet::RestartDelegationAck(p) => postcard::to_stdvec(p)?,
            Packet::RestartDelegationResult(p) => postcard::to_stdvec(p)?,
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
            PacketType::RestartDelegationRequest => Packet::RestartDelegationRequest(
                postcard::from_bytes::<RestartDelegationRequest>(payload)?,
            ),
            PacketType::RestartDelegationAck => Packet::RestartDelegationAck(
                postcard::from_bytes::<RestartDelegationAck>(payload)?,
            ),
            PacketType::RestartDelegationResult => Packet::RestartDelegationResult(
                postcard::from_bytes::<RestartDelegationResult>(payload)?,
            ),
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

#[cfg(test)]
mod wire_v4_fallback_tests {
    //! **M-Prod sub-track #4 Phase β verify-finding patch.**
    //! Unit tests for the v4 dual-struct fallback on `Hello` AND
    //! `ChallengeAck`. The handshake-level integration tests in
    //! `crate::handshake::rolling_version_tests` exercise the Hello
    //! v4 path (a v4 dialer connecting to a v5 server). The
    //! ChallengeAck v4 fallback fires in the OPPOSITE direction
    //! (a v5 dialer reading a v4 server's ChallengeAck), which is
    //! exactly the rolling-upgrade ordering where the dialer was
    //! upgraded first. The handshake-level tests don't reach that
    //! direction; these pure codec-level tests do.
    use super::*;
    use crate::wire::packet::{ChallengeAckV4, HelloV4};

    #[test]
    fn decode_v4_hello_synthesises_zero_bitmap() {
        let v4 = HelloV4 {
            version: 4,
            flags: 0,
            node_name: "v4-peer@127.0.0.1".to_string(),
            creation: 7,
            capabilities: vec!["pg-v1".into()],
        };
        let bytes = postcard::to_stdvec(&v4).expect("encode v4");
        let decoded = WireCodec::decode(PacketType::Hello, &bytes).expect("decode v4 Hello");
        match decoded {
            Packet::Hello(h) => {
                assert_eq!(h.version, 4);
                assert_eq!(h.node_name, "v4-peer@127.0.0.1");
                assert_eq!(h.capabilities, vec!["pg-v1".to_string()]);
                assert_eq!(
                    h.capabilities_bitmap, 0,
                    "v4 dual-struct fallback MUST synthesise zero bitmap"
                );
            }
            other => panic!("expected Packet::Hello, got {other:?}"),
        }
    }

    #[test]
    fn decode_v4_challenge_ack_synthesises_zero_bitmap() {
        let v4 = ChallengeAckV4 {
            digest: [0x42; 32],
            server_capabilities: vec!["mon-v1".into(), "observer-otel-v1".into()],
        };
        let bytes = postcard::to_stdvec(&v4).expect("encode v4 ChallengeAck");
        let decoded =
            WireCodec::decode(PacketType::ChallengeAck, &bytes).expect("decode v4 ChallengeAck");
        match decoded {
            Packet::ChallengeAck(a) => {
                assert_eq!(a.digest, [0x42; 32]);
                assert_eq!(
                    a.server_capabilities,
                    vec!["mon-v1".to_string(), "observer-otel-v1".to_string()]
                );
                assert_eq!(
                    a.capabilities_bitmap, 0,
                    "v4 dual-struct fallback MUST synthesise zero bitmap on ChallengeAck"
                );
            }
            other => panic!("expected Packet::ChallengeAck, got {other:?}"),
        }
    }

    #[test]
    fn decode_v5_hello_preserves_bitmap() {
        // Sanity: the v5-direct path still works AND carries
        // a non-zero bitmap through unchanged.
        let v5 = Hello {
            version: 5,
            flags: 0,
            node_name: "v5-peer@127.0.0.1".to_string(),
            creation: 9,
            capabilities: vec!["pg-v1".into()],
            capabilities_bitmap: 0x5a5a_5a5a,
        };
        let bytes = postcard::to_stdvec(&v5).expect("encode v5");
        let decoded = WireCodec::decode(PacketType::Hello, &bytes).expect("decode v5 Hello");
        match decoded {
            Packet::Hello(h) => {
                assert_eq!(h.version, 5);
                assert_eq!(h.capabilities_bitmap, 0x5a5a_5a5a);
            }
            other => panic!("expected Packet::Hello, got {other:?}"),
        }
    }

    #[test]
    fn decode_v5_challenge_ack_preserves_bitmap() {
        let v5 = ChallengeAck {
            digest: [0x7e; 32],
            server_capabilities: vec!["mon-v1".into()],
            capabilities_bitmap: 0xdead_beef,
        };
        let bytes = postcard::to_stdvec(&v5).expect("encode v5 ChallengeAck");
        let decoded =
            WireCodec::decode(PacketType::ChallengeAck, &bytes).expect("decode v5 ChallengeAck");
        match decoded {
            Packet::ChallengeAck(a) => {
                assert_eq!(a.digest, [0x7e; 32]);
                assert_eq!(a.capabilities_bitmap, 0xdead_beef);
            }
            other => panic!("expected Packet::ChallengeAck, got {other:?}"),
        }
    }
}
