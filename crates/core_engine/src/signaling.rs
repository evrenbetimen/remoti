//! WebSocket signaling client with authentication, heartbeats and automatic
//! reconnection (exponential backoff with jitter).

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use remoti_proto::MAX_SIGNALING_BYTES;
use remoti_proto::bytes::Bytes;
use remoti_proto::signaling::{ClientMessage, DeviceId, ErrorCode, ServerMessage, SessionId};
use ring::rand::{SecureRandom as _, SystemRandom};
use tokio::sync::mpsc;
use tokio::time::{Instant, interval, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::identity::DeviceIdentity;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT: Duration = Duration::from_secs(20);
const IDLE_LIMIT: Duration = Duration::from_secs(60);
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub enum SignalingEvent {
    Connected { device_id: DeviceId, stun: Vec<String> },
    Disconnected { reason: String },
    Signal { from: DeviceId, session_id: SessionId, payload: Bytes },
    RelayAllocated { session_id: SessionId, url: String, token: String },
    PeerUnavailable { session_id: SessionId, peer: DeviceId },
    ServerError { code: ErrorCode, message: String },
}

#[derive(Debug, thiserror::Error)]
pub enum SignalingError {
    #[error("websocket: {0}")]
    Ws(Box<tokio_tungstenite::tungstenite::Error>),
    #[error("timed out: {0}")]
    Timeout(&'static str),
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("server rejected authentication: {0}")]
    Rejected(String),
    #[error("signaling client stopped")]
    Closed,
}

impl From<tokio_tungstenite::tungstenite::Error> for SignalingError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        SignalingError::Ws(Box::new(e))
    }
}

/// Cloneable sender half used by sessions.
#[derive(Clone)]
pub struct SignalingSender {
    tx: mpsc::Sender<ClientMessage>,
}

impl SignalingSender {
    pub async fn send(&self, msg: ClientMessage) -> Result<(), SignalingError> {
        self.tx.send(msg).await.map_err(|_| SignalingError::Closed)
    }
}

pub struct SignalingClient {
    pub sender: SignalingSender,
    pub events: mpsc::Receiver<SignalingEvent>,
}

impl SignalingClient {
    pub fn spawn(url: String, identity: Arc<DeviceIdentity>, cancel: CancellationToken) -> Self {
        let (out_tx, out_rx) = mpsc::channel(64);
        let (ev_tx, ev_rx) = mpsc::channel(64);
        tokio::spawn(run(url, identity, out_rx, ev_tx, cancel));
        Self { sender: SignalingSender { tx: out_tx }, events: ev_rx }
    }
}

