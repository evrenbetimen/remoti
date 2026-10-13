//! Software H.264 via OpenH264 (constrained baseline, screen-content
//! real-time tuning, Annex B output).

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
        Encoder::with_api_config(OpenH264API::from_source(), config).map_err(|e| EncodeError::Backend(e.to_string()))
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn backend(&self) -> &'static str {
        "openh264"
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
