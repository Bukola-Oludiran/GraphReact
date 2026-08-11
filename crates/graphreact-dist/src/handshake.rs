//! OTP-style cluster handshake.
//!
//! ## State machine
//!
//! Both sides drive a 4-packet exchange:
//!
//! ```text
//!  Client                                     Server
//!  ──────                                     ──────
//!     │                                          │
//!     │  ─── Hello ──────────────────────────▶  │  recv_hello
//!     │  (version, name, creation, caps)        │  validate_version, parse_name
//!     │                                          │
//!     │  ◀─── Challenge ─────────────────────   │  send_challenge
//!     │  (server_challenge, server_name, …)     │  (server_challenge = OsRng[32])
//!     │                                          │
//!     │  ─── ChallengeReply ────────────────▶   │  recv_reply
//!     │  (client_challenge, digest_a)            │  verify digest_a == HMAC(cookie, server_challenge)
//!     │  digest_a = HMAC(cookie, server_chal)    │  on mismatch → AuthFailed
//!     │                                          │
//!     │  ◀─── ChallengeAck ──────────────────   │  send_ack
//!     │  (digest_b)                              │  digest_b = HMAC(cookie, client_challenge)
//!     │                                          │
//!     │  verify digest_b == HMAC(cookie, …)      │
//!     │  on mismatch → AuthFailed                │
//!     │                                          │
//! ```
//!
//! On success, both sides return [`HandshakeResult`] with the peer's
//! `NodeId` + `Creation`. The connection is fully authenticated and
//! ready for application-level traffic.
//!
//! ## Cryptographic notes
//!
//! - HMAC-SHA256 (NIST SP 800-107). Modern primitive; replaces OTP's
//!   MD5 (which is obsolete; 20+ years of cryptanalysis demonstrate
//!   length-extension and collision attacks). The key is the cluster
//!   cookie (any non-empty byte sequence — `Hmac::new_from_slice`
//!   accepts arbitrary key lengths).
//! - Challenges are 32 cryptographic random bytes from `OsRng`. They
//!   are NEVER reused. Replay-resistance + birthday-collision-safety
//!   come from the 256-bit space.
//! - Digest comparisons go through [`subtle::ConstantTimeEq`] so a
//!   malicious peer can't time-channel-leak the cookie's prefix.
//!
//! ## Layered with TLS
//!
//! TLS (M14.2) authenticates the peer at the *certificate* layer; this
//! handshake authenticates at the *cluster cookie* layer. Both must
//! pass — owning a valid cluster cert isn't enough; the peer also has
//! to know the cluster cookie. This is the same defence-in-depth
//! shape OTP uses (TLS at the transport, cookie at the application).

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::node_id::{Creation, NodeId};
use crate::secret::SecretCookie;
use crate::transport::Connection;
use crate::wire::{
    Challenge, ChallengeAck, ChallengeReply, Hello, Packet, MIN_SUPPORTED_VERSION, WIRE_VERSION,
};
use crate::DistError;

type HmacSha256 = Hmac<Sha256>;

/// Outcome of a successful handshake. Returned by both sides so
/// the caller can register the peer in `NodeRegistry`.
#[derive(Debug, Clone)]
pub struct HandshakeResult {
    pub peer_node: NodeId,
    pub peer_creation: Creation,
    pub peer_capabilities: Vec<String>,
    /// **Phase β (sub-track #4).** Bitwise AND of our local
    /// substrate capability bitmap and the peer's, computed at
    /// handshake completion. v4 peers (which don't ship a bitmap
    /// on the wire) decode through the
    /// [`crate::wire::WireCodec`] fallback path with
    /// `capabilities_bitmap = 0`, so the AND with our local
    /// bitmap is also `0` — they advertise no substrate-bitmap
    /// capabilities, by design.
    ///
    /// Capability-gated dispatch (Phase γ +) consults this via
    /// [`crate::Cluster::peer_effective_capabilities`] to decide
    /// whether to ship feature-specific packets to a given peer.
    pub effective_capabilities: u64,
}

