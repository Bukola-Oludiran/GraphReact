//! Length-prefix codec for `[Type:u8] [Len:u32 BE] [Payload]` frames.
//!
//! Implements `tokio_util::codec::{Encoder, Decoder}` so it composes
//! cleanly with `Framed<TlsStream<TcpStream>, WireFrame>` (M14.2).
//!
//! The codec is **stateless** — it inspects the buffer, emits one
//! frame at a time, and leaves trailing bytes for the next call.
//!
//! # Frame size cap
//!
//! Inbound frames with `Len > MAX_FRAME_BYTES` are rejected with
//! [`crate::DistError::FrameTooLarge`]. The cap is 16 MiB —
//! comfortably above any realistic packet (the largest variant is
//! [`super::packet::PgSync`] with a full membership snapshot, which
//! tops out in the low MB even on large clusters) but small enough
//! to stop a malicious peer from forcing 4 GiB allocations via an
//! inflated `Len:u32` header.

use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use super::codec::WireCodec;
use super::packet::{Packet, PacketType};
use crate::error::DistError;

/// Maximum payload size accepted on the wire. 16 MiB.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;
const HEADER_BYTES: usize = 1 + 4; // Type:u8 + Len:u32 BE

/// `tokio_util::codec` adapter that maps a TCP byte stream to a
/// stream of [`Packet`]s.
#[derive(Debug, Default, Clone, Copy)]
pub struct WireFrame;

impl Decoder for WireFrame {
    type Item = Packet;
    type Error = DistError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Packet>, DistError> {
        // Need the full header before we can size the payload.
        if src.len() < HEADER_BYTES {
            // Hint to tokio's reader that more bytes are needed.
            src.reserve(HEADER_BYTES - src.len());
            return Ok(None);
        }

        // Peek the header without advancing — we may not have the
        // full payload yet, in which case we want to leave the
        // header in place for the next call.
        let type_code = src[0];
        let len = u32::from_be_bytes([src[1], src[2], src[3], src[4]]);

        if len > MAX_FRAME_BYTES {
            return Err(DistError::FrameTooLarge {
                len,
                cap: MAX_FRAME_BYTES,
            });
        }

        let total = HEADER_BYTES + len as usize;
        if src.len() < total {
            // Reserve the remaining bytes so the reader can fill in
            // one read syscall ideally.
            src.reserve(total - src.len());
            return Ok(None);
        }

        // Full frame is present — consume header + payload.
        let ty = PacketType::from_code(type_code)
            .ok_or(DistError::UnknownPacketType { code: type_code })?;
        src.advance(HEADER_BYTES);
        let payload = src.split_to(len as usize);
        let packet = WireCodec::decode(ty, &payload)?;
        Ok(Some(packet))
    }
}

impl Encoder<Packet> for WireFrame {
    type Error = DistError;

