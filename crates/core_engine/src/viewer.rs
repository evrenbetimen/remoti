//! Viewer side: offer, connect (QUIC first, relay fallback), receive video for
//! the UI, forward local input, and reconnect automatically on path loss.

use std::sync::Arc;
use std::time::{Duration, Instant};

use remoti_proto::bytes::Bytes;
use remoti_proto::encode_binary;
use remoti_proto::input::InputEvent;
use remoti_proto::peer::{Answer, Capabilities, PeerSignal, VideoCodec};
use remoti_proto::signaling::{ClientMessage, DeviceId, SessionId};
use remoti_proto::wire::WireMessage;
use ring::rand::{SecureRandom as _, SystemRandom};
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::handshake::{HandshakeError, ViewerHandshake};
use crate::ipc::{SessionRole, SessionState, UiEvent, UiSink, VideoPacket, webcodecs_codec};
use crate::network;
use crate::relay;
use crate::router::{Router, SessionInbox};
use crate::signaling::SignalingSender;
use crate::transport::{LinkError, LinkKind, SecureLink};

const ANSWER_WAIT: Duration = Duration::from_secs(15);
const QUIC_WAIT: Duration = Duration::from_secs(6);
const RELAY_WAIT: Duration = Duration::from_secs(12);
const CONFIRM_WAIT: Duration = Duration::from_secs(5);
const MAX_RETRIES: u32 = 6;
const PEER_SILENCE: Duration = Duration::from_secs(10);
const KEYFRAME_REQUEST_GAP: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub enum ViewerCommand {
    Input(InputEvent),
    RequestKeyframe,
    Report { frames_dropped: u32, decode_queue: u16 },
    Disconnect,
}

pub struct ViewerContext {
    pub viewer_id: DeviceId,
    pub host_id: DeviceId,
    pub password: String,
    pub signaling: SignalingSender,
    pub router: Router,
    pub ui: UiSink,
    pub stun: Vec<String>,
    pub udp_port: u16,
    /// Skip direct QUIC (networks known to block UDP).
    pub force_relay: bool,
    /// Codecs the UI can decode, offered to the host.
    pub codecs: Vec<VideoCodec>,
    pub cancel: CancellationToken,
}

