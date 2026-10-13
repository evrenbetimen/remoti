//! macOS 12.3+ capture through ScreenCaptureKit.
//!
//! The window server pushes frames into an `SCStream` only when the screen
//! changes (idle frames are flagged and skipped), at up to 60 fps, as
//! IOSurface-backed BGRA pixel buffers. The newest buffer is parked in a
//! slot; `capture` waits on it with a timeout and copies it once into the
//! reusable [`Frame`]. Older frames the encoder did not get to are dropped.

use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamFrameInfoStatus,
    SCStreamOutput, SCStreamOutputType,
};
use tracing::{debug, warn};

use super::{CaptureError, CaptureStatus, ScreenCapturer};
use crate::frame::Frame;

const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FPS: i32 = 60;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
}

/// Retained pixel buffer handed from the dispatch queue to the capture thread.
struct Buffer(CFRetained<CVPixelBuffer>);
// SAFETY: CVPixelBuffers are reference-counted, thread-safe CF objects; the
// buffer is only read after SCK has finished writing it.
unsafe impl Send for Buffer {}

#[derive(Default)]
struct Slot {
    buffer: Option<Buffer>,
    seq: u64,
    stopped: bool,
}

#[derive(Default)]
struct Shared {
    slot: Mutex<Slot>,
    ready: Condvar,
}

impl Shared {
    /// Parks the newest frame; an older one the consumer missed is dropped.
    fn push(&self, buffer: Buffer) {
        let Ok(mut s) = self.slot.lock() else { return };
        s.buffer = Some(buffer);
        s.seq = s.seq.wrapping_add(1);
        self.ready.notify_one();
    }
}

/// Reads the `SCStreamFrameInfoStatus` attachment.
///
/// Dirty-rectangle attachments are deliberately not used: their coordinate
/// space (points vs. pixels) differs across macOS releases, and a wrong hint
/// would leave stale tiles. Idle frames are filtered here, and the tile
/// tracker hashes changed frames in full.
///
/// # Safety
/// `sample` must be a live sample buffer delivered by SCStream.
unsafe fn frame_status(sample: &CMSampleBuffer) -> Option<isize> {
    // SAFETY: CF getters on live objects; the key is an NSString static,
    // toll-free bridged to CFString.
    unsafe {
        let arr = sample.sample_attachments_array(false)?;
        if arr.count() == 0 {
            return None;
        }
        let dict = arr.value_at_index(0).cast::<CFDictionary>();
        if dict.is_null() {
            return None;
        }
        let status = (*dict).value((SCStreamFrameInfoStatus as *const NSString).cast());
        if status.is_null() { None } else { (*status.cast::<CFNumber>()).as_isize() }
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "RemotiScreenCaptureOutput"]
    #[ivars = Arc<Shared>]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // SAFETY: SCK hands us a live sample buffer for this call.
            let status = unsafe { frame_status(sample) };
            if status.is_some_and(|s| s != SCFrameStatus::Complete.0) {
                // Idle / blank / suspended frames carry no new pixels.
                return;
            }
            // SAFETY: as above.
            if let Some(image) = unsafe { sample.image_buffer() } {
                self.ivars().push(Buffer(image));
            }
        }
    }
);

impl StreamOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        // SAFETY: NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

fn wait_error(rx: &mpsc::Receiver<Result<(), String>>, what: &str) -> Result<(), CaptureError> {
    match rx.recv_timeout(SETUP_TIMEOUT) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(CaptureError::Os(format!("{what}: {e}"))),
        Err(_) => Err(CaptureError::Os(format!("{what}: timed out"))),
    }
}

fn describe(err: *mut NSError) -> String {
    if err.is_null() {
        return "unknown error".into();
    }
    // SAFETY: non-null NSError delivered to a completion handler.
    unsafe { (*err).localizedDescription().to_string() }
}

pub struct SckCapturer {
    stream: Retained<SCStream>,
    // Kept alive for the stream's lifetime (SCK holds it weakly).
    _output: Retained<StreamOutput>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    size: (u32, u32),
    last_seq: u64,
}

// SAFETY: SCStream and the output object are thread-safe Objective-C
// objects; the capturer is owned by one thread at a time.
unsafe impl Send for SckCapturer {}

