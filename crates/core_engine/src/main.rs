//! `remoti-agent`: the native process behind the Electron shell.
//!
//! * `remoti-agent ipc` (default): framed stdio protocol for Electron.
//! * `remoti-agent host`: headless host, prints its id/password to the log.
//! * `remoti-agent connect <ID> --password <PW>`: headless viewer (diagnostics).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use remoti_core::agent::{self, AgentConfig};
use remoti_core::capture::CaptureBackend;
use remoti_core::congestion::RateConfig;
use remoti_core::encoder::CodecPreference;
use remoti_core::ipc::{self, UiCommand, UiEvent, UiOutputs};
use remoti_proto::peer::VideoCodec;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "remoti-agent", version, about = "Remoti native agent")]
struct Cli {
    /// Signaling server WebSocket URL.
    #[arg(long, env = "REMOTI_SIGNALING_URL", default_value = "wss://signal.remoti.app/ws")]
    signaling: String,
    /// Extra STUN servers (host:port); signaling advertises its own as well.
    #[arg(long = "stun", env = "REMOTI_STUN", value_delimiter = ',')]
    stun: Vec<String>,
    /// UDP port for QUIC (0 = ephemeral).
    #[arg(long, default_value_t = 0)]
    udp_port: u16,
    #[arg(long, value_enum, default_value_t = CaptureBackend::Auto)]
    capture: CaptureBackend,
    /// Video codec to send when hosting (negotiated with the viewer).
    #[arg(long, value_enum, env = "REMOTI_CODEC", default_value_t = CodecPreference::Auto)]
    codec: CodecPreference,
    #[arg(long, default_value_t = 20_000)]
    max_bitrate_kbps: u32,
    #[arg(long, default_value_t = 60)]
    max_fps: u16,
    /// Directory for the persistent device key.
    #[arg(long, env = "REMOTI_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Use a throwaway identity (new device id every run).
    #[arg(long)]
    ephemeral_identity: bool,
    /// Always use the TCP/WebSocket relay (for networks that block UDP).
    #[arg(long, env = "REMOTI_FORCE_RELAY")]
    force_relay: bool,
    #[arg(long, value_enum, default_value_t = LogFormat::Text)]
    log_format: LogFormat,
    #[command(subcommand)]
    mode: Option<Mode>,
}

#[derive(Subcommand, Debug, Clone)]
enum Mode {
    /// Talk to the Electron shell over stdio (default).
    Ipc,
    /// Headless host.
    Host {
        /// Refuse remote input (view-only).
        #[arg(long)]
        view_only: bool,
    },
    /// Headless viewer: connects and reports stream statistics.
    Connect {
        device_id: String,
        #[arg(long, env = "REMOTI_PASSWORD")]
        password: String,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum LogFormat {
    Text,
    Json,
}

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_env("REMOTI_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    // stdout is reserved for IPC frames: logs always go to stderr.
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_target(true);
    let result = match format {
        LogFormat::Text => builder.with_ansi(false).try_init(),
        LogFormat::Json => builder.json().flatten_event(true).try_init(),
    };
    if let Err(e) = result {
        eprintln!("tracing init failed: {e}");
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.log_format);

    if rustls::crypto::ring::default_provider().install_default().is_err() {
        warn!("rustls crypto provider was already installed");
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("remoti-rt").build() {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "failed to start async runtime");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "agent failed");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let cfg = AgentConfig {
        signaling_url: cli.signaling.clone(),
        stun: cli.stun.clone(),
        udp_port: cli.udp_port,
        capture: cli.capture,
        codec: cli.codec,
        rate: RateConfig {
            max_bitrate_bps: cli.max_bitrate_kbps.saturating_mul(1000),
            max_fps: cli.max_fps,
            ..RateConfig::default()
        },
        data_dir: cli.data_dir.clone(),
        ephemeral_identity: cli.ephemeral_identity,
        force_relay: cli.force_relay,
    };
    let cancel = CancellationToken::new();
    spawn_signal_handler(cancel.clone());

    let (ui, outputs) = ipc::ui_channel();
    let (cmd_tx, cmd_rx) = mpsc::channel::<UiCommand>(256);
    let mode = cli.mode.unwrap_or(Mode::Ipc);

    let output_task = match &mode {
        Mode::Ipc => {
            let reader_cancel = cancel.clone();
            tokio::spawn(async move {
                if let Err(e) = ipc::read_loop(tokio::io::stdin(), cmd_tx).await {
                    warn!(error = %e, "ipc read failed");
                }
                // Shell closed stdin: shut down instead of lingering as an orphan.
                reader_cancel.cancel();
            });
            tokio::spawn(async move {
                if let Err(e) = ipc::write_loop(tokio::io::stdout(), outputs).await {
                    warn!(error = %e, "ipc write failed");
                }
            })
        }
        Mode::Host { view_only } => {
            cmd_tx.send(UiCommand::HostStart { allow_control: !view_only }).await?;
            keep_open(cmd_tx, cancel.clone());
            tokio::spawn(log_outputs(outputs))
        }
        Mode::Connect { device_id, password } => {
            // Headless: nothing is decoded, so accept any codec the host offers.
            let codecs = vec![VideoCodec::H265, VideoCodec::H264, VideoCodec::Av1];
            cmd_tx
                .send(UiCommand::ViewerConnect { device_id: device_id.clone(), password: password.clone(), codecs })
                .await?;
            keep_open(cmd_tx, cancel.clone());
            tokio::spawn(log_outputs(outputs))
        }
    };

    let result = agent::run(cfg, ui, cmd_rx, cancel.clone()).await;
    cancel.cancel();
    // All UiSink clones are dropped once sessions finish; the writer then drains.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), output_task).await;
    result
}

/// Headless modes have no command source; hold the sender until shutdown.
fn keep_open(tx: mpsc::Sender<UiCommand>, cancel: CancellationToken) {
    tokio::spawn(async move {
        cancel.cancelled().await;
        let _ = tx.send(UiCommand::Shutdown).await;
    });
}

async fn log_outputs(mut out: UiOutputs) {
    let mut frames = 0u64;
    let mut bytes = 0u64;
    loop {
        tokio::select! {
            ev = out.events.recv() => match ev {
                Some(UiEvent::HostReady { device_id, password }) => {
                    info!(%device_id, %password, "hosting: share this id and password with the viewer");
                }
                Some(ev) => info!(event = %serde_json::to_string(&ev).unwrap_or_default(), "ui event"),
                None => break,
            },
            pkt = out.video.recv() => match pkt {
                Some(p) => {
                    frames += 1;
                    bytes += p.data.len() as u64;
                    if frames % 120 == 0 {
                        info!(frames, mib = bytes as f64 / 1_048_576.0, "video received");
                    }
                }
                None => break,
            },
        }
    }
}

fn spawn_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::terminate()) {
                Ok(mut term) => {
                    tokio::select! {
                        r = tokio::signal::ctrl_c() => if let Err(e) = r { warn!(error = %e, "cannot listen for ctrl-c") },
                        _ = term.recv() => {}
                    }
                }
                Err(e) => {
                    warn!(error = %e, "cannot listen for SIGTERM");
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
        #[cfg(not(unix))]
        if let Err(e) = tokio::signal::ctrl_c().await {
            warn!(error = %e, "cannot listen for ctrl-c");
        }
        info!("shutdown requested");
        cancel.cancel();
    });
}
