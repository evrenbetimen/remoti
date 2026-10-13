//! End-to-end session handshake.
//!
//! Key schedule (both peers compute it; the signaling server only relays):
//!
//! ```text
//! dh        = X25519(ephemeral_viewer, ephemeral_host)
//! pake      = SPAKE2(one_time_password, A = viewer_id, B = host_id)
//! transcript= SHA-256(len-prefixed: label, session, viewer, host, offer.spake,
//!                     offer.x25519, answer.spake, answer.x25519, answer.cert)
//! prk       = HKDF-Extract(salt = transcript, ikm = dh || pake)
//! k_v2h, k_h2v, c_viewer, c_host = HKDF-Expand(prk, label_i)
//! ```
//!
//! * Forward secrecy: both X25519 keys are ephemeral and dropped after use.
//! * Zero trust towards the server: an active MITM on signaling must also guess
//!   the out-of-band one-time password online (SPAKE2), one guess per attempt;
//!   swapping the QUIC certificate fingerprint changes the transcript and
//!   fails key confirmation.

use remoti_proto::PROTOCOL_VERSION;
use remoti_proto::bytes::Bytes;
use remoti_proto::peer::{Answer, Candidate, Capabilities, Offer, VideoCodec};
use remoti_proto::signaling::{DeviceId, SessionId};
use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, X25519};
use ring::digest::{self, SHA256};
use ring::hkdf::{self, HKDF_SHA256, KeyType};
use ring::hmac;
use ring::rand::SystemRandom;
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::crypto::{Direction, KEY_LEN, RecordOpener, RecordSealer};

const TRANSCRIPT_LABEL: &[u8] = b"remoti/handshake/v1";
const X25519_LEN: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("peer speaks protocol {0}, expected {PROTOCOL_VERSION}")]
    Version(u16),
    #[error("malformed handshake field: {0}")]
    Malformed(&'static str),
    #[error("password-authenticated key exchange failed")]
    Pake,
    #[error("key agreement failed")]
    Agreement,
    #[error("key confirmation failed (wrong password or tampered handshake)")]
    Confirmation,
    #[error("random number generator failure")]
    Rng,
    #[error(transparent)]
    Crypto(#[from] crate::crypto::CryptoError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Viewer,
    Host,
}

/// Material produced by a completed key exchange.
pub struct SessionSecrets {
    pub role: Role,
    pub transcript: [u8; 32],
    pub sealer: RecordSealer,
    pub opener: RecordOpener,
    confirm_local: hmac::Key,
    confirm_remote: hmac::Key,
}

impl SessionSecrets {
    /// Tag this side sends as its first record.
    pub fn local_confirmation(&self) -> Vec<u8> {
        hmac::sign(&self.confirm_local, &self.transcript).as_ref().to_vec()
    }

    /// Constant-time check of the peer's confirmation tag.
    pub fn verify_remote(&self, tag: &[u8]) -> Result<(), HandshakeError> {
        hmac::verify(&self.confirm_remote, &self.transcript, tag).map_err(|_| HandshakeError::Confirmation)
    }
}

impl std::fmt::Debug for SessionSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSecrets").field("role", &self.role).finish_non_exhaustive()
    }
}

/// Normalizes a user-typed password: case-insensitive, ignores spaces/dashes.
pub fn normalize_password(raw: &str) -> String {
    raw.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

/// Generates a one-time password (8 symbols, ~40 bits) from an alphabet without
/// look-alike characters. Online guessing is bounded by the host's lockout.
pub fn generate_password(rng: &SystemRandom) -> Result<String, HandshakeError> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut out = String::with_capacity(8);
    while out.len() < 8 {
        let mut b = [0u8; 16];
        ring::rand::SecureRandom::fill(rng, &mut b).map_err(|_| HandshakeError::Rng)?;
        for byte in b {
            // Rejection sampling keeps the distribution uniform.
            let limit = 256 - (256 % ALPHABET.len());
            if (byte as usize) < limit && out.len() < 8 {
                out.push(ALPHABET[byte as usize % ALPHABET.len()] as char);
            }
        }
    }
    Ok(out)
}

struct Transcript(digest::Context);

impl Transcript {
    fn new() -> Self {
        let mut t = Transcript(digest::Context::new(&SHA256));
        t.field(TRANSCRIPT_LABEL);
        t
    }

    fn field(&mut self, data: &[u8]) {
        self.0.update(&(data.len() as u32).to_be_bytes());
        self.0.update(data);
    }

    fn finish(self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(self.0.finish().as_ref());
        out
    }
}

