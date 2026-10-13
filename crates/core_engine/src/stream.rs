//! Capture -> delta -> encode pipeline on a dedicated OS thread.
//!
//! Capture APIs block (DXGI waits on the compositor), and encoding is CPU
//! bound, so neither runs on the async runtime. The thread paces itself to
//! the controller's FPS target and pushes encoded frames into a tiny bounded
//! queue; when the queue is full the frame is dropped and the next one is
//! forced to be a keyframe so the decoder never references a missing frame.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use remoti_proto::peer::VideoCodec;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::capture::{self, CaptureBackend, CaptureError, CaptureStatus};
use crate::encoder;
use crate::frame::{DeltaTracker, Frame};

/// Shared knobs between the session (async) and the pipeline thread.
pub struct StreamParams {
    pub bitrate_bps: AtomicU32,
    pub fps: AtomicU16,
    pub force_keyframe: AtomicBool,
    pub stop: AtomicBool,
    pub source_drops: AtomicU64,
}

impl StreamParams {
    pub fn new(bitrate_bps: u32, fps: u16) -> Arc<Self> {
        Arc::new(Self {
            bitrate_bps: AtomicU32::new(bitrate_bps),
            fps: AtomicU16::new(fps.max(1)),
            force_keyframe: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            source_drops: AtomicU64::new(0),
        })
    }

    pub fn request_keyframe(&self) {
        self.force_keyframe.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub enum StreamOutput {
    Config { codec: VideoCodec, width: u32, height: u32 },
    Frame { pts_us: u64, keyframe: bool, data: Vec<u8> },
    Fatal(String),
}

pub const OUTPUT_QUEUE: usize = 2;

pub fn spawn(
    backend: CaptureBackend,
    params: Arc<StreamParams>,
    out: mpsc::Sender<StreamOutput>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name("remoti-capture".into()).spawn(move || {
        if let Err(msg) = pipeline(backend, &params, &out) {
            warn!(error = %msg, "capture pipeline stopped");
            let _ = out.blocking_send(StreamOutput::Fatal(msg));
        }
    })
}

fn pipeline(backend: CaptureBackend, params: &StreamParams, out: &mpsc::Sender<StreamOutput>) -> Result<(), String> {
    let mut capturer = capture::open(backend).map_err(|e| e.to_string())?;
    let mut encoder = encoder::create(params.bitrate_bps.load(Ordering::Relaxed), params.fps.load(Ordering::Relaxed))
        .map_err(|e| e.to_string())?;
    let mut frame = Frame::new();
    let mut tracker = DeltaTracker::new();
    let started = Instant::now();
    let mut last_sent: Option<Instant> = None;
    let mut have_frame = false;
    let (w, h) = capturer.size();
    info!(width = w, height = h, "capture started");

    while !params.stop.load(Ordering::Relaxed) {
        let fps = params.fps.load(Ordering::Relaxed).max(1);
        let interval = Duration::from_secs(1) / u32::from(fps);

        // Frame-rate cap: never encode faster than the controller allows.
        if let Some(t) = last_sent {
            let next = t + interval;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            }
        }

        let status = match capturer.capture(interval.min(Duration::from_millis(100)), &mut frame) {
            Ok(s) => s,
            Err(CaptureError::AccessLost) => {
                // Desktop switch / resolution change: rebuild the capturer.
                warn!("capture access lost; reopening");
                std::thread::sleep(Duration::from_millis(200));
                capturer = capture::open(backend).map_err(|e| e.to_string())?;
                tracker.reset();
                params.request_keyframe();
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };

        let force = params.force_keyframe.load(Ordering::Relaxed);
        match status {
            CaptureStatus::Unchanged if !(force && have_frame) => continue,
            CaptureStatus::Unchanged => {}
            CaptureStatus::Updated { dirty } => {
                have_frame = true;
                let delta = tracker.update(&frame, dirty.as_deref());
                if delta.resized {
                    let cfg = StreamOutput::Config { codec: encoder.codec(), width: frame.width, height: frame.height };
                    if out.blocking_send(cfg).is_err() {
                        return Ok(());
                    }
                    params.request_keyframe();
                } else if delta.is_empty() && !force {
                    continue;
                }
            }
        }

        let force = params.force_keyframe.swap(false, Ordering::Relaxed);
        encoder.set_rate(params.bitrate_bps.load(Ordering::Relaxed), fps);
        let pts = started.elapsed();
        let encoded = match encoder.encode(&frame, pts, force) {
            Ok(Some(e)) => e,
            Ok(None) => {
                if force {
                    params.request_keyframe();
                }
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        last_sent = Some(Instant::now());
        let msg = StreamOutput::Frame {
            pts_us: pts.as_micros().min(u128::from(u64::MAX)) as u64,
            keyframe: encoded.keyframe,
            data: encoded.data,
        };
        match out.try_send(msg) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                params.source_drops.fetch_add(1, Ordering::Relaxed);
                params.request_keyframe();
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
        }
    }
    Ok(())
}
