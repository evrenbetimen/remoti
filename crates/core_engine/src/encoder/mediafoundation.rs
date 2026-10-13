//! Hardware H.264 / H.265 through Media Foundation encoder MFTs (NVENC,
//! Quick Sync, AMF expose themselves this way).
//!
//! Hardware MFTs are asynchronous: they announce readiness with
//! `METransformNeedInput` / `METransformHaveOutput` events instead of the
//! synchronous NEED_MORE_INPUT protocol. Both models are handled. Input is
//! system-memory NV12 (BT.709 limited range), output is an Annex B elementary
//! stream with parameter sets on IDR frames. Low-latency mode, CBR, no
//! B-frames and a long GOP keep the stream interactive; keyframes are forced
//! on demand through `ICodecAPI`, and bitrate changes are applied in place.

use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use remoti_proto::peer::VideoCodec;
use tracing::{debug, warn};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform, METransformHaveOutput, METransformNeedInput,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFNominalRange_16_235,
    MFSTARTUP_NOSOCKET, MFSampleExtension_CleanPoint, MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709,
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base, eAVEncH265VProfile_Main_420_8,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface};

use super::{EncodeError, EncodedFrame, VideoEncoder};
use crate::color::bgra_to_nv12;
use crate::frame::Frame;

/// How long one `encode` call waits for an asynchronous MFT.
const ASYNC_WAIT: Duration = Duration::from_millis(250);
const POLL: Duration = Duration::from_millis(1);

fn backend_err(context: &str, e: windows::core::Error) -> EncodeError {
    EncodeError::Backend(format!("media foundation {context}: {e}"))
}

fn subtype(codec: VideoCodec) -> Option<GUID> {
    match codec {
        VideoCodec::H264 => Some(MFVideoFormat_H264),
        VideoCodec::H265 => Some(MFVideoFormat_HEVC),
        VideoCodec::Av1 => None,
    }
}

/// COM + Media Foundation lifetime for the current thread.
struct MfRuntime {
    uninit_com: bool,
}

impl MfRuntime {
    fn start() -> Result<Self, EncodeError> {
        // SAFETY: plain COM/MF initialisation; balanced in Drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // RPC_E_CHANGED_MODE: the thread is already STA; COM is usable but we
        // must not uninitialise it.
        let uninit_com = hr.is_ok();
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return Err(EncodeError::Backend(format!("CoInitializeEx: {hr:?}")));
        }
        // SAFETY: MFStartup is reference counted and balanced by MFShutdown.
        if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) } {
            if uninit_com {
                // SAFETY: balances the successful CoInitializeEx above.
                unsafe { CoUninitialize() };
            }
            return Err(backend_err("startup", e));
        }
        Ok(Self { uninit_com })
    }
}

impl Drop for MfRuntime {
    fn drop(&mut self) {
        // SAFETY: balances MFStartup / CoInitializeEx from `start`.
        unsafe {
            let _ = MFShutdown();
            if self.uninit_com {
                CoUninitialize();
            }
        }
    }
}

/// Hardware encoder activation objects for `codec`, best first.
fn enumerate(codec: VideoCodec) -> Result<Vec<IMFActivate>, EncodeError> {
    let Some(sub) = subtype(codec) else { return Ok(Vec::new()) };
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: sub };
    let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: out-pointers are valid; the returned array is CoTaskMem
    // allocated and freed below after taking ownership of each element.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut raw,
            &mut count,
        )
        .map_err(|e| backend_err("enumerate", e))?;
        if raw.is_null() {
            return Ok(Vec::new());
        }
        let items = std::slice::from_raw_parts_mut(raw, count as usize);
        let out = items.iter_mut().filter_map(Option::take).collect();
        CoTaskMemFree(Some(raw as *const _));
        Ok(out)
    }
}