async fn run(
    url: String,
    identity: Arc<DeviceIdentity>,
    mut outbound: mpsc::Receiver<ClientMessage>,
    events: mpsc::Sender<SignalingEvent>,
    cancel: CancellationToken,
) {
    let rng = SystemRandom::new();
    let mut backoff = BACKOFF_MIN;
    loop {
        let started = Instant::now();
        let result = tokio::select! {
            _ = cancel.cancelled() => return,
            r = connection(&url, &identity, &mut outbound, &events) => r,
        };
        let reason = match result {
            Ok(()) => "closed by server".to_owned(),
            Err(SignalingError::Closed) => return,
            Err(e) => e.to_string(),
        };
        warn!(%reason, "signaling disconnected");
        if events.send(SignalingEvent::Disconnected { reason }).await.is_err() {
            return;
        }
        // A connection that stayed up for a while resets the backoff.
        if started.elapsed() > BACKOFF_MAX {
            backoff = BACKOFF_MIN;
        }
        let delay = with_jitter(backoff, &rng);
        debug!(?delay, "reconnecting to signaling");
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = sleep(delay) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

fn with_jitter(base: Duration, rng: &SystemRandom) -> Duration {
    let mut b = [0u8; 2];
    let r = if rng.fill(&mut b).is_ok() { u16::from_le_bytes(b) } else { 0 };
    // 50%..100% of base ("equal jitter").
    let half = base / 2;
    half + half.mul_f64(f64::from(r) / f64::from(u16::MAX))
}

async fn connection(
    url: &str,
    identity: &DeviceIdentity,
    outbound: &mut mpsc::Receiver<ClientMessage>,
    events: &mpsc::Sender<SignalingEvent>,
) -> Result<(), SignalingError> {
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_SIGNALING_BYTES);
    config.max_frame_size = Some(MAX_SIGNALING_BYTES);
    let (ws, _) = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async_with_config(url, Some(config), true))
        .await
        .map_err(|_| SignalingError::Timeout("connect"))??;
    let (mut sink, mut stream) = ws.split();

    let send = |m: &ClientMessage| -> Result<Message, SignalingError> {
        serde_json::to_string(m).map(Message::text).map_err(|e| SignalingError::Protocol(e.to_string()))
    };

    // --- Authentication: Hello -> Challenge -> Prove -> Welcome.
    sink.send(send(&ClientMessage::Hello {
        public_key: Bytes(identity.public_key().to_vec()),
        agent_version: env!("CARGO_PKG_VERSION").to_owned(),
    })?)
    .await?;

    let auth = async {
        loop {
            match next_server_message(&mut stream).await? {
                ServerMessage::Challenge { nonce } => {
                    if nonce.len() < 16 {
                        return Err(SignalingError::Protocol("challenge nonce too short".into()));
                    }
                    let signature = Bytes(identity.sign_challenge(nonce.as_slice()));
                    sink.send(send(&ClientMessage::Prove { signature })?).await?;
                }
                ServerMessage::Welcome { device_id, stun, .. } => {
                    if &device_id != identity.device_id() {
                        return Err(SignalingError::Protocol("server assigned an unexpected device id".into()));
                    }
                    return Ok(stun);
                }
                ServerMessage::Error { message, .. } => return Err(SignalingError::Rejected(message)),
                other => debug!(?other, "ignoring message before welcome"),
            }
        }
    };
    let stun = timeout(AUTH_TIMEOUT, auth).await.map_err(|_| SignalingError::Timeout("authentication"))??;
    info!(device_id = %identity.device_id(), "signaling authenticated");
    events
        .send(SignalingEvent::Connected { device_id: identity.device_id().clone(), stun })
        .await
        .map_err(|_| SignalingError::Closed)?;

    // --- Steady state.
    let mut heartbeat = interval(HEARTBEAT);
    let mut last_seen = Instant::now();
    let mut ping_nonce = 0u64;
    loop {
        tokio::select! {
            msg = outbound.recv() => {
                let msg = msg.ok_or(SignalingError::Closed)?;
                sink.send(send(&msg)?).await?;
            }
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > IDLE_LIMIT {
                    return Err(SignalingError::Timeout("heartbeat"));
                }
                ping_nonce = ping_nonce.wrapping_add(1);
                sink.send(send(&ClientMessage::Ping { nonce: ping_nonce })?).await?;
            }
            incoming = stream.next() => {
                let Some(frame) = incoming else { return Ok(()) };
                last_seen = Instant::now();
                let text = match frame? {
                    Message::Text(t) => t,
                    Message::Close(_) => return Ok(()),
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                    Message::Binary(_) => return Err(SignalingError::Protocol("unexpected binary frame".into())),
                };
                let msg: ServerMessage = serde_json::from_str(text.as_str())
                    .map_err(|e| SignalingError::Protocol(e.to_string()))?;
                let ev = match msg {
                    ServerMessage::Signal { from, session_id, payload } => SignalingEvent::Signal { from, session_id, payload },
                    ServerMessage::RelayAllocated { session_id, url, token } => SignalingEvent::RelayAllocated { session_id, url, token },
                    ServerMessage::PeerUnavailable { session_id, peer } => SignalingEvent::PeerUnavailable { session_id, peer },
                    ServerMessage::Error { code, message } => SignalingEvent::ServerError { code, message },
                    ServerMessage::Pong { .. } => continue,
                    ServerMessage::Challenge { .. } | ServerMessage::Welcome { .. } => {
                        return Err(SignalingError::Protocol("unexpected re-authentication".into()));
                    }
                };
                events.send(ev).await.map_err(|_| SignalingError::Closed)?;
            }
        }
    }
}

async fn next_server_message<S>(stream: &mut S) -> Result<ServerMessage, SignalingError>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await {
            None => return Err(SignalingError::Protocol("connection closed during authentication".into())),
            Some(Err(e)) => return Err(e.into()),
            Some(Ok(Message::Text(t))) => {
                return serde_json::from_str(t.as_str()).map_err(|e| SignalingError::Protocol(e.to_string()));
            }
            Some(Ok(Message::Close(_))) => {
                return Err(SignalingError::Protocol("connection closed during authentication".into()));
            }
            Some(Ok(_)) => continue,
        }
    }
}
