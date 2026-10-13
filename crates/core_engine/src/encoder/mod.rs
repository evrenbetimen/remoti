//! Video encoding and codec negotiation.
//!
//! Backends, in the order the host prefers them:
//!
//! | codec | hardware                                   | software            |
//! |-------|--------------------------------------------|---------------------|
//! | H.265 | Media Foundation (Windows), VideoToolbox (macOS) | –             |
//! | H.264 | Media Foundation (Windows), VideoToolbox (macOS) | OpenH264      |
//! | AV1   | –                                          | rav1e               |
//!
//! Every backend emits self-contained access units the viewer feeds straight
//! into WebCodecs `VideoDecoder` without a container: Annex B with in-band
//! parameter sets on keyframes for H.264/H.265, low-overhead OBU temporal
//! units with a sequence header on keyframes for AV1.

pub mod annexb;
#[cfg(feature = "av1")]
mod av1;
#[cfg(windows)]
mod mediafoundation;
#[cfg(feature = "h264")]
mod openh264;
#[cfg(target_os = "macos")]
mod videotoolbox;

use std::time::{Duration, Instant};

use remoti_proto::peer::VideoCodec;
use tracing::{info, warn};

use crate::frame::Frame;

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("no encoder available for {0:?} on this build/platform")]
    Unavailable(VideoCodec),
    #[error("encoder: {0}")]
    Backend(String),
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

pub trait VideoEncoder: Send {
    fn codec(&self) -> VideoCodec;
    /// Short backend name for logs and diagnostics.
    fn backend(&self) -> &'static str;
    /// `true` for GPU/ASIC encoders; a failing hardware encoder is replaced by
    /// a software one for the rest of the session.
    fn is_hardware(&self) -> bool {
        false
    }
    /// Applies new rate targets; may internally restart the encoder.
    fn set_rate(&mut self, bitrate_bps: u32, fps: u16);
    /// Encodes one frame. `Ok(None)` when the encoder decided to skip it.
    fn encode(
        &mut self,
        frame: &Frame,
        pts: Duration,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError>;
}

/// Host-side codec choice (`--codec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum CodecPreference {
    /// Hardware H.265 when available, otherwise H.264; AV1 as a last resort.
    #[default]
    Auto,
    H264,
    H265,
    Av1,
}

impl CodecPreference {
    fn codec(self) -> Option<VideoCodec> {
        match self {
            CodecPreference::Auto => None,
            CodecPreference::H264 => Some(VideoCodec::H264),
            CodecPreference::H265 => Some(VideoCodec::H265),
            CodecPreference::Av1 => Some(VideoCodec::Av1),
        }
    }
}

fn hardware_supports(codec: VideoCodec) -> bool {
    #[cfg(windows)]
    {
        mediafoundation::supports(codec)
    }
    #[cfg(target_os = "macos")]
    {
        videotoolbox::supports(codec)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = codec;
        false
    }
}

fn software_supports(codec: VideoCodec) -> bool {
    match codec {
        VideoCodec::H264 => cfg!(feature = "h264"),
        VideoCodec::H265 => false,
        VideoCodec::Av1 => cfg!(feature = "av1"),
    }
}

/// Codecs this host can encode, most preferred first.
pub fn encodable(pref: CodecPreference) -> Vec<VideoCodec> {
    let mut order = Vec::with_capacity(3);
    if let Some(c) = pref.codec() {
        order.push(c);
    }
    for c in [VideoCodec::H265, VideoCodec::H264, VideoCodec::Av1] {
        // Software H.265 does not exist here, so Auto only picks H.265 on
        // hardware; AV1 is software-only and therefore last.
        if !order.contains(&c) {
            order.push(c);
        }
    }
    order.retain(|&c| hardware_supports(c) || software_supports(c));
    order
}

/// Picks the session codec: the host's most preferred codec that the viewer
/// can decode.
pub fn negotiate(pref: CodecPreference, viewer: &[VideoCodec]) -> Option<VideoCodec> {
    encodable(pref).into_iter().find(|c| viewer.contains(c))
}

/// Creates the best encoder for `codec`: hardware first, then software.
pub fn create(codec: VideoCodec, bitrate_bps: u32, fps: u16) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    if hardware_supports(codec) {
        match create_hardware(codec, bitrate_bps, fps) {
            Ok(e) => {
                info!(?codec, backend = e.backend(), "hardware video encoder ready");
                return Ok(e);
            }
            Err(e) => warn!(?codec, error = %e, "hardware encoder unavailable; trying software"),
        }
    }
    let e = create_software(codec, bitrate_bps, fps)?;
    info!(?codec, backend = e.backend(), "software video encoder ready");
    Ok(e)
}

#[allow(unused_variables)]
fn create_hardware(codec: VideoCodec, bitrate_bps: u32, fps: u16) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    #[cfg(windows)]
    {
        return Ok(Box::new(mediafoundation::MfEncoder::new(codec, bitrate_bps, fps)?));
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(Box::new(videotoolbox::VtEncoder::new(codec, bitrate_bps, fps)?));
    }
    #[allow(unreachable_code)]
    Err(EncodeError::Unavailable(codec))
}