    fn encode(&mut self, item: Packet, dst: &mut BytesMut) -> Result<(), DistError> {
        let (ty, payload) = WireCodec::encode(&item)?;
        let len = payload.len();
        // Sanity: the encoded payload itself must fit in u32 + the
        // configured cap. A pathological encoder bug or a giant pg
        // snapshot would otherwise produce a frame the receiver
        // would reject.
        let len_u32 = u32::try_from(len).map_err(|_| DistError::FrameTooLarge {
            len: u32::MAX,
            cap: MAX_FRAME_BYTES,
        })?;
        if len_u32 > MAX_FRAME_BYTES {
            return Err(DistError::FrameTooLarge {
                len: len_u32,
                cap: MAX_FRAME_BYTES,
            });
        }
        dst.reserve(HEADER_BYTES + len);
        dst.put_u8(ty.code());
        dst.put_u32(len_u32); // BufMut::put_u32 is big-endian.
        dst.extend_from_slice(&payload);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::packet::{
        ChallengeAck, Hello, Message, Tick, WireExitReason, MIN_SUPPORTED_VERSION, WIRE_VERSION,
    };

    fn roundtrip(packet: Packet) {
        let mut codec = WireFrame;
        let mut buf = BytesMut::new();
        codec.encode(packet.clone(), &mut buf).expect("encode");
        let decoded = codec.decode(&mut buf).expect("decode").expect("ready");
        assert_eq!(packet, decoded, "roundtrip mismatch");
        assert!(buf.is_empty(), "trailing bytes left after decode");
    }

    #[test]
    fn roundtrip_hello() {
        roundtrip(Packet::Hello(Hello {
            version: WIRE_VERSION,
            flags: 0,
            node_name: "alice@127.0.0.1".into(),
            creation: 42,
            capabilities: vec!["pg-v1".into(), "monitor-v1".into()],
        }));
    }

    #[test]
    fn roundtrip_challenge_ack() {
        roundtrip(Packet::ChallengeAck(ChallengeAck {
            digest: [7u8; 32],
            server_capabilities: vec!["pg-v1".into(), "reload-v1".into()],
        }));
    }

    /// **M15 Phase 10.** ReloadModule must postcard-roundtrip with
    /// the embedded `ir_payload` byte-stable so peers decode the
    /// origin-authoritative IR shape without drift.
    #[test]
    fn roundtrip_reload_module() {
        roundtrip(Packet::ReloadModule(
            crate::wire::ReloadModulePacket {
                origin: "alice@127.0.0.1".into(),
                file_path: "/srv/grx/system.grx".into(),
                ir_hash: [0xABu8; 32],
                ir_payload: vec![1, 2, 3, 4, 5, 6, 7, 8],
                version: 17,
            },
        ));
    }

    #[test]
    fn roundtrip_message_with_inherited_ttl() {
        roundtrip(Packet::Message(Message {
            id: "m1".into(),
            msg_type: "ping".into(),
            from: "pinger@a".into(),
            to: "responder@b".into(),
            payload: serde_json::to_vec(&serde_json::json!({"n": 42})).unwrap(),
            ttl: 7,
            timestamp: 1_700_000_000_000,
            direct: false,
            inherited_ttl: Some(6),
            trace_context: None,
            correlation_id: None,
        }));
    }

    /// **M16-Backend P10.** Wire-format round-trip with the new
    /// `trace_context` field populated. Validates the field
    /// serializes / deserializes cleanly through postcard.
    #[test]
    fn roundtrip_message_with_trace_context() {
        let mut tc = [0u8; 24];
        for (i, b) in tc.iter_mut().enumerate() {
            *b = u8::try_from(i).unwrap_or(0).wrapping_add(0x42);
        }
        roundtrip(Packet::Message(Message {
            id: "m2".into(),
            msg_type: "traced".into(),
            from: "pinger@a".into(),
            to: "responder@b".into(),
            payload: serde_json::to_vec(&serde_json::json!({"n": 1})).unwrap(),
            ttl: 4,
            timestamp: 1_700_000_000_000,
            direct: false,
            inherited_ttl: None,
            trace_context: Some(tc),
            correlation_id: None,
        }));
    }

    /// **M-Ingress phase 2.** Wire-format round-trip with the new
    /// `correlation_id` field populated. Validates the field
    /// serializes / deserializes cleanly through postcard alongside
    /// the existing v3-era fields.
    #[test]
    fn roundtrip_message_with_correlation_id() {
        roundtrip(Packet::Message(Message {
            id: "m3".into(),
            msg_type: "query_result".into(),
            from: "aggregator@a".into(),
            to: "__ingress__".into(),
            payload: serde_json::to_vec(&serde_json::json!({"answer": 42})).unwrap(),
            ttl: 8,
            timestamp: 1_700_000_000_000,
            direct: false,
            inherited_ttl: None,
            trace_context: None,
            correlation_id: Some("7b3f8a2e-9c1d-4f8b-a3e5-2b8a1f3c6d7e".into()),
        }));
    }

    #[test]
    fn roundtrip_disconnect_has_zero_payload() {
        let mut codec = WireFrame;
        let mut buf = BytesMut::new();
        codec.encode(Packet::Disconnect, &mut buf).expect("encode");
        // 1 byte type + 4 bytes len(=0) + 0 byte payload = 5 bytes total.
        assert_eq!(buf.len(), HEADER_BYTES);
        assert_eq!(buf[0], PacketType::Disconnect as u8);
        assert_eq!(&buf[1..5], &0u32.to_be_bytes());
        let decoded = codec.decode(&mut buf).expect("decode").expect("ready");
        assert!(matches!(decoded, Packet::Disconnect));
    }

    #[test]
    fn roundtrip_tick() {
        roundtrip(Packet::Tick(Tick {
            seq: 42,
            sent_at_ms: 1_700_000_000_000,
        }));
    }

    #[test]
    fn unknown_type_code_rejected() {
        let mut codec = WireFrame;
        let mut buf = BytesMut::new();
        buf.put_u8(0x42); // not a valid PacketType discriminant
        buf.put_u32(0);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, DistError::UnknownPacketType { code: 0x42 }));
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut codec = WireFrame;
        let mut buf = BytesMut::new();
        buf.put_u8(PacketType::Tick as u8);
        buf.put_u32(MAX_FRAME_BYTES + 1);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, DistError::FrameTooLarge { .. }));
    }

    /// Partial reads must leave the buffer state intact so the next
    /// decode call sees the same prefix.
    #[test]
    fn partial_frame_returns_none_without_consuming() {
        let mut codec = WireFrame;
        let packet = Packet::Hello(Hello {
            version: WIRE_VERSION,
            flags: 0,
            node_name: "alice@host".into(),
            creation: 1,
            capabilities: vec![],
        });
        let mut full = BytesMut::new();
        codec.encode(packet.clone(), &mut full).unwrap();

        // Feed bytes one-at-a-time. The first HEADER_BYTES-1 calls
        // must return Ok(None) and leave the buffer unchanged. The
        // call that completes the header but not the payload also
        // returns Ok(None). The final call returns Some(packet).
        let mut chunk = BytesMut::new();
        for (i, byte) in full.iter().enumerate() {
            chunk.put_u8(*byte);
            let was_full = i + 1 == full.len();
            let result = codec.decode(&mut chunk).unwrap();
            if was_full {
                assert_eq!(result, Some(packet.clone()));
                assert!(chunk.is_empty());
            } else {
                assert!(result.is_none(), "expected partial state at byte {i}");
            }
        }
    }

    /// Two back-to-back frames in one buffer must decode independently.
    #[test]
    fn two_concatenated_frames_decode_separately() {
        let mut codec = WireFrame;
        let mut buf = BytesMut::new();
        codec
            .encode(
                Packet::Tick(Tick {
                    seq: 1,
                    sent_at_ms: 1,
                }),
                &mut buf,
            )
            .unwrap();
        codec
            .encode(
                Packet::Exit(crate::wire::packet::Exit {
                    from: "a@b".into(),
                    reason: WireExitReason::NodeDown,
                }),
                &mut buf,
            )
            .unwrap();

        let first = codec.decode(&mut buf).unwrap().expect("first ready");
        let second = codec.decode(&mut buf).unwrap().expect("second ready");
        assert!(matches!(first, Packet::Tick(_)));
        assert!(matches!(second, Packet::Exit(_)));
        assert!(buf.is_empty());
    }

    #[test]
    fn min_supported_version_matches_wire_version() {
        // Equal today; widens on backwards-compatible extension.
        // Asserting here so a future bump that breaks the invariant
        // forces a deliberate code change.
        assert_eq!(MIN_SUPPORTED_VERSION, WIRE_VERSION);
    }
}
