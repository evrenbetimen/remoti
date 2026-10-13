//! Length-prefixed binary IPC over stdio between the agent and the Electron
//! main process.
//!
//! Frame: `len: u32 LE (kind + payload) || kind: u8 || payload`.
//! * `0x01` JSON message ([`UiCommand`] inbound, [`UiEvent`] outbound).
//! * `0x02` video access unit (outbound only):
//!   `pts_us: u64 LE || seq: u32 LE || flags: u8 (bit0 = keyframe) || codec: u8 || reserved: u16 || data`.
//!
//! stdout carries only frames (logs go to stderr). Queues are bounded; video is
//! dropped (and a keyframe requested) instead of buffering when the UI lags.
//! EOF on stdin means the shell is gone, and the agent shuts down.

use remoti_proto::input::InputEvent;
use remoti_proto::peer::VideoCodec;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufWriter};
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub const KIND_JSON: u8 = 0x01;
pub const KIND_VIDEO: u8 = 0x02;
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
const VIDEO_HEADER: usize = 16;
const EVENT_QUEUE: usize = 256;
const VIDEO_QUEUE: usize = 4;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum UiCommand {
    #[serde(rename = "host.start")]
    HostStart {
        #[serde(default = "default_true", rename = "allowControl")]
        allow_control: bool,
    },
    #[serde(rename = "host.stop")]
    HostStop,
    #[serde(rename = "host.rotatePassword")]
    HostRotatePassword,
    #[serde(rename = "host.setControl")]
    HostSetControl { allowed: bool },
    #[serde(rename = "viewer.connect")]
    ViewerConnect {
        #[serde(rename = "deviceId")]
        device_id: String,
        password: String,
        /// Codecs the UI's WebCodecs decoder supports; H.264 when omitted.
        #[serde(default = "default_codecs")]
        codecs: Vec<VideoCodec>,
    },
    #[serde(rename = "viewer.disconnect")]
    ViewerDisconnect,
    #[serde(rename = "viewer.input")]
    ViewerInput { event: InputEvent },
    #[serde(rename = "viewer.requestKeyframe")]
    ViewerRequestKeyframe,
    #[serde(rename = "viewer.report")]
    ViewerReport {
        #[serde(rename = "framesDropped")]
        frames_dropped: u32,
        #[serde(rename = "decodeQueue")]
        decode_queue: u16,
    },
    #[serde(rename = "permissions.check")]
    PermissionsCheck,
    #[serde(rename = "permissions.request")]
    PermissionsRequest,
    #[serde(rename = "shutdown")]
    Shutdown,
}

fn default_true() -> bool {
    true
}

