use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use remoti_proto::signaling::{DeviceId, ServerMessage, SessionId};
use ring::rand::{SecureRandom as _, SystemRandom};
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};

use crate::relay::RelaySocket;

#[derive(Debug, Clone)]
pub struct Config {
    /// Public WebSocket URL of the relay endpoint handed to agents.
    pub relay_url: String,
    /// STUN servers advertised to agents (`host:port`).
    pub stun: Vec<String>,
    pub relay_pair_timeout: Duration,
    pub relay_idle_timeout: Duration,
    pub relay_max_lifetime: Duration,
}

pub struct Device {
    pub conn_id: u64,
    pub tx: mpsc::Sender<ServerMessage>,
}

/// One side of a relay allocation waiting for its partner.
pub struct RelaySlot {
    pub session_id: SessionId,
    pub device: DeviceId,
    pub pair: Arc<RelayPair>,
    pub expires: Instant,
}

/// Rendezvous point for the two sockets of one relay allocation.
#[derive(Default)]
pub struct RelayPair {
    pub parked: Mutex<Option<oneshot::Sender<RelaySocket>>>,
}

pub struct AppState {
    pub config: Config,
    pub rng: SystemRandom,
    pub devices: RwLock<HashMap<DeviceId, Device>>,
    pub relay_slots: Mutex<HashMap<String, RelaySlot>>,
    next_conn: AtomicU64,
}

impl AppState {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            rng: SystemRandom::new(),
            devices: RwLock::new(HashMap::new()),
            relay_slots: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
        })
    }

    pub fn next_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    pub fn random_bytes<const N: usize>(&self) -> Option<[u8; N]> {
        let mut b = [0u8; N];
        self.rng.fill(&mut b).ok()?;
        Some(b)
    }

    /// 32 lowercase hex chars: unguessable single-use relay credential.
    pub fn new_token(&self) -> Option<String> {
        let bytes: [u8; 16] = self.random_bytes()?;
        Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    pub async fn send_to(&self, device: &DeviceId, msg: ServerMessage) -> bool {
        let tx = self.devices.read().await.get(device).map(|d| d.tx.clone());
        match tx {
            Some(tx) => tx.try_send(msg).is_ok(),
            None => false,
        }
    }

    pub async fn is_online(&self, device: &DeviceId) -> bool {
        self.devices.read().await.contains_key(device)
    }

    /// Drops relay slots nobody claimed in time.
    pub async fn sweep_relays(&self) {
        let now = Instant::now();
        self.relay_slots.lock().await.retain(|_, s| s.expires > now);
    }
}
