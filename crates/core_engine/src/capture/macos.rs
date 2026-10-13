//! macOS capture through CoreGraphics, gated on the Screen Recording (TCC)
//! permission. Frame pacing comes from the capture loop; unchanged frames are
//! filtered by the tile delta tracker.

use std::time::{Duration, Instant};

use core_graphics::display::CGDisplay;

use super::{CaptureError, CaptureStatus, ScreenCapturer};
use crate::frame::Frame;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

pub fn preflight_permission() -> bool {
    // SAFETY: no arguments; returns the cached TCC decision.
    unsafe { CGPreflightScreenCaptureAccess() }
}

pub fn request_permission() -> bool {
    // SAFETY: shows the system prompt at most once per process.
    unsafe { CGRequestScreenCaptureAccess() }
}

pub struct CgCapturer {
    display: CGDisplay,
    interval: Duration,
    last: Option<Instant>,
}

impl CgCapturer {
    pub fn new() -> Result<Self, CaptureError> {
        if !preflight_permission() {
            return Err(CaptureError::PermissionDenied(
                "grant Screen Recording to Remoti in System Settings > Privacy & Security",
            ));
        }
        Ok(Self { display: CGDisplay::main(), interval: Duration::from_micros(16_667), last: None })
    }
}

impl ScreenCapturer for CgCapturer {
    fn size(&self) -> (u32, u32) {
        (self.display.pixels_wide() as u32, self.display.pixels_high() as u32)
    }

    fn capture(&mut self, timeout: Duration, frame: &mut Frame) -> Result<CaptureStatus, CaptureError> {
        if let Some(last) = self.last {
            let due = last + self.interval;
            let now = Instant::now();
            if due > now {
                let wait = due - now;
                if wait > timeout {
                    std::thread::sleep(timeout);
                    return Ok(CaptureStatus::Unchanged);
                }
                std::thread::sleep(wait);
            }
        }
        self.last = Some(Instant::now());

        let image = self
            .display
            .image()
            .ok_or(CaptureError::PermissionDenied("display image unavailable (permission revoked?)"))?;
        if image.bits_per_pixel() != 32 {
            return Err(CaptureError::Os(format!("unsupported pixel depth {}", image.bits_per_pixel())));
        }
        let data = image.data();
        let ok =
            frame.copy_from_strided(data.bytes(), image.width() as u32, image.height() as u32, image.bytes_per_row());
        if !ok {
            return Err(CaptureError::Os("display image buffer smaller than reported".into()));
        }
        Ok(CaptureStatus::Updated { dirty: None })
    }
}