/// Drive the server-side handshake. The connection must already be
/// post-TLS (i.e. produced by [`crate::Listener::accept`] or
/// [`crate::transport::accept_over`]). On success, the peer's
/// identity is verified at both the TLS cert layer and the cluster
/// cookie layer.
///
/// **Phase β (sub-track #4):** also accepts/advertises a per-engine
/// capability bitmap. The peer's bitmap is bitwise-ANDed with our
/// local bitmap to produce
/// [`HandshakeResult::effective_capabilities`]. v4 peers that don't
/// ship a bitmap on the wire decode through the codec's fallback
/// path with `capabilities_bitmap = 0`, so the AND with our local
/// bitmap is also `0` (they advertise no substrate-bitmap
/// capabilities, by design).
///
/// Audit events are emitted on the `graphreact::observer::audit`
/// target on both rejection paths (`cluster_handshake_rejected`,
/// reason ∈ `{wire_version_below_floor, wire_version_above_ceiling}`)
/// and on success (`cluster_handshake_complete`).
#[allow(clippy::too_many_arguments)]
pub async fn server_handshake<S>(
    conn: &mut Connection<S>,
    cookie: &SecretCookie,
    our_name: &NodeId,
    our_creation: Creation,
    our_capabilities: Vec<String>,
    our_capabilities_bitmap: u64,
) -> Result<HandshakeResult, DistError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1. Recv Hello — peer announces itself.
    let hello = match conn.recv().await? {
        Some(Packet::Hello(h)) => h,
        Some(other) => {
            return Err(DistError::Handshake(format!(
                "expected Hello, got {:?}",
                other.packet_type()
            )))
        }
        None => return Err(DistError::PeerDisconnected),
    };
    // Phase β: reject above-ceiling FIRST — future peers we don't
    // understand are caught here. The ceiling is exclusive
    // (WIRE_VERSION is the current build's version; anything
    // higher is unknown). v4 peers (below WIRE_VERSION but at or
    // above MIN_SUPPORTED_VERSION) fall through this block.
    if hello.version > WIRE_VERSION {
        tracing::warn!(
            target: "graphreact::observer::audit",
            event = "cluster_handshake_rejected",
            peer_wire_version = hello.version,
            local_wire_version = WIRE_VERSION,
            reason = "wire_version_above_ceiling",
            "cluster handshake rejected: peer version above local ceiling"
        );
        return Err(DistError::UnsupportedVersion {
            got: hello.version,
            // The error variant uses `min` for both bounds in
            // Phase β; see DistError doc for the dual-use note.
            min: MIN_SUPPORTED_VERSION,
        });
    }
    if hello.version < MIN_SUPPORTED_VERSION {
        tracing::warn!(
            target: "graphreact::observer::audit",
            event = "cluster_handshake_rejected",
            peer_wire_version = hello.version,
            local_wire_version = WIRE_VERSION,
            reason = "wire_version_below_floor",
            "cluster handshake rejected: peer version below local floor"
        );
        return Err(DistError::UnsupportedVersion {
            got: hello.version,
            min: MIN_SUPPORTED_VERSION,
        });
    }
    let peer_node: NodeId = hello.node_name.parse().map_err(|e| {
        DistError::Handshake(format!("invalid peer node name `{}`: {e}", hello.node_name))
    })?;
    let peer_creation = Creation(hello.creation);
    let peer_capabilities = hello.capabilities;
    // Phase β: capture peer's substrate-bitmap and compute the
    // effective (negotiated) set as the bitwise AND with ours.
    let peer_bitmap = hello.capabilities_bitmap;
    let effective_capabilities = our_capabilities_bitmap & peer_bitmap;
    let peer_wire_version = hello.version;

    // 2. Send Challenge — random nonce + our identity.
    let server_challenge = generate_challenge();
    conn.send(Packet::Challenge(Challenge {
        challenge: server_challenge,
        server_name: our_name.to_string(),
        creation: our_creation.0,
    }))
    .await?;

    // 3. Recv ChallengeReply — peer's digest + their own challenge.
    let reply = match conn.recv().await? {
        Some(Packet::ChallengeReply(r)) => r,
        Some(other) => {
            return Err(DistError::Handshake(format!(
                "expected ChallengeReply, got {:?}",
                other.packet_type()
            )))
        }
        None => return Err(DistError::PeerDisconnected),
    };

    // Verify peer proved knowledge of the cookie.
    let expected = compute_digest(cookie, &server_challenge);
    if !constant_time_digest_eq(&reply.digest, &expected) {
        return Err(DistError::AuthFailed);
    }

    // 4. Send ChallengeAck — proof that WE know the cookie.
    //    Wire v2 (M15 Phase 10): also echo our capabilities so the
    //    client side learns them — closes the M14.3 substrate gap
    //    where client-side `client_handshake` returned an empty
    //    `peer_capabilities`.
    //    Wire v5 (Phase β sub-track #4): also echo our substrate-
    //    bitmap so the client side can compute its own effective
    //    set symmetrically.
    let ack_digest = compute_digest(cookie, &reply.challenge);
    conn.send(Packet::ChallengeAck(ChallengeAck {
        digest: ack_digest,
        server_capabilities: our_capabilities,
        capabilities_bitmap: our_capabilities_bitmap,
    }))
    .await?;

    // Phase β: success audit event. `peer_wire_version` is the
    // value from the peer's Hello — authoritative on the server
    // path. `negotiated_capabilities` is the bitwise AND.
    tracing::info!(
        target: "graphreact::observer::audit",
        event = "cluster_handshake_complete",
        peer_id = %peer_node,
        peer_wire_version,
        local_wire_version = WIRE_VERSION,
        negotiated_capabilities = effective_capabilities,
        "cluster handshake complete"
    );

    Ok(HandshakeResult {
        peer_node,
        peer_creation,
        peer_capabilities,
        effective_capabilities,
    })
}

