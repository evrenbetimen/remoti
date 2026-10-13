//! Hardware H.264 / H.265 through VideoToolbox (Apple silicon media engine or
//! Intel Quick Sync on older Macs).
//!
//! Real-time mode, no frame reordering, low-latency rate control (H.264) and
//! on-demand keyframes. `VTCompressionSessionCompleteFrames` is called after
//! every frame so the output callback runs before `encode` returns: one frame
//! in, at most one access unit out, no pipeline latency. Output is converted
//! from length-prefixed sample buffers to Annex B, with SPS/PPS (and VPS for
//! HEVC) prepended to keyframes so WebCodecs can decode without a container.

use std::collections::VecDeque;
use std::ffi::{c_int, c_void};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime, CMVideoCodecType, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex, kCMSampleAttachmentKey_NotSync, kCMTimeInvalid,
    kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_32BGRA,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ColorPrimaries,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};
use remoti_proto::peer::VideoCodec;
use tracing::{debug, warn};

use super::annexb::{from_length_prefixed, push_nal};
use super::{EncodeError, EncodedFrame, VideoEncoder};
use crate::frame::Frame;

type Output = Mutex<VecDeque<Result<EncodedFrame, i32>>>;

fn codec_type(codec: VideoCodec) -> Option<CMVideoCodecType> {
    match codec {
        VideoCodec::H264 => Some(kCMVideoCodecType_H264),
        VideoCodec::H265 => Some(kCMVideoCodecType_HEVC),
        VideoCodec::Av1 => None,
    }
}

fn os_err(context: &str, status: i32) -> EncodeError {
    EncodeError::Backend(format!("videotoolbox {context}: OSStatus {status}"))
}

/// Whether a hardware encoder for `codec` can be opened (probed once).
pub fn supports(codec: VideoCodec) -> bool {
    static H264: OnceLock<bool> = OnceLock::new();
    static H265: OnceLock<bool> = OnceLock::new();
    let cell = match codec {
        VideoCodec::H264 => &H264,
        VideoCodec::H265 => &H265,
        VideoCodec::Av1 => return false,
    };
    *cell.get_or_init(|| match Session::open(codec, 640, 360, 1_000_000, 30) {
        Ok(_) => true,
        Err(e) => {
            debug!(?codec, error = %e, "no hardware VideoToolbox encoder");
            false
        }
    })
}

/// Collects parameter-set NAL units of a keyframe's format description.
///
/// # Safety
/// `desc` must be a live video format description of `codec`.
unsafe fn parameter_sets(codec: VideoCodec, desc: &CMFormatDescription, out: &mut Vec<u8>) -> c_int {
    let get = match codec {
        VideoCodec::H265 => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
        _ => CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    };
    let mut count = 0usize;
    let mut nal_len: c_int = 4;
    // SAFETY: out-pointers are valid locals; null data pointers are allowed.
    let status = unsafe { get(desc, 0, std::ptr::null_mut(), std::ptr::null_mut(), &mut count, &mut nal_len) };
    if status != 0 {
        return 4;
    }
    for i in 0..count {
        let mut ptr: *const u8 = std::ptr::null();
        let mut size = 0usize;
        // SAFETY: as above; the returned pointer is valid while `desc` lives.
        let st = unsafe { get(desc, i, &mut ptr, &mut size, std::ptr::null_mut(), std::ptr::null_mut()) };
        if st == 0 && !ptr.is_null() && size > 0 {
            // SAFETY: VideoToolbox guarantees `size` readable bytes at `ptr`.
            push_nal(out, unsafe { std::slice::from_raw_parts(ptr, size) });
        }
    }
    nal_len
}

