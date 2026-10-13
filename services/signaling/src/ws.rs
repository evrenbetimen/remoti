//! Signaling WebSocket: Ed25519 proof-of-possession, then opaque routing.
//!
//! The server learns *who* talks to *whom* (needed for routing) but never the
//! session keys: payloads are end-to-end handshake messages it cannot alter
//! without failing the peers' key confirmation.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt as _, StreamExt as _};
use remoti_proto::MAX_SIGNALING_BYTES;
use remoti_proto::bytes::Bytes;
use remoti_proto::signaling::{AUTH_CONTEXT, ClientMessage, DeviceId, ErrorCode, ServerMessage};
use ring::digest::{SHA256, digest};
use ring::signature::{ED25519, UnparsedPublicKey};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::state::{AppState, Device, RelayPair, RelaySlot};

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const OUTBOUND_QUEUE: usize = 64;
const MAX_PAYLOAD: usize = MAX_SIGNALING_BYTES / 2;
const RATE_PER_SEC: f64 = 20.0;
const RATE_BURST: f64 = 40.0;

pub async fn upgrade(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.max_message_size(MAX_SIGNALING_BYTES).max_frame_size(MAX_SIGNALING_BYTES).on_upgrade(move |socket| async move {
        if let Err(e) = handle(socket, state).await {
            debug!(error = %e, "signaling connection ended");
        }
    })
}

