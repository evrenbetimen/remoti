//! Remoti signaling service (Axum).
//!
//! * `GET /ws`: authenticated signaling (device discovery + opaque handshake routing).
//! * `GET /relay?token=…`: blind TCP/WebSocket relay for UDP-blocked peers.
//! * `GET /healthz`: liveness.
//! * UDP STUN Binding responder for server-reflexive address discovery.

pub mod relay;
pub mod state;
pub mod stun;
pub mod ws;

use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/ws", get(ws::upgrade))
        .route("/relay", get(relay::upgrade))
        .route("/healthz", get(|| async { "ok" }))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
