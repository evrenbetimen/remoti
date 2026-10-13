//! End-to-end: signaling + STUN + relay in-process, a synthetic-capture host
//! and a headless viewer agent. Asserts that encrypted video reaches the
//! viewer over direct QUIC and over the forced WebSocket relay, and that a
//! wrong password is rejected.

use std::net::SocketAddr;
use std::time::Duration;

use remoti_core::agent::{self, AgentConfig};
use remoti_core::capture::CaptureBackend;
use remoti_core::congestion::RateConfig;
use remoti_core::encoder::CodecPreference;
use remoti_core::ipc::{SessionRole, SessionState, UiCommand, UiEvent, UiOutputs, ui_channel};
use remoti_core::transport::LinkKind;
use remoti_proto::peer::VideoCodec;
use remoti_signaling::state::{AppState, Config};
use remoti_signaling::{app, stun};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const STEP: Duration = Duration::from_secs(20);
/// rav1e without its assembly paths is slow on small CI runners; the first
/// AV1 frames only have to arrive, not arrive in real time.
const AV1_STREAM: Duration = Duration::from_secs(90);

struct Server {
    ws_url: String,
    stun: SocketAddr,
    cancel: CancellationToken,
}

async fn start_server() -> Server {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cancel = CancellationToken::new();
    let udp = UdpSocket::bind("127.0.0.1:0").await.expect("bind stun");
    let stun_addr = udp.local_addr().expect("stun addr");
    tokio::spawn(stun::serve(udp, cancel.clone()));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind http");
    let http = listener.local_addr().expect("http addr");
    let state = AppState::new(Config {
        relay_url: format!("ws://{http}/relay"),
        stun: vec![stun_addr.to_string()],
        relay_pair_timeout: Duration::from_secs(10),
        relay_idle_timeout: Duration::from_secs(30),
        relay_max_lifetime: Duration::from_secs(600),
    });
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).with_graceful_shutdown(shutdown.cancelled_owned()).await;
    });
    Server { ws_url: format!("ws://{http}/ws"), stun: stun_addr, cancel }
}

struct Agent {
    commands: mpsc::Sender<UiCommand>,
    out: UiOutputs,
    cancel: CancellationToken,
}

