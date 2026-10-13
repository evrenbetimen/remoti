//! Direct peer-to-peer QUIC transport (quinn + rustls/ring).
//!
//! * Candidate gathering: local interface address + STUN server-reflexive
//!   address, discovered on the *same* UDP socket QUIC later uses so the NAT
//!   mapping is reused.
//! * Hole punching: both peers spray small UDP probes at each other's
//!   candidates from a clone of that socket while QUIC handshakes run.
//! * Authentication: the host's QUIC certificate is ephemeral and self-signed;
//!   the viewer pins its SHA-256, which is bound into the E2EE handshake
//!   transcript (see [`crate::handshake`]).
//! * Streams: one bidirectional control stream (length-prefixed records) plus
//!   one unidirectional stream per video frame, so a lost packet never blocks
//!   newer frames behind it (no head-of-line blocking across frames).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, RecvStream, SendStream, TransportConfig, VarInt};
use remoti_proto::MAX_MESSAGE_BYTES;
use remoti_proto::peer::{Candidate, CandidateKind};
use remoti_proto::stun;
use remoti_proto::wire::Channel;
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom as _, SystemRandom};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::net::UdpSocket;
use tokio::time::{interval, sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::crypto::RECORD_OVERHEAD;
use crate::transport::{FLUSH_TIMEOUT, LinkKind, RawLink, RawLinkPump, TransportStats, raw_link};

pub const ALPN: &[u8] = b"remoti/1";
const SERVER_NAME: &str = "remoti.peer";
const MAX_RECORD: usize = MAX_MESSAGE_BYTES + RECORD_OVERHEAD;
const STUN_TIMEOUT: Duration = Duration::from_millis(1500);
const PUNCH_INTERVAL: Duration = Duration::from_millis(80);
const PUNCH_DURATION: Duration = Duration::from_secs(4);
const STATS_INTERVAL: Duration = Duration::from_millis(250);
const PUNCH_PAYLOAD: &[u8] = b"RMTI";

#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls configuration: {0}")]
    Tls(#[from] rustls::Error),
    #[error("certificate generation: {0}")]
    Cert(#[from] rcgen::Error),
    #[error("quic configuration: {0}")]
    QuicConfig(String),
    #[error("quic connect: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("quic connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("no candidate answered within the deadline")]
    Unreachable,
}

/// Ephemeral, self-signed QUIC identity for one hosting session.
pub struct HostCertificate {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    pub sha256: [u8; 32],
}

impl HostCertificate {
    pub fn generate() -> Result<Self, NetworkError> {
        let certified = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_owned()])?;
        let cert = certified.cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(digest(&SHA256, cert.as_ref()).as_ref());
        Ok(Self { cert, key, sha256 })
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport_config() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(3)));
    // Short idle timeout so dead paths are detected quickly and the session
    // layer can reconnect or fall back.
    if let Ok(idle) = quinn::IdleTimeout::try_from(Duration::from_secs(10)) {
        t.max_idle_timeout(Some(idle));
    }
    t.max_concurrent_uni_streams(VarInt::from_u32(256));
    t.max_concurrent_bidi_streams(VarInt::from_u32(4));
    // BBR tracks bandwidth/RTT instead of reacting only to loss: better for
    // interactive video on lossy Wi-Fi/cellular links.
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    t.initial_rtt(Duration::from_millis(80));
    Arc::new(t)
}

/// Binds the UDP socket used for STUN, punching and QUIC.
pub async fn bind_udp(port: u16) -> Result<UdpSocket, NetworkError> {
    Ok(UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)).await?)
}

/// Best local (LAN) address: "connect" a throwaway UDP socket towards a public
/// address; no packet is sent, the OS just picks the outbound interface.
fn primary_local_ip() -> Option<IpAddr> {
    let s = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified())
}

/// Resolves `host:port` STUN server strings.
async fn resolve(server: &str) -> Option<SocketAddr> {
    let target = server.trim_start_matches("stun:");
    match tokio::net::lookup_host(target).await {
        Ok(mut addrs) => addrs.find(SocketAddr::is_ipv4),
        Err(e) => {
            debug!(server, error = %e, "stun server did not resolve");
            None
        }
    }
}