#[derive(Debug, thiserror::Error)]
enum AttemptError {
    #[error("host rejected the connection: {0:?}")]
    Rejected(remoti_proto::peer::RejectReason),
    #[error("wrong password")]
    Auth,
    #[error("host is offline")]
    HostOffline,
    #[error("no answer from host")]
    NoAnswer,
    #[error("handshake: {0}")]
    Handshake(HandshakeError),
    #[error("peer unreachable over udp and tcp")]
    Unreachable,
    #[error("relay: {0}")]
    Relay(#[from] relay::RelayError),
    #[error("link: {0}")]
    Link(LinkError),
    #[error("signaling unavailable")]
    Signaling,
}

impl AttemptError {
    /// Errors that a retry cannot fix (and that would burn the host's lockout).
    fn is_final(&self) -> bool {
        match self {
            // The host may be finishing a previous session; worth a retry.
            AttemptError::Rejected(remoti_proto::peer::RejectReason::Busy) => false,
            AttemptError::Rejected(_) | AttemptError::Auth | AttemptError::Handshake(_) => true,
            _ => false,
        }
    }
}

impl From<LinkError> for AttemptError {
    fn from(e: LinkError) -> Self {
        match e {
            LinkError::Handshake(HandshakeError::Confirmation) => AttemptError::Auth,
            other => AttemptError::Link(other),
        }
    }
}

enum StreamEnd {
    UserClosed,
    HostClosed,
    PathLost(String),
}

pub async fn run(ctx: ViewerContext, mut commands: mpsc::Receiver<ViewerCommand>) {
    let emit = |state: SessionState, transport: Option<LinkKind>, reason: Option<String>| {
        ctx.ui.event(UiEvent::Session {
            role: SessionRole::Viewer,
            state,
            peer: Some(ctx.host_id.to_string()),
            transport,
            reason,
        });
    };
    emit(SessionState::Connecting, None, None);

    let rng = SystemRandom::new();
    let mut failures = 0u32;
    let final_reason = loop {
        // `attempt` observes the cancel token itself so it can say Bye.
        let attempt = attempt(&ctx, &rng, &mut commands, &emit).await;
        let reason = match attempt {
            Ok(StreamEnd::UserClosed) => break None,
            Ok(StreamEnd::HostClosed) => break Some("host ended the session".to_owned()),
            Ok(StreamEnd::PathLost(why)) => {
                // We were connected: reset the retry budget.
                failures = 0;
                why
            }
            Err(e) if e.is_final() => break Some(e.to_string()),
            Err(e) => e.to_string(),
        };
        failures += 1;
        if failures > MAX_RETRIES {
            break Some(format!("giving up after {MAX_RETRIES} attempts: {reason}"));
        }
        let backoff = Duration::from_millis(500 * 2u64.pow(failures.min(5) - 1)).min(Duration::from_secs(10));
        warn!(%reason, attempt = failures, ?backoff, "viewer reconnecting");
        emit(SessionState::Reconnecting, None, Some(reason));
        tokio::select! {
            _ = ctx.cancel.cancelled() => break None,
            _ = sleep(backoff) => {}
            cmd = commands.recv() => if matches!(cmd, None | Some(ViewerCommand::Disconnect)) { break None },
        }
    };
    emit(SessionState::Ended, None, final_reason);
}

fn new_session_id(rng: &SystemRandom) -> Option<SessionId> {
    let mut b = [0u8; 16];
    rng.fill(&mut b).ok()?;
    Some(SessionId::from_random(b))
}

async fn send_peer(ctx: &ViewerContext, session: &SessionId, signal: &PeerSignal) -> Result<(), AttemptError> {
    let payload = encode_binary(signal).map_err(|_| AttemptError::Signaling)?;
    ctx.signaling
        .send(ClientMessage::Signal { to: ctx.host_id.clone(), session_id: session.clone(), payload: Bytes(payload) })
        .await
        .map_err(|_| AttemptError::Signaling)
}

async fn attempt(
    ctx: &ViewerContext,
    rng: &SystemRandom,
    commands: &mut mpsc::Receiver<ViewerCommand>,
    emit: &impl Fn(SessionState, Option<LinkKind>, Option<String>),
) -> Result<StreamEnd, AttemptError> {
    let session = new_session_id(rng).ok_or(AttemptError::Handshake(HandshakeError::Rng))?;
    let mut route = ctx.router.register(session.clone());

    let connecting = async {
        let socket = network::bind_udp(ctx.udp_port).await.map_err(|_| AttemptError::Unreachable)?;
        let candidates = network::gather_candidates(&socket, &ctx.stun, rng).await;
        let caps = Capabilities { codecs: ctx.codecs.clone(), max_width: 7680, max_height: 4320, max_fps: 120 };
        let (hs, offer) = ViewerHandshake::start(
            rng,
            session.clone(),
            ctx.viewer_id.clone(),
            ctx.host_id.clone(),
            &ctx.password,
            candidates,
            caps,
        )
        .map_err(AttemptError::Handshake)?;
        send_peer(ctx, &session, &PeerSignal::Offer(offer)).await?;

        let answer: Answer = timeout(ANSWER_WAIT, async {
            loop {
                match route.inbox.recv().await {
                    Some(SessionInbox::Peer { from, signal }) if from == ctx.host_id => match signal {
                        PeerSignal::Answer(a) => return Ok(a),
                        PeerSignal::Reject(remoti_proto::peer::RejectReason::AuthFailed) => {
                            return Err(AttemptError::Auth);
                        }
                        PeerSignal::Reject(r) => return Err(AttemptError::Rejected(r)),
                        PeerSignal::Bye => return Err(AttemptError::NoAnswer),
                        PeerSignal::Offer(_) => continue,
                    },
                    Some(SessionInbox::PeerUnavailable) => return Err(AttemptError::HostOffline),
                    Some(other) => debug!(?other, "ignoring signal before answer"),
                    None => return Err(AttemptError::Signaling),
                }
            }
        })
        .await
        .map_err(|_| AttemptError::NoAnswer)??;

        let secrets = Arc::new(hs.finish(&answer).map_err(|e| match e {
            HandshakeError::Pake => AttemptError::Auth,
            other => AttemptError::Handshake(other),
        })?);
        let mut cert = [0u8; 32];
        cert.copy_from_slice(answer.cert_sha256.as_slice());

        let direct = if ctx.force_relay {
            Err(network::NetworkError::Unreachable)
        } else {
            network::connect_viewer(socket, &answer.candidates, cert, QUIC_WAIT).await
        };
        let raw = match direct {
            Ok(link) => link,
            Err(e) => {
                info!(error = %e, "direct QUIC failed; requesting relay (TCP fallback)");
                ctx.signaling
                    .send(ClientMessage::AllocateRelay { session_id: session.clone(), peer: ctx.host_id.clone() })
                    .await
                    .map_err(|_| AttemptError::Signaling)?;
                let (url, token) = timeout(RELAY_WAIT, async {
                    loop {
                        match route.inbox.recv().await {
                            Some(SessionInbox::Relay { url, token }) => return Ok((url, token)),
                            Some(SessionInbox::PeerUnavailable) => return Err(AttemptError::HostOffline),
                            Some(SessionInbox::Peer { signal: PeerSignal::Bye, .. }) => {
                                return Err(AttemptError::Unreachable);
                            }
                            Some(_) => continue,
                            None => return Err(AttemptError::Signaling),
                        }
                    }
                })
                .await
                .map_err(|_| AttemptError::Unreachable)??;
                relay::connect(&url, &token, RELAY_WAIT).await?
            }
        };

        match SecureLink::establish(raw, secrets, CONFIRM_WAIT).await {
            Ok(link) => Ok(link),
            Err(e) => {
                // The host rejects a bad password via signaling right after
                // closing the link; give that verdict a moment to arrive.
                let verdict = timeout(Duration::from_secs(2), async {
                    loop {
                        match route.inbox.recv().await {
                            Some(SessionInbox::Peer { signal: PeerSignal::Reject(r), .. }) => return Some(r),
                            Some(_) => continue,
                            None => return None,
                        }
                    }
                })
                .await;
                match verdict {
                    Ok(Some(remoti_proto::peer::RejectReason::AuthFailed)) => Err(AttemptError::Auth),
                    Ok(Some(r)) => Err(AttemptError::Rejected(r)),
                    _ => Err(e.into()),
                }
            }
        }
    };
    let link = tokio::select! {
        _ = ctx.cancel.cancelled() => return Ok(StreamEnd::UserClosed),
        r = connecting => r?,
    };
    info!(transport = ?link.kind, "viewer session secured");
    emit(SessionState::Connected, Some(link.kind), None);
    let end = stream_loop(ctx, &mut route, commands, link).await;
    if matches!(end, StreamEnd::UserClosed) {
        let _ = send_peer(ctx, &session, &PeerSignal::Bye).await;
    }
    Ok(end)
}

async fn stream_loop(
    ctx: &ViewerContext,
    route: &mut crate::router::Route,
    commands: &mut mpsc::Receiver<ViewerCommand>,
    mut link: SecureLink,
) -> StreamEnd {
    let started = Instant::now();
    let mut codec = VideoCodec::H264;
    let mut highest_seq: Option<u64> = None;
    let mut ui_drops = 0u32;
    let mut report = (0u32, 0u16);
    let mut last_keyframe_request: Option<Instant> = None;
    let mut last_rx = Instant::now();
    let mut rtt = Duration::ZERO;
    let mut ping = interval(Duration::from_secs(1));
    let mut reports = interval(Duration::from_millis(500));
    let mut nonce = 0u64;
    let mut bytes_window = 0u64;
    let mut frames_window = 0u32;
    let mut window_start = Instant::now();

    let request_keyframe = |link: &SecureLink, last: &mut Option<Instant>| {
        if last.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_GAP) {
            *last = Some(Instant::now());
            let sender = link.sender.clone();
            tokio::spawn(async move {
                let _ = sender.send(&WireMessage::KeyframeRequest).await;
            });
        }
    };

