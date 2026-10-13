//! End-to-end handshake payloads. These travel inside
//! [`crate::signaling::ClientMessage::Signal`] as opaque bytes; the signaling
//! server forwards them without being able to influence the derived keys
//! (see `core_engine::handshake` for the key schedule).

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::bytes::Bytes;
use crate::signaling::DeviceId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CandidateKind {
    /// Address of a local interface.
    Host,
    /// Public address observed by a STUN server.
    ServerReflexive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub addr: SocketAddr,
    pub kind: CandidateKind,
}

/// Serialized as `h264` / `h265` / `av1` in JSON (UI); variant index in postcard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub codecs: Vec<VideoCodec>,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u16,
}

/// Viewer -> host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Offer {
    pub protocol: u16,
    pub viewer: DeviceId,
    /// SPAKE2 message (side A) bound to the host's one-time password.
    pub spake: Bytes,
    /// Ephemeral X25519 public key.
    pub x25519: Bytes,
    /// Ephemeral ML-KEM-768 encapsulation key (FIPS 203) for the hybrid,
    /// post-quantum half of the key exchange.
    pub mlkem_ek: Bytes,
    pub candidates: Vec<Candidate>,
    pub capabilities: Capabilities,
}

/// Host -> viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Answer {
    pub protocol: u16,
    /// SPAKE2 message (side B).
    pub spake: Bytes,
    /// Ephemeral X25519 public key.
    pub x25519: Bytes,
    /// ML-KEM-768 ciphertext encapsulated to the viewer's `mlkem_ek`.
    pub mlkem_ct: Bytes,
    pub candidates: Vec<Candidate>,
    /// SHA-256 of the host's ephemeral QUIC certificate (DER). Bound into the
    /// handshake transcript, so a forged certificate fails key confirmation.
    pub cert_sha256: Bytes,
    pub codec: VideoCodec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    Busy,
    Declined,
    VersionMismatch,
    RateLimited,
    /// Key confirmation failed: wrong one-time password.
    AuthFailed,
    /// The host cannot encode any codec the viewer can decode.
    NoCommonCodec,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PeerSignal {
    Offer(Offer),
    Answer(Answer),
    Reject(RejectReason),
    /// Ends the session (either side).
    Bye,
}