/// Gathers host + server-reflexive candidates on `socket`.
pub async fn gather_candidates(socket: &UdpSocket, stun_servers: &[String], rng: &SystemRandom) -> Vec<Candidate> {
    let mut out = Vec::new();
    let port = match socket.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            warn!(error = %e, "udp socket has no local address");
            return out;
        }
    };
    if let Some(ip) = primary_local_ip() {
        out.push(Candidate { addr: SocketAddr::new(ip, port), kind: CandidateKind::Host });
    }
    for server in stun_servers.iter().take(3) {
        let Some(server_addr) = resolve(server).await else { continue };
        match stun_query(socket, server_addr, rng).await {
            Some(mapped) => {
                if !out.iter().any(|c| c.addr == mapped) {
                    out.push(Candidate { addr: mapped, kind: CandidateKind::ServerReflexive });
                }
                break;
            }
            None => debug!(%server_addr, "stun query got no answer"),
        }
    }
    out
}

async fn stun_query(socket: &UdpSocket, server: SocketAddr, rng: &SystemRandom) -> Option<SocketAddr> {
    let mut txid = [0u8; 12];
    rng.fill(&mut txid).ok()?;
    let req = stun::encode_request(&txid);
    let mut buf = [0u8; 512];
    let attempt = async {
        let mut retransmit = interval(Duration::from_millis(300));
        loop {
            tokio::select! {
                _ = retransmit.tick() => {
                    if let Err(e) = socket.send_to(&req, server).await {
                        debug!(error = %e, "stun send failed");
                        return None;
                    }
                }
                r = socket.recv_from(&mut buf) => {
                    let Ok((n, from)) = r else { return None };
                    if from == server
                        && let Some(addr) = stun::parse_response(&buf[..n], &txid)
                    {
                        return Some(addr);
                    }
                }
            }
        }
    };
    timeout(STUN_TIMEOUT, attempt).await.ok().flatten()
}

/// Sends hole-punching probes towards `targets` until cancelled or timed out.
fn spawn_punching(socket: StdUdpSocket, targets: Vec<SocketAddr>, cancel: CancellationToken) {
    tokio::spawn(async move {
        let Ok(socket) = socket.set_nonblocking(true).and_then(|()| UdpSocket::from_std(socket)) else {
            warn!("could not prepare punching socket");
            return;
        };
        let mut tick = interval(PUNCH_INTERVAL);
        let deadline = sleep(PUNCH_DURATION);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = &mut deadline => return,
                _ = tick.tick() => {
                    for t in &targets {
                        if let Err(e) = socket.send_to(PUNCH_PAYLOAD, t).await {
                            debug!(target = %t, error = %e, "punch probe failed");
                        }
                    }
                }
            }
        }
    });
}

fn into_endpoint(
    socket: UdpSocket,
    server: Option<quinn::ServerConfig>,
) -> Result<(Endpoint, StdUdpSocket), NetworkError> {
    let std_socket = socket.into_std()?;
    let punch = std_socket.try_clone()?;
    let endpoint = Endpoint::new(quinn::EndpointConfig::default(), server, std_socket, Arc::new(quinn::TokioRuntime))?;
    Ok((endpoint, punch))
}

/// Host side: accept the viewer's QUIC connection.
pub struct QuicHost {
    endpoint: Endpoint,
    punch_cancel: CancellationToken,
}