/// Software-only encoder for `codec`; also the replacement when a hardware
/// encoder fails mid-session.
#[allow(unused_variables)]
pub fn create_software(codec: VideoCodec, bitrate_bps: u32, fps: u16) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    match codec {
        #[cfg(feature = "h264")]
        VideoCodec::H264 => Ok(Box::new(openh264::OpenH264Encoder::new(bitrate_bps, fps))),
        #[cfg(feature = "av1")]
        VideoCodec::Av1 => Ok(Box::new(av1::Rav1eEncoder::new(bitrate_bps, fps))),
        other => Err(EncodeError::Unavailable(other)),
    }
}

/// Rebuilding the encoder costs a keyframe, so small rate changes are
/// batched: rebuild only on large changes or after a cool-down.
#[cfg_attr(not(any(feature = "h264", feature = "av1")), allow(dead_code))]
pub(crate) struct RebuildPolicy {
    last_rebuild: Option<Instant>,
}

#[cfg_attr(not(any(feature = "h264", feature = "av1")), allow(dead_code))]
impl RebuildPolicy {
    const COOLDOWN: Duration = Duration::from_secs(2);

    pub(crate) fn new() -> Self {
        Self { last_rebuild: None }
    }

    pub(crate) fn should_rebuild(&mut self, current: (u32, u16), wanted: (u32, u16), now: Instant) -> bool {
        if current == wanted {
            return false;
        }
        let rel = (f64::from(wanted.0) - f64::from(current.0)) / f64::from(current.0.max(1));
        let urgent = rel <= -0.40;
        let cooled = self.last_rebuild.is_none_or(|t| now.duration_since(t) >= Self::COOLDOWN);
        let significant = rel.abs() >= 0.20 || wanted.1 != current.1;
        if urgent || (cooled && significant) {
            self.last_rebuild = Some(now);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CaptureStatus, ScreenCapturer, SyntheticCapturer};

    #[test]
    fn rebuild_policy_batches_small_changes() {
        let mut p = RebuildPolicy::new();
        let t0 = Instant::now();
        assert!(!p.should_rebuild((1_000_000, 30), (1_050_000, 30), t0));
        assert!(p.should_rebuild((1_000_000, 30), (1_300_000, 30), t0));
        // Within cool-down: moderate change waits, large drop goes through.
        assert!(!p.should_rebuild((1_300_000, 30), (1_000_000, 30), t0 + Duration::from_millis(500)));
        assert!(p.should_rebuild((1_300_000, 30), (600_000, 30), t0 + Duration::from_millis(600)));
    }

    #[test]
    fn negotiation_respects_viewer_and_preference() {
        let all = [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1];
        if cfg!(feature = "av1") {
            assert_eq!(negotiate(CodecPreference::Av1, &all), Some(VideoCodec::Av1));
            assert_eq!(negotiate(CodecPreference::Auto, &[VideoCodec::Av1]), Some(VideoCodec::Av1));
        }
        if cfg!(feature = "h264") {
            assert_eq!(negotiate(CodecPreference::Av1, &[VideoCodec::H264]), Some(VideoCodec::H264));
            let auto = negotiate(CodecPreference::Auto, &all);
            assert!(matches!(auto, Some(VideoCodec::H264 | VideoCodec::H265)), "{auto:?}");
        }
        assert_eq!(negotiate(CodecPreference::Auto, &[]), None);
    }

    /// Encodes ten synthetic frames and returns `(frames, keyframes)`.
    pub(crate) fn encode_synthetic(enc: &mut dyn VideoEncoder, check: impl Fn(&EncodedFrame)) -> (usize, usize) {
        let mut cap = SyntheticCapturer::new(320, 240);
        let mut frame = Frame::new();
        let (mut total, mut keyframes) = (0, 0);
        for i in 0..10u64 {
            let st = cap.capture(Duration::from_millis(100), &mut frame).unwrap();
            assert!(matches!(st, CaptureStatus::Updated { .. }));
            if let Some(out) = enc.encode(&frame, Duration::from_millis(i * 33), i == 0).unwrap() {
                total += 1;
                keyframes += usize::from(out.keyframe);
                check(&out);
            }
        }
        (total, keyframes)
    }

    #[cfg(feature = "h264")]
    #[test]
    fn openh264_emits_annexb() {
        let mut enc = create_software(VideoCodec::H264, 1_000_000, 30).unwrap();
        let (total, keyframes) = encode_synthetic(enc.as_mut(), |out| {
            assert!(out.data.starts_with(&[0, 0, 0, 1]) || out.data.starts_with(&[0, 0, 1]), "annex b start code");
        });
        assert!(total >= 5);
        assert!(keyframes >= 1);
    }

    /// Hardware encoders exist only on some machines (CI runners usually have
    /// none); when one is present it must produce decodable Annex B.
    #[test]
    fn hardware_encoders_when_present() {
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            if !hardware_supports(codec) {
                continue;
            }
            let mut enc = match create_hardware(codec, 2_000_000, 30) {
                Ok(e) => e,
                Err(e) => panic!("{codec:?} advertised but failed to open: {e}"),
            };
            let (total, keyframes) = encode_synthetic(enc.as_mut(), |out| {
                assert!(out.data.starts_with(&[0, 0, 0, 1]), "annex b start code");
            });
            assert!(total >= 1, "{codec:?} produced no frames");
            assert!(keyframes >= 1, "{codec:?} produced no keyframe");
        }
    }
}
