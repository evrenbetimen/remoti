//! Software AV1 via rav1e in its low-latency configuration (no frame
//! reordering, minimal lookahead, tiles for parallelism). Output packets are
//! OBU temporal units; keyframes carry the sequence header in-band.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use rav1e::prelude::{
    ChromaSampling, ColorDescription, ColorPrimaries, Config, Context, EncoderConfig, EncoderStatus, FrameParameters,
    FrameType, FrameTypeOverride, MatrixCoefficients, PixelRange, Rational, SpeedSettings, TransferCharacteristics,
    Tune,
};
use remoti_proto::peer::VideoCodec;

use super::{EncodeError, EncodedFrame, RebuildPolicy, VideoEncoder};
use crate::color::I420;
use crate::frame::Frame;

/// Fastest preset; screen content is mostly static so quality holds up.
const SPEED_PRESET: u8 = 10;

pub struct Rav1eEncoder {
    ctx: Option<Context<u8>>,
    yuv: I420,
    dims: (u32, u32),
    active: (u32, u16),
    wanted: (u32, u16),
    policy: RebuildPolicy,
    pending: VecDeque<EncodedFrame>,
}

impl Rav1eEncoder {
    pub fn new(bitrate_bps: u32, fps: u16) -> Self {
        Self {
            ctx: None,
            yuv: I420::default(),
            dims: (0, 0),
            active: (bitrate_bps, fps),
            wanted: (bitrate_bps, fps),
            policy: RebuildPolicy::new(),
            pending: VecDeque::new(),
        }
    }

    fn build(&self, width: u32, height: u32) -> Result<Context<u8>, EncodeError> {
        let threads = std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(1);
        let fps = u64::from(self.wanted.1.max(1));
        let mut speed = SpeedSettings::from_preset(SPEED_PRESET);
        speed.rdo_lookahead_frames = 1;
        let enc = EncoderConfig {
            width: width as usize,
            height: height as usize,
            time_base: Rational::new(1, fps),
            bit_depth: 8,
            chroma_sampling: ChromaSampling::Cs420,
            pixel_range: PixelRange::Limited,
            color_description: Some(ColorDescription {
                color_primaries: ColorPrimaries::BT709,
                transfer_characteristics: TransferCharacteristics::BT709,
                matrix_coefficients: MatrixCoefficients::BT709,
            }),
            low_latency: true,
            min_key_frame_interval: 0,
            max_key_frame_interval: fps * 10,
            reservoir_frame_delay: Some(12),
            bitrate: i32::try_from(self.wanted.0).unwrap_or(i32::MAX),
            tune: Tune::Psnr,
            tiles: threads,
            speed_settings: speed,
            ..EncoderConfig::default()
        };
        Config::new()
            .with_encoder_config(enc)
            .with_threads(threads)
            .new_context()
            .map_err(|e| EncodeError::Backend(format!("rav1e config: {e}")))
    }

    fn drain(&mut self) -> Result<(), EncodeError> {
        let Some(ctx) = self.ctx.as_mut() else { return Ok(()) };
        loop {
            match ctx.receive_packet() {
                Ok(packet) => {
                    if !packet.data.is_empty() {
                        self.pending.push_back(EncodedFrame {
                            keyframe: packet.frame_type == FrameType::KEY,
                            data: packet.data,
                        });
                    }
                }
                Err(EncoderStatus::Encoded) => continue,
                Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => return Ok(()),
                Err(e) => return Err(EncodeError::Backend(format!("rav1e: {e}"))),
            }
        }
    }
}

impl VideoEncoder for Rav1eEncoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::Av1
    }

    fn backend(&self) -> &'static str {
        "rav1e"
    }

    fn set_rate(&mut self, bitrate_bps: u32, fps: u16) {
        self.wanted = (bitrate_bps, fps);
    }

    fn encode(
        &mut self,
        frame: &Frame,
        _pts: Duration,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError> {
        if frame.width == 0 || frame.height == 0 {
            return Ok(None);
        }
        let dims = (frame.width, frame.height);
        let rebuild = self.ctx.is_none()
            || dims != self.dims
            || self.policy.should_rebuild(self.active, self.wanted, Instant::now());
        if rebuild {
            // Flush the old context so frames still in its lookahead are
            // delivered; the new context starts with a keyframe.
            if let Some(ctx) = self.ctx.as_mut() {
                ctx.flush();
                self.drain()?;
            }
            self.ctx = Some(self.build(frame.width, frame.height)?);
            self.active = self.wanted;
            self.dims = dims;
        }
        self.yuv.convert(frame);
        let Some(ctx) = self.ctx.as_mut() else {
            return Err(EncodeError::Backend("encoder not initialised".into()));
        };
        let mut f = ctx.new_frame();
        let (w, h) = (self.yuv.width, self.yuv.height);
        f.planes[0].copy_from_raw_u8(&self.yuv.y, w, 1);
        f.planes[1].copy_from_raw_u8(&self.yuv.u, w / 2, 1);
        f.planes[2].copy_from_raw_u8(&self.yuv.v, w / 2, 1);
        debug_assert_eq!(self.yuv.y.len(), w * h);
        let params = FrameParameters {
            frame_type_override: if force_keyframe || rebuild { FrameTypeOverride::Key } else { FrameTypeOverride::No },
            ..FrameParameters::default()
        };
        match ctx.send_frame((f, params)) {
            Ok(()) | Err(EncoderStatus::EnoughData) => {}
            Err(e) => return Err(EncodeError::Backend(format!("rav1e: {e}"))),
        }
        self.drain()?;
        Ok(self.pending.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::encode_synthetic;
    use super::*;

    /// Splits a temporal unit into OBU types (all OBUs carry a size field).
    fn obu_types(mut data: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        while !data.is_empty() {
            let header = data[0];
            assert_eq!(header & 0x02, 0x02, "obu_has_size_field");
            let ext = usize::from(header & 0x04 != 0);
            let mut i = 1 + ext;
            let (mut size, mut shift) = (0usize, 0);
            loop {
                let b = data[i];
                size |= usize::from(b & 0x7f) << shift;
                i += 1;
                shift += 7;
                if b & 0x80 == 0 {
                    break;
                }
            }
            types.push((header >> 3) & 0x0f);
            data = &data[i + size..];
        }
        types
    }

    #[test]
    fn emits_temporal_units_with_sequence_header_on_keyframes() {
        let mut enc = Rav1eEncoder::new(1_000_000, 30);
        let (total, keyframes) = encode_synthetic(&mut enc, |out| {
            let types = obu_types(&out.data);
            assert_eq!(types.first(), Some(&2), "temporal delimiter first: {types:?}");
            if out.keyframe {
                assert!(types.contains(&1), "keyframe without sequence header: {types:?}");
            }
        });
        assert!(total >= 5, "low-latency mode should emit almost every frame, got {total}");
        assert!(keyframes >= 1);
    }
}