impl QuicHost {
    pub fn new(
        socket: UdpSocket,
        cert: &HostCertificate,
        viewer_candidates: &[Candidate],
    ) -> Result<Self, NetworkError> {
        let mut tls = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.clone()], PrivateKeyDer::Pkcs8(cert.key.clone_key()))?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        tls.max_early_data_size = 0;
        let crypto = QuicServerConfig::try_from(tls).map_err(|e| NetworkError::QuicConfig(e.to_string()))?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        server.transport_config(transport_config());
        server.migration(true);

        let (endpoint, punch) = into_endpoint(socket, Some(server))?;
        let punch_cancel = CancellationToken::new();
        spawn_punching(punch, viewer_candidates.iter().map(|c| c.addr).collect(), punch_cancel.clone());
        Ok(Self { endpoint, punch_cancel })
    }

    /// Accepts the next incoming connection and wraps it as a [`RawLink`].
    /// The caller authenticates it via key confirmation.
    pub async fn accept(&self, wait: Duration) -> Result<RawLink, NetworkError> {
        let conn = timeout(wait, async {
            loop {
                let Some(incoming) = self.endpoint.accept().await else {
                    return Err(NetworkError::Unreachable);
                };
                match incoming.await {
                    Ok(conn) => return Ok(conn),
                    Err(e) => debug!(error = %e, "incoming quic handshake failed"),
                }
            }
        })
        .await
        .map_err(|_| NetworkError::Unreachable)??;
        info!(remote = %conn.remote_address(), "quic connection accepted");
        self.punch_cancel.cancel();
        let control = timeout(wait, conn.accept_bi()).await.map_err(|_| NetworkError::Unreachable)??;
        Ok(spawn_pump(conn, control, self.endpoint.clone()))
    }
}

impl Drop for QuicHost {
    fn drop(&mut self) {
        self.punch_cancel.cancel();
    }
}

/// Pins the host certificate by SHA-256; WebPKI names are meaningless for
/// ephemeral peer certificates.
#[derive(Debug)]
struct PinnedCert {
    sha256: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = digest(&SHA256, end_entity.as_ref());
        // Fingerprints are public; equality is all that matters here.
        if got.as_ref() == self.sha256.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // QUIC mandates TLS 1.3.
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Viewer side: connect to whichever host candidate answers first.
pub async fn connect_viewer(
    socket: UdpSocket,
    host_candidates: &[Candidate],
    cert_sha256: [u8; 32],
    wait: Duration,
) -> Result<RawLink, NetworkError> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert { sha256: cert_sha256, provider }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.enable_early_data = false;
    let crypto = QuicClientConfig::try_from(tls).map_err(|e| NetworkError::QuicConfig(e.to_string()))?;
    let mut client = quinn::ClientConfig::new(Arc::new(crypto));
    client.transport_config(transport_config());

    let (mut endpoint, punch) = into_endpoint(socket, None)?;
    endpoint.set_default_client_config(client);
    let punch_cancel = CancellationToken::new();
    let targets: Vec<SocketAddr> = host_candidates.iter().map(|c| c.addr).collect();
    spawn_punching(punch, targets.clone(), punch_cancel.clone());

    let race = async {
        let mut attempts = FuturesUnordered::new();
        for addr in targets {
            match endpoint.connect(addr, SERVER_NAME) {
                Ok(connecting) => attempts.push(async move { (addr, connecting.await) }),
                Err(e) => debug!(%addr, error = %e, "cannot dial candidate"),
            }
        }
        while let Some((addr, result)) = attempts.next().await {
            match result {
                Ok(conn) => return Ok(conn),
                Err(e) => debug!(%addr, error = %e, "candidate failed"),
            }
        }
        Err(NetworkError::Unreachable)
    };
    let conn = timeout(wait, race).await.map_err(|_| NetworkError::Unreachable);
    punch_cancel.cancel();
    let conn = conn??;
    info!(remote = %conn.remote_address(), "quic connection established");
    let (mut send, recv) = conn.open_bi().await?;
    // Streams are only announced once data flows; send a zero-length marker
    // so the host's accept_bi() resolves immediately.
    send.write_all(&0u32.to_le_bytes()).await.map_err(|e| NetworkError::QuicConfig(e.to_string()))?;
    Ok(spawn_pump(conn, (send, recv), endpoint))
}

fn spawn_pump(conn: Connection, control: (SendStream, RecvStream), endpoint: Endpoint) -> RawLink {
    let cancel = CancellationToken::new();
    let (link, pump) = raw_link(LinkKind::Quic, cancel);
    tokio::spawn(run_pump(conn, control, pump, endpoint));
    link
}

