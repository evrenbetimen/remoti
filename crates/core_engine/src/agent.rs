//! Agent event loop: owns signaling, routes peer signals, and supervises the
//! host and viewer session tasks on behalf of the UI.

use std::path::PathBuf;
use std::sync::Arc;

use remoti_proto::bytes::Bytes;
use remoti_proto::decode_binary;
use remoti_proto::peer::{PeerSignal, RejectReason};
use remoti_proto::signaling::{ClientMessage, DeviceId, SessionId};
use ring::rand::SystemRandom;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::capture::{self, CaptureBackend};
use crate::congestion::RateConfig;
use crate::handshake;
use crate::host::{self, HostContext, HostOutcome};
use crate::identity::{self, DeviceIdentity};
use crate::input;
use crate::ipc::{UiCommand, UiEvent, UiSink};
use crate::router::{Router, SessionInbox};
use crate::signaling::{SignalingClient, SignalingEvent, SignalingSender};
use crate::viewer::{self, ViewerCommand, ViewerContext};

/// Failed password attempts before the one-time password is rotated.
const MAX_AUTH_FAILURES: u32 = 5;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub signaling_url: String,
    /// STUN servers used in addition to those advertised by signaling.
    pub stun: Vec<String>,
    pub udp_port: u16,
    pub capture: CaptureBackend,
    pub rate: RateConfig,
    pub data_dir: Option<PathBuf>,
    pub ephemeral_identity: bool,
    pub force_relay: bool,
}

struct HostState {
    password: String,
    failures: u32,
    allow_control: watch::Sender<bool>,
    active: Option<ActiveHostSession>,
    cancel: CancellationToken,
}

struct ActiveHostSession {
    session: SessionId,
    viewer: DeviceId,
    cancel: CancellationToken,
}

struct ViewerState {
    commands: mpsc::Sender<ViewerCommand>,
    cancel: CancellationToken,
}

struct Agent {
    cfg: AgentConfig,
    rng: SystemRandom,
    device_id: DeviceId,
    signaling: SignalingSender,
    router: Router,
    ui: UiSink,
    stun: Vec<String>,
    host: Option<HostState>,
    viewer: Option<ViewerState>,
    host_reports: mpsc::Sender<(SessionId, HostOutcome)>,
}

pub async fn run(
    cfg: AgentConfig,
    ui: UiSink,
    mut commands: mpsc::Receiver<UiCommand>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let rng = SystemRandom::new();
    let identity = if cfg.ephemeral_identity {
        DeviceIdentity::ephemeral(&rng)?
    } else {
        let dir = cfg.data_dir.clone().unwrap_or_else(identity::default_data_dir);
        DeviceIdentity::load_or_create(&dir, &rng)?
    };
    let identity = Arc::new(identity);
    info!(device_id = %identity.device_id(), "agent identity ready");
    ui.event(UiEvent::Ready { version: env!("CARGO_PKG_VERSION"), platform: std::env::consts::OS });

    let SignalingClient { sender, mut events } =
        SignalingClient::spawn(cfg.signaling_url.clone(), identity.clone(), cancel.child_token());
    let (host_reports, mut reports) = mpsc::channel(16);
    let mut agent = Agent {
        stun: cfg.stun.clone(),
        cfg,
        rng,
        device_id: identity.device_id().clone(),
        signaling: sender,
        router: Router::default(),
        ui,
        host: None,
        viewer: None,
        host_reports,
    };

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            cmd = commands.recv() => match cmd {
                None | Some(UiCommand::Shutdown) => break,
                Some(cmd) => agent.on_command(cmd),
            },
            ev = events.recv() => match ev {
                Some(ev) => agent.on_signaling(ev).await,
                None => break,
            },
            Some((session, outcome)) = reports.recv() => agent.on_host_report(session, outcome),
        }
    }
    agent.stop_host();
    agent.stop_viewer();
    info!("agent stopped");
    Ok(())
}

impl Agent {
    fn error(&self, code: &'static str, message: impl Into<String>) {
        self.ui.event(UiEvent::Error { code, message: message.into() });
    }

