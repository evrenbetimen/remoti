//! Video encoding. The default backend is OpenH264 (constrained baseline,
//! screen-content tuning, Annex B output) which the viewer decodes with
//! WebCodecs `VideoDecoder` without any container.

use std::time::{Duration, Instant};

use remoti_proto::peer::VideoCodec;

use crate::frame::Frame;

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("no video encoder compiled in (enable the `h264` feature)")]
    Unavailable,
    #[error("encoder: {0}")]
    Backend(String),
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

pub trait VideoEncoder: Send {
    fn codec(&self) -> VideoCodec;
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

pub fn create(bitrate_bps: u32, fps: u16) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    #[cfg(feature = "h264")]
    {
        Ok(Box::new(h264::OpenH264Encoder::new(bitrate_bps, fps)))
    }
    #[cfg(not(feature = "h264"))]
    {
        let _ = (bitrate_bps, fps);
        Err(EncodeError::Unavailable)
    }
}

/// Rebuilding the encoder costs an IDR frame, so small rate changes are
/// batched: rebuild only on large changes or after a cool-down.
#[cfg_attr(not(feature = "h264"), allow(dead_code))]
pub(crate) struct RebuildPolicy {
    last_rebuild: Option<Instant>,
}

#[cfg_attr(not(feature = "h264"), allow(dead_code))]
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

#[cfg(feature = "h264")]
mod h264 {
    use std::time::{Duration, Instant};

    use openh264::OpenH264API;
    use openh264::Timestamp;
    use openh264::encoder::{
        BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, RateControlMode,
        UsageType,
    };
    use openh264::formats::{BgraSliceU8, YUVBuffer};
    use remoti_proto::peer::VideoCodec;

    use super::{EncodeError, EncodedFrame, RebuildPolicy, VideoEncoder};
    use crate::frame::Frame;

    pub struct OpenH264Encoder {
        encoder: Option<Encoder>,
        yuv: Option<YUVBuffer>,
        dims: (u32, u32),
        active: (u32, u16),
        wanted: (u32, u16),
        policy: RebuildPolicy,
    }

    impl OpenH264Encoder {
        pub fn new(bitrate_bps: u32, fps: u16) -> Self {
            Self {
                encoder: None,
                yuv: None,
                dims: (0, 0),
                active: (bitrate_bps, fps),
                wanted: (bitrate_bps, fps),
                policy: RebuildPolicy::new(),
            }
        }

        fn build(&self) -> Result<Encoder, EncodeError> {
            let threads = std::thread::available_parallelism().map(|n| n.get().min(4) as u16).unwrap_or(1);
            let config = EncoderConfig::new()
                .usage_type(UsageType::ScreenContentRealTime)
                .profile(Profile::Baseline)
                .complexity(Complexity::Low)
                .rate_control_mode(RateControlMode::Bitrate)
                .bitrate(BitRate::from_bps(self.wanted.0))
                .max_frame_rate(FrameRate::from_hz(f32::from(self.wanted.1)))
                // Keyframes only on demand (viewer request, loss recovery) plus a
                // long safety period, avoiding periodic bitrate spikes.
                .intra_frame_period(IntraFramePeriod::from_num_frames(u32::from(self.wanted.1) * 10))
                .skip_frames(true)
                .scene_change_detect(true)
                // Not supported for screen content; set explicitly to keep logs quiet.
                .adaptive_quantization(false)
                .background_detection(false)
                .num_threads(threads);
            Encoder::with_api_config(OpenH264API::from_source(), config)
                .map_err(|e| EncodeError::Backend(e.to_string()))
        }
    }

    impl VideoEncoder for OpenH264Encoder {
        fn codec(&self) -> VideoCodec {
            VideoCodec::H264
        }

        fn set_rate(&mut self, bitrate_bps: u32, fps: u16) {
            self.wanted = (bitrate_bps, fps);
        }

        fn encode(
            &mut self,
            frame: &Frame,
            pts: Duration,
            force_keyframe: bool,
        ) -> Result<Option<EncodedFrame>, EncodeError> {
            if frame.width == 0 || frame.height == 0 {
                return Ok(None);
            }
            let dims = (frame.width, frame.height);
            let rebuild = self.encoder.is_none()
                || dims != self.dims
                || self.policy.should_rebuild(self.active, self.wanted, Instant::now());
            if rebuild {
                self.encoder = Some(self.build()?);
                self.active = self.wanted;
                if dims != self.dims {
                    self.yuv = Some(YUVBuffer::new(frame.width as usize, frame.height as usize));
                    self.dims = dims;
                }
            }
            let (Some(encoder), Some(yuv)) = (self.encoder.as_mut(), self.yuv.as_mut()) else {
                return Err(EncodeError::Backend("encoder not initialised".into()));
            };
            yuv.read_bgra8(BgraSliceU8::new(&frame.data, (frame.width as usize, frame.height as usize)));
            if force_keyframe && !rebuild {
                encoder.force_intra_frame();
            }
            let ts = Timestamp::from_millis(pts.as_millis().min(u128::from(u64::MAX)) as u64);
            let bitstream = encoder.encode_at(&*yuv, ts).map_err(|e| EncodeError::Backend(e.to_string()))?;
            let keyframe = match bitstream.frame_type() {
                FrameType::IDR | FrameType::I => true,
                FrameType::P | FrameType::IPMixed => false,
                FrameType::Skip | FrameType::Invalid => return Ok(None),
            };
            let data = bitstream.to_vec();
            if data.is_empty() {
                return Ok(None);
            }
            Ok(Some(EncodedFrame { data, keyframe }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[cfg(feature = "h264")]
    #[test]
    fn encodes_synthetic_frames() {
        use crate::capture::{CaptureStatus, ScreenCapturer, SyntheticCapturer};
        let mut cap = SyntheticCapturer::new(320, 240);
        let mut enc = create(1_000_000, 30).unwrap();
        let mut frame = Frame::new();
        let mut keyframes = 0;
        let mut total = 0;
        for i in 0..10u64 {
            let st = cap.capture(Duration::from_millis(100), &mut frame).unwrap();
            assert!(matches!(st, CaptureStatus::Updated { .. }));
            if let Some(out) = enc.encode(&frame, Duration::from_millis(i * 33), i == 0).unwrap() {
                total += 1;
                keyframes += usize::from(out.keyframe);
                assert!(out.data.starts_with(&[0, 0, 0, 1]) || out.data.starts_with(&[0, 0, 1]), "annex b start code");
            }
        }
        assert!(total >= 5);
        assert!(keyframes >= 1);
    }
}
