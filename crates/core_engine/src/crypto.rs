//! Record layer: AES-256-GCM with explicit 64-bit sequence numbers and a
//! sliding replay window.
//!
//! Every peer-to-peer message is sealed here before reaching QUIC or the TCP
//! fallback, so confidentiality never depends on the transport (or on the relay
//! that may forward TCP fallback traffic).
//!
//! Record layout: `seq: u64 BE || ciphertext || tag(16)`.
//! Nonce: `direction(4) || seq(8)`. AAD: `channel(1) || seq(8)`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use remoti_proto::wire::Channel;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};

pub const KEY_LEN: usize = 32;
pub const TAG_LEN: usize = 16;
pub const SEQ_LEN: usize = 8;
pub const RECORD_OVERHEAD: usize = SEQ_LEN + TAG_LEN;

/// Records older than this many sequence numbers are rejected outright. Video
/// frames travel on independent QUIC streams and may arrive out of order.
const REPLAY_WINDOW: u64 = 1024;
const WINDOW_WORDS: usize = (REPLAY_WINDOW / 64) as usize;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("invalid key material")]
    InvalidKey,
    #[error("record too short")]
    Truncated,
    #[error("authentication failed")]
    BadRecord,
    #[error("replayed or stale record (seq {0})")]
    Replay(u64),
    #[error("sequence space exhausted; session must be re-keyed")]
    SequenceExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ViewerToHost,
    HostToViewer,
}

impl Direction {
    fn prefix(self) -> [u8; 4] {
        match self {
            Direction::ViewerToHost => *b"v2h\0",
            Direction::HostToViewer => *b"h2v\0",
        }
    }
}

fn nonce_for(direction: Direction, seq: u64) -> Nonce {
    let mut n = [0u8; NONCE_LEN];
    n[..4].copy_from_slice(&direction.prefix());
    n[4..].copy_from_slice(&seq.to_be_bytes());
    Nonce::assume_unique_for_key(n)
}

fn aad_for(channel: Channel, seq: u64) -> [u8; 9] {
    let mut a = [0u8; 9];
    a[0] = channel as u8;
    a[1..].copy_from_slice(&seq.to_be_bytes());
    a
}

/// Encrypts outbound records. Cheap to share behind an `Arc`.
pub struct RecordSealer {
    key: LessSafeKey,
    direction: Direction,
    next_seq: AtomicU64,
}

impl RecordSealer {
    pub fn new(key: &[u8; KEY_LEN], direction: Direction) -> Result<Self, CryptoError> {
        let unbound = UnboundKey::new(&AES_256_GCM, key).map_err(|_| CryptoError::InvalidKey)?;
        Ok(Self { key: LessSafeKey::new(unbound), direction, next_seq: AtomicU64::new(0) })
    }

    pub fn seal(&self, channel: Channel, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        if seq == u64::MAX {
            return Err(CryptoError::SequenceExhausted);
        }
        let mut out = Vec::with_capacity(RECORD_OVERHEAD + plaintext.len());
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(plaintext);
        let tag = self
            .key
            .seal_in_place_separate_tag(
                nonce_for(self.direction, seq),
                Aad::from(aad_for(channel, seq)),
                &mut out[SEQ_LEN..],
            )
            .map_err(|_| CryptoError::BadRecord)?;
        out.extend_from_slice(tag.as_ref());
        Ok(out)
    }
}

/// Sliding-window replay protection (RFC 4303 style, 1024 entries).
struct ReplayWindow {
    highest: Option<u64>,
    bits: [u64; WINDOW_WORDS],
}

impl ReplayWindow {
    fn new() -> Self {
        Self { highest: None, bits: [0; WINDOW_WORDS] }
    }

    fn bit(&self, offset: u64) -> bool {
        let (w, b) = ((offset / 64) as usize, offset % 64);
        self.bits[w] & (1 << b) != 0
    }

    fn set(&mut self, offset: u64) {
        let (w, b) = ((offset / 64) as usize, offset % 64);
        self.bits[w] |= 1 << b;
    }

    /// Returns true if `seq` is acceptable (not yet authenticated though).
    fn check(&self, seq: u64) -> bool {
        match self.highest {
            None => true,
            Some(h) if seq > h => true,
            Some(h) => {
                let offset = h - seq;
                offset < REPLAY_WINDOW && !self.bit(offset)
            }
        }
    }

    /// Records `seq` as seen. Only call after successful authentication.
    fn commit(&mut self, seq: u64) {
        match self.highest {
            None => {
                self.highest = Some(seq);
                self.set(0);
            }
            Some(h) if seq > h => {
                let shift = seq - h;
                if shift >= REPLAY_WINDOW {
                    self.bits = [0; WINDOW_WORDS];
                } else {
                    self.shift_left(shift);
                }
                self.highest = Some(seq);
                self.set(0);
            }
            Some(h) => self.set(h - seq),
        }
    }