fn default_codecs() -> Vec<VideoCodec> {
    vec![VideoCodec::H264]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionRole {
    Host,
    Viewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionState {
    Connecting,
    Connected,
    Reconnecting,
    Ended,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum UiEvent {
    #[serde(rename = "ready")]
    Ready { version: &'static str, platform: &'static str },
    #[serde(rename = "signaling")]
    Signaling {
        connected: bool,
        #[serde(rename = "deviceId", skip_serializing_if = "Option::is_none")]
        device_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(rename = "host.ready")]
    HostReady {
        #[serde(rename = "deviceId")]
        device_id: String,
        password: String,
    },
    #[serde(rename = "host.stopped")]
    HostStopped,
    #[serde(rename = "session")]
    Session {
        role: SessionRole,
        state: SessionState,
        #[serde(skip_serializing_if = "Option::is_none")]
        peer: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        transport: Option<crate::transport::LinkKind>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(rename = "video.config")]
    VideoConfig {
        /// WebCodecs codec string.
        codec: &'static str,
        width: u32,
        height: u32,
        fps: u16,
    },
    #[serde(rename = "control")]
    Control { allowed: bool },
    #[serde(rename = "stats")]
    Stats {
        role: SessionRole,
        #[serde(rename = "bitrateBps")]
        bitrate_bps: u32,
        fps: u16,
        #[serde(rename = "rttMs")]
        rtt_ms: u32,
        loss: f32,
        transport: crate::transport::LinkKind,
    },
    #[serde(rename = "permissions")]
    Permissions { screen: bool, input: bool },
    #[serde(rename = "error")]
    Error { code: &'static str, message: String },
}

/// WebCodecs codec string for the stream we produce.
pub fn webcodecs_codec(codec: VideoCodec) -> &'static str {
    match codec {
        // Constrained Baseline, level 5.1 (covers 4K30 / 1440p60).
        VideoCodec::H264 => "avc1.42E033",
        // Main profile, level 5.1, parameter sets in-band (hev1).
        VideoCodec::H265 => "hev1.1.6.L153.B0",
        // Main profile, level 5.1, 8-bit.
        VideoCodec::Av1 => "av01.0.13M.08",
    }
}

#[derive(Debug, Clone)]
pub struct VideoPacket {
    pub pts_us: u64,
    pub seq: u32,
    pub keyframe: bool,
    pub codec: VideoCodec,
    pub data: Vec<u8>,
}

/// Cloneable handle for emitting events and video to the UI.
#[derive(Clone)]
pub struct UiSink {
    events: mpsc::Sender<UiEvent>,
    video: mpsc::Sender<VideoPacket>,
}

impl UiSink {
    pub fn event(&self, ev: UiEvent) {
        if let Err(e) = self.events.try_send(ev) {
            warn!(error = %e, "ui event queue full or closed; event dropped");
        }
    }

    /// Returns `false` when the frame was dropped (caller should request a keyframe).
    pub fn video(&self, pkt: VideoPacket) -> bool {
        self.video.try_send(pkt).is_ok()
    }
}

/// Receiving ends consumed by an IPC writer (or the headless logger).
pub struct UiOutputs {
    pub events: mpsc::Receiver<UiEvent>,
    pub video: mpsc::Receiver<VideoPacket>,
}

pub fn ui_channel() -> (UiSink, UiOutputs) {
    let (etx, erx) = mpsc::channel(EVENT_QUEUE);
    let (vtx, vrx) = mpsc::channel(VIDEO_QUEUE);
    (UiSink { events: etx, video: vtx }, UiOutputs { events: erx, video: vrx })
}

fn codec_byte(c: VideoCodec) -> u8 {
    match c {
        VideoCodec::H264 => 1,
        VideoCodec::H265 => 2,
        VideoCodec::Av1 => 3,
    }
}

/// Serializes UI output to `writer` until both queues close.
pub async fn write_loop<W: AsyncWrite + Unpin>(writer: W, mut out: UiOutputs) -> std::io::Result<()> {
    let mut w = BufWriter::with_capacity(256 * 1024, writer);
    let (mut events_open, mut video_open) = (true, true);
    while events_open || video_open {
        tokio::select! {
            // Control events first: state changes must not wait behind video.
            biased;
            ev = out.events.recv(), if events_open => match ev {
                Some(ev) => {
                    let json = serde_json::to_vec(&ev).map_err(std::io::Error::other)?;
                    write_frame(&mut w, KIND_JSON, &[], &json).await?;
                }
                None => events_open = false,
            },
            pkt = out.video.recv(), if video_open => match pkt {
                Some(p) => {
                    let mut header = [0u8; VIDEO_HEADER];
                    header[0..8].copy_from_slice(&p.pts_us.to_le_bytes());
                    header[8..12].copy_from_slice(&p.seq.to_le_bytes());
                    header[12] = u8::from(p.keyframe);
                    header[13] = codec_byte(p.codec);
                    write_frame(&mut w, KIND_VIDEO, &header, &p.data).await?;
                }
                None => video_open = false,
            },
        }
        // Flush when idle so frames are not held back by buffering.
        if out.events.is_empty() && out.video.is_empty() {
            w.flush().await?;
        }
    }
    w.flush().await
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, kind: u8, header: &[u8], body: &[u8]) -> std::io::Result<()> {
    let len = 1 + header.len() + body.len();
    if len > MAX_FRAME {
        warn!(len, "ipc frame too large; dropped");
        return Ok(());
    }
    w.write_all(&(len as u32).to_le_bytes()).await?;
    w.write_all(&[kind]).await?;
    w.write_all(header).await?;
    w.write_all(body).await
}

#[derive(Debug, thiserror::Error)]
pub enum IpcReadError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds limit")]
    TooLarge(usize),
}

/// Reads commands from `reader`, forwarding them to `tx`. Returns on EOF.
pub async fn read_loop<R: AsyncRead + Unpin>(mut reader: R, tx: mpsc::Sender<UiCommand>) -> Result<(), IpcReadError> {
    let mut len_buf = [0u8; 4];
    let mut buf = Vec::with_capacity(4096);
    loop {
        match reader.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 {
            continue;
        }
        // Commands are small; cap far below MAX_FRAME to bound memory.
        if len > 1024 * 1024 {
            return Err(IpcReadError::TooLarge(len));
        }
        buf.resize(len, 0);
        reader.read_exact(&mut buf).await?;
        let (kind, payload) = (buf[0], &buf[1..]);
        if kind != KIND_JSON {
            warn!(kind, "unknown ipc frame kind; ignored");
            continue;
        }
        match serde_json::from_slice::<UiCommand>(payload) {
            Ok(cmd) => {
                if tx.send(cmd).await.is_err() {
                    return Ok(());
                }
            }
            Err(e) => debug!(error = %e, "invalid ipc command ignored"),
        }
        // Shrink if a large command inflated the buffer.
        if buf.capacity() > 64 * 1024 {
            buf = Vec::with_capacity(4096);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_roundtrip_through_pipe() {
        let (sink, outputs) = ui_channel();
        sink.event(UiEvent::Control { allowed: true });
        assert!(sink.video(VideoPacket {
            pts_us: 5,
            seq: 9,
            keyframe: true,
            codec: VideoCodec::H264,
            data: vec![0, 0, 1, 0x65]
        }));
        drop(sink);
        let mut out = Vec::new();
        write_loop(&mut out, outputs).await.unwrap();

        // First frame: JSON event.
        let len = u32::from_le_bytes(out[0..4].try_into().unwrap()) as usize;
        assert_eq!(out[4], KIND_JSON);
        let json: serde_json::Value = serde_json::from_slice(&out[5..4 + len]).unwrap();
        assert_eq!(json["type"], "control");
        // Second frame: video with 16-byte header.
        let rest = &out[4 + len..];
        let vlen = u32::from_le_bytes(rest[0..4].try_into().unwrap()) as usize;
        assert_eq!(rest[4], KIND_VIDEO);
        assert_eq!(vlen, 1 + 16 + 4);
        assert_eq!(rest[5 + 12], 1);
    }

    #[tokio::test]
    async fn parses_commands_and_stops_on_eof() {
        let cmd = br#"{"type":"viewer.input","event":{"mouseMove":{"x":0.5,"y":0.25}}}"#;
        let mut input = Vec::new();
        input.extend_from_slice(&((cmd.len() + 1) as u32).to_le_bytes());
        input.push(KIND_JSON);
        input.extend_from_slice(cmd);
        let (tx, mut rx) = mpsc::channel(4);
        read_loop(&input[..], tx).await.unwrap();
        match rx.recv().await {
            Some(UiCommand::ViewerInput { event: InputEvent::MouseMove { x, y } }) => assert_eq!((x, y), (0.5, 0.25)),
            other => panic!("unexpected {other:?}"),
        }
    }
}