/// Converts one compressed sample buffer to an Annex B access unit.
///
/// # Safety
/// `sample` must be a live compressed sample buffer produced for `codec`.
unsafe fn convert(codec: VideoCodec, sample: &CMSampleBuffer) -> Option<EncodedFrame> {
    // SAFETY: CoreMedia getters on a live sample buffer.
    unsafe {
        let keyframe = match sample.sample_attachments_array(false) {
            Some(arr) if arr.count() > 0 => {
                let dict = arr.value_at_index(0).cast::<CFDictionary>();
                dict.is_null() || !(*dict).contains_ptr_key((kCMSampleAttachmentKey_NotSync as *const CFString).cast())
            }
            // No attachments: every sample is a sync sample.
            _ => true,
        };
        let mut data = Vec::new();
        let mut nal_len = 4;
        if keyframe {
            if let Some(desc) = sample.format_description() {
                nal_len = parameter_sets(codec, &desc, &mut data);
            }
        }
        let block = sample.data_buffer()?;
        let len = block.data_length();
        let mut raw = vec![0u8; len];
        let dst = NonNull::new(raw.as_mut_ptr().cast::<c_void>())?;
        if block.copy_data_bytes(0, len, dst) != 0 {
            return None;
        }
        if !from_length_prefixed(&raw, usize::try_from(nal_len).unwrap_or(4), &mut data) {
            return None;
        }
        Some(EncodedFrame { data, keyframe })
    }
}

struct CallbackCtx {
    codec: VideoCodec,
    output: Arc<Output>,
}

unsafe extern "C-unwind" fn on_output(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: `refcon` is the `CallbackCtx` owned by the session, which
    // outlives every callback (the session is invalidated before it is freed).
    let ctx = unsafe { &*(refcon as *const CallbackCtx) };
    let item = if status != 0 {
        Some(Err(status))
    } else if flags.contains(VTEncodeInfoFlags::FrameDropped) || sample.is_null() {
        None
    } else {
        // SAFETY: non-null sample buffer handed to us for the callback's duration.
        unsafe { convert(ctx.codec, &*sample) }.map(Ok)
    };
    if let Some(item) = item {
        if let Ok(mut q) = ctx.output.lock() {
            q.push_back(item);
        }
    }
}

fn set(session: &VTCompressionSession, key: &CFString, value: &CFType) -> i32 {
    // SAFETY: a compression session is a VTSession; key/value are CF objects.
    unsafe { VTSessionSetProperty(session, key, Some(value)) }
}

/// Applies real-time encoding properties and returns the source pixel pool.
fn configure(
    session: &VTCompressionSession,
    codec: VideoCodec,
    bitrate_bps: u32,
    fps: u16,
) -> Result<CFRetained<CVPixelBufferPool>, EncodeError> {
    // SAFETY: CF statics; property values are CF objects. Unsupported
    // optional properties are ignored by the encoder.
    unsafe {
        let t: &CFType = CFBoolean::new(true).as_ref();
        let f: &CFType = CFBoolean::new(false).as_ref();
        set(session, kVTCompressionPropertyKey_RealTime, t);
        set(session, kVTCompressionPropertyKey_AllowFrameReordering, f);
        let profile = match codec {
            VideoCodec::H265 => kVTProfileLevel_HEVC_Main_AutoLevel,
            _ => kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
        };
        set(session, kVTCompressionPropertyKey_ProfileLevel, profile.as_ref());
        let br = CFNumber::new_i32(i32::try_from(bitrate_bps).unwrap_or(i32::MAX));
        set(session, kVTCompressionPropertyKey_AverageBitRate, br.as_ref());
        let fr = CFNumber::new_i32(i32::from(fps.max(1)));
        set(session, kVTCompressionPropertyKey_ExpectedFrameRate, fr.as_ref());
        let gop = CFNumber::new_i32(i32::from(fps.max(1)) * 10);
        set(session, kVTCompressionPropertyKey_MaxKeyFrameInterval, gop.as_ref());
        let gop_s = CFNumber::new_f64(10.0);
        set(session, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, gop_s.as_ref());
        // Matches what the WebCodecs decoder assumes for SDR content.
        set(session, kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2.as_ref());
        set(session, kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2.as_ref());
        set(session, kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2.as_ref());
        let st = session.prepare_to_encode_frames();
        if st != 0 {
            return Err(os_err("prepare", st));
        }
        session.pixel_buffer_pool().ok_or_else(|| EncodeError::Backend("no pixel buffer pool".into()))
    }
}