    /// Moves every bit `n` positions towards older offsets.
    fn shift_left(&mut self, n: u64) {
        let words = (n / 64) as usize;
        let bits = (n % 64) as u32;
        if words > 0 {
            for i in (0..WINDOW_WORDS).rev() {
                self.bits[i] = if i >= words { self.bits[i - words] } else { 0 };
            }
        }
        if bits > 0 {
            for i in (0..WINDOW_WORDS).rev() {
                let carry = if i > 0 { self.bits[i - 1] >> (64 - bits) } else { 0 };
                self.bits[i] = (self.bits[i] << bits) | carry;
            }
        }
    }
}

/// Decrypts inbound records and enforces replay protection.
pub struct RecordOpener {
    key: LessSafeKey,
    direction: Direction,
    window: Mutex<ReplayWindow>,
}

impl RecordOpener {
    pub fn new(key: &[u8; KEY_LEN], direction: Direction) -> Result<Self, CryptoError> {
        let unbound = UnboundKey::new(&AES_256_GCM, key).map_err(|_| CryptoError::InvalidKey)?;
        Ok(Self { key: LessSafeKey::new(unbound), direction, window: Mutex::new(ReplayWindow::new()) })
    }

    pub fn open(&self, channel: Channel, mut record: Vec<u8>) -> Result<Vec<u8>, CryptoError> {
        if record.len() < RECORD_OVERHEAD {
            return Err(CryptoError::Truncated);
        }
        let mut seq_bytes = [0u8; SEQ_LEN];
        seq_bytes.copy_from_slice(&record[..SEQ_LEN]);
        let seq = u64::from_be_bytes(seq_bytes);

        // Cheap pre-check before spending time on decryption.
        if !self.lock_window().check(seq) {
            return Err(CryptoError::Replay(seq));
        }

        let plain_len = self
            .key
            .open_in_place(nonce_for(self.direction, seq), Aad::from(aad_for(channel, seq)), &mut record[SEQ_LEN..])
            .map_err(|_| CryptoError::BadRecord)?
            .len();

        // Re-check under the lock: another task may have accepted `seq` meanwhile.
        let mut window = self.lock_window();
        if !window.check(seq) {
            return Err(CryptoError::Replay(seq));
        }
        window.commit(seq);
        drop(window);

        record.copy_within(SEQ_LEN..SEQ_LEN + plain_len, 0);
        record.truncate(plain_len);
        Ok(record)
    }

    fn lock_window(&self) -> std::sync::MutexGuard<'_, ReplayWindow> {
        // A poisoned lock only means another thread panicked mid-update of
        // plain bitmaps; the data is still structurally valid.
        self.window.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (RecordSealer, RecordOpener) {
        let key = [7u8; KEY_LEN];
        (
            RecordSealer::new(&key, Direction::HostToViewer).unwrap(),
            RecordOpener::new(&key, Direction::HostToViewer).unwrap(),
        )
    }

    #[test]
    fn roundtrip() {
        let (s, o) = pair();
        let rec = s.seal(Channel::Video, b"frame").unwrap();
        assert_eq!(o.open(Channel::Video, rec).unwrap(), b"frame");
    }

    #[test]
    fn rejects_replay() {
        let (s, o) = pair();
        let rec = s.seal(Channel::Control, b"x").unwrap();
        o.open(Channel::Control, rec.clone()).unwrap();
        assert_eq!(o.open(Channel::Control, rec), Err(CryptoError::Replay(0)));
    }

    #[test]
    fn rejects_wrong_channel_and_tamper() {
        let (s, o) = pair();
        let rec = s.seal(Channel::Control, b"x").unwrap();
        assert_eq!(o.open(Channel::Video, rec.clone()), Err(CryptoError::BadRecord));
        let mut t = rec;
        let last = t.len() - 1;
        t[last] ^= 1;
        assert_eq!(o.open(Channel::Control, t), Err(CryptoError::BadRecord));
    }

    #[test]
    fn accepts_reordering_within_window() {
        let (s, o) = pair();
        let recs: Vec<_> = (0..200).map(|i| s.seal(Channel::Video, &[i as u8]).unwrap()).collect();
        for r in recs.iter().rev() {
            o.open(Channel::Video, r.clone()).unwrap();
        }
        assert!(matches!(o.open(Channel::Video, recs[5].clone()), Err(CryptoError::Replay(5))));
    }

    #[test]
    fn rejects_stale_beyond_window() {
        let (s, o) = pair();
        let first = s.seal(Channel::Video, b"old").unwrap();
        let mut last = None;
        for _ in 0..(REPLAY_WINDOW + 10) {
            last = Some(s.seal(Channel::Video, b"n").unwrap());
        }
        o.open(Channel::Video, last.unwrap()).unwrap();
        assert!(matches!(o.open(Channel::Video, first), Err(CryptoError::Replay(0))));
    }

    #[test]
    fn wrong_direction_fails() {
        let key = [1u8; KEY_LEN];
        let s = RecordSealer::new(&key, Direction::HostToViewer).unwrap();
        let o = RecordOpener::new(&key, Direction::ViewerToHost).unwrap();
        let rec = s.seal(Channel::Control, b"x").unwrap();
        assert_eq!(o.open(Channel::Control, rec), Err(CryptoError::BadRecord));
    }
}
