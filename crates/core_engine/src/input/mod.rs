//! Remote input: sanitization plus OS-level injection.
//!
//! Every event from the viewer passes through [`InputSanitizer`] before it can
//! reach [`InputInjector`]. Only [`SafeEvent`]s, which are already clamped,
//! rate-limited and policy-checked, can be injected.

#[cfg(target_os = "macos")]
mod macos;
mod sanitize;
#[cfg(windows)]
mod windows;

pub use sanitize::{Chord, InputPolicy, InputSanitizer, Modifiers, SafeEvent};

#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    #[error("input injection is not supported on this platform")]
    Unsupported,
    #[error("input injection permission denied: {0}")]
    PermissionDenied(&'static str),
    #[error("os input error: {0}")]
    Os(String),
}

pub trait InputInjector: Send {
    fn inject(&mut self, event: &SafeEvent) -> Result<(), InjectError>;
}

/// Native injector for the current OS.
pub fn native() -> Result<Box<dyn InputInjector>, InjectError> {
    #[cfg(windows)]
    {
        Ok(Box::new(windows::SendInputInjector::new()))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::CgEventInjector::new()?))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(InjectError::Unsupported)
    }
}

/// Whether the OS lets this process inject input (macOS Accessibility).
pub fn permission_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::accessibility_trusted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Injector that only logs; used for view-only sessions and unsupported OSes.
pub struct NullInjector;

impl InputInjector for NullInjector {
    fn inject(&mut self, event: &SafeEvent) -> Result<(), InjectError> {
        tracing::trace!(?event, "input discarded (no injector)");
        Ok(())
    }
}