struct Session {
    session: CFRetained<VTCompressionSession>,
    pool: CFRetained<CVPixelBufferPool>,
    ctx: *mut CallbackCtx,
    output: Arc<Output>,
    width: u32,
    height: u32,
}

impl Session {
    fn open(codec: VideoCodec, width: u32, height: u32, bitrate_bps: u32, fps: u16) -> Result<Self, EncodeError> {
        let ct = codec_type(codec).ok_or(EncodeError::Unavailable(codec))?;
        let w = i32::try_from(width).map_err(|_| EncodeError::Backend("width out of range".into()))?;
        let h = i32::try_from(height).map_err(|_| EncodeError::Backend("height out of range".into()))?;

        // SAFETY: CF statics are valid for the process lifetime.
        let (spec, source) = unsafe {
            let mut keys: Vec<&CFString> = vec![kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder];
            let mut vals: Vec<&CFType> = vec![CFBoolean::new(true).as_ref()];
            if codec == VideoCodec::H264 {
                keys.push(kVTVideoEncoderSpecification_EnableLowLatencyRateControl);
                vals.push(CFBoolean::new(true).as_ref());
            }
            let spec = CFDictionary::<CFString, CFType>::from_slices(&keys, &vals);
            let fmt = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
            let wn = CFNumber::new_i32(w);
            let hn = CFNumber::new_i32(h);
            let io = CFDictionary::<CFString, CFType>::empty();
            let source = CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferWidthKey,
                    kCVPixelBufferHeightKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                ],
                &[fmt.as_ref(), wn.as_ref(), hn.as_ref(), io.as_ref()],
            );
            (spec, source)
        };

        let output: Arc<Output> = Arc::new(Mutex::new(VecDeque::new()));
        let ctx = Box::into_raw(Box::new(CallbackCtx { codec, output: output.clone() }));
        let mut raw: *mut VTCompressionSession = std::ptr::null_mut();
        // SAFETY: valid dictionaries, callback and out-pointer; `ctx` lives
        // until after the session is invalidated (see Drop).
        let status = unsafe {
            VTCompressionSession::create(
                None,
                w,
                h,
                ct,
                Some(spec.as_opaque()),
                Some(source.as_opaque()),
                None,
                Some(on_output),
                ctx.cast(),
                NonNull::from(&mut raw),
            )
        };
        let Some(ptr) = NonNull::new(raw).filter(|_| status == 0) else {
            // SAFETY: no session references `ctx`; reclaim it.
            drop(unsafe { Box::from_raw(ctx) });
            return Err(os_err("create session", status));
        };
        // SAFETY: VTCompressionSessionCreate returns a +1 reference.
        let session = unsafe { CFRetained::from_raw(ptr) };

        match configure(&session, codec, bitrate_bps, fps) {
            Ok(pool) => Ok(Self { session, pool, ctx, output, width, height }),
            Err(e) => {
                // SAFETY: invalidation stops callbacks before `ctx` is freed.
                unsafe {
                    session.invalidate();
                    drop(Box::from_raw(ctx));
                }
                Err(e)
            }
        }
    }

    fn encode(
        &mut self,
        frame: &Frame,
        pts: Duration,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError> {
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: valid pool and out-pointer.
        let st = unsafe { CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut raw)) };
        let Some(ptr) = NonNull::new(raw).filter(|_| st == 0) else {
            return Err(os_err("pixel buffer", st));
        };
        // SAFETY: +1 reference from the pool.
        let pixels = unsafe { CFRetained::from_raw(ptr) };
        // SAFETY: the buffer is locked while its memory is written.
        unsafe {
            if CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::empty()) != 0 {
                return Err(EncodeError::Backend("pixel buffer lock failed".into()));
            }
            let base = CVPixelBufferGetBaseAddress(&pixels).cast::<u8>();
            let dst_stride = CVPixelBufferGetBytesPerRow(&pixels);
            let rows = CVPixelBufferGetHeight(&pixels).min(frame.height as usize);
            let row_bytes = (CVPixelBufferGetWidth(&pixels).min(frame.width as usize)) * 4;
            let src_stride = frame.stride();
            if !base.is_null() && dst_stride >= row_bytes {
                for y in 0..rows {
                    std::ptr::copy_nonoverlapping(
                        frame.data.as_ptr().add(y * src_stride),
                        base.add(y * dst_stride),
                        row_bytes,
                    );
                }
            }
            CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::empty());
        }

        let us = i64::try_from(pts.as_micros()).unwrap_or(i64::MAX);
        // SAFETY: plain value constructor.
        let time = unsafe { CMTime::new(us, 1_000_000) };
        let props = force_keyframe.then(|| {
            // SAFETY: CF statics.
            unsafe {
                CFDictionary::<CFString, CFType>::from_slices(
                    &[kVTEncodeFrameOptionKey_ForceKeyFrame],
                    &[CFBoolean::new(true).as_ref()],
                )
            }
        });
        // SAFETY: live session and pixel buffer; the callback context is valid.
        unsafe {
            let st = self.session.encode_frame(
                &pixels,
                time,
                kCMTimeInvalid,
                props.as_deref().map(|d| d.as_opaque()),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if st != 0 {
                return Err(os_err("encode", st));
            }
            // Synchronous flush: the output callback has run when this returns.
            let st = self.session.complete_frames(time);
            if st != 0 {
                return Err(os_err("complete", st));
            }
        }
        let mut q = self.output.lock().map_err(|_| EncodeError::Backend("output queue poisoned".into()))?;
        match q.pop_front() {
            Some(Ok(f)) => Ok(Some(f)),
            Some(Err(status)) => Err(os_err("callback", status)),
            None => Ok(None),
        }
    }

    fn set_bitrate(&self, bitrate_bps: u32) -> bool {
        let br = CFNumber::new_i32(i32::try_from(bitrate_bps).unwrap_or(i32::MAX));
        // SAFETY: CF static key.
        unsafe { set(&self.session, kVTCompressionPropertyKey_AverageBitRate, br.as_ref()) == 0 }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: invalidation stops all callbacks; afterwards the context
        // can be freed.
        unsafe {
            self.session.invalidate();
            drop(Box::from_raw(self.ctx));
        }
    }
}

