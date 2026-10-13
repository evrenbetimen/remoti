//! UDP STUN Binding responder: tells agents their public (server-reflexive)
//! address for hole punching. Stateless; replies only to valid requests.

use remoti_proto::stun;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

pub async fn serve(socket: UdpSocket, cancel: CancellationToken) -> std::io::Result<()> {
    let mut buf = [0u8; 1500];
    loop {
        let (n, from) = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            r = socket.recv_from(&mut buf) => match r {
                Ok(v) => v,
                Err(e) => {
                    // ICMP port-unreachable surfaces as recv errors on some OSes.
                    debug!(error = %e, "stun recv error");
                    continue;
                }
            },
        };
        let Some(txid) = stun::parse_request(&buf[..n]) else { continue };
        let resp = stun::encode_response(&txid, from);
        if let Err(e) = socket.send_to(&resp, from).await {
            warn!(%from, error = %e, "stun send failed");
        }
    }
}