struct Len(usize);
impl KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(prk: &hkdf::Prk, label: &'static [u8]) -> Result<[u8; KEY_LEN], HandshakeError> {
    let mut out = [0u8; KEY_LEN];
    let info = [label];
    prk.expand(&info, Len(KEY_LEN)).and_then(|okm| okm.fill(&mut out)).map_err(|_| HandshakeError::Agreement)?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn derive(
    role: Role,
    session: &SessionId,
    viewer: &DeviceId,
    host: &DeviceId,
    offer_spake: &[u8],
    offer_x: &[u8],
    answer_spake: &[u8],
    answer_x: &[u8],
    cert_sha256: &[u8],
    dh: &[u8],
    pake: &[u8],
) -> Result<SessionSecrets, HandshakeError> {
    let mut t = Transcript::new();
    for f in [
        session.as_str().as_bytes(),
        viewer.as_str().as_bytes(),
        host.as_str().as_bytes(),
        offer_spake,
        offer_x,
        answer_spake,
        answer_x,
        cert_sha256,
    ] {
        t.field(f);
    }
    let transcript = t.finish();

    let mut ikm = Vec::with_capacity(dh.len() + pake.len());
    ikm.extend_from_slice(dh);
    ikm.extend_from_slice(pake);
    let prk = hkdf::Salt::new(HKDF_SHA256, &transcript).extract(&ikm);
    // Best-effort wipe of the combined secret.
    ikm.iter_mut().for_each(|b| *b = 0);

    let k_v2h = expand(&prk, b"remoti key v2h")?;
    let k_h2v = expand(&prk, b"remoti key h2v")?;
    let c_viewer = hmac::Key::new(hmac::HMAC_SHA256, &expand(&prk, b"remoti confirm viewer")?);
    let c_host = hmac::Key::new(hmac::HMAC_SHA256, &expand(&prk, b"remoti confirm host")?);

    let (sealer, opener, confirm_local, confirm_remote) = match role {
        Role::Viewer => (
            RecordSealer::new(&k_v2h, Direction::ViewerToHost)?,
            RecordOpener::new(&k_h2v, Direction::HostToViewer)?,
            c_viewer,
            c_host,
        ),
        Role::Host => (
            RecordSealer::new(&k_h2v, Direction::HostToViewer)?,
            RecordOpener::new(&k_v2h, Direction::ViewerToHost)?,
            c_host,
            c_viewer,
        ),
    };
    Ok(SessionSecrets { role, transcript, sealer, opener, confirm_local, confirm_remote })
}

fn x25519_keypair(rng: &SystemRandom) -> Result<(EphemeralPrivateKey, Vec<u8>), HandshakeError> {
    let private = EphemeralPrivateKey::generate(&X25519, rng).map_err(|_| HandshakeError::Rng)?;
    let public = private.compute_public_key().map_err(|_| HandshakeError::Agreement)?;
    Ok((private, public.as_ref().to_vec()))
}

fn agree(private: EphemeralPrivateKey, peer_public: &[u8]) -> Result<Vec<u8>, HandshakeError> {
    if peer_public.len() != X25519_LEN {
        return Err(HandshakeError::Malformed("x25519 key length"));
    }
    agreement::agree_ephemeral(private, &UnparsedPublicKey::new(&X25519, peer_public), |s| s.to_vec())
        .map_err(|_| HandshakeError::Agreement)
}

/// Viewer side, created before sending the [`Offer`].
pub struct ViewerHandshake {
    session: SessionId,
    viewer: DeviceId,
    host: DeviceId,
    spake: Spake2<Ed25519Group>,
    spake_msg: Vec<u8>,
    x_private: EphemeralPrivateKey,
    x_public: Vec<u8>,
}

impl ViewerHandshake {
    pub fn start(
        rng: &SystemRandom,
        session: SessionId,
        viewer: DeviceId,
        host: DeviceId,
        password: &str,
        candidates: Vec<Candidate>,
        capabilities: Capabilities,
    ) -> Result<(Self, Offer), HandshakeError> {
        let pw = normalize_password(password);
        let (spake, spake_msg) = Spake2::<Ed25519Group>::start_a(
            &Password::new(pw.as_bytes()),
            &Identity::new(viewer.as_str().as_bytes()),
            &Identity::new(host.as_str().as_bytes()),
        );
        let (x_private, x_public) = x25519_keypair(rng)?;
        let offer = Offer {
            protocol: PROTOCOL_VERSION,
            viewer: viewer.clone(),
            spake: Bytes(spake_msg.clone()),
            x25519: Bytes(x_public.clone()),
            candidates,
            capabilities,
        };
        Ok((Self { session, viewer, host, spake, spake_msg, x_private, x_public }, offer))
    }

    pub fn finish(self, answer: &Answer) -> Result<SessionSecrets, HandshakeError> {
        if answer.protocol != PROTOCOL_VERSION {
            return Err(HandshakeError::Version(answer.protocol));
        }
        if answer.cert_sha256.len() != 32 {
            return Err(HandshakeError::Malformed("certificate fingerprint length"));
        }
        let pake = self.spake.finish(answer.spake.as_slice()).map_err(|_| HandshakeError::Pake)?;
        let dh = agree(self.x_private, answer.x25519.as_slice())?;
        derive(
            Role::Viewer,
            &self.session,
            &self.viewer,
            &self.host,
            &self.spake_msg,
            &self.x_public,
            answer.spake.as_slice(),
            answer.x25519.as_slice(),
            answer.cert_sha256.as_slice(),
            &dh,
            &pake,
        )
    }
}

/// Host side: consumes an [`Offer`] and produces the [`Answer`] plus secrets.
#[allow(clippy::too_many_arguments)]
pub fn host_respond(
    rng: &SystemRandom,
    session: &SessionId,
    host: &DeviceId,
    password: &str,
    offer: &Offer,
    candidates: Vec<Candidate>,
    cert_sha256: [u8; 32],
    codec: VideoCodec,
) -> Result<(Answer, SessionSecrets), HandshakeError> {
    if offer.protocol != PROTOCOL_VERSION {
        return Err(HandshakeError::Version(offer.protocol));
    }
    let pw = normalize_password(password);
    let (spake, spake_msg) = Spake2::<Ed25519Group>::start_b(
        &Password::new(pw.as_bytes()),
        &Identity::new(offer.viewer.as_str().as_bytes()),
        &Identity::new(host.as_str().as_bytes()),
    );
    let pake = spake.finish(offer.spake.as_slice()).map_err(|_| HandshakeError::Pake)?;
    let (x_private, x_public) = x25519_keypair(rng)?;
    let dh = agree(x_private, offer.x25519.as_slice())?;
    let secrets = derive(
        Role::Host,
        session,
        &offer.viewer,
        host,
        offer.spake.as_slice(),
        offer.x25519.as_slice(),
        &spake_msg,
        &x_public,
        &cert_sha256,
        &dh,
        &pake,
    )?;
    let answer = Answer {
        protocol: PROTOCOL_VERSION,
        spake: Bytes(spake_msg),
        x25519: Bytes(x_public),
        candidates,
        cert_sha256: Bytes(cert_sha256.to_vec()),
        codec,
    };
    Ok((answer, secrets))
}

#[cfg(test)]
mod tests {
    use super::*;
    use remoti_proto::wire::Channel;

    fn ids() -> (SessionId, DeviceId, DeviceId) {
        (
            SessionId::from_random([3; 16]),
            DeviceId::try_from("111222333".to_owned()).unwrap(),
            DeviceId::try_from("444555666".to_owned()).unwrap(),
        )
    }

    fn caps() -> Capabilities {
        Capabilities { codecs: vec![VideoCodec::H264], max_width: 3840, max_height: 2160, max_fps: 60 }
    }

    fn run(viewer_pw: &str, host_pw: &str) -> (SessionSecrets, SessionSecrets) {
        let rng = SystemRandom::new();
        let (s, v, h) = ids();
        let (vh, offer) = ViewerHandshake::start(&rng, s.clone(), v, h.clone(), viewer_pw, vec![], caps()).unwrap();
        let (answer, host) = host_respond(&rng, &s, &h, host_pw, &offer, vec![], [9; 32], VideoCodec::H264).unwrap();
        let viewer = vh.finish(&answer).unwrap();
        (viewer, host)
    }

    #[test]
    fn same_password_confirms_and_encrypts() {
        let (viewer, host) = run("abcd-2345", "ABCD2345");
        host.verify_remote(&viewer.local_confirmation()).unwrap();
        viewer.verify_remote(&host.local_confirmation()).unwrap();
        let rec = viewer.sealer.seal(Channel::Control, b"hello").unwrap();
        assert_eq!(host.opener.open(Channel::Control, rec).unwrap(), b"hello");
        let rec = host.sealer.seal(Channel::Video, b"frame").unwrap();
        assert_eq!(viewer.opener.open(Channel::Video, rec).unwrap(), b"frame");
    }

    #[test]
    fn wrong_password_fails_confirmation() {
        let (viewer, host) = run("WRONG234", "ABCD2345");
        assert!(host.verify_remote(&viewer.local_confirmation()).is_err());
        assert!(viewer.verify_remote(&host.local_confirmation()).is_err());
    }

    #[test]
    fn tampered_certificate_fingerprint_fails() {
        let rng = SystemRandom::new();
        let (s, v, h) = ids();
        let (vh, offer) = ViewerHandshake::start(&rng, s.clone(), v, h.clone(), "PW234567", vec![], caps()).unwrap();
        let (mut answer, host) =
            host_respond(&rng, &s, &h, "PW234567", &offer, vec![], [9; 32], VideoCodec::H264).unwrap();
        answer.cert_sha256 = Bytes(vec![1; 32]);
        let viewer = vh.finish(&answer).unwrap();
        assert!(viewer.verify_remote(&host.local_confirmation()).is_err());
    }

    #[test]
    fn password_alphabet_and_length() {
        let rng = SystemRandom::new();
        for _ in 0..50 {
            let p = generate_password(&rng).unwrap();
            assert_eq!(p.len(), 8);
            assert!(p.chars().all(|c| !"01OIL".contains(c)));
        }
    }
}