/// Whether a hardware encoder for `codec` is registered. Probed once on a
/// dedicated thread so the caller's COM apartment is never touched.
pub fn supports(codec: VideoCodec) -> bool {
    static H264: OnceLock<bool> = OnceLock::new();
    static H265: OnceLock<bool> = OnceLock::new();
    let cell = match codec {
        VideoCodec::H264 => &H264,
        VideoCodec::H265 => &H265,
        VideoCodec::Av1 => return false,
    };
    *cell.get_or_init(|| {
        std::thread::Builder::new()
            .name("remoti-mf-probe".into())
            .spawn(move || {
                let _rt = MfRuntime::start().ok()?;
                Some(!enumerate(codec).ok()?.is_empty())
            })
            .ok()
            .and_then(|h| h.join().ok())
            .flatten()
            .unwrap_or(false)
    })
}

fn variant_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

fn variant_true() -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { boolVal: VARIANT_TRUE },
            }),
        },
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

struct Session {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    provides_samples: bool,
    output_size: u32,
    /// Async MFTs: number of `METransformNeedInput` events not yet consumed.
    input_credits: u32,
    nv12: Vec<u8>,
    width: u32,
    height: u32,
}

impl Session {
    fn open(codec: VideoCodec, width: u32, height: u32, bitrate_bps: u32, fps: u16) -> Result<Self, EncodeError> {
        let sub = subtype(codec).ok_or(EncodeError::Unavailable(codec))?;
        let mut last_err = EncodeError::Unavailable(codec);
        for activate in enumerate(codec)? {
            // SAFETY: COM calls on a live activation object.
            match unsafe { Self::configure(&activate, codec, sub, width, height, bitrate_bps, fps) } {
                Ok(s) => return Ok(s),
                Err(e) => {
                    debug!(error = %e, "hardware MFT rejected configuration; trying next");
                    // SAFETY: releases the half-configured MFT.
                    let _ = unsafe { activate.ShutdownObject() };
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn configure(
        activate: &IMFActivate,
        codec: VideoCodec,
        sub: GUID,
        width: u32,
        height: u32,
        bitrate_bps: u32,
        fps: u16,
    ) -> Result<Self, EncodeError> {
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().map_err(|e| backend_err("activate", e))?;
            let attrs = transform.GetAttributes().ok();
            let is_async = attrs.as_ref().and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).ok()).unwrap_or(0) != 0;
            if let Some(a) = attrs.as_ref() {
                if is_async {
                    a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(|e| backend_err("async unlock", e))?;
                }
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }

            let codec_api = transform.cast::<ICodecAPI>().ok();
            if let Some(api) = codec_api.as_ref() {
                // Best effort: not every vendor implements every property.
                let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &variant_true());
                let _ = api.SetValue(
                    &CODECAPI_AVEncCommonRateControlMode,
                    &variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
                );
                let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate_bps));
                let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &variant_u32(0));
                let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &variant_u32(u32::from(fps) * 10));
            }

            let rate = pack(u32::from(fps.max(1)), 1);
            let size = pack(width, height);
            let out: IMFMediaType = MFCreateMediaType().map_err(|e| backend_err("media type", e))?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| backend_err("output type", e))?;
            out.SetGUID(&MF_MT_SUBTYPE, &sub).map_err(|e| backend_err("output type", e))?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps).map_err(|e| backend_err("output type", e))?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(|e| backend_err("output type", e))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(|e| backend_err("output type", e))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(|e| backend_err("output type", e))?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| backend_err("output type", e))?;
            let profile = match codec {
                VideoCodec::H265 => eAVEncH265VProfile_Main_420_8.0 as u32,
                _ => eAVEncH264VProfile_Base.0 as u32,
            };
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile).map_err(|e| backend_err("output type", e))?;
            transform.SetOutputType(0, &out, 0).map_err(|e| backend_err("set output type", e))?;

            let input: IMFMediaType = MFCreateMediaType().map_err(|e| backend_err("media type", e))?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| backend_err("input type", e))?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| backend_err("input type", e))?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(|e| backend_err("input type", e))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(|e| backend_err("input type", e))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(|e| backend_err("input type", e))?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| backend_err("input type", e))?;
            // Matches crate::color: BT.709, limited range.
            let _ = input.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
            let _ = input.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
            let _ = input.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
            let _ = input.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
            transform.SetInputType(0, &input, 0).map_err(|e| backend_err("set input type", e))?;

            let info = transform.GetOutputStreamInfo(0).map_err(|e| backend_err("stream info", e))?;
            let provides_samples = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| backend_err("begin", e))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| backend_err("start", e))?;

            let events = if is_async {
                Some(transform.cast::<IMFMediaEventGenerator>().map_err(|e| backend_err("event generator", e))?)
            } else {
                None
            };
            let nv12 = vec![0u8; width as usize * height as usize * 3 / 2];
            Ok(Self {
                transform,
                events,
                codec_api,
                provides_samples,
                output_size: info.cbSize.max(width * height),
                input_credits: 0,
                nv12,
                width,
                height,
            })
        }
    }

    fn input_sample(&mut self, frame: &Frame, pts: Duration) -> Result<IMFSample, EncodeError> {
        if !bgra_to_nv12(frame, &mut self.nv12, self.width as usize) {
            return Err(EncodeError::Backend("nv12 conversion buffer too small".into()));
        }
        let len = self.nv12.len() as u32;
        // SAFETY: the buffer is locked for exactly `len` bytes while copying.
        unsafe {
            let buffer = MFCreateMemoryBuffer(len).map_err(|e| backend_err("buffer", e))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None).map_err(|e| backend_err("lock", e))?;
            if ptr.is_null() {
                let _ = buffer.Unlock();
                return Err(EncodeError::Backend("media buffer lock returned null".into()));
            }
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, self.nv12.len());
            buffer.Unlock().map_err(|e| backend_err("unlock", e))?;
            buffer.SetCurrentLength(len).map_err(|e| backend_err("length", e))?;
            let sample = MFCreateSample().map_err(|e| backend_err("sample", e))?;
            sample.AddBuffer(&buffer).map_err(|e| backend_err("add buffer", e))?;
            let hns = i64::try_from(pts.as_nanos() / 100).unwrap_or(i64::MAX);
            sample.SetSampleTime(hns).map_err(|e| backend_err("sample time", e))?;
            Ok(sample)
        }
    }

    /// One `ProcessOutput` call. `Ok(None)` when the MFT has nothing yet.
    fn pull(&mut self) -> Result<Option<EncodedFrame>, EncodeError> {
        // SAFETY: COM calls; the output sample is owned by `buffer` and
        // released by dropping the ManuallyDrop fields explicitly.
        unsafe {
            let own_sample = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample().map_err(|e| backend_err("sample", e))?;
                let b = MFCreateMemoryBuffer(self.output_size).map_err(|e| backend_err("buffer", e))?;
                s.AddBuffer(&b).map_err(|e| backend_err("add buffer", e))?;
                Some(s)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            let sample = ManuallyDrop::take(&mut buffers[0].pSample);
            drop(ManuallyDrop::take(&mut buffers[0].pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The MFT picked new output parameters (e.g. aligned size).
                    let t = self.transform.GetOutputAvailableType(0, 0).map_err(|e| backend_err("stream change", e))?;
                    self.transform.SetOutputType(0, &t, 0).map_err(|e| backend_err("stream change", e))?;
                    return Ok(None);
                }
                Err(e) => return Err(backend_err("process output", e)),
            }
            let Some(sample) = sample else { return Ok(None) };
            let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let buffer = sample.ConvertToContiguousBuffer().map_err(|e| backend_err("contiguous", e))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len)).map_err(|e| backend_err("lock", e))?;
            let data = if ptr.is_null() { Vec::new() } else { std::slice::from_raw_parts(ptr, len as usize).to_vec() };
            buffer.Unlock().map_err(|e| backend_err("unlock", e))?;
            if data.is_empty() {
                return Ok(None);
            }
            Ok(Some(EncodedFrame { data, keyframe }))
        }
    }

    /// Drains pending async events; returns `true` if output is ready.
    fn poll_events(&mut self) -> Result<bool, EncodeError> {
        let Some(events) = self.events.as_ref() else { return Ok(false) };
        let mut have_output = false;
        loop {
            // SAFETY: non-blocking event fetch on a live generator.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => {
                    // SAFETY: reading the event type of a live event.
                    let kind = unsafe { ev.GetType() }.map_err(|e| backend_err("event", e))?;
                    if kind == METransformNeedInput.0 as u32 {
                        self.input_credits += 1;
                    } else if kind == METransformHaveOutput.0 as u32 {
                        have_output = true;
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(have_output),
                Err(e) => return Err(backend_err("event queue", e)),
            }
        }
    }

    fn encode(
        &mut self,
        frame: &Frame,
        pts: Duration,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError> {
        if force_keyframe {
            if let Some(api) = self.codec_api.as_ref() {
                // SAFETY: COM property set on a live encoder.
                let _ = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1)) };
            }
        }
        let sample = self.input_sample(frame, pts)?;
        if self.events.is_none() {
            // Synchronous MFT.
            // SAFETY: COM call with a valid sample.
            unsafe { self.transform.ProcessInput(0, &sample, 0) }.map_err(|e| backend_err("process input", e))?;
            return self.pull();
        }

        let deadline = Instant::now() + ASYNC_WAIT;
        let mut submitted = false;
        let mut produced = None;
        while Instant::now() < deadline {
            let have_output = self.poll_events()?;
            if have_output && produced.is_none() {
                produced = self.pull()?;
            }
            if !submitted && self.input_credits > 0 {
                self.input_credits -= 1;
                // SAFETY: COM call with a valid sample, allowed by a NeedInput credit.
                unsafe { self.transform.ProcessInput(0, &sample, 0) }.map_err(|e| backend_err("process input", e))?;
                submitted = true;
            }
            if submitted && produced.is_some() {
                break;
            }
            std::thread::sleep(POLL);
        }
        if !submitted {
            return Err(EncodeError::Backend("hardware encoder stopped accepting input".into()));
        }
        Ok(produced)
    }

    fn set_bitrate(&self, bitrate_bps: u32) -> bool {
        match self.codec_api.as_ref() {
            // SAFETY: COM property set on a live encoder.
            Some(api) => unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate_bps)) }.is_ok(),
            None => false,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: orderly stream shutdown on a live MFT.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