impl SckCapturer {
    pub fn new() -> Result<Self, CaptureError> {
        // SAFETY: CoreGraphics display queries.
        let (main, width, height) = unsafe {
            let id = CGMainDisplayID();
            (id, CGDisplayPixelsWide(id), CGDisplayPixelsHigh(id))
        };

        let (tx, rx) = mpsc::sync_channel::<Result<Retained<SCShareableContent>, String>>(1);
        let handler = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
            // SAFETY: SCK passes either a valid content object or an error.
            let r = unsafe { Retained::retain(content) }.ok_or_else(|| describe(err));
            let _ = tx.try_send(r);
        });
        // SAFETY: class method with a valid completion block.
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
        let content = match rx.recv_timeout(SETUP_TIMEOUT) {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                warn!(error = %e, "ScreenCaptureKit refused shareable content");
                return Err(CaptureError::PermissionDenied(
                    "grant Screen Recording to Remoti in System Settings > Privacy & Security",
                ));
            }
            Err(_) => return Err(CaptureError::Os("ScreenCaptureKit did not answer".into())),
        };

        // SAFETY: read-only property access on live objects.
        let display = unsafe {
            let displays = content.displays();
            let mut chosen = None;
            for d in displays.iter() {
                if d.displayID() == main {
                    chosen = Some(d);
                    break;
                }
            }
            chosen.or_else(|| displays.firstObject()).ok_or(CaptureError::Os("no display to capture".into()))?
        };

        let shared = Arc::new(Shared::default());
        let output = StreamOutput::new(shared.clone());
        let queue = DispatchQueue::new("app.remoti.capture", None);
        let w = u32::try_from(width).unwrap_or(0) & !1;
        let h = u32::try_from(height).unwrap_or(0) & !1;
        if w == 0 || h == 0 {
            return Err(CaptureError::Os("display reports zero size".into()));
        }

        // SAFETY: standard SCK object construction and configuration.
        let stream = unsafe {
            let filter =
                SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new());
            let config = SCStreamConfiguration::new();
            config.setWidth(w as usize);
            config.setHeight(h as usize);
            config.setMinimumFrameInterval(CMTime::new(1, MAX_FPS));
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setShowsCursor(true);
            config.setQueueDepth(3);
            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| CaptureError::Os(format!("add stream output: {}", e.localizedDescription())))?;
            stream
        };

        let (tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let started = RcBlock::new(move |err: *mut NSError| {
            let _ = tx.try_send(if err.is_null() { Ok(()) } else { Err(describe(err)) });
        });
        // SAFETY: valid stream and completion block.
        unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
        wait_error(&rx, "start capture")?;
        debug!(width = w, height = h, "ScreenCaptureKit stream started");
        Ok(Self { stream, _output: output, _queue: queue, shared, size: (w, h), last_seq: 0 })
    }
}

impl ScreenCapturer for SckCapturer {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn capture(&mut self, timeout: Duration, frame: &mut Frame) -> Result<CaptureStatus, CaptureError> {
        let deadline = Instant::now() + timeout;
        let mut slot = self.shared.slot.lock().map_err(|_| CaptureError::Os("capture slot poisoned".into()))?;
        while slot.seq == self.last_seq && !slot.stopped {
            let now = Instant::now();
            if now >= deadline {
                return Ok(CaptureStatus::Unchanged);
            }
            slot = self
                .shared
                .ready
                .wait_timeout(slot, deadline - now)
                .map_err(|_| CaptureError::Os("capture slot poisoned".into()))?
                .0;
        }
        if slot.stopped {
            return Err(CaptureError::AccessLost);
        }
        self.last_seq = slot.seq;
        let Some(Buffer(pixels)) = slot.buffer.take() else { return Ok(CaptureStatus::Unchanged) };
        drop(slot);

        // SAFETY: the buffer is locked read-only for the duration of the copy.
        unsafe {
            if CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) != 0 {
                return Err(CaptureError::Os("pixel buffer lock failed".into()));
            }
            let base = CVPixelBufferGetBaseAddress(&pixels).cast::<u8>().cast_const();
            let stride = CVPixelBufferGetBytesPerRow(&pixels);
            let w = CVPixelBufferGetWidth(&pixels);
            let h = CVPixelBufferGetHeight(&pixels);
            let ok = !base.is_null() && {
                let data = std::slice::from_raw_parts(base, stride * h);
                frame.copy_from_strided(data, w as u32, h as u32, stride)
            };
            CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly);
            if !ok {
                return Err(CaptureError::Os("pixel buffer smaller than reported".into()));
            }
        }
        frame.captured_at = Instant::now();
        Ok(CaptureStatus::Updated { dirty: None })
    }
}

impl Drop for SckCapturer {
    fn drop(&mut self) {
        if let Ok(mut s) = self.shared.slot.lock() {
            s.stopped = true;
        }
        // SAFETY: stopping a running stream; the completion is not awaited.
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
        debug!("ScreenCaptureKit stream stopped");
    }
}
