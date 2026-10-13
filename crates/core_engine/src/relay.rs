//! TCP/WebSocket fallback used when UDP is blocked between the peers.
//!
//! The relay endpoint pairs two authenticated tokens and forwards binary
//! frames verbatim. Each frame is `channel(1) || sealed record`, so the relay
//! only ever handles AES-256-GCM ciphertext it has no keys for.

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use remoti_proto::MAX_MESSAGE_BYTES;
use remoti_proto::wire::Channel;
use tokio::time::{interval, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::crypto::RECORD_OVERHEAD;
use crate::transport::{FLUSH_TIMEOUT, LinkKind, RawLink, RawLinkPump, TransportStats, raw_link};

const MAX_FRAME: usize = MAX_MESSAGE_BYTES + RECORD_OVERHEAD + 1;
const STATS_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("relay websocket: {0}")]
    Ws(Box<tokio_tungstenite::tungstenite::Error>),
    #[error("relay connect timed out")]
    Timeout,
    #[error("invalid relay url")]
    BadUrl,
}

impl From<tokio_tungstenite::tungstenite::Error> for RelayError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        RelayError::Ws(Box::new(e))
    }
}

/// Connects to the relay with a single-use token issued by signaling.
pub async fn connect(url: &str, token: &str, wait: Duration) -> Result<RawLink, RelayError> {
    if !(url.starts_with("wss://") || url.starts_with("ws://")) || token.is_empty() {
        return Err(RelayError::BadUrl);
    }
    if !token.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(RelayError::BadUrl);
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    let full = format!("{url}{sep}token={token}");
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_FRAME);
    config.max_frame_size = Some(MAX_FRAME);
    let (ws, _) = timeout(wait, tokio_tungstenite::connect_async_with_config(full, Some(config), true))
        .await
        .map_err(|_| RelayError::Timeout)??;
    info!("relay tunnel connected (udp unavailable, using tcp fallback)");

    let (link, pump) = raw_link(LinkKind::Relay, CancellationToken::new());
    tokio::spawn(run_pump(ws, pump));
    Ok(link)
}

fn frame(channel: Channel, rec: Vec<u8>) -> Message {
    let mut out = Vec::with_capacity(rec.len() + 1);
    out.push(channel as u8);
    out.extend_from_slice(&rec);
    Message::binary(out)
}

async fn run_pump<S>(ws: tokio_tungstenite::WebSocketStream<S>, pump: RawLinkPump)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let RawLinkPump { mut control_rx, mut video_rx, inbound_tx, stats_tx, cancel, done } = pump;
    let (mut sink, mut stream) = ws.split();

    let writer = async {
        let mut tick = interval(STATS_INTERVAL);
        loop {
            // `biased`: drain control before video so input never waits behind frames.
            tokio::select! {
                biased;
                rec = control_rx.recv() => {
                    let Some(rec) = rec else { return };
                    if sink.send(frame(Channel::Control, rec)).await.is_err() { return }
                }
                rec = video_rx.recv() => {
                    let Some(rec) = rec else { return };
                    if sink.send(frame(Channel::Video, rec)).await.is_err() { return }
                }
                _ = tick.tick() => {
                    let snapshot = TransportStats { video_backlog: video_rx.len(), ..TransportStats::default() };
                    if stats_tx.send(snapshot).is_err() { return }
                }
            }
        }
    };

    let reader = async {
        while let Some(msg) = stream.next().await {
            let data = match msg {
                Ok(Message::Binary(b)) => b,
                Ok(Message::Close(_)) => return,
                Ok(_) => continue,
                Err(e) => {
                    debug!(error = %e, "relay read failed");
                    return;
                }
            };
            let Some((&tag, rec)) = data.split_first() else { continue };
            let channel = match tag {
                1 => Channel::Control,
                2 => Channel::Video,
                other => {
                    warn!(tag = other, "unknown relay channel; closing");
                    return;
                }
            };
            let item = (channel, rec.to_vec());
            let sent = match channel {
                Channel::Control => inbound_tx.send(item).await.is_ok(),
                // Drop video rather than stall the shared TCP stream.
                Channel::Video => {
                    !matches!(inbound_tx.try_send(item), Err(tokio::sync::mpsc::error::TrySendError::Closed(_)))
                }
            };
            if !sent {
                return;
            }
        }
    };

    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = writer => {}
        _ = reader => {}
    }
    let flush = async {
        while let Ok(rec) = control_rx.try_recv() {
            if sink.send(frame(Channel::Control, rec)).await.is_err() {
                return;
            }
        }
        let _ = sink.close().await;
    };
    let _ = tokio::time::timeout(FLUSH_TIMEOUT, flush).await;
    cancel.cancel();
    done.cancel();
    info!("relay tunnel closed");
}