pub struct MfEncoder {
    codec: VideoCodec,
    session: Option<Session>,
    active: (u32, u16),
    wanted: (u32, u16),
    // Declared last: dropped after the session so MF outlives the MFT.
    _runtime: MfRuntime,
}

// SAFETY: the encoder is created and used only on the capture thread, which
// joined the multithreaded apartment; MTA COM objects may be called from any
// MTA thread, and the value is never shared concurrently.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    pub fn new(codec: VideoCodec, bitrate_bps: u32, fps: u16) -> Result<Self, EncodeError> {
        let runtime = MfRuntime::start()?;
        // Validate that a session can actually be built before the stream
        // starts, so negotiation failures surface as a software fallback.
        drop(Session::open(codec, 640, 360, bitrate_bps, fps)?);
        Ok(Self { codec, session: None, active: (bitrate_bps, fps), wanted: (bitrate_bps, fps), _runtime: runtime })
    }
}

impl VideoEncoder for MfEncoder {
    fn codec(&self) -> VideoCodec {
        self.codec
    }

    fn backend(&self) -> &'static str {
        "media-foundation"
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
        let resized = self.session.as_ref().is_none_or(|s| (s.width, s.height) != (frame.width, frame.height));
        // Frame-rate changes need no rebuild: pacing comes from timestamps.
        let mut force = force_keyframe;
        if resized {
            self.session = None;
            self.session = Some(Session::open(self.codec, frame.width, frame.height, self.wanted.0, self.wanted.1)?);
            self.active = self.wanted;
            force = true;
        } else if self.wanted.0 != self.active.0 {
            let applied = self.session.as_ref().is_some_and(|s| s.set_bitrate(self.wanted.0));
            if !applied {
                warn!("encoder rejected in-place bitrate change; rebuilding");
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