fn spawn_agent(server: &Server, force_relay: bool) -> Agent {
    let cfg = AgentConfig {
        signaling_url: server.ws_url.clone(),
        stun: vec![server.stun.to_string()],
        udp_port: 0,
        capture: CaptureBackend::Synthetic,
        codec: CodecPreference::Auto,
        rate: RateConfig { max_bitrate_bps: 4_000_000, max_fps: 30, ..RateConfig::default() },
        data_dir: None,
        ephemeral_identity: true,
        force_relay,
    };
    let (sink, out) = ui_channel();
    let (tx, rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    tokio::spawn(agent::run(cfg, sink, rx, cancel.clone()));
    Agent { commands: tx, out, cancel }
}

impl Agent {
    async fn wait_event<T>(&mut self, mut pick: impl FnMut(&UiEvent) -> Option<T>) -> T {
        timeout(STEP, async {
            loop {
                tokio::select! {
                    ev = self.out.events.recv() => {
                        let ev = ev.expect("agent event stream closed");
                        if let Some(v) = pick(&ev) {
                            return v;
                        }
                    }
                    // Keep draining video so the agent never sees a full queue.
                    Some(_) = self.out.video.recv() => {}
                }
            }
        })
        .await
        .expect("timed out waiting for agent event")
    }

    /// Waits until the viewer is connected, has a video config and has
    /// received `frames` video packets, recording what it saw on the way.
    async fn wait_stream(&mut self, frames: usize, limit: Duration) -> StreamSeen {
        timeout(limit, async {
            let mut seen = StreamSeen::default();
            let mut connected = false;
            let mut packets = 0;
            while !(connected && seen.codec.is_some() && packets >= frames) {
                tokio::select! {
                    ev = self.out.events.recv() => match ev.expect("agent event stream closed") {
                        UiEvent::Session { role: SessionRole::Viewer, state: SessionState::Connected, transport, .. } => {
                            connected = true;
                            seen.transport = transport;
                        }
                        UiEvent::Session { state: SessionState::Ended, reason, .. } => panic!("session ended: {reason:?}"),
                        UiEvent::VideoConfig { codec, .. } => seen.codec = Some(codec),
                        _ => {}
                    },
                    Some(pkt) = self.out.video.recv() => {
                        assert!(!pkt.data.is_empty());
                        if packets == 0 {
                            seen.first_keyframe = pkt.keyframe;
                        }
                        packets += 1;
                    }
                }
            }
            seen
        })
        .await
        .expect("timed out waiting for the video stream")
    }
}

#[derive(Default)]
struct StreamSeen {
    transport: Option<LinkKind>,
    codec: Option<&'static str>,
    first_keyframe: bool,
}

async fn start_host(server: &Server) -> (Agent, String, String) {
    let mut host = spawn_agent(server, false);
    host.commands.send(UiCommand::HostStart { allow_control: true }).await.expect("host command");
    let (id, password) = host
        .wait_event(|ev| match ev {
            UiEvent::HostReady { device_id, password } => Some((device_id.clone(), password.clone())),
            _ => None,
        })
        .await;
    (host, id, password)
}

async fn run_session(force_relay: bool, expected: LinkKind, codec: VideoCodec) {
    let server = start_server().await;
    let (host, id, password) = start_host(&server).await;
    let mut viewer = spawn_agent(&server, force_relay);
    viewer.wait_event(|ev| matches!(ev, UiEvent::Signaling { connected: true, .. }).then_some(())).await;
    viewer
        .commands
        .send(UiCommand::ViewerConnect { device_id: id, password, codecs: vec![codec] })
        .await
        .expect("viewer command");

    let limit = if codec == VideoCodec::Av1 { AV1_STREAM } else { STEP };
    let seen = viewer.wait_stream(3, limit).await;
    assert_eq!(seen.transport, Some(expected));
    assert_eq!(seen.codec, Some(remoti_core::ipc::webcodecs_codec(codec)));
    assert!(seen.first_keyframe, "first frame must be a keyframe");

    viewer.commands.send(UiCommand::ViewerDisconnect).await.expect("viewer command");
    viewer
        .wait_event(|ev| {
            matches!(ev, UiEvent::Session { role: SessionRole::Viewer, state: SessionState::Ended, .. }).then_some(())
        })
        .await;

    viewer.cancel.cancel();
    host.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_over_direct_quic() {
    run_session(false, LinkKind::Quic, VideoCodec::H264).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn av1_when_viewer_only_decodes_av1() {
    run_session(false, LinkKind::Quic, VideoCodec::Av1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_over_forced_relay() {
    run_session(true, LinkKind::Relay, VideoCodec::H264).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_password_is_rejected() {
    let server = start_server().await;
    let (host, id, _password) = start_host(&server).await;
    let mut viewer = spawn_agent(&server, false);
    viewer.wait_event(|ev| matches!(ev, UiEvent::Signaling { connected: true, .. }).then_some(())).await;
    viewer
        .commands
        .send(UiCommand::ViewerConnect { device_id: id, password: "WRONGPW1".into(), codecs: vec![VideoCodec::H264] })
        .await
        .expect("viewer command");
    let reached_connected = viewer
        .wait_event(|ev| match ev {
            UiEvent::Session { role: SessionRole::Viewer, state: SessionState::Connected, .. } => Some(true),
            UiEvent::Session { role: SessionRole::Viewer, state: SessionState::Ended, .. } => Some(false),
            _ => None,
        })
        .await;
    assert!(!reached_connected, "a wrong password must never yield a connected session");
    viewer.cancel.cancel();
    host.cancel.cancel();
    server.cancel.cancel();
}