    fn on_command(&mut self, cmd: UiCommand) {
        match cmd {
            UiCommand::HostStart { allow_control } => self.start_host(allow_control),
            UiCommand::HostStop => {
                self.stop_host();
                self.ui.event(UiEvent::HostStopped);
            }
            UiCommand::HostRotatePassword => self.rotate_password(),
            UiCommand::HostSetControl { allowed } => {
                if let Some(h) = &self.host {
                    h.allow_control.send_replace(allowed);
                }
            }
            UiCommand::ViewerConnect { device_id, password } => self.start_viewer(device_id, password),
            UiCommand::ViewerDisconnect => self.stop_viewer(),
            UiCommand::ViewerInput { event } => self.to_viewer(ViewerCommand::Input(event)),
            UiCommand::ViewerRequestKeyframe => self.to_viewer(ViewerCommand::RequestKeyframe),
            UiCommand::ViewerReport { frames_dropped, decode_queue } => {
                self.to_viewer(ViewerCommand::Report { frames_dropped, decode_queue })
            }
            UiCommand::PermissionsCheck => self.report_permissions(),
            UiCommand::PermissionsRequest => {
                capture::request_permission();
                self.report_permissions();
            }
            UiCommand::Shutdown => {}
        }
    }

    fn report_permissions(&self) {
        self.ui
            .event(UiEvent::Permissions { screen: capture::permission_granted(), input: input::permission_granted() });
    }

    fn to_viewer(&self, cmd: ViewerCommand) {
        if let Some(v) = &self.viewer
            && v.commands.try_send(cmd).is_err()
        {
            debug!("viewer command queue full; command dropped");
        }
    }

    fn new_password(&self) -> Option<String> {
        match handshake::generate_password(&self.rng) {
            Ok(p) => Some(p),
            Err(e) => {
                self.error("rng", e.to_string());
                None
            }
        }
    }

    fn start_host(&mut self, allow_control: bool) {
        if let Some(h) = &self.host {
            h.allow_control.send_replace(allow_control);
            self.ui.event(UiEvent::HostReady { device_id: self.device_id.to_string(), password: h.password.clone() });
            return;
        }
        let Some(password) = self.new_password() else { return };
        let (allow_tx, _) = watch::channel(allow_control);
        self.ui.event(UiEvent::HostReady { device_id: self.device_id.to_string(), password: password.clone() });
        self.host = Some(HostState {
            password,
            failures: 0,
            allow_control: allow_tx,
            active: None,
            cancel: CancellationToken::new(),
        });
    }

    fn stop_host(&mut self) {
        if let Some(h) = self.host.take() {
            h.cancel.cancel();
        }
    }

    fn rotate_password(&mut self) {
        let Some(password) = self.new_password() else { return };
        if let Some(h) = &mut self.host {
            h.password = password.clone();
            h.failures = 0;
            self.ui.event(UiEvent::HostReady { device_id: self.device_id.to_string(), password });
        }
    }