async fn run_pump(conn: Connection, control: (SendStream, RecvStream), pump: RawLinkPump, endpoint: Endpoint) {
    let RawLinkPump { mut control_rx, mut video_rx, inbound_tx, stats_tx, cancel, done } = pump;
    let (mut ctrl_send, mut ctrl_recv) = control;
    let backlog = Arc::new(AtomicUsize::new(0));
    // Control outranks video when the congestion window is tight.
    if let Err(e) = ctrl_send.set_priority(10) {
        debug!(error = %e, "could not prioritize control stream");
    }

    let writer = async {
        loop {
            tokio::select! {
                rec = control_rx.recv() => {
                    let Some(rec) = rec else { return };
                    let len = rec.len() as u32;
                    if ctrl_send.write_all(&len.to_le_bytes()).await.is_err() || ctrl_send.write_all(&rec).await.is_err() {
                        return;
                    }
                }
                rec = video_rx.recv() => {
                    let Some(rec) = rec else { return };
                    let conn = conn.clone();
                    let backlog = backlog.clone();
                    backlog.fetch_add(1, Ordering::Relaxed);
                    tokio::spawn(async move {
                        let result = async {
                            let mut s = conn.open_uni().await.map_err(|e| e.to_string())?;
                            s.write_all(&rec).await.map_err(|e| e.to_string())?;
                            s.finish().map_err(|e| e.to_string())
                        }.await;
                        if let Err(e) = result {
                            debug!(error = %e, "video stream write failed");
                        }
                        backlog.fetch_sub(1, Ordering::Relaxed);
                    });
                }
            }
        }
    };

    let control_reader = async {
        let mut len_buf = [0u8; 4];
        loop {
            if ctrl_recv.read_exact(&mut len_buf).await.is_err() {
                return;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            if len == 0 {
                continue; // stream-open marker
            }
            if len > MAX_RECORD {
                warn!(len, "oversized control record; closing");
                return;
            }
            let mut rec = vec![0u8; len];
            if ctrl_recv.read_exact(&mut rec).await.is_err() {
                return;
            }
            if inbound_tx.send((Channel::Control, rec)).await.is_err() {
                return;
            }
        }
    };

    let video_reader = async {
        loop {
            let Ok(mut stream) = conn.accept_uni().await else { return };
            let tx = inbound_tx.clone();
            tokio::spawn(async move {
                match stream.read_to_end(MAX_RECORD).await {
                    Ok(rec) => {
                        // Never block the reader on a slow consumer: drop instead.
                        if tx.try_send((Channel::Video, rec)).is_err() {
                            debug!("inbound video queue full; frame dropped");
                        }
                    }
                    Err(e) => debug!(error = %e, "video stream read failed"),
                }
            });
        }
    };

    let stats = async {
        let mut tick = interval(STATS_INTERVAL);
        let (mut last_sent, mut last_lost) = (0u64, 0u64);
        loop {
            tick.tick().await;
            let s = conn.stats();
            let sent = s.path.sent_packets.saturating_sub(last_sent);
            let lost = s.path.lost_packets.saturating_sub(last_lost);
            last_sent = s.path.sent_packets;
            last_lost = s.path.lost_packets;
            let loss = if sent == 0 { 0.0 } else { (lost as f32 / sent as f32).min(1.0) };
            let snapshot = TransportStats {
                rtt: s.path.rtt,
                loss,
                cwnd: Some(s.path.cwnd),
                video_backlog: backlog.load(Ordering::Relaxed),
            };
            if stats_tx.send(snapshot).is_err() {
                return;
            }
        }
    };

    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = writer => {}
        _ = control_reader => {}
        _ = video_reader => {}
        _ = stats => {}
        reason = conn.closed() => info!(%reason, "quic connection closed"),
    }
    // Flush control records queued before the close (Bye, input releases)
    // and wait for the peer to acknowledge them before closing the connection.
    let flush = async {
        while let Ok(rec) = control_rx.try_recv() {
            let len = rec.len() as u32;
            if ctrl_send.write_all(&len.to_le_bytes()).await.is_err() || ctrl_send.write_all(&rec).await.is_err() {
                return;
            }
        }
        if ctrl_send.finish().is_ok() {
            let _ = ctrl_send.stopped().await;
        }
    };
    let _ = timeout(FLUSH_TIMEOUT, flush).await;
    conn.close(VarInt::from_u32(0), b"bye");
    cancel.cancel();
    // Give the CONNECTION_CLOSE frame a moment to leave before the socket drops.
    let _ = timeout(Duration::from_millis(200), endpoint.wait_idle()).await;
    done.cancel();
}
