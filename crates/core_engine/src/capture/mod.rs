//! Screen capture backends.
//!
//! * Windows: DXGI Desktop Duplication (GPU copy, OS dirty rectangles).
//! * macOS: CoreGraphics display capture with Screen Recording permission
//!   handling.
//! * Any OS: a synthetic test pattern for development and CI.

use std::time::Duration;

use crate::frame::{Frame, Rect};

#[cfg(windows)]
mod dxgi;
#[cfg(target_os = "macos")]
mod macos;
mod synthetic;

pub use synthetic::SyntheticCapturer;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("screen capture is not supported on this platform")]
    Unsupported,
    #[error("screen capture permission denied: {0}")]
    PermissionDenied(&'static str),
    /// Desktop switched (UAC, lock screen, mode change); recreate the capturer.
    #[error("capture access lost")]
    AccessLost,
    #[error("os capture error: {0}")]
    Os(String),
}

#[derive(Debug)]
pub enum CaptureStatus {
    /// `frame` holds new content. `dirty` are OS hints, when available.
    Updated { dirty: Option<Vec<Rect>> },
    /// Nothing changed within the timeout.
    Unchanged,
}

pub trait ScreenCapturer: Send {
    /// Native resolution of the captured display.
    fn size(&self) -> (u32, u32);

    /// Waits up to `timeout` for a new frame and copies it into `frame`.
    fn capture(&mut self, timeout: Duration, frame: &mut Frame) -> Result<CaptureStatus, CaptureError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CaptureBackend {
    /// Native backend for the current OS.
    Auto,
    /// Moving test pattern; works everywhere.
    Synthetic,
}

/// Opens the primary display.
pub fn open(backend: CaptureBackend) -> Result<Box<dyn ScreenCapturer>, CaptureError> {
    match backend {
        CaptureBackend::Synthetic => Ok(Box::new(SyntheticCapturer::new(1280, 720))),
        CaptureBackend::Auto => open_native(),
    }
}

#[cfg(windows)]
fn open_native() -> Result<Box<dyn ScreenCapturer>, CaptureError> {
    Ok(Box::new(dxgi::DxgiCapturer::new(0)?))
}

#[cfg(target_os = "macos")]
fn open_native() -> Result<Box<dyn ScreenCapturer>, CaptureError> {
    Ok(Box::new(macos::CgCapturer::new()?))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn open_native() -> Result<Box<dyn ScreenCapturer>, CaptureError> {
    Err(CaptureError::Unsupported)
}

/// Whether the OS currently allows this process to capture the screen.
pub fn permission_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::preflight_permission()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Asks the OS to prompt for capture permission where that exists.
pub fn request_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::request_permission()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}