    let end = loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => break StreamEnd::UserClosed,
            msg = link.recv() => {
                let msg = match msg {
                    Ok(m) => m,
                    Err(LinkError::Closed) => break StreamEnd::PathLost("connection lost".into()),
                    Err(e) => break StreamEnd::PathLost(e.to_string()),
                };
                last_rx = Instant::now();
                match msg {
                    WireMessage::VideoFrame(f) => {
                        // Frames on independent streams can arrive out of order.
                        // H.264 P-frames depend on their predecessor, so a late or
                        // missing frame means: drop/deliver, then resync on a keyframe.
                        if let Some(h) = highest_seq {
                            if f.seq <= h && !f.keyframe {
                                request_keyframe(&link, &mut last_keyframe_request);
                                continue;
                            }
                            if f.seq > h + 1 && !f.keyframe {
                                request_keyframe(&link, &mut last_keyframe_request);
                            }
                        }
                        highest_seq = Some(highest_seq.map_or(f.seq, |h| h.max(f.seq)));
                        bytes_window += f.data.len() as u64;
                        frames_window += 1;
                        let pkt = VideoPacket {
                            pts_us: f.pts_us,
                            seq: f.seq as u32,
                            keyframe: f.keyframe,
                            codec,
                            data: f.data.into_vec(),
                        };
                        if !ctx.ui.video(pkt) {
                            ui_drops += 1;
                            request_keyframe(&link, &mut last_keyframe_request);
                        }
                    }
                    WireMessage::StreamConfig { codec: c, width, height, fps } => {
                        codec = c;
                        ctx.ui.event(UiEvent::VideoConfig { codec: webcodecs_codec(c), width, height, fps });
                    }
                    WireMessage::ControlPermission { allowed } => ctx.ui.event(UiEvent::Control { allowed }),
                    WireMessage::Ping { nonce, sent_us } => {
                        if link.sender.send(&WireMessage::Pong { nonce, sent_us }).await.is_err() {
                            break StreamEnd::PathLost("send failed".into());
                        }
                    }
                    WireMessage::Pong { sent_us, .. } => {
                        let now_us = started.elapsed().as_micros() as u64;
                        rtt = Duration::from_micros(now_us.saturating_sub(sent_us));
                    }
                    WireMessage::Bye { reason } => {
                        info!(%reason, "host closed the session");
                        break StreamEnd::HostClosed;
                    }
                    other => debug!(?other, "unexpected message from host"),
                }
            }
            cmd = commands.recv() => match cmd {
                Some(ViewerCommand::Input(ev)) => {
                    if link.sender.send(&WireMessage::Input(ev)).await.is_err() {
                        break StreamEnd::PathLost("send failed".into());
                    }
                }
                Some(ViewerCommand::RequestKeyframe) => request_keyframe(&link, &mut last_keyframe_request),
                Some(ViewerCommand::Report { frames_dropped, decode_queue }) => {
                    report = (report.0.saturating_add(frames_dropped), decode_queue);
                    if frames_dropped > 0 {
                        request_keyframe(&link, &mut last_keyframe_request);
                    }
                }
                Some(ViewerCommand::Disconnect) | None => break StreamEnd::UserClosed,
            },
            signal = route.inbox.recv() => match signal {
                Some(SessionInbox::Peer { signal: PeerSignal::Bye, .. }) => break StreamEnd::HostClosed,
                Some(_) => {}
                None => break StreamEnd::PathLost("signaling closed".into()),
            },
            _ = ping.tick() => {
                if last_rx.elapsed() > PEER_SILENCE {
                    break StreamEnd::PathLost("peer silent".into());
                }
                nonce += 1;
                let sent_us = started.elapsed().as_micros() as u64;
                if link.sender.send(&WireMessage::Ping { nonce, sent_us }).await.is_err() {
                    break StreamEnd::PathLost("send failed".into());
                }
                let secs = window_start.elapsed().as_secs_f64().max(0.001);
                let stats = *link.stats.borrow();
                ctx.ui.event(UiEvent::Stats {
                    role: SessionRole::Viewer,
                    bitrate_bps: (bytes_window as f64 * 8.0 / secs) as u32,
                    fps: (f64::from(frames_window) / secs).round() as u16,
                    rtt_ms: rtt.max(stats.rtt).as_millis() as u32,
                    loss: stats.loss,
                    transport: link.kind,
                });
                bytes_window = 0;
                frames_window = 0;
                window_start = Instant::now();
            }
            _ = reports.tick() => {
                let msg = WireMessage::ReceiverReport {
                    highest_seq: highest_seq.unwrap_or(0),
                    frames_dropped: std::mem::take(&mut report.0) + std::mem::take(&mut ui_drops),
                    decode_queue: report.1,
                };
                if link.sender.send(&msg).await.is_err() {
                    break StreamEnd::PathLost("send failed".into());
                }
            }
        }
    };
    if matches!(end, StreamEnd::UserClosed) {
        let _ = timeout(
            Duration::from_millis(300),
            link.sender.send(&WireMessage::Bye { reason: "viewer disconnected".into() }),
        )
        .await;
    }
    link.shutdown().await;
    end
}
