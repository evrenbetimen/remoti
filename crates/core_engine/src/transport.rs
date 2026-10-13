//! Transport-agnostic link used by sessions.
//!
//! A [`RawLink`] moves *sealed* records over QUIC ([`crate::network`]) or the
//! TCP/WebSocket fallback ([`crate::relay`]). [`SecureLink`] adds the record
//! layer on top, so both paths carry identical AES-256-GCM ciphertext.

use std::sync::Arc;
use std::time::Duration;

use remoti_proto::bytes::Bytes;
use remoti_proto::wire::{Channel, VideoFrame, WireMessage};
use remoti_proto::{decode_binary, encode_binary};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::crypto::CryptoError;
use crate::handshake::{HandshakeError, Role, SessionSecrets};

/// Bounded queue sizes; video is intentionally tiny so stale frames are
/// dropped at the source instead of building latency.
pub const CONTROL_QUEUE: usize = 256;
pub const VIDEO_QUEUE: usize = 3;
pub const INBOUND_QUEUE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LinkKind {
    /// Direct QUIC over UDP (hole-punched).
    Quic,
    /// TCP/WebSocket through the blind relay (UDP blocked).
    Relay,
}

/// Transport-level statistics sampled by the link pump.
#[derive(Debug, Clone, Copy, Default)]
pub struct TransportStats {
    pub rtt: Duration,
    /// Fraction of packets lost over the last sampling interval (0..1).
    pub loss: f32,
    /// Bytes the transport currently allows in flight (QUIC cwnd), if known.
    pub cwnd: Option<u64>,
    /// Video records accepted but not yet fully written.
    pub video_backlog: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("link closed")]
    Closed,
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Codec(#[from] remoti_proto::CodecError),
    #[error(transparent)]
    Handshake(#[from] HandshakeError),
    #[error("peer did not confirm keys in time")]
    ConfirmTimeout,
    #[error("unexpected message before key confirmation")]
    Unconfirmed,
}

/// Record queues connecting a session to a transport pump.
pub struct RawLink {
    pub kind: LinkKind,
    pub control_tx: mpsc::Sender<Vec<u8>>,
    pub video_tx: mpsc::Sender<Vec<u8>>,
    pub inbound_rx: mpsc::Receiver<(Channel, Vec<u8>)>,
    pub stats_rx: watch::Receiver<TransportStats>,
    /// Cancelling tears the transport down (after flushing control records).
    pub cancel: CancellationToken,
    /// Cancelled by the pump once it has fully shut down.
    pub done: CancellationToken,
}

/// The pump-side ends of a [`RawLink`].
pub struct RawLinkPump {
    pub control_rx: mpsc::Receiver<Vec<u8>>,
    pub video_rx: mpsc::Receiver<Vec<u8>>,
    pub inbound_tx: mpsc::Sender<(Channel, Vec<u8>)>,
    pub stats_tx: watch::Sender<TransportStats>,
    pub cancel: CancellationToken,
    pub done: CancellationToken,
}

/// How long a closing pump may spend flushing queued control records.
pub const FLUSH_TIMEOUT: Duration = Duration::from_millis(300);

pub fn raw_link(kind: LinkKind, cancel: CancellationToken) -> (RawLink, RawLinkPump) {
    let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE);
    let (video_tx, video_rx) = mpsc::channel(VIDEO_QUEUE);
    let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_QUEUE);
    let (stats_tx, stats_rx) = watch::channel(TransportStats::default());
    let done = CancellationToken::new();
    (
        RawLink { kind, control_tx, video_tx, inbound_rx, stats_rx, cancel: cancel.clone(), done: done.clone() },
        RawLinkPump { control_rx, video_rx, inbound_tx, stats_tx, cancel, done },
    )
}

/// Outcome of offering a video frame to the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoSend {
    Queued,
    /// Queue full: the frame was dropped to protect latency.
    Dropped,
}

/// Cloneable sending half of a [`SecureLink`].
#[derive(Clone)]
pub struct SecureSender {
    secrets: Arc<SessionSecrets>,
    control_tx: mpsc::Sender<Vec<u8>>,
    video_tx: mpsc::Sender<Vec<u8>>,
}

impl SecureSender {
    pub async fn send(&self, msg: &WireMessage) -> Result<(), LinkError> {
        let plain = encode_binary(msg)?;
        let rec = self.secrets.sealer.seal(msg.channel(), &plain)?;
        match msg.channel() {
            Channel::Control => self.control_tx.send(rec).await.map_err(|_| LinkError::Closed),
            Channel::Video => self.video_tx.send(rec).await.map_err(|_| LinkError::Closed),
        }
    }