/// Drive the client-side handshake. The connection must already be
/// post-TLS (i.e. produced by [`crate::transport::dial`] or
/// [`crate::transport::dial_over`]).
///
/// `expected_peer` is the NodeId we *expect* to authenticate as the
/// remote side. If the peer's `Challenge.server_name` parses to a
/// different NodeId, the handshake fails with
/// [`DistError::UnexpectedPeer`] — protects against misconfigured
/// discovery entries and impersonation attempts.
#[allow(clippy::too_many_arguments)]
pub async fn client_handshake<S>(
    conn: &mut Connection<S>,
    cookie: &SecretCookie,
    our_name: &NodeId,
    our_creation: Creation,
    expected_peer: &NodeId,
    capabilities: Vec<String>,
    our_capabilities_bitmap: u64,
) -> Result<HandshakeResult, DistError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1. Send Hello.
    conn.send(Packet::Hello(Hello {
        version: WIRE_VERSION,
        flags: 0,
        node_name: our_name.to_string(),
        creation: our_creation.0,
        capabilities: capabilities.clone(),
        capabilities_bitmap: our_capabilities_bitmap,
    }))
    .await?;

    // 2. Recv Challenge.
    let challenge = match conn.recv().await? {
        Some(Packet::Challenge(c)) => c,
        Some(other) => {
            return Err(DistError::Handshake(format!(
                "expected Challenge, got {:?}",
                other.packet_type()
            )))
        }
        None => return Err(DistError::PeerDisconnected),
    };
    let server_node: NodeId = challenge.server_name.parse().map_err(|e| {
        DistError::Handshake(format!(
            "invalid server name `{}`: {e}",
            challenge.server_name
        ))
    })?;
    if &server_node != expected_peer {
        return Err(DistError::UnexpectedPeer {
            expected: expected_peer.to_string(),
            got: server_node.to_string(),
        });
    }

    // 3. Send ChallengeReply.
    let client_challenge = generate_challenge();
    let reply_digest = compute_digest(cookie, &challenge.challenge);
    conn.send(Packet::ChallengeReply(ChallengeReply {
        challenge: client_challenge,
        digest: reply_digest,
    }))
    .await?;

    // 4. Recv ChallengeAck — verify server proved knowledge of cookie.
    let ack = match conn.recv().await? {
        Some(Packet::ChallengeAck(a)) => a,
        Some(other) => {
            return Err(DistError::Handshake(format!(
                "expected ChallengeAck, got {:?}",
                other.packet_type()
            )))
        }
        None => return Err(DistError::PeerDisconnected),
    };
    let expected = compute_digest(cookie, &client_challenge);
    if !constant_time_digest_eq(&ack.digest, &expected) {
        return Err(DistError::AuthFailed);
    }

    // Phase β: capture peer's substrate-bitmap (v4 peers decode
    // with bitmap = 0 via the codec fallback path) and compute
    // the effective set.
    let peer_bitmap = ack.capabilities_bitmap;
    let effective_capabilities = our_capabilities_bitmap & peer_bitmap;

    // Phase β: success audit event. NOTE — on the client path the
    // peer's `Hello`-version is NOT delivered (the `Challenge`
    // packet doesn't carry a version field today). We populate
    // `peer_wire_version` with our LOCAL WIRE_VERSION as a
    // best-effort tag: if the handshake reached this point the
    // peer is AT LEAST at our floor (else server-side reject
    // would've fired) AND AT MOST at our ceiling (else the
    // peer's parsing of our Hello would've gone sideways before
    // we got the Challenge). Phase γ may extend the
    // `Challenge` packet shape to carry the peer's version
    // explicitly if precise peer-version tracking becomes
    // load-bearing.
    tracing::info!(
        target: "graphreact::observer::audit",
        event = "cluster_handshake_complete",
        peer_id = %server_node,
        peer_wire_version = WIRE_VERSION,
        local_wire_version = WIRE_VERSION,
        negotiated_capabilities = effective_capabilities,
        "cluster handshake complete"
    );

    Ok(HandshakeResult {
        peer_node: server_node,
        peer_creation: Creation(challenge.creation),
        // Wire v2 (M15 Phase 10): the server's capabilities ride
        // back on `ChallengeAck.server_capabilities`. Pre-v2 this
        // field didn't exist and we returned an empty Vec; that
        // substrate gap blocked client-side capability filtering
        // for cluster-wide hot reload + any future capability-
        // gated dispatch.
        peer_capabilities: ack.server_capabilities,
        effective_capabilities,
    })
}

/// HMAC-SHA256(cookie, challenge). Output is a fixed 32-byte digest.
fn compute_digest(cookie: &SecretCookie, challenge: &[u8; 32]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(cookie.as_bytes())
        .expect("HMAC-SHA256 accepts arbitrary key lengths");
    mac.update(challenge);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Constant-time digest comparison. Returns `true` iff `a == b`,
/// without short-circuiting on the first mismatching byte.
fn constant_time_digest_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.ct_eq(b).into()
}

