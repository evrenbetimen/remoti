//! Routes signaling events to the session task that owns a session id.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use remoti_proto::peer::PeerSignal;
use remoti_proto::signaling::{DeviceId, SessionId};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum SessionInbox {
    Peer { from: DeviceId, signal: PeerSignal },
    Relay { url: String, token: String },
    PeerUnavailable,
}

#[derive(Clone, Default)]
pub struct Router {
    routes: Arc<Mutex<HashMap<SessionId, mpsc::Sender<SessionInbox>>>>,
}

/// Unregisters the route when dropped.
pub struct Route {
    pub id: SessionId,
    pub inbox: mpsc::Receiver<SessionInbox>,
    router: Router,
}

impl Drop for Route {
    fn drop(&mut self) {
        self.router.lock().remove(&self.id);
    }
}

impl Router {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, mpsc::Sender<SessionInbox>>> {
        self.routes.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn register(&self, id: SessionId) -> Route {
        let (tx, rx) = mpsc::channel(16);
        self.lock().insert(id.clone(), tx);
        Route { id, inbox: rx, router: self.clone() }
    }

    pub fn contains(&self, id: &SessionId) -> bool {
        self.lock().contains_key(id)
    }

    /// Delivers to the owning session; returns `false` if no session owns `id`.
    pub fn deliver(&self, id: &SessionId, msg: SessionInbox) -> bool {
        let tx = self.lock().get(id).cloned();
        match tx {
            Some(tx) => {
                if tx.try_send(msg).is_err() {
                    tracing::warn!(session = %id, "session inbox full; signal dropped");
                }
                true
            }
            None => false,
        }
    }
}