    /// Non-blocking video send; never waits behind congestion.
    pub fn try_send_video(&self, frame: VideoFrame) -> Result<VideoSend, LinkError> {
        let permit = match self.video_tx.try_reserve() {
            Ok(p) => p,
            Err(mpsc::error::TrySendError::Full(())) => return Ok(VideoSend::Dropped),
            Err(mpsc::error::TrySendError::Closed(())) => return Err(LinkError::Closed),
        };
        let plain = encode_binary(&WireMessage::VideoFrame(frame))?;
        permit.send(self.secrets.sealer.seal(Channel::Video, &plain)?);
        Ok(VideoSend::Queued)
    }
}

pub struct SecureLink {
    pub kind: LinkKind,
    pub sender: SecureSender,
    pub stats: watch::Receiver<TransportStats>,
    pub cancel: CancellationToken,
    done: CancellationToken,
    secrets: Arc<SessionSecrets>,
    inbound: mpsc::Receiver<(Channel, Vec<u8>)>,
}

impl SecureLink {
    /// Wraps a raw link and runs key confirmation in both directions.
    pub async fn establish(raw: RawLink, secrets: Arc<SessionSecrets>, wait: Duration) -> Result<Self, LinkError> {
        let mut link = SecureLink {
            kind: raw.kind,
            sender: SecureSender { secrets: secrets.clone(), control_tx: raw.control_tx, video_tx: raw.video_tx },
            stats: raw.stats_rx,
            cancel: raw.cancel,
            done: raw.done,
            secrets,
            inbound: raw.inbound_rx,
        };
        // Ordering matters for password safety: the viewer proves knowledge
        // first, and the host only answers after verifying it. A fake viewer
        // therefore never receives host-keyed data it could use to test
        // password guesses offline; every guess costs it one online attempt,
        // which the host counts towards its lockout.
        let tag = Bytes(link.secrets.local_confirmation());
        let role = link.secrets.role;
        let confirmed = timeout(wait, async {
            if role == Role::Viewer {
                link.sender.send(&WireMessage::Confirm { tag: tag.clone() }).await?;
            }
            loop {
                match link.recv_raw().await? {
                    WireMessage::Confirm { tag: remote } => {
                        link.secrets.verify_remote(remote.as_slice())?;
                        break;
                    }
                    // Host video may overtake its Confirm on independent QUIC
                    // streams; it authenticated, but is dropped until confirmed.
                    WireMessage::VideoFrame(_) if role == Role::Viewer => continue,
                    _ => return Err(LinkError::Unconfirmed),
                }
            }
            if role == Role::Host {
                link.sender.send(&WireMessage::Confirm { tag }).await?;
            }
            Ok::<(), LinkError>(())
        })
        .await
        // Different keys (wrong password, tampered transcript) surface as AEAD
        // failures on the peer's Confirm record.
        .map(|r| match r {
            Err(LinkError::Crypto(CryptoError::BadRecord)) => Err(LinkError::Handshake(HandshakeError::Confirmation)),
            other => other,
        });
        match confirmed {
            Ok(Ok(())) => Ok(link),
            Ok(Err(e)) => {
                link.cancel.cancel();
                Err(e)
            }
            Err(_) => {
                link.cancel.cancel();
                Err(LinkError::ConfirmTimeout)
            }
        }
    }

    async fn recv_raw(&mut self) -> Result<WireMessage, LinkError> {
        loop {
            let (channel, rec) = self.inbound.recv().await.ok_or(LinkError::Closed)?;
            match self.secrets.opener.open(channel, rec) {
                Ok(plain) => {
                    let msg: WireMessage = decode_binary(&plain)?;
                    if msg.channel() != channel {
                        warn!("message arrived on the wrong channel; dropping");
                        continue;
                    }
                    return Ok(msg);
                }
                // Replays/stale records are expected noise on lossy paths.
                Err(CryptoError::Replay(seq)) => {
                    warn!(seq, "dropping replayed or stale record");
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Next authenticated message; `Err(Closed)` when the transport ends.
    pub async fn recv(&mut self) -> Result<WireMessage, LinkError> {
        self.recv_raw().await
    }

    pub fn close(&self) {
        self.cancel.cancel();
    }

    /// Closes the link after queued control records (e.g. `Bye`) are flushed.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let _ = timeout(FLUSH_TIMEOUT * 2, self.done.cancelled()).await;
    }
}

impl Drop for SecureLink {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