    fn start_viewer(&mut self, device_id: String, password: String) {
        self.stop_viewer();
        let host_id = match DeviceId::try_from(device_id) {
            Ok(id) => id,
            Err(e) => return self.error("invalid_device_id", e),
        };
        if host_id == self.device_id {
            return self.error("invalid_device_id", "cannot connect to this device itself");
        }
        if handshake::normalize_password(&password).is_empty() {
            return self.error("invalid_password", "password is empty");
        }
        let (tx, rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();
        let ctx = ViewerContext {
            viewer_id: self.device_id.clone(),
            host_id,
            password,
            signaling: self.signaling.clone(),
            router: self.router.clone(),
            ui: self.ui.clone(),
            stun: self.stun.clone(),
            udp_port: 0,
            force_relay: self.cfg.force_relay,
            cancel: cancel.clone(),
        };
        tokio::spawn(viewer::run(ctx, rx));
        self.viewer = Some(ViewerState { commands: tx, cancel });
    }

    fn stop_viewer(&mut self) {
        if let Some(v) = self.viewer.take() {
            // Graceful first (sends Bye), then hard cancel as a backstop.
            let _ = v.commands.try_send(ViewerCommand::Disconnect);
            let cancel = v.cancel;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                cancel.cancel();
            });
        }
    }

    fn on_host_report(&mut self, session: SessionId, outcome: HostOutcome) {
        let Some(h) = &mut self.host else { return };
        if h.active.as_ref().is_some_and(|a| a.session == session) {
            h.active = None;
        }
        if let HostOutcome::AuthFailed = outcome {
            h.failures += 1;
            if h.failures >= MAX_AUTH_FAILURES {
                warn!(failures = h.failures, "too many failed attempts; rotating password");
                self.rotate_password();
            }
        }
    }

    async fn on_signaling(&mut self, ev: SignalingEvent) {
        match ev {
            SignalingEvent::Connected { device_id, stun } => {
                let mut merged = self.cfg.stun.clone();
                merged.extend(stun.into_iter().filter(|s| !self.cfg.stun.contains(s)));
                self.stun = merged;
                self.ui.event(UiEvent::Signaling {
                    connected: true,
                    device_id: Some(device_id.to_string()),
                    reason: None,
                });
            }
            SignalingEvent::Disconnected { reason } => {
                self.ui.event(UiEvent::Signaling { connected: false, device_id: None, reason: Some(reason) });
            }
            SignalingEvent::Signal { from, session_id, payload } => {
                let signal: PeerSignal = match decode_binary(payload.as_slice()) {
                    Ok(s) => s,
                    Err(e) => return debug!(%from, error = %e, "undecodable peer signal"),
                };
                if let PeerSignal::Offer(offer) = signal {
                    if !self.router.contains(&session_id) {
                        self.on_offer(from, session_id, offer).await;
                    }
                    return;
                }
                if !self.router.deliver(&session_id, SessionInbox::Peer { from, signal }) {
                    debug!(session = %session_id, "signal for unknown session");
                }
            }
            SignalingEvent::RelayAllocated { session_id, url, token } => {
                self.router.deliver(&session_id, SessionInbox::Relay { url, token });
            }
            SignalingEvent::PeerUnavailable { session_id, .. } => {
                self.router.deliver(&session_id, SessionInbox::PeerUnavailable);
            }
            SignalingEvent::ServerError { code, message } => {
                warn!(?code, %message, "signaling error");
                self.error("signaling", message);
            }
        }
    }

    async fn reject(&self, to: DeviceId, session: SessionId, reason: RejectReason) {
        let Ok(payload) = remoti_proto::encode_binary(&PeerSignal::Reject(reason)) else { return };
        let msg = ClientMessage::Signal { to, session_id: session, payload: Bytes(payload) };
        if self.signaling.send(msg).await.is_err() {
            debug!("could not send reject: signaling closed");
        }
    }

    async fn on_offer(&mut self, from: DeviceId, session: SessionId, offer: remoti_proto::peer::Offer) {
        let Some(h) = &mut self.host else {
            return self.reject(from, session, RejectReason::Declined).await;
        };
        if offer.viewer != from {
            // The signaling server authenticated `from`; the offer must agree.
            return self.reject(from, session, RejectReason::Declined).await;
        }
        if let Some(active) = &h.active {
            if active.viewer != from {
                return self.reject(from, session, RejectReason::Busy).await;
            }
            // Same viewer reconnecting (path change, crash): the old session is
            // stale, so replace it instead of locking the viewer out.
            info!(viewer = %from, "viewer reconnected; replacing its previous session");
            active.cancel.cancel();
            h.active = None;
        }
        let cancel = h.cancel.child_token();
        let ctx = HostContext {
            host_id: self.device_id.clone(),
            password: h.password.clone(),
            signaling: self.signaling.clone(),
            ui: self.ui.clone(),
            stun: self.stun.clone(),
            udp_port: self.cfg.udp_port,
            capture: self.cfg.capture,
            rate: self.cfg.rate,
            allow_control: h.allow_control.subscribe(),
            cancel: cancel.clone(),
        };
        h.active = Some(ActiveHostSession { session: session.clone(), viewer: from.clone(), cancel });
        let route = self.router.register(session.clone());
        let reports = self.host_reports.clone();
        info!(viewer = %from, session = %session, "incoming session");
        tokio::spawn(async move {
            let outcome = host::run(ctx, route, from, offer).await;
            let _ = reports.send((session, outcome)).await;
        });
    }
}
