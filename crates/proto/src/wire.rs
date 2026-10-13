//! Peer-to-peer messages. Every message is postcard-encoded and then sealed in
//! an AES-256-GCM record before it touches any transport.

use serde::{Deserialize, Serialize};

use crate::bytes::Bytes;
use crate::input::InputEvent;
use crate::peer::VideoCodec;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoFrame {
    pub seq: u64,
    /// Capture timestamp in microseconds since session start (host clock).
    pub pts_us: u64,
    pub keyframe: bool,
    /// One encoded access unit (Annex B for H.264).
    pub data: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireMessage {
    /// First record on every transport: proves both sides derived the same
    /// keys from the same transcript.
    Confirm {
        tag: Bytes,
    },
    StreamConfig {
        codec: VideoCodec,
        width: u32,
        height: u32,
        fps: u16,
    },
    VideoFrame(VideoFrame),
    Input(InputEvent),
    KeyframeRequest,
    /// Viewer feedback used by the host's rate controller in addition to
    /// transport statistics.
    ReceiverReport {
        highest_seq: u64,
        frames_dropped: u32,
        decode_queue: u16,
    },
    Ping {
        nonce: u64,
        sent_us: u64,
    },
    Pong {
        nonce: u64,
        sent_us: u64,
    },
    /// Host toggles whether the viewer may inject input.
    ControlPermission {
        allowed: bool,
    },
    Bye {
        reason: String,
    },
}

impl WireMessage {
    /// Logical channel used for AEAD associated data and scheduling.
    pub fn channel(&self) -> Channel {
        match self {
            WireMessage::VideoFrame(_) => Channel::Video,
            _ => Channel::Control,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Channel {
    Control = 1,
    Video = 2,
}