/// Fill a 32-byte buffer from `OsRng`. Each call produces an
/// independent challenge; never reuse across handshakes.
fn generate_challenge() -> [u8; 32] {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

/// OTP-style lex-tiebreak for simultaneous reciprocal connects.
/// When both sides dial each other concurrently, they end up with
/// two TCP connections to the same NodeId. Convention: the
/// lexicographically smaller name's outbound connection wins. The
/// other side detects the in-flight outbound and tears it down.
///
/// Returns `true` if WE (`our_name`) should keep our outbound
/// connection to `peer_name` and the peer should drop theirs.
#[must_use]
pub fn should_keep_outbound(our_name: &NodeId, peer_name: &NodeId) -> bool {
    our_name.as_str() < peer_name.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev_tls;
    use crate::transport::{accept_over, dial_over};

    fn cookie(s: &str) -> SecretCookie {
        SecretCookie::from_bytes(s.as_bytes().to_vec()).unwrap()
    }

    /// Run a full TLS + handshake exchange over `tokio::io::duplex`,
    /// returning each side's result. The server is alice@127.0.0.1
    /// and the client is bob@127.0.0.1; cert SANs are "alice"/"bob"
    /// per `dev_tls::pair`.
    ///
    /// `client_expected_peer_arg` is passed verbatim to
    /// `client_handshake`'s `expected_peer` parameter so callers can
    /// exercise both the matching path and the
    /// `UnexpectedPeer`-rejection path.
    async fn run_pair(
        server_cookie: SecretCookie,
        client_cookie: SecretCookie,
        client_expected_peer_arg: &str,
    ) -> (
        Result<HandshakeResult, DistError>,
        Result<HandshakeResult, DistError>,
    ) {
        let (alice, bob) = dev_tls::pair("alice", "bob").expect("pair");
        let (alice_stream, bob_stream) = tokio::io::duplex(64 * 1024);
        let alice_cfg = alice.config.clone();
        let bob_cfg = bob.config.clone();

        let alice_node: NodeId = "alice@127.0.0.1".parse().unwrap();
        let bob_node: NodeId = "bob@127.0.0.1".parse().unwrap();
        let expected_peer: NodeId = client_expected_peer_arg.parse().unwrap();

        let server_task = tokio::spawn(async move {
            let mut conn = accept_over(alice_stream, &alice_cfg).await?;
            let result = server_handshake(
                &mut conn,
                &server_cookie,
                &alice_node,
                Creation(1),
                vec!["mon-v1".into(), "reload-v1".into()],
                0,
            )
            .await;
            let _ = conn.close().await;
            result
        });

        let client_task = tokio::spawn(async move {
            let mut conn = dial_over(bob_stream, "alice", &bob_cfg).await?;
            let result = client_handshake(
                &mut conn,
                &client_cookie,
                &bob_node,
                Creation(2),
                &expected_peer,
                vec!["pg-v1".into()],
                0,
            )
            .await;
            let _ = conn.close().await;
            result
        });

        let server_result = server_task.await.expect("server join");
        let client_result = client_task.await.expect("client join");
        (server_result, client_result)
    }

    #[tokio::test]
    async fn shared_cookie_completes_handshake() {
        let (s, c) = run_pair(cookie("shared"), cookie("shared"), "alice@127.0.0.1").await;
        let server = s.expect("server handshake");
        let client = c.expect("client handshake");
        assert_eq!(server.peer_node.to_string(), "bob@127.0.0.1");
        assert_eq!(client.peer_node.to_string(), "alice@127.0.0.1");
        assert_eq!(server.peer_creation.0, 2);
        assert_eq!(client.peer_creation.0, 1);
        assert_eq!(server.peer_capabilities, vec!["pg-v1".to_string()]);
        // Wire v2 (M15 Phase 10): client also learns server caps
        // via the widened ChallengeAck. Pre-v2 this was empty.
        assert_eq!(
            client.peer_capabilities,
            vec!["mon-v1".to_string(), "reload-v1".to_string()]
        );
    }

    #[tokio::test]
    async fn mismatched_cookies_fail_authentication() {
        let (s, c) = run_pair(cookie("alpha"), cookie("beta"), "alice@127.0.0.1").await;
        // The server detects the mismatch first (it verifies the
        // client's reply digest before sending its own ack). The
        // client then sees the connection close and reports
        // either PeerDisconnected or AuthFailed (depending on
        // whether the close arrives before/after the ack-recv path
        // peeks). Both error shapes are acceptable; what matters is
        // that NEITHER side reports success.
        assert!(s.is_err(), "server should fail: got {s:?}");
        assert!(c.is_err(), "client should fail: got {c:?}");
        match s {
            Err(DistError::AuthFailed) => {}
            other => panic!("expected server AuthFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn client_rejects_unexpected_peer_name() {
        // The server identifies as "alice@127.0.0.1" in its
        // Challenge.server_name; the client passes
        // "carol@127.0.0.1" as expected_peer → UnexpectedPeer.
        let (_s, c) = run_pair(cookie("c"), cookie("c"), "carol@127.0.0.1").await;
        match c {
            Err(DistError::UnexpectedPeer { expected, got }) => {
                assert_eq!(expected, "carol@127.0.0.1");
                assert_eq!(got, "alice@127.0.0.1");
            }
            other => panic!("expected UnexpectedPeer, got {other:?}"),
        }
    }

    #[test]
    fn lex_tiebreak_orders_outbound_for_smaller_name() {
        let alice: NodeId = "alice@host".parse().unwrap();
        let bob: NodeId = "bob@host".parse().unwrap();
        assert!(should_keep_outbound(&alice, &bob));
        assert!(!should_keep_outbound(&bob, &alice));
    }

    #[test]
    fn digest_constant_time_eq_matches_naive_eq_on_random_inputs() {
        let mut rng = rand::thread_rng();
        for _ in 0..16 {
            let mut a = [0u8; 32];
            let mut b = [0u8; 32];
            rng.fill_bytes(&mut a);
            // Half the time set b == a, half random.
            if rand::Rng::r#gen::<bool>(&mut rng) {
                b = a;
            } else {
                rng.fill_bytes(&mut b);
            }
            assert_eq!(constant_time_digest_eq(&a, &b), a == b);
        }
    }

    #[test]
    fn challenge_outputs_differ_across_calls() {
        // Probability of two 32-byte OsRng outputs colliding is 2^-256;
        // a single inequality is enough to validate the function isn't
        // returning a constant.
        let a = generate_challenge();
        let b = generate_challenge();
        assert_ne!(a, b);
    }
}

/// **Phase β (sub-track #4).** Rolling-version handshake tests.
///
/// Lives inside `src/handshake.rs` rather than an external
/// `tests/` directory because it needs the `pub(crate) dev_tls`
/// helpers (gated behind `#[cfg(test)]`) and the v4-payload
/// raw-byte injection helper below.
#[cfg(test)]
mod rolling_version_tests {
    use super::*;
    use crate::dev_tls;
    use crate::transport::{accept_over, dial_over};
    use crate::wire::packet::{ChallengeReply, HelloV4, PacketType};
    use crate::wire::{
        CAP_AUDIT_CHAIN, CAP_DELEGATED_RESTART, CAP_MIXED_RUNTIME, MIN_SUPPORTED_VERSION,
        WIRE_VERSION,
    };

    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::codec::Decoder as _;
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    fn cookie_str(s: &str) -> SecretCookie {
        SecretCookie::from_bytes(s.as_bytes().to_vec()).unwrap()
    }

    /// Build a v4-shape Hello frame (no `capabilities_bitmap`)
    /// using the [`HelloV4`] private struct. Returns the
    /// `[Type:u8][Len:u32 BE][Payload]` byte vector ready to be
    /// shipped via a raw `AsyncWrite::write_all`.
    ///
    /// Used for above-ceiling rejection (the codec doesn't care
    /// which shape sent a too-high version) and the legacy v4
    /// fallback test name. Phase γ floor-tests build the modern
    /// (v5+) shape via [`build_v5_hello_frame`].
    fn build_v4_hello_frame(version: u16, node_name: &str, creation: u32) -> Vec<u8> {
        let payload = postcard::to_stdvec(&HelloV4 {
            version,
            flags: 0,
            node_name: node_name.to_string(),
            creation,
            capabilities: vec!["pg-v1".to_string()],
        })
        .expect("encode HelloV4");
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(PacketType::Hello as u8);
        let len_u32: u32 = u32::try_from(payload.len()).unwrap();
        frame.extend_from_slice(&len_u32.to_be_bytes());
        frame.extend_from_slice(&payload);
        frame
    }

    /// **Phase γ.** Build a v5-shape Hello frame (with
    /// `capabilities_bitmap`). Used by the floor-peer tests now
    /// that the v6 build's floor is `MIN_SUPPORTED_VERSION = 5`.
    fn build_v5_hello_frame(
        version: u16,
        node_name: &str,
        creation: u32,
        capabilities_bitmap: u64,
    ) -> Vec<u8> {
        let payload = postcard::to_stdvec(&Hello {
            version,
            flags: 0,
            node_name: node_name.to_string(),
            creation,
            capabilities: vec!["pg-v1".to_string()],
            capabilities_bitmap,
        })
        .expect("encode Hello");
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(PacketType::Hello as u8);
        let len_u32: u32 = u32::try_from(payload.len()).unwrap();
        frame.extend_from_slice(&len_u32.to_be_bytes());
        frame.extend_from_slice(&payload);
        frame
    }

    /// Build a real v5 ChallengeReply frame so the simulated v4
    /// client can complete step 3 of the handshake after the
    /// server's Challenge arrives.
    fn build_challenge_reply_frame(
        cookie: &SecretCookie,
        server_challenge: &[u8; 32],
        client_challenge: &[u8; 32],
    ) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(cookie.as_bytes())
            .expect("HMAC-SHA256 accepts arbitrary key lengths");
        mac.update(server_challenge);
        let result = mac.finalize().into_bytes();
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&result);
        let payload = postcard::to_stdvec(&ChallengeReply {
            challenge: *client_challenge,
            digest,
        })
        .expect("encode ChallengeReply");
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(PacketType::ChallengeReply as u8);
        let len_u32: u32 = u32::try_from(payload.len()).unwrap();
        frame.extend_from_slice(&len_u32.to_be_bytes());
        frame.extend_from_slice(&payload);
        frame
    }

    /// Capture audit-target tracing events into a shared buffer
    /// (one JSON line per event with target/level/event plus the
    /// emitted field key/values). Configured into the subscriber
    /// via [`make_audit_subscriber`].
    #[derive(Clone, Default)]
    struct AuditCapture(Arc<Mutex<Vec<String>>>);

    impl AuditCapture {
        fn new() -> Self {
            Self::default()
        }
        fn lines(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    /// Field-value visitor for [`AuditLayer`] — collects every
    /// (name, formatted-value) pair for serialisation.
    #[derive(Default)]
    struct AuditFieldVisitor {
        fields: Vec<(String, String)>,
    }
    impl tracing::field::Visit for AuditFieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.fields
                .push((field.name().to_string(), format!("{value:?}")));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }

    /// Minimal `tracing-subscriber::Layer` that JSON-serialises
    /// every event whose target is `graphreact::observer::audit`.
    /// Self-contained so this test module doesn't add a
    /// graphreact-observer dev-dep cycle.
    struct AuditLayer {
        buf: AuditCapture,
    }
    impl<S> tracing_subscriber::Layer<S> for AuditLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            use std::fmt::Write as _;
            let meta = event.metadata();
            if meta.target() != "graphreact::observer::audit" {
                return;
            }
            let mut visitor = AuditFieldVisitor::default();
            event.record(&mut visitor);
            let level = meta.level().to_string();
            let mut json = format!("{{\"target\":\"{}\",\"level\":\"{level}\"", meta.target());
            for (k, v) in &visitor.fields {
                // Escape just the inner quotes — values from tracing
                // come pre-formatted; the goal here is "captureable
                // by string-contains" not strict JSON.
                let v_esc = v.replace('"', "\\\"");
                let _ = write!(json, ",\"{k}\":\"{v_esc}\"");
            }
            json.push('}');
            self.buf.0.lock().unwrap().push(json);
        }
    }

    fn make_audit_subscriber(
        buf: &AuditCapture,
    ) -> impl tracing::Subscriber + Send + Sync + 'static {
        Registry::default().with(AuditLayer { buf: buf.clone() })
    }

    /// Test 1: a floor peer (one bump below WIRE_VERSION) handshakes
    /// successfully. Phase γ floor is `MIN_SUPPORTED_VERSION = 5`,
    /// so the test ships a v5-shape Hello with
    /// `capabilities_bitmap = 0`. The server's negotiated
    /// `effective_capabilities` is `0 & our_bitmap = 0`. The
    /// `cluster_handshake_complete` audit row carries
    /// `peer_wire_version = MIN_SUPPORTED_VERSION = 5`,
    /// `local_wire_version = WIRE_VERSION = 6`,
    /// `negotiated_capabilities = 0`.
    #[tokio::test]
    async fn floor_minus_one_peer_handshakes_with_zero_bitmap() {
        assert_eq!(MIN_SUPPORTED_VERSION, WIRE_VERSION - 1);

        let (alice, bob) = dev_tls::pair("alice", "bob").expect("pair");
        let (alice_stream, bob_stream) = tokio::io::duplex(64 * 1024);
        let alice_cfg = alice.config.clone();
        let bob_cfg = bob.config.clone();
        let alice_node: NodeId = "alice@127.0.0.1".parse().unwrap();
        let server_cookie = cookie_str("shared");
        let client_cookie = cookie_str("shared");

        // Server runs the real `server_handshake`. Wrap the future
        // with our test-local audit subscriber so the audit events
        // emitted inside the spawn-task end up in our buffer.
        let buf = AuditCapture::new();
        let server_subscriber = make_audit_subscriber(&buf);
        let our_bitmap_server: u64 = CAP_AUDIT_CHAIN;
        let server_task = tokio::spawn(
            async move {
                let mut conn = accept_over(alice_stream, &alice_cfg).await?;
                let result = server_handshake(
                    &mut conn,
                    &server_cookie,
                    &alice_node,
                    Creation(1),
                    vec!["mon-v1".into()],
                    our_bitmap_server,
                )
                .await;
                let _ = conn.close().await;
                result
            }
            .with_subscriber(server_subscriber),
        );

        // Client manually completes TLS via dial_over → Connection,
        // then we drop into raw-byte mode to ship a v4-shape Hello
        // and ride the rest of the handshake. We want to drive the
        // server's audit path without depending on a v4 daemon
        // binary.
        let client_task = tokio::spawn(async move {
            // Drive TLS handshake via the normal client path, then
            // tear back down to the raw framed stream beneath.
            let conn = dial_over(bob_stream, "alice", &bob_cfg).await?;
            // Build a v4-shape Hello and ship it via the framed
            // codec's raw byte path. We can't go through
            // `conn.send` because `Packet::Hello` is the v5 shape;
            // we need the v4 trailing-u64-absent shape. Reach into
            // the framed stream's `get_mut` to grab the raw stream
            // and write the bytes.
            // Phase γ: floor is now MIN_SUPPORTED_VERSION=5, which is
            // a v5-shape Hello (carries capabilities_bitmap).
            let raw_frame = build_v5_hello_frame(MIN_SUPPORTED_VERSION, "bob@127.0.0.1", 2, 0);
            // `Connection` does not expose its inner stream. We
            // work around by piggy-backing on `WireFrame`'s codec:
            // we can write the bytes via `framed.get_mut()` once
            // they expose it. Since they don't, we use a separate
            // raw write path — re-acquiring the inner stream via
            // `Framed::into_parts`.
            let parts = conn.into_parts();
            let (mut tls_stream, _codec, mut read_buf, mut write_buf) =
                (parts.io, parts.codec, parts.read_buf, parts.write_buf);
            // Drain any pending write_buf bytes that the codec may
            // have left there (none expected this early in the
            // handshake, but it costs nothing to flush).
            if !write_buf.is_empty() {
                tls_stream
                    .write_all(&write_buf)
                    .await
                    .map_err(DistError::from)?;
                write_buf.clear();
            }
            tls_stream
                .write_all(&raw_frame)
                .await
                .map_err(DistError::from)?;
            tls_stream.flush().await.map_err(DistError::from)?;

            // Receive Challenge from server via raw read +
            // codec-shaped parse. We read into `read_buf` until a
            // full frame is parseable.
            let mut codec = crate::wire::WireFrame;
            let challenge = loop {
                if let Some(packet) = codec.decode(&mut read_buf)? {
                    break match packet {
                        Packet::Challenge(c) => c,
                        other => {
                            return Err(DistError::Handshake(format!(
                                "expected Challenge, got {:?}",
                                other.packet_type()
                            )));
                        }
                    };
                }
                let mut tmp = [0u8; 4096];
                let n = tls_stream.read(&mut tmp).await.map_err(DistError::from)?;
                if n == 0 {
                    return Err(DistError::PeerDisconnected);
                }
                read_buf.extend_from_slice(&tmp[..n]);
            };

            // Ship a ChallengeReply with the right HMAC over the
            // server's challenge.
            let client_challenge = generate_challenge();
            let reply_frame = build_challenge_reply_frame(
                &client_cookie,
                &challenge.challenge,
                &client_challenge,
            );
            tls_stream
                .write_all(&reply_frame)
                .await
                .map_err(DistError::from)?;
            tls_stream.flush().await.map_err(DistError::from)?;

            // Receive ChallengeAck (we don't need to verify it for
            // the server-side audit path test).
            loop {
                if let Some(_packet) = codec.decode(&mut read_buf)? {
                    break;
                }
                let mut tmp = [0u8; 4096];
                let n = tls_stream.read(&mut tmp).await.map_err(DistError::from)?;
                if n == 0 {
                    return Err(DistError::PeerDisconnected);
                }
                read_buf.extend_from_slice(&tmp[..n]);
            }
            Ok::<(), DistError>(())
        });

        let server_result = server_task.await.expect("server join");
        let _ = client_task.await.expect("client join");

        let result = server_result.expect("server handshake");
        // v4 peer decoded via fallback → bitmap = 0 → AND with our
        // bitmap = 0.
        assert_eq!(result.effective_capabilities, 0);
        // String-vec capability still arrives untouched.
        assert_eq!(result.peer_capabilities, vec!["pg-v1".to_string()]);
        assert_eq!(result.peer_node.to_string(), "bob@127.0.0.1");

        // Audit-row inspection — find the `cluster_handshake_complete`
        // line and assert the version fields.
        let lines = buf.lines();
        let complete = lines
            .iter()
            .find(|l| l.contains("cluster_handshake_complete"))
            .expect("cluster_handshake_complete audit row");
        assert!(
            complete.contains(&format!(
                "\"peer_wire_version\":\"{MIN_SUPPORTED_VERSION}\""
            )),
            "expected peer_wire_version={MIN_SUPPORTED_VERSION} in {complete}"
        );
        assert!(
            complete.contains(&format!("\"local_wire_version\":\"{WIRE_VERSION}\"")),
            "expected local_wire_version={WIRE_VERSION} in {complete}"
        );
        assert!(
            complete.contains("\"negotiated_capabilities\":\"0\""),
            "expected negotiated_capabilities=0 in {complete}"
        );
    }

    /// Test 2: a v3 peer (two bumps below `MIN_SUPPORTED_VERSION`,
    /// three bumps below `WIRE_VERSION` post-γ) is REJECTED at the
    /// floor with audit event `cluster_handshake_rejected`,
    /// `reason="wire_version_below_floor"`. Under v6 floor=5 the
    /// reject is unambiguous.
    #[tokio::test]
    async fn floor_minus_two_peer_rejected_with_below_floor_audit() {
        let (alice, bob) = dev_tls::pair("alice", "bob").expect("pair");
        let (alice_stream, bob_stream) = tokio::io::duplex(64 * 1024);
        let alice_cfg = alice.config.clone();
        let bob_cfg = bob.config.clone();
        let alice_node: NodeId = "alice@127.0.0.1".parse().unwrap();
        let server_cookie = cookie_str("shared");

        let buf = AuditCapture::new();
        let server_subscriber = make_audit_subscriber(&buf);
        let server_task = tokio::spawn(
            async move {
                let mut conn = accept_over(alice_stream, &alice_cfg).await?;
                let result = server_handshake(
                    &mut conn,
                    &server_cookie,
                    &alice_node,
                    Creation(1),
                    vec!["mon-v1".into()],
                    0,
                )
                .await;
                let _ = conn.close().await;
                result
            }
            .with_subscriber(server_subscriber),
        );

        let client_task = tokio::spawn(async move {
            let conn = dial_over(bob_stream, "alice", &bob_cfg).await?;
            // v3 — three bumps below the current WIRE_VERSION = 6,
            // unambiguously below MIN_SUPPORTED_VERSION = 5.
            let frame = build_v4_hello_frame(3, "bob@127.0.0.1", 1);
            let parts = conn.into_parts();
            let mut tls_stream = parts.io;
            let mut write_buf = parts.write_buf;
            if !write_buf.is_empty() {
                tls_stream
                    .write_all(&write_buf)
                    .await
                    .map_err(DistError::from)?;
                write_buf.clear();
            }
            tls_stream
                .write_all(&frame)
                .await
                .map_err(DistError::from)?;
            tls_stream.flush().await.map_err(DistError::from)?;
            // Drain the close signal.
            let mut tmp = [0u8; 4096];
            let _ = tls_stream.read(&mut tmp).await;
            Ok::<(), DistError>(())
        });

        let server_result = server_task.await.expect("server join");
        let _ = client_task.await.expect("client join");

        match server_result {
            Err(DistError::UnsupportedVersion { got, min }) => {
                assert_eq!(got, 3);
                assert_eq!(min, MIN_SUPPORTED_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }

        let lines = buf.lines();
        let rejected = lines
            .iter()
            .find(|l| l.contains("cluster_handshake_rejected"))
            .expect("cluster_handshake_rejected audit row");
        assert!(
            rejected.contains("\"peer_wire_version\":\"3\""),
            "expected peer_wire_version=3 in {rejected}"
        );
        assert!(
            rejected.contains(&format!("\"local_wire_version\":\"{WIRE_VERSION}\"")),
            "expected local_wire_version={WIRE_VERSION} in {rejected}"
        );
        assert!(
            rejected.contains("wire_version_below_floor"),
            "expected reason=wire_version_below_floor in {rejected}"
        );
    }

    /// Test 4 (verify-finding patch): above-ceiling peer rejection
    /// fires `wire_version_above_ceiling` audit row. Hand-encodes a
    /// HelloV4-shape payload with `version = WIRE_VERSION + 1` (= 7
    /// under v6). Postcard silently ignores trailing bytes when
    /// present, so the v4 shape is sufficient — the server's
    /// strict ceiling check fires on `hello.version > WIRE_VERSION`
    /// BEFORE any floor logic. Symmetric to
    /// `floor_minus_two_peer_rejected` but on the OTHER side of the
    /// wire-version window.
    #[tokio::test]
    async fn above_ceiling_peer_rejected_with_above_ceiling_audit() {
        let (alice, bob) = dev_tls::pair("alice", "bob").expect("pair");
        let (alice_stream, bob_stream) = tokio::io::duplex(64 * 1024);
        let alice_cfg = alice.config.clone();
        let bob_cfg = bob.config.clone();
        let alice_node: NodeId = "alice@127.0.0.1".parse().unwrap();
        let server_cookie = cookie_str("shared");

        let buf = AuditCapture::new();
        let server_subscriber = make_audit_subscriber(&buf);
        let server_task = tokio::spawn(
            async move {
                let mut conn = accept_over(alice_stream, &alice_cfg).await?;
                let result = server_handshake(
                    &mut conn,
                    &server_cookie,
                    &alice_node,
                    Creation(1),
                    vec!["mon-v1".into()],
                    0,
                )
                .await;
                let _ = conn.close().await;
                result
            }
            .with_subscriber(server_subscriber),
        );

        let client_task = tokio::spawn(async move {
            let conn = dial_over(bob_stream, "alice", &bob_cfg).await?;
            // version = WIRE_VERSION + 1 — one bump above the
            // current ceiling (= 7 under v6). v4-shape encode is
            // fine: postcard silently drops trailing bytes when
            // the wire shape differs, and the server's ceiling
            // check fires on the version field alone.
            let frame = build_v4_hello_frame(WIRE_VERSION + 1, "bob@127.0.0.1", 1);
            let parts = conn.into_parts();
            let mut tls_stream = parts.io;
            let mut write_buf = parts.write_buf;
            if !write_buf.is_empty() {
                tls_stream
                    .write_all(&write_buf)
                    .await
                    .map_err(DistError::from)?;
                write_buf.clear();
            }
            tls_stream
                .write_all(&frame)
                .await
                .map_err(DistError::from)?;
            tls_stream.flush().await.map_err(DistError::from)?;
            // Drain the close signal.
            let mut tmp = [0u8; 4096];
            let _ = tls_stream.read(&mut tmp).await;
            Ok::<(), DistError>(())
        });

        let server_result = server_task.await.expect("server join");
        let _ = client_task.await.expect("client join");

        match server_result {
            Err(DistError::UnsupportedVersion { got, .. }) => {
                assert_eq!(got, WIRE_VERSION + 1);
            }
            other => panic!("expected UnsupportedVersion (above-ceiling), got {other:?}"),
        }

        let lines = buf.lines();
        let rejected = lines
            .iter()
            .find(|l| l.contains("cluster_handshake_rejected"))
            .expect("cluster_handshake_rejected audit row");
        assert!(
            rejected.contains(&format!("\"peer_wire_version\":\"{}\"", WIRE_VERSION + 1)),
            "expected peer_wire_version={} in {rejected}",
            WIRE_VERSION + 1
        );
        assert!(
            rejected.contains(&format!("\"local_wire_version\":\"{WIRE_VERSION}\"")),
            "expected local_wire_version={WIRE_VERSION} in {rejected}"
        );
        assert!(
            rejected.contains("wire_version_above_ceiling"),
            "expected reason=wire_version_above_ceiling in {rejected}"
        );
    }

    /// Test 3: two same-version peers exchange bitmaps; the
    /// negotiated set is the bitwise AND. Server advertises
    /// `CAP_DELEGATED_RESTART | CAP_AUDIT_CHAIN` and client
    /// advertises `CAP_AUDIT_CHAIN | CAP_MIXED_RUNTIME`; the
    /// intersection is `CAP_AUDIT_CHAIN`. (Originally named for
    /// v5; the test is version-agnostic and continues to work at
    /// v6.)
    #[tokio::test]
    async fn v5_peer_with_bitmap_intersects_correctly() {
        let (alice, bob) = dev_tls::pair("alice", "bob").expect("pair");
        let (alice_stream, bob_stream) = tokio::io::duplex(64 * 1024);
        let alice_cfg = alice.config.clone();
        let bob_cfg = bob.config.clone();
        let alice_node: NodeId = "alice@127.0.0.1".parse().unwrap();
        let bob_node: NodeId = "bob@127.0.0.1".parse().unwrap();
        let cookie = cookie_str("shared");

        let server_bitmap = CAP_DELEGATED_RESTART | CAP_AUDIT_CHAIN;
        let client_bitmap = CAP_AUDIT_CHAIN | CAP_MIXED_RUNTIME;
        let expected_effective = CAP_AUDIT_CHAIN;

        let s_cookie = cookie.clone();
        let server_task = tokio::spawn(async move {
            let mut conn = accept_over(alice_stream, &alice_cfg).await?;
            let result = server_handshake(
                &mut conn,
                &s_cookie,
                &alice_node,
                Creation(1),
                vec!["mon-v1".into()],
                server_bitmap,
            )
            .await;
            let _ = conn.close().await;
            result
        });
        let c_cookie = cookie;
        let alice_expected_id: NodeId = "alice@127.0.0.1".parse().unwrap();
        let client_task = tokio::spawn(async move {
            let mut conn = dial_over(bob_stream, "alice", &bob_cfg).await?;
            let result = client_handshake(
                &mut conn,
                &c_cookie,
                &bob_node,
                Creation(2),
                &alice_expected_id,
                vec!["pg-v1".into()],
                client_bitmap,
            )
            .await;
            let _ = conn.close().await;
            result
        });

        let server = server_task
            .await
            .expect("server join")
            .expect("server handshake");
        let client = client_task
            .await
            .expect("client join")
            .expect("client handshake");

        assert_eq!(server.effective_capabilities, expected_effective);
        assert_eq!(client.effective_capabilities, expected_effective);
    }
}
