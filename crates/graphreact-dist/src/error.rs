//! Failure taxonomy for the distribution layer.
//!
//! One variant per wire-protocol failure mode + transport / handshake
//! / routing failure modes added by later sub-phases. M14.1 only
//! populates the wire-protocol variants; M14.2+ extend the enum
//! without breaking existing matches (non-exhaustive over time).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DistError {
    /// An incoming frame's declared payload length exceeds
    /// [`crate::MAX_FRAME_BYTES`]. Refusing the frame prevents memory-
    /// amplification DoS via inflated `Len:u32` headers.
    #[error("dist: frame too large ({len} bytes; cap {cap})")]
    FrameTooLarge { len: u32, cap: u32 },

    /// Type code on the wire didn't match any known [`crate::PacketType`].
    /// Possible causes: protocol downgrade, version skew, corrupted
    /// stream, malicious peer probing the codec.
    #[error("dist: unknown packet type code {code:#04x}")]
    UnknownPacketType { code: u8 },

    /// postcard could not decode the packet payload. Wraps the
    /// upstream error for diagnostics.
    #[error("dist: postcard decode failure: {0}")]
    Codec(#[from] postcard::Error),

    /// Underlying I/O error (TCP / TLS read/write).
    #[error("dist: io error: {0}")]
    Io(#[from] std::io::Error),

    /// Peer announced a wire version below
    /// [`crate::MIN_SUPPORTED_VERSION`]. Refusing prevents downgrade
    /// attacks where a malicious node forces the cluster onto an
    /// older + less-secure protocol revision.
    #[error("dist: peer announced unsupported version {got} (minimum {min})")]
    UnsupportedVersion { got: u16, min: u16 },

    /// Connection closed mid-frame. Occurs when the peer drops or the
    /// network partitions before the full payload arrives.
    #[error("dist: connection closed mid-frame after {bytes_read} bytes (expected {expected})")]
    Truncated { bytes_read: usize, expected: u32 },

    /// TLS configuration or handshake failure. Wraps the rustls error
    /// kind as a string so the variant doesn't pull `rustls::Error`
    /// into downstream `DistError` matches (avoids version churn).
    #[error("dist: tls error: {0}")]
    Tls(String),

    /// Peer dropped the connection cleanly. Carries no payload —
    /// distinguishes "EOF mid-handshake" / "EOF mid-frame" from
    /// `Truncated` (which has bytes-read context).
    #[error("dist: peer disconnected")]
    PeerDisconnected,

    /// Cookie load / validation failure. Wraps a context string
    /// rather than a typed cause because cookie loading is platform-
    /// dependent (env-var vs file vs systemd secrets).
    #[error("dist: cookie error: {0}")]
    Cookie(String),

    /// Application-layer authentication failure during the OTP-style
    /// handshake — peer's HMAC digest didn't match what we computed
    /// against our cookie. Constant-time compare via
    /// `subtle::ConstantTimeEq`. Distinguished from [`Self::Tls`]:
    /// TLS auth is cluster-cert; this is cluster-cookie.
    #[error("dist: cookie authentication failed (cluster cookie mismatch)")]
    AuthFailed,

    /// Handshake protocol violation — wrong packet at the wrong
    /// stage of the state machine (e.g. expected `Hello`, got
    /// `Tick`), or the peer announced a name we couldn't parse.
    #[error("dist: handshake error: {0}")]
    Handshake(String),

    /// Client expected to dial peer X but the server identified as Y.
    /// Indicates either a misconfigured static discovery entry or an
    /// adversary attempting to impersonate a different node.
    #[error("dist: expected peer `{expected}`, got `{got}`")]
    UnexpectedPeer { expected: String, got: String },

    /// Two simultaneous attempts to register the same NodeId with
    /// the local registry. Higher layers (M14.4) resolve via lex-
    /// tiebreak; this variant is the signal.
    #[error("dist: duplicate peer registration: {node}")]
    DuplicatePeer { node: String },

    /// Discovery (static / DNS / custom) couldn't resolve a NodeId
    /// to a SocketAddr. Wraps a context string.
    #[error("dist: discovery error: {0}")]
    Discovery(String),

    /// A new inbound/outbound handshake was attempted after the
    /// cluster entered graceful drain. Existing peer connections
    /// remain alive until normal shutdown.
    #[error("dist: cluster is draining; new handshakes are not admitted")]
    Draining,

    /// JSON payload encode/decode error. Dist messages carry their
    /// app-level payload as a serde_json blob inside the postcard
    /// envelope; if the receiving end can't decode it, the wire
    /// shape is malformed.
    #[error("dist: payload error: {0}")]
    Payload(String),
}

impl From<rustls::Error> for DistError {
    fn from(e: rustls::Error) -> Self {
        Self::Tls(e.to_string())
    }
}