#[derive(Debug, thiserror::Error)]
enum ConnError {
    #[error("websocket: {0}")]
    Ws(#[from] axum::Error),
    #[error("authentication: {0}")]
    Auth(&'static str),
    #[error("timeout")]
    Timeout,
    #[error("closed")]
    Closed,
}

fn encode(msg: &ServerMessage) -> Option<Message> {
    serde_json::to_string(msg).ok().map(|s| Message::Text(s.into()))
}

async fn next_client_message(socket: &mut WebSocket) -> Result<ClientMessage, ConnError> {
    loop {
        match socket.recv().await {
            None => return Err(ConnError::Closed),
            Some(Err(e)) => return Err(e.into()),
            Some(Ok(Message::Text(t))) => {
                return serde_json::from_str(t.as_str()).map_err(|_| ConnError::Auth("malformed message"));
            }
            Some(Ok(Message::Close(_))) => return Err(ConnError::Closed),
            Some(Ok(_)) => continue,
        }
    }
}

async fn authenticate(socket: &mut WebSocket, state: &AppState) -> Result<DeviceId, ConnError> {
    let public_key = match next_client_message(socket).await? {
        ClientMessage::Hello { public_key, .. } if public_key.len() == 32 => public_key,
        _ => return Err(ConnError::Auth("expected hello with a 32-byte ed25519 key")),
    };
    let nonce: [u8; 32] = state.random_bytes().ok_or(ConnError::Auth("rng failure"))?;
    let challenge = encode(&ServerMessage::Challenge { nonce: Bytes(nonce.to_vec()) }).ok_or(ConnError::Closed)?;
    socket.send(challenge).await?;
    let signature = match next_client_message(socket).await? {
        ClientMessage::Prove { signature } => signature,
        _ => return Err(ConnError::Auth("expected prove")),
    };
    let mut msg = AUTH_CONTEXT.to_vec();
    msg.extend_from_slice(&nonce);
    UnparsedPublicKey::new(&ED25519, public_key.as_slice())
        .verify(&msg, signature.as_slice())
        .map_err(|_| ConnError::Auth("bad signature"))?;
    let mut d = [0u8; 32];
    d.copy_from_slice(digest(&SHA256, public_key.as_slice()).as_ref());
    Ok(DeviceId::from_key_digest(&d))
}

async fn handle(mut socket: WebSocket, state: Arc<AppState>) -> Result<(), ConnError> {
    let device_id = match timeout(AUTH_TIMEOUT, authenticate(&mut socket, &state)).await {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => {
            if let Some(m) = encode(&ServerMessage::Error { code: ErrorCode::Unauthenticated, message: e.to_string() })
            {
                let _ = socket.send(m).await;
            }
            return Err(e);
        }
        Err(_) => return Err(ConnError::Timeout),
    };

    let conn_id = state.next_conn_id();
    let (tx, mut rx) = mpsc::channel::<ServerMessage>(OUTBOUND_QUEUE);
    // A newer connection for the same key replaces the old one; the old
    // writer ends when its sender is dropped.
    if state.devices.write().await.insert(device_id.clone(), Device { conn_id, tx: tx.clone() }).is_some() {
        info!(%device_id, "device reconnected; replacing previous connection");
    }
    let now_ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let welcome = ServerMessage::Welcome {
        device_id: device_id.clone(),
        server_time_ms: now_ms,
        stun: state.config.stun.clone(),
    };
    let _ = tx.send(welcome).await;
    info!(%device_id, "device online");

    let (mut sink, mut stream) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let Some(frame) = encode(&msg) else { continue };
            if sink.send(frame).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut tokens = RATE_BURST;
    let mut last = Instant::now();
    let result = loop {
        let frame = match timeout(IDLE_TIMEOUT, stream.next()).await {
            Err(_) => break Err(ConnError::Timeout),
            Ok(None) => break Ok(()),
            Ok(Some(Err(e))) => break Err(e.into()),
            Ok(Some(Ok(f))) => f,
        };
        let text = match frame {
            Message::Text(t) => t,
            Message::Close(_) => break Ok(()),
            _ => continue,
        };
        let now = Instant::now();
        tokens = (tokens + now.duration_since(last).as_secs_f64() * RATE_PER_SEC).min(RATE_BURST);
        last = now;
        if tokens < 1.0 {
            let _ = tx.try_send(ServerMessage::Error { code: ErrorCode::RateLimited, message: "slow down".into() });
            continue;
        }
        tokens -= 1.0;
        let msg: ClientMessage = match serde_json::from_str(text.as_str()) {
            Ok(m) => m,
            Err(_) => {
                let _ = tx.try_send(ServerMessage::Error {
                    code: ErrorCode::BadRequest,
                    message: "malformed message".into(),
                });
                continue;
            }
        };
        on_message(&state, &device_id, &tx, msg).await;
    };

    // Only unregister if we are still the current connection for this device.
    {
        let mut devices = state.devices.write().await;
        if devices.get(&device_id).is_some_and(|d| d.conn_id == conn_id) {
            devices.remove(&device_id);
        }
    }
    drop(tx);
    writer.abort();
    info!(%device_id, "device offline");
    result
}

async fn on_message(state: &AppState, from: &DeviceId, tx: &mpsc::Sender<ServerMessage>, msg: ClientMessage) {
    match msg {
        ClientMessage::Signal { to, session_id, payload } => {
            if payload.len() > MAX_PAYLOAD {
                let _ = tx.try_send(ServerMessage::Error {
                    code: ErrorCode::PayloadTooLarge,
                    message: "payload too large".into(),
                });
                return;
            }
            let delivered = state
                .send_to(&to, ServerMessage::Signal { from: from.clone(), session_id: session_id.clone(), payload })
                .await;
            if !delivered {
                let _ = tx.try_send(ServerMessage::PeerUnavailable { session_id, peer: to });
            }
        }
        ClientMessage::AllocateRelay { session_id, peer } => {
            if &peer == from || !state.is_online(&peer).await {
                let _ = tx.try_send(ServerMessage::PeerUnavailable { session_id, peer });
                return;
            }
            let (Some(token_a), Some(token_b)) = (state.new_token(), state.new_token()) else {
                let _ = tx.try_send(ServerMessage::Error { code: ErrorCode::Internal, message: "rng failure".into() });
                return;
            };
            let pair = Arc::new(RelayPair::default());
            let expires = Instant::now() + state.config.relay_pair_timeout;
            {
                let mut slots = state.relay_slots.lock().await;
                slots.insert(
                    token_a.clone(),
                    RelaySlot { session_id: session_id.clone(), device: from.clone(), pair: pair.clone(), expires },
                );
                slots.insert(
                    token_b.clone(),
                    RelaySlot { session_id: session_id.clone(), device: peer.clone(), pair, expires },
                );
            }
            let url = state.config.relay_url.clone();
            let _ = tx.try_send(ServerMessage::RelayAllocated {
                session_id: session_id.clone(),
                url: url.clone(),
                token: token_a,
            });
            if !state.send_to(&peer, ServerMessage::RelayAllocated { session_id, url, token: token_b }).await {
                warn!(%peer, "peer vanished during relay allocation");
            }
        }
        ClientMessage::Ping { nonce } => {
            let _ = tx.try_send(ServerMessage::Pong { nonce });
        }
        ClientMessage::Hello { .. } | ClientMessage::Prove { .. } => {
            let _ = tx.try_send(ServerMessage::Error {
                code: ErrorCode::BadRequest,
                message: "already authenticated".into(),
            });
        }
    }
}
