//! Blind TCP/WebSocket relay (TURN-like fallback for UDP-hostile networks).
//!
//! Two agents present single-use tokens issued over authenticated signaling;
//! the relay pairs them and copies binary frames verbatim. Frames are
//! AES-256-GCM records sealed end to end, so the relay cannot read or forge
//! them.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt as _, StreamExt as _};
use remoti_proto::MAX_MESSAGE_BYTES;
use serde::Deserialize;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tracing::{debug, info};

use crate::state::AppState;

/// Record + AEAD overhead + channel byte.
const MAX_FRAME: usize = MAX_MESSAGE_BYTES + 64;

pub type RelaySocket = WebSocket;

#[derive(Deserialize)]
pub struct RelayQuery {
    token: String,
}

pub async fn upgrade(
    ws: WebSocketUpgrade,
    Query(q): Query<RelayQuery>,
    State(state): State<Arc<AppState>>,
) -> Response {
    if q.token.len() != 32 || !q.token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // Single use: the slot is removed on first presentation.
    let slot = state.relay_slots.lock().await.remove(&q.token);
    let Some(slot) = slot else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if slot.expires < std::time::Instant::now() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let session = slot.session_id.clone();
    let device = slot.device.clone();
    ws.max_message_size(MAX_FRAME).max_frame_size(MAX_FRAME).on_upgrade(move |socket| async move {
        debug!(%session, %device, "relay leg connected");
        let partner = {
            let mut parked = slot.pair.parked.lock().await;
            match parked.take() {
                // Partner is already waiting: hand our socket to its task.
                Some(waiting) => {
                    if waiting.send(socket).is_err() {
                        debug!(%session, "partner gave up before pairing");
                    }
                    return;
                }
                None => {
                    let (tx, rx) = oneshot::channel();
                    *parked = Some(tx);
                    rx
                }
            }
        };
        match timeout(state.config.relay_pair_timeout, partner).await {
            Ok(Ok(other)) => {
                info!(%session, "relay paired");
                pipe(socket, other, state.config.relay_idle_timeout, state.config.relay_max_lifetime).await;
                info!(%session, "relay closed");
            }
            _ => debug!(%session, "relay partner never arrived"),
        }
    })
}

/// Copies binary frames in both directions until either side closes, the
/// path idles out or the allocation reaches its maximum lifetime.
async fn pipe(a: WebSocket, b: WebSocket, idle: Duration, lifetime: Duration) {
    let (mut a_tx, mut a_rx) = a.split();
    let (mut b_tx, mut b_rx) = b.split();
    let deadline = Instant::now() + lifetime;

    let a_to_b = async {
        loop {
            match timeout(idle, a_rx.next()).await {
                Ok(Some(Ok(Message::Binary(data)))) => {
                    if b_tx.send(Message::Binary(data)).await.is_err() {
                        return;
                    }
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) | Err(_) => return,
                Ok(Some(Ok(_))) => continue,
            }
        }
    };
    let b_to_a = async {
        loop {
            match timeout(idle, b_rx.next()).await {
                Ok(Some(Ok(Message::Binary(data)))) => {
                    if a_tx.send(Message::Binary(data)).await.is_err() {
                        return;
                    }
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) | Err(_) => return,
                Ok(Some(Ok(_))) => continue,
            }
        }
    };
    tokio::select! {
        _ = a_to_b => {}
        _ = b_to_a => {}
        _ = tokio::time::sleep_until(deadline) => debug!("relay lifetime reached"),
    }
}
