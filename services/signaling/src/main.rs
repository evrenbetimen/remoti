//! `remoti-signaling` binary: configuration, STUN responder, HTTP server.

use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;
use remoti_signaling::state::{AppState, Config};
use remoti_signaling::{app, stun};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "remoti-signaling", version, about = "Remoti signaling, STUN and relay service")]
struct Cli {
    /// HTTP/WebSocket listen address (terminate TLS at the load balancer).
    #[arg(long, env = "REMOTI_LISTEN", default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// UDP address for the STUN responder.
    #[arg(long, env = "REMOTI_STUN_LISTEN", default_value = "0.0.0.0:3478")]
    stun_listen: SocketAddr,
    /// Public relay URL given to agents, e.g. wss://signal.example.com/relay.
    #[arg(long, env = "REMOTI_RELAY_URL", default_value = "ws://127.0.0.1:8080/relay")]
    relay_url: String,
    /// STUN servers advertised to agents (host:port, comma separated).
    #[arg(long, env = "REMOTI_PUBLIC_STUN", value_delimiter = ',', default_value = "127.0.0.1:3478")]
    public_stun: Vec<String>,
    #[arg(long, env = "REMOTI_JSON_LOGS")]
    json_logs: bool,
}

fn init_tracing(json: bool) {
    let filter = EnvFilter::try_from_env("REMOTI_LOG").unwrap_or_else(|_| EnvFilter::new("info,tower_http=info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_target(true);
    let result = if json { builder.json().flatten_event(true).try_init() } else { builder.try_init() };
    if let Err(e) = result {
        eprintln!("tracing init failed: {e}");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.json_logs);
    let state = AppState::new(Config {
        relay_url: cli.relay_url.clone(),
        stun: cli.public_stun.clone(),
        relay_pair_timeout: Duration::from_secs(20),
        relay_idle_timeout: Duration::from_secs(30),
        relay_max_lifetime: Duration::from_secs(12 * 60 * 60),
    });
    let cancel = CancellationToken::new();

    let stun_cancel = cancel.clone();
    let stun_socket = tokio::net::UdpSocket::bind(cli.stun_listen).await?;
    info!(addr = %cli.stun_listen, "stun responder listening");
    tokio::spawn(async move {
        if let Err(e) = stun::serve(stun_socket, stun_cancel).await {
            warn!(error = %e, "stun responder stopped");
        }
    });

    let sweeper = state.clone();
    let sweep_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        loop {
            tokio::select! {
                _ = sweep_cancel.cancelled() => return,
                _ = tick.tick() => sweeper.sweep_relays().await,
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(cli.listen).await?;
    info!(addr = %cli.listen, "signaling listening");
    let shutdown = cancel.clone();
    axum::serve(listener, app(state))
        .with_graceful_shutdown(async move {
            if tokio::signal::ctrl_c().await.is_err() {
                warn!("cannot listen for ctrl-c");
            }
            info!("shutting down");
            shutdown.cancel();
        })
        .await?;
    Ok(())
}
