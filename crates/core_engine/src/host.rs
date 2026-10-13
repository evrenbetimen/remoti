//! Host side of a session: answer the viewer's offer, bring up QUIC (or the
//! relay fallback), then stream the screen and apply sanitized input.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use remoti_proto::bytes::Bytes;
use remoti_proto::encode_binary;
use remoti_proto::input::InputEvent;
use remoti_proto::peer::{Offer, PeerSignal, RejectReason, VideoCodec};
use remoti_proto::signaling::{ClientMessage, DeviceId, SessionId};
use remoti_proto::wire::{VideoFrame, WireMessage};
use ring::rand::SystemRandom;
use tokio::sync::{mpsc, watch};
use tokio::time::{interval, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::capture::CaptureBackend;
use crate::congestion::{RateConfig, RateController, RateSample};
use crate::encoder::{self, CodecPreference};
use crate::handshake::{self, HandshakeError};
use crate::input::{InputInjector, InputPolicy, InputSanitizer, NullInjector, SafeEvent};
use crate::ipc::{SessionRole, SessionState, UiEvent, UiSink};
use crate::network::{self, HostCertificate};
use crate::relay;
use crate::router::{Route, SessionInbox};
use crate::signaling::SignalingSender;
use crate::stream::{self, StreamOutput, StreamParams};
use crate::transport::{LinkError, LinkKind, SecureLink, VideoSend};

const QUIC_WAIT: Duration = Duration::from_secs(8);
const RELAY_WAIT: Duration = Duration::from_secs(12);
const CONFIRM_WAIT: Duration = Duration::from_secs(5);
const CONTROL_TICK: Duration = Duration::from_millis(250);
const PEER_SILENCE: Duration = Duration::from_secs(10);

pub struct HostContext {
    pub host_id: DeviceId,
    pub password: String,
    pub signaling: SignalingSender,
    pub ui: UiSink,
    pub stun: Vec<String>,
    pub udp_port: u16,
    pub capture: CaptureBackend,
    pub codec: CodecPreference,
    pub rate: RateConfig,
    pub allow_control: watch::Receiver<bool>,
    pub cancel: CancellationToken,
}

#[derive(Debug)]
pub enum HostOutcome {
    Ended,
    /// Wrong password (or tampering); counts towards the lockout.
    AuthFailed,
}

pub async fn run(ctx: HostContext, mut route: Route, viewer: DeviceId, offer: Offer) -> HostOutcome {
    let session = route.id.clone();
    let emit = |state: SessionState, transport: Option<LinkKind>, reason: Option<String>| {
        ctx.ui.event(UiEvent::Session {
            role: SessionRole::Host,
            state,
            peer: Some(viewer.to_string()),
            transport,
            reason,
        });
    };
    emit(SessionState::Connecting, None, None);

    let established = tokio::select! {
        _ = ctx.cancel.cancelled() => Err(SessionError::Cancelled),
        r = establish(&ctx, &mut route, &viewer, &offer) => r,
    };
    let result = match established {
        Ok((link, codec)) => {
            emit(SessionState::Connected, Some(link.kind), None);
            // The stream loop watches the cancel token itself so it can say Bye.
            stream_loop(&ctx, &mut route, link, codec).await
        }
        Err(e) => Err(e),
    };
    if !matches!(result, Err(SessionError::Auth)) {
        let _ = send_peer(&ctx.signaling, &viewer, &session, &PeerSignal::Bye).await;
    }
    match result {
        Ok(()) | Err(SessionError::Cancelled) => {
            emit(SessionState::Ended, None, None);
            HostOutcome::Ended
        }
        Err(SessionError::Auth) => {
            warn!(%viewer, "viewer failed password authentication");
            // Tell the viewer explicitly so it stops retrying with a bad password.
            let _ = send_peer(&ctx.signaling, &viewer, &session, &PeerSignal::Reject(RejectReason::AuthFailed)).await;
            emit(SessionState::Ended, None, Some("authentication failed".into()));
            HostOutcome::AuthFailed
        }
        Err(e) => {
            warn!(%viewer, error = %e, "host session ended with error");
            emit(SessionState::Ended, None, Some(e.to_string()));
            HostOutcome::Ended
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum SessionError {
    #[error("cancelled")]
    Cancelled,
    #[error("authentication failed")]
    Auth,
    #[error("no video codec in common with the viewer")]
    NoCommonCodec,
    #[error("handshake: {0}")]
    Handshake(HandshakeError),
    #[error("network: {0}")]
    Network(#[from] network::NetworkError),
    #[error("relay: {0}")]
    Relay(#[from] relay::RelayError),
    #[error("link: {0}")]
    Link(LinkError),
    #[error("viewer unreachable over udp and tcp")]
    Unreachable,
    #[error("capture: {0}")]
    Capture(String),
    #[error("signaling unavailable")]
    Signaling,
}

impl From<LinkError> for SessionError {
    fn from(e: LinkError) -> Self {
        match e {
            LinkError::Handshake(HandshakeError::Confirmation) => SessionError::Auth,
            other => SessionError::Link(other),
        }
    }
}

impl From<HandshakeError> for SessionError {
    fn from(e: HandshakeError) -> Self {
        match e {
            HandshakeError::Pake | HandshakeError::Confirmation => SessionError::Auth,
            other => SessionError::Handshake(other),
        }
    }
}

async fn send_peer(
    sig: &SignalingSender,
    to: &DeviceId,
    session: &SessionId,
    signal: &PeerSignal,
) -> Result<(), SessionError> {
    let payload = encode_binary(signal).map_err(|_| SessionError::Signaling)?;
    sig.send(ClientMessage::Signal { to: to.clone(), session_id: session.clone(), payload: Bytes(payload) })
        .await
        .map_err(|_| SessionError::Signaling)
}

async fn establish(
    ctx: &HostContext,
    route: &mut Route,
    viewer: &DeviceId,
    offer: &Offer,
) -> Result<(SecureLink, VideoCodec), SessionError> {
    let session = route.id.clone();
    let Some(codec) = encoder::negotiate(ctx.codec, &offer.capabilities.codecs) else {
        let _ = send_peer(&ctx.signaling, viewer, &session, &PeerSignal::Reject(RejectReason::NoCommonCodec)).await;
        return Err(SessionError::NoCommonCodec);
    };
    info!(?codec, "negotiated video codec");
    let rng = SystemRandom::new();
    let socket = network::bind_udp(ctx.udp_port).await?;
    let candidates = network::gather_candidates(&socket, &ctx.stun, &rng).await;
    debug!(?candidates, "host candidates");
    let cert = HostCertificate::generate()?;
    let (answer, secrets) = match handshake::host_respond(
        &rng,
        &session,
        &ctx.host_id,
        &ctx.password,
        offer,
        candidates,
        cert.sha256,
        codec,
    ) {
        Ok(v) => v,
        Err(HandshakeError::Version(v)) => {
            let _ =
                send_peer(&ctx.signaling, viewer, &session, &PeerSignal::Reject(RejectReason::VersionMismatch)).await;
            return Err(SessionError::Handshake(HandshakeError::Version(v)));
        }
        Err(e) => return Err(e.into()),
    };
    let secrets = Arc::new(secrets);
    send_peer(&ctx.signaling, viewer, &session, &PeerSignal::Answer(answer)).await?;

    // Race direct QUIC against a relay allocation the viewer may request.
    let quic = network::QuicHost::new(socket, &cert, &offer.candidates)?;
    let raw = tokio::select! {
        r = quic.accept(QUIC_WAIT) => match r {
            Ok(link) => link,
            Err(e) => {
                info!(error = %e, "direct path failed; waiting for relay");
                wait_relay(route).await?
            }
        },
        r = wait_relay(route) => r?,
    };
    let link = match SecureLink::establish(raw, secrets, CONFIRM_WAIT).await {
        Ok(l) => l,
        // A viewer that completes SPAKE2 but never proves the password is
        // treated as a failed guess, so stalling cannot dodge the lockout.
        Err(LinkError::ConfirmTimeout | LinkError::Unconfirmed) => return Err(SessionError::Auth),
        Err(e) => return Err(e.into()),
    };
    info!(transport = ?link.kind, "host session secured");
    Ok((link, codec))
}

async fn wait_relay(route: &mut Route) -> Result<crate::transport::RawLink, SessionError> {
    let deadline = Instant::now() + QUIC_WAIT + RELAY_WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, route.inbox.recv()).await {
            Ok(Some(SessionInbox::Relay { url, token })) => return Ok(relay::connect(&url, &token, RELAY_WAIT).await?),
            Ok(Some(SessionInbox::Peer { signal: PeerSignal::Bye, .. })) => return Err(SessionError::Cancelled),
            Ok(Some(other)) => debug!(?other, "ignoring signal while connecting"),
            Ok(None) => return Err(SessionError::Cancelled),
            Err(_) => return Err(SessionError::Unreachable),
        }
    }
}

fn spawn_injector() -> (std::sync::mpsc::SyncSender<SafeEvent>, Option<String>) {
    let (tx, rx) = std::sync::mpsc::sync_channel::<SafeEvent>(512);
    let (mut injector, err): (Box<dyn InputInjector>, Option<String>) = match crate::input::native() {
        Ok(i) => (i, None),
        Err(e) => (Box::new(NullInjector), Some(e.to_string())),
    };
    let spawned = std::thread::Builder::new().name("remoti-input".into()).spawn(move || {
        let mut failures = 0u32;
        while let Ok(ev) = rx.recv() {
            if let Err(e) = injector.inject(&ev) {
                failures += 1;
                // Log the first failures, then every 100th, to avoid log floods.
                if failures <= 3 || failures % 100 == 0 {
                    warn!(error = %e, failures, "input injection failed");
                }
            }
        }
    });
    match spawned {
        Ok(_) => (tx, err),
        Err(e) => {
            let (dead_tx, _) = std::sync::mpsc::sync_channel(1);
            (dead_tx, Some(format!("input thread: {e}")))
        }
    }
}

fn forward(tx: &std::sync::mpsc::SyncSender<SafeEvent>, events: Vec<SafeEvent>) {
    for ev in events {
        if let Err(e) = tx.try_send(ev) {
            debug!(error = %e, "input queue full; event dropped");
        }
    }
}

async fn stream_loop(
    ctx: &HostContext,
    route: &mut Route,
    mut link: SecureLink,
    codec: VideoCodec,
) -> Result<(), SessionError> {
    let mut allow = ctx.allow_control.clone();
    let initially_allowed = *allow.borrow_and_update();
    let mut sanitizer =
        InputSanitizer::new(InputPolicy { allow_control: initially_allowed, ..InputPolicy::default() }, Instant::now());
    let (input_tx, input_err) = spawn_injector();
    if let Some(msg) = input_err {
        ctx.ui.event(UiEvent::Error { code: "input_unavailable", message: msg });
    }

    let now = Instant::now();
    let mut controller = RateController::new(ctx.rate, now);
    let initial = controller.current();
    let params = StreamParams::new(initial.bitrate_bps, initial.fps);
    let (frames_tx, mut frames_rx) = mpsc::channel(stream::OUTPUT_QUEUE);
    let capture_thread = stream::spawn(ctx.capture, codec, params.clone(), frames_tx)
        .map_err(|e| SessionError::Capture(e.to_string()))?;

    link.sender.send(&WireMessage::ControlPermission { allowed: initially_allowed }).await?;

    let mut tick = interval(CONTROL_TICK);
    let mut seq = 0u64;
    let mut last_report = (0u32, 0u16);
    let mut link_drops = 0u32;
    let mut last_rx = Instant::now();
    let mut ping_rtt = Duration::ZERO;
    let mut ping_nonce = 0u64;
    let mut ticks = 0u64;
    let mut sent_bytes = 0u64;
    let session_start = Instant::now();

    let result: Result<(), SessionError> = loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => break Ok(()),
            out = frames_rx.recv() => match out {
                Some(StreamOutput::Frame { pts_us, keyframe, data }) => {
                    sent_bytes += data.len() as u64;
                    let frame = VideoFrame { seq, pts_us, keyframe, data: Bytes(data) };
                    seq += 1;
                    if link.sender.try_send_video(frame)? == VideoSend::Dropped {
                        link_drops += 1;
                        params.request_keyframe();
                    }
                }
                Some(StreamOutput::Config { codec, width, height }) => {
                    let fps = params.fps.load(Ordering::Relaxed);
                    link.sender.send(&WireMessage::StreamConfig { codec, width, height, fps }).await?;
                }
                Some(StreamOutput::Fatal(msg)) => break Err(SessionError::Capture(msg)),
                None => break Err(SessionError::Capture("capture thread exited".into())),
            },
            msg = link.recv() => {
                let msg = match msg {
                    Ok(m) => m,
                    Err(LinkError::Closed) => break Ok(()),
                    Err(e) => break Err(e.into()),
                };
                last_rx = Instant::now();
                match msg {
                    WireMessage::Input(ev) => forward(&input_tx, sanitizer.process(ev, Instant::now())),
                    WireMessage::KeyframeRequest => params.request_keyframe(),
                    WireMessage::ReceiverReport { frames_dropped, decode_queue, .. } => last_report = (frames_dropped, decode_queue),
                    WireMessage::Ping { nonce, sent_us } => link.sender.send(&WireMessage::Pong { nonce, sent_us }).await?,
                    WireMessage::Pong { sent_us, .. } => {
                        let now_us = session_start.elapsed().as_micros() as u64;
                        ping_rtt = Duration::from_micros(now_us.saturating_sub(sent_us));
                    }
                    WireMessage::Bye { reason } => {
                        info!(%reason, "viewer closed the session");
                        break Ok(());
                    }
                    other => debug!(?other, "unexpected message from viewer"),
                }
            }
            changed = allow.changed() => {
                if changed.is_err() { break Ok(()) }
                let allowed = *allow.borrow_and_update();
                forward(&input_tx, sanitizer.set_allow_control(allowed));
                link.sender.send(&WireMessage::ControlPermission { allowed }).await?;
            }
            signal = route.inbox.recv() => match signal {
                Some(SessionInbox::Peer { signal: PeerSignal::Bye, .. }) | None => break Ok(()),
                Some(other) => debug!(?other, "ignoring signal during session"),
            },
            _ = tick.tick() => {
                ticks += 1;
                if last_rx.elapsed() > PEER_SILENCE {
                    break Err(SessionError::Link(LinkError::Closed));
                }
                if ticks % 4 == 0 {
                    ping_nonce += 1;
                    let sent_us = session_start.elapsed().as_micros() as u64;
                    link.sender.send(&WireMessage::Ping { nonce: ping_nonce, sent_us }).await?;
                }
                let stats = *link.stats.borrow();
                let source_drops = params.source_drops.swap(0, Ordering::Relaxed) as u32;
                let sample = RateSample {
                    rtt: stats.rtt.max(ping_rtt),
                    loss: stats.loss,
                    video_backlog: stats.video_backlog,
                    frames_dropped: std::mem::take(&mut link_drops) + source_drops + std::mem::take(&mut last_report.0),
                    decode_queue: last_report.1,
                    sent_bps: (std::mem::take(&mut sent_bytes) as f64 * 8.0 / CONTROL_TICK.as_secs_f64()) as u32,
                };
                if let Some(d) = controller.on_sample(sample, Instant::now()) {
                    debug!(bitrate = d.bitrate_bps, fps = d.fps, "rate update");
                    params.bitrate_bps.store(d.bitrate_bps, Ordering::Relaxed);
                    params.fps.store(d.fps, Ordering::Relaxed);
                }
                if ticks % 4 == 0 {
                    let d = controller.current();
                    ctx.ui.event(UiEvent::Stats {
                        role: SessionRole::Host,
                        bitrate_bps: d.bitrate_bps,
                        fps: d.fps,
                        rtt_ms: sample.rtt.as_millis() as u32,
                        loss: sample.loss,
                        transport: link.kind,
                    });
                }
            }
        }
    };

    // Teardown: stop capture, release any held keys/buttons, say goodbye.
    params.stop.store(true, Ordering::Relaxed);
    forward(&input_tx, sanitizer.process(InputEvent::ReleaseAll, Instant::now()));
    let _ =
        timeout(Duration::from_millis(300), link.sender.send(&WireMessage::Bye { reason: "host ended".into() })).await;
    drop(frames_rx);
    if tokio::task::spawn_blocking(move || capture_thread.join()).await.is_err() {
        warn!("capture thread join failed");
    }
    link.shutdown().await;
    result
}
