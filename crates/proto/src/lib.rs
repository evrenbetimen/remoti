//! Protocol definitions shared by the Remoti agent (`core_engine`) and the
//! signaling service.
//!
//! Three layers live here:
//! * [`signaling`]: JSON messages exchanged with the signaling server. The server
//!   only routes these; it never sees media or input.
//! * [`peer`]: end-to-end handshake payloads carried *opaquely* inside
//!   signaling messages (the server forwards bytes it cannot interpret).
//! * [`wire`]: messages exchanged directly between peers inside AES-256-GCM
//!   records once the session is established.

pub mod bytes;
pub mod input;
pub mod peer;
pub mod signaling;
pub mod stun;
pub mod wire;

/// Protocol revision. Peers refuse to talk to a different major revision.
pub const PROTOCOL_VERSION: u16 = 2;

/// Hard cap for any single encoded message (video access units included).
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Hard cap for signaling frames; signaling never carries bulk data.
pub const MAX_SIGNALING_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("message exceeds {limit} bytes (got {size})")]
    TooLarge { size: usize, limit: usize },
    #[error("binary decode failed: {0}")]
    Binary(#[from] postcard::Error),
    #[error("json decode failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// Encodes a value with postcard, enforcing [`MAX_MESSAGE_BYTES`].
pub fn encode_binary<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let out = postcard::to_stdvec(value)?;
    if out.len() > MAX_MESSAGE_BYTES {
        return Err(CodecError::TooLarge { size: out.len(), limit: MAX_MESSAGE_BYTES });
    }
    Ok(out)
}

/// Decodes a postcard value, rejecting oversized input before parsing.
pub fn decode_binary<T: serde::de::DeserializeOwned>(buf: &[u8]) -> Result<T, CodecError> {
    if buf.len() > MAX_MESSAGE_BYTES {
        return Err(CodecError::TooLarge { size: buf.len(), limit: MAX_MESSAGE_BYTES });
    }
    Ok(postcard::from_bytes(buf)?)
}
