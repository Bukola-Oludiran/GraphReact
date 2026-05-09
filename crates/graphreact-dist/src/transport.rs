//! TLS transport — `Listener` (server side) + `Connection`
//! (post-handshake, generic over the underlying byte stream).
//!
//! Generic-over-stream is the key shape: production code uses
//! `TlsStream<TcpStream>`; tests use `TlsStream<DuplexStream>` so
//! the entire transport including TLS handshake can be exercised
//! without binding ports. Same `Connection` type, same code paths.
//!
//! Frame I/O lives at [`crate::wire::WireFrame`]; this module just
//! handles the TLS handshake + the `(Listener, Connection)` lifecycle.
//!
//! ## Authentication boundary
//!
//! The TLS layer authenticates the peer via cert chain (see
//! [`crate::TlsConfig`]). The dist crate's handshake state machine
//! (M14.3) layers an OTP-style cookie challenge-response on top to
//! verify cluster membership at the application layer — one node's
//! valid cluster cert isn't enough; it also has to know the cluster
//! cookie. M14.2 just terminates TLS and exposes a packet-shaped
//! `Connection`.

use std::net::SocketAddr;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::server::TlsStream as ServerTlsStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_util::codec::Framed;

use crate::wire::{Packet, WireFrame};
use crate::{DistError, TlsConfig};

/// Side annotation — preserved on every `Connection` so higher
/// layers (M14.3 handshake) can branch on whether they're driving
/// the server-side or client-side state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionSide {
    Server,
    Client,
}

/// Post-handshake TLS connection wrapping a [`WireFrame`] codec.
///
/// Generic over `S: AsyncRead + AsyncWrite + Unpin` so the same type
/// covers `TlsStream<TcpStream>` (production) and
/// `TlsStream<DuplexStream>` (in-memory tests) uniformly.
pub struct Connection<S> {
    framed: Framed<S, WireFrame>,
    side: ConnectionSide,
}

impl<S> Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(stream: S, side: ConnectionSide) -> Self {
        Self {
            framed: Framed::new(stream, WireFrame),
            side,
        }
    }

    pub fn side(&self) -> ConnectionSide {
        self.side
    }

    /// Send a single packet. Returns when the underlying writer
    /// has accepted the bytes (does NOT wait for delivery).
    pub async fn send(&mut self, packet: Packet) -> Result<(), DistError> {
        self.framed.send(packet).await
    }

    /// Receive the next packet. Returns:
    /// - `Ok(Some(packet))` on a complete frame.
    /// - `Ok(None)` on graceful EOF (peer closed connection cleanly
    ///   between frames).
    /// - `Err(DistError)` on malformed / oversized / decoding /
    ///   I/O failure.
    pub async fn recv(&mut self) -> Result<Option<Packet>, DistError> {
        self.framed.next().await.transpose()
    }

    /// Send a `Disconnect` packet and close the underlying stream.
    /// Higher layers should prefer this over a raw drop so the peer
    /// can distinguish "gone away cleanly" from "crashed" on its
    /// side.
    pub async fn close(mut self) -> Result<(), DistError> {
        let _ = self.framed.send(Packet::Disconnect).await;
        self.framed.close().await
    }
}

/// Server-side listener — wraps `tokio::net::TcpListener` + a
/// `TlsAcceptor` cloned from a [`TlsConfig`]. Each `accept()`
/// terminates one TLS handshake and returns a [`Connection`].
pub struct Listener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    addr: SocketAddr,
}