pub struct VtEncoder {
    codec: VideoCodec,
    session: Option<Session>,
    active: (u32, u16),
    wanted: (u32, u16),
}

// SAFETY: VideoToolbox sessions are thread-safe CF objects; the encoder is
// owned by the capture thread and the callback context is only touched
// through its mutex.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    pub fn new(codec: VideoCodec, bitrate_bps: u32, fps: u16) -> Result<Self, EncodeError> {
        if !supports(codec) {
            return Err(EncodeError::Unavailable(codec));
        }
        Ok(Self { codec, session: None, active: (bitrate_bps, fps), wanted: (bitrate_bps, fps) })
    }
}

impl VideoEncoder for VtEncoder {
    fn codec(&self) -> VideoCodec {
        self.codec
    }

    fn backend(&self) -> &'static str {
        "videotoolbox"
    }

    fn is_hardware(&self) -> bool {
        true
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
        let mut force = force_keyframe;
        let resized = self.session.as_ref().is_none_or(|s| (s.width, s.height) != (frame.width, frame.height));
        if resized {
            self.session = None;
            self.session = Some(Session::open(self.codec, frame.width, frame.height, self.wanted.0, self.wanted.1)?);
            self.active = self.wanted;
            force = true;
        } else if self.wanted.0 != self.active.0 {
            if !self.session.as_ref().is_some_and(|s| s.set_bitrate(self.wanted.0)) {
                warn!("VideoToolbox rejected in-place bitrate change; rebuilding");
                self.session = None;
                self.session =
                    Some(Session::open(self.codec, frame.width, frame.height, self.wanted.0, self.wanted.1)?);
                force = true;
            }
            self.active = self.wanted;
        }
        let Some(session) = self.session.as_mut() else {
            return Err(EncodeError::Backend("encoder not initialised".into()));
        };
        session.encode(frame, pts, force)
    }
}
