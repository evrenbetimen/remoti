//! Messages between an agent and the signaling service (JSON over WebSocket).
//!
//! The service authenticates devices by Ed25519 proof-of-possession and then
//! routes opaque [`Bytes`] payloads between them. It never learns session keys:
//! the payloads are the end-to-end handshake defined in [`crate::peer`].

use serde::{Deserialize, Serialize};

use crate::bytes::Bytes;

/// Domain-separation prefix for the device authentication signature.
pub const AUTH_CONTEXT: &[u8] = b"remoti/signal-auth/v1";

/// Number of decimal digits in a device id.
pub const DEVICE_ID_DIGITS: usize = 9;

/// Public, human-friendly device identifier (9 digits, TeamViewer style),
/// derived from the SHA-256 digest of the device's Ed25519 public key so it
/// cannot be claimed without the private key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DeviceId(String);

impl DeviceId {
    /// Derives the id from `sha256(public_key)`.
    pub fn from_key_digest(digest: &[u8; 32]) -> Self {
        let mut n = [0u8; 8];
        n.copy_from_slice(&digest[..8]);
        let v = u64::from_be_bytes(n) % 10u64.pow(DEVICE_ID_DIGITS as u32);
        // Never start with 0 so ids keep their length when typed as numbers.
        let v = if v < 10u64.pow(DEVICE_ID_DIGITS as u32 - 1) { v + 10u64.pow(DEVICE_ID_DIGITS as u32 - 1) } else { v };
        DeviceId(v.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for DeviceId {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let digits: String = value.chars().filter(|c| !c.is_whitespace()).collect();
        if digits.len() == DEVICE_ID_DIGITS && digits.bytes().all(|b| b.is_ascii_digit()) {
            Ok(DeviceId(digits))
        } else {
            Err(format!("device id must be {DEVICE_ID_DIGITS} digits"))
        }
    }
}

impl From<DeviceId> for String {
    fn from(value: DeviceId) -> Self {
        value.0
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Random 128-bit session identifier, lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(String);

impl SessionId {
    pub fn from_random(bytes: [u8; 16]) -> Self {
        let mut s = String::with_capacity(32);
        for b in bytes {
            s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
            s.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
        }
        SessionId(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SessionId {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() == 32 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(SessionId(value))
        } else {
            Err("session id must be 32 lowercase hex characters".to_owned())
        }
    }
}

impl From<SessionId> for String {
    fn from(value: SessionId) -> Self {
        value.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ClientMessage {
    /// First message: announces the device's Ed25519 public key.
    Hello {
        public_key: Bytes,
        agent_version: String,
    },
    /// Signature over `AUTH_CONTEXT || nonce` proving key possession.
    Prove {
        signature: Bytes,
    },
    /// Opaque end-to-end payload to route to another device.
    Signal {
        to: DeviceId,
        session_id: SessionId,
        payload: Bytes,
    },
    /// Requests a blind TCP relay for when UDP is blocked between the peers.
    AllocateRelay {
        session_id: SessionId,
        peer: DeviceId,
    },
    Ping {
        nonce: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ServerMessage {
    Challenge {
        nonce: Bytes,
    },
    Welcome {
        device_id: DeviceId,
        server_time_ms: u64,
        stun: Vec<String>,
    },
    Signal {
        from: DeviceId,
        session_id: SessionId,
        payload: Bytes,
    },
    /// Both peers receive the same `url`; each gets its own single-use token.
    RelayAllocated {
        session_id: SessionId,
        url: String,
        token: String,
    },
    PeerUnavailable {
        session_id: SessionId,
        peer: DeviceId,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
    Pong {
        nonce: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    Unauthenticated,
    RateLimited,
    PayloadTooLarge,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_nine_digits() {
        for seed in 0u8..=255 {
            let id = DeviceId::from_key_digest(&[seed; 32]);
            assert_eq!(id.as_str().len(), DEVICE_ID_DIGITS);
            assert!(!id.as_str().starts_with('0'));
        }
    }

    #[test]
    fn device_id_rejects_garbage() {
        assert!(DeviceId::try_from("12345".to_owned()).is_err());
        assert!(DeviceId::try_from("12345678a".to_owned()).is_err());
        assert_eq!(DeviceId::try_from("123 456 789".to_owned()).unwrap().as_str(), "123456789");
    }

    #[test]
    fn signaling_json_shape() {
        let m = ClientMessage::Ping { nonce: 7 };
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"type":"ping","nonce":7}"#);
        let s: ServerMessage = serde_json::from_str(r#"{"type":"pong","nonce":7}"#).unwrap();
        assert!(matches!(s, ServerMessage::Pong { nonce: 7 }));
    }
}