impl Listener {
    pub async fn bind(addr: SocketAddr, tls: TlsConfig) -> Result<Self, DistError> {
        let tcp = TcpListener::bind(addr).await?;
        let actual = tcp.local_addr()?;
        let acceptor = TlsAcceptor::from(tls.server);
        Ok(Self {
            tcp,
            acceptor,
            addr: actual,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Accept one inbound connection, drive its TLS handshake to
    /// completion, and return the wrapped [`Connection`]. Errors
    /// out on TCP accept failure, TLS handshake failure, or peer
    /// authentication failure (mTLS-required cert verification).
    pub async fn accept(&self) -> Result<Connection<ServerTlsStream<TcpStream>>, DistError> {
        let (tcp, _peer_addr) = self.tcp.accept().await?;
        let tls = self.acceptor.accept(tcp).await?;
        Ok(Connection::new(tls, ConnectionSide::Server))
    }
}

/// Client-side dialler. Issues an outbound TCP connect + drives the
/// TLS handshake against the supplied [`TlsConfig`]'s client config.
/// `expected_peer_dns` is the SAN/CN the server must present in its
/// cert (cluster-cert mode: this is the peer's NodeId name).
pub async fn dial(
    addr: SocketAddr,
    expected_peer_dns: &str,
    tls: &TlsConfig,
) -> Result<Connection<ClientTlsStream<TcpStream>>, DistError> {
    let connector = TlsConnector::from(tls.client.clone());
    let server_name: ServerName<'static> = ServerName::try_from(expected_peer_dns.to_string())
        .map_err(|e| DistError::Tls(format!("invalid server name `{expected_peer_dns}`: {e}")))?;
    let tcp = TcpStream::connect(addr).await?;
    let tls = connector.connect(server_name, tcp).await?;
    Ok(Connection::new(tls, ConnectionSide::Client))
}

/// Generic dial over an arbitrary `AsyncRead + AsyncWrite + Unpin`
/// stream — used by tests against `tokio::io::DuplexStream` to
/// exercise the TLS layer without binding TCP.
pub async fn dial_over<S>(
    stream: S,
    expected_peer_dns: &str,
    tls: &TlsConfig,
) -> Result<Connection<ClientTlsStream<S>>, DistError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let connector = TlsConnector::from(tls.client.clone());
    let server_name: ServerName<'static> = ServerName::try_from(expected_peer_dns.to_string())
        .map_err(|e| DistError::Tls(format!("invalid server name `{expected_peer_dns}`: {e}")))?;
    let tls = connector.connect(server_name, stream).await?;
    Ok(Connection::new(tls, ConnectionSide::Client))
}

/// Generic accept over an arbitrary `AsyncRead + AsyncWrite + Unpin`
/// stream — symmetric counterpart to [`dial_over`].
pub async fn accept_over<S>(
    stream: S,
    tls: &TlsConfig,
) -> Result<Connection<ServerTlsStream<S>>, DistError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let acceptor = TlsAcceptor::from(Arc::clone(&tls.server));
    let tls_stream = acceptor.accept(stream).await?;
    Ok(Connection::new(tls_stream, ConnectionSide::Server))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev_tls;
    use crate::wire::{Packet, Tick};

    /// Round-trip over `tokio::io::duplex` — exercises the full
    /// TLS handshake + framed codec path without binding TCP.
    #[tokio::test]
    async fn handshake_and_roundtrip_over_duplex() {
        let (alice, bob) = dev_tls::pair("alice", "bob").expect("pair");
        let (alice_stream, bob_stream) = tokio::io::duplex(64 * 1024);

        let alice_cfg = alice.config.clone();
        let bob_cfg = bob.config.clone();

        let server = tokio::spawn(async move {
            let mut conn = accept_over(alice_stream, &alice_cfg).await.expect("accept");
            // Echo whatever bob sends.
            let pkt = conn.recv().await.expect("recv ok").expect("not eof");
            conn.send(pkt).await.expect("send echo");
            conn.close().await.expect("close");
        });

        let client = tokio::spawn(async move {
            let mut conn = dial_over(bob_stream, "alice", &bob_cfg).await.expect("dial");
            conn.send(Packet::Tick(Tick { seq: 42, sent_at_ms: 0 }))
                .await
                .expect("send");
            let echo = conn.recv().await.expect("recv ok").expect("not eof");
            assert!(matches!(echo, Packet::Tick(Tick { seq: 42, .. })));
            // Wait for the server's Disconnect; the recv will yield
            // None on graceful close.
            assert!(conn.recv().await.expect("recv disconnect").is_some());
        });

        server.await.expect("server task");
        client.await.expect("client task");
    }

    /// Server cert signed by CA1; client trusts CA2. Handshake must
    /// fail with a TLS error.
    #[tokio::test]
    async fn cert_mismatch_rejected() {
        let ca_a = dev_tls::make_ca().expect("ca_a");
        let ca_b = dev_tls::make_ca().expect("ca_b");
        let alice = dev_tls::issue(&ca_a, "alice", true).expect("alice");
        let bob = dev_tls::issue(&ca_b, "bob", true).expect("bob");

        let (alice_stream, bob_stream) = tokio::io::duplex(64 * 1024);
        let alice_cfg = alice.config.clone();
        let bob_cfg = bob.config.clone();

        let server = tokio::spawn(async move { accept_over(alice_stream, &alice_cfg).await });
        let client = tokio::spawn(async move { dial_over(bob_stream, "alice", &bob_cfg).await });

        let server_res = server.await.expect("server task");
        let client_res = client.await.expect("client task");
        // At least one side must reject the handshake. Typically the
        // client errors on server-cert validation and the server
        // errors on client-cert validation (mTLS) — both are TLS
        // failures, not protocol failures.
        assert!(
            server_res.is_err() || client_res.is_err(),
            "cert-mismatch handshake unexpectedly succeeded"
        );
    }

    /// Server cert has SAN `alice`; client connects expecting `bob`.
    /// Handshake must fail with a name-verification error.
    #[tokio::test]
    async fn unexpected_peer_name_rejected() {
        let (alice, bob) = dev_tls::pair("alice", "bob").expect("pair");
        let (alice_stream, bob_stream) = tokio::io::duplex(64 * 1024);
        let alice_cfg = alice.config.clone();
        let bob_cfg = bob.config.clone();

        let server = tokio::spawn(async move { accept_over(alice_stream, &alice_cfg).await });
        let client = tokio::spawn(
            async move { dial_over(bob_stream, "carol-impostor", &bob_cfg).await },
        );

        let server_res = server.await.expect("server task");
        let client_res = client.await.expect("client task");
        assert!(
            server_res.is_err() || client_res.is_err(),
            "name-mismatch handshake unexpectedly succeeded"
        );
    }

    /// Drop the client AFTER handshake; server's recv must return
    /// either EOF (`Ok(None)`) or an I/O error rather than panicking.
    /// rustls 0.23 reports abrupt drops without `close_notify` as
    /// `UnexpectedEof` by default — both Ok(None) and Err shapes
    /// are acceptable here; what matters is that the server doesn't
    /// hang forever or crash.
    #[tokio::test]
    async fn server_recv_terminates_on_client_drop() {
        let (alice, bob) = dev_tls::pair("alice", "bob").expect("pair");
        let (alice_stream, bob_stream) = tokio::io::duplex(64 * 1024);
        let alice_cfg = alice.config.clone();
        let bob_cfg = bob.config.clone();

        // Run both handshakes concurrently — neither side completes
        // until the other does, so we can't drop the client until
        // BOTH are post-handshake.
        let server_handle = tokio::spawn(async move {
            accept_over(alice_stream, &alice_cfg).await
        });
        let client_handle = tokio::spawn(async move {
            dial_over(bob_stream, "alice", &bob_cfg).await
        });

        let server_conn = server_handle.await.expect("server join").expect("accept");
        let client_conn = client_handle.await.expect("client join").expect("dial");

        // Now drop the client; the underlying duplex pipe closes,
        // and the server's TLS stream surfaces EOF on its next read.
        drop(client_conn);

        let mut server_conn = server_conn;
        // `Ok(None)` (graceful EOF) and `Err(...)` (abrupt close
        // without close_notify, rustls's strict-mode default) are
        // both acceptable terminations. The only failure shape is
        // a phantom packet arriving after the client dropped.
        match server_conn.recv().await {
            Ok(None) | Err(_) => {}
            Ok(Some(p)) => panic!("server received packet after client drop: {p:?}"),
        }
    }
}
