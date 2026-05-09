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
}

/// Drive the server-side handshake. The connection must already be
/// post-TLS (i.e. produced by [`crate::Listener::accept`] or
/// [`crate::transport::accept_over`]). On success, the peer's
/// identity is verified at both the TLS cert layer and the cluster
/// cookie layer.
pub async fn server_handshake<S>(
    conn: &mut Connection<S>,
    cookie: &SecretCookie,
    our_name: &NodeId,
    our_creation: Creation,
    our_capabilities: Vec<String>,
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
    if hello.version < MIN_SUPPORTED_VERSION {
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
    let ack_digest = compute_digest(cookie, &reply.challenge);
    conn.send(Packet::ChallengeAck(ChallengeAck {
        digest: ack_digest,
        server_capabilities: our_capabilities,
    }))
    .await?;

    Ok(HandshakeResult {
        peer_node,
        peer_creation,
        peer_capabilities,
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
pub async fn client_handshake<S>(
    conn: &mut Connection<S>,
    cookie: &SecretCookie,
    our_name: &NodeId,
    our_creation: Creation,
    expected_peer: &NodeId,
    capabilities: Vec<String>,
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
    ) -> (Result<HandshakeResult, DistError>, Result<HandshakeResult, DistError>) {
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
