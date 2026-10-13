//! macOS CGEvent injector. Requires the Accessibility permission; without it
//! the OS silently drops posted events, so we check up front.

use core_graphics::display::CGDisplay;
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField, ScrollEventUnit};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use remoti_proto::input::{KeyCode, MouseButton};

use super::{InjectError, InputInjector, SafeEvent};

const EXTRA_INFO: i64 = 0x524D_5449;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

pub fn accessibility_trusted() -> bool {
    // SAFETY: no arguments; reads the TCC Accessibility decision.
    unsafe { AXIsProcessTrusted() }
}

pub struct CgEventInjector {
    position: CGPoint,
    held: Vec<MouseButton>,
}

impl CgEventInjector {
    pub fn new() -> Result<Self, InjectError> {
        if !accessibility_trusted() {
            return Err(InjectError::PermissionDenied(
                "grant Accessibility to Remoti in System Settings > Privacy & Security",
            ));
        }
        Ok(Self { position: CGPoint::new(0.0, 0.0), held: Vec::new() })
    }

    fn source() -> Result<CGEventSource, InjectError> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|()| InjectError::Os("CGEventSource".into()))
    }

    fn post(event: CGEvent) {
        event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, EXTRA_INFO);
        event.post(CGEventTapLocation::HID);
    }

    fn mouse(&self, ty: CGEventType, button: CGMouseButton, number: Option<i64>) -> Result<(), InjectError> {
        let ev = CGEvent::new_mouse_event(Self::source()?, ty, self.position, button)
            .map_err(|()| InjectError::Os("CGEventCreateMouseEvent".into()))?;
        if let Some(n) = number {
            ev.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, n);
        }
        Self::post(ev);
        Ok(())
    }
}

impl InputInjector for CgEventInjector {
    fn inject(&mut self, event: &SafeEvent) -> Result<(), InjectError> {
        match event {
            SafeEvent::MouseMove { x, y } => {
                let b = CGDisplay::main().bounds();
                self.position =
                    CGPoint::new(b.origin.x + f64::from(*x) * b.size.width, b.origin.y + f64::from(*y) * b.size.height);
                // While a button is held macOS expects drag events, not moves.
                let (ty, button) = match self.held.first() {
                    Some(MouseButton::Left) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
                    Some(MouseButton::Right) => (CGEventType::RightMouseDragged, CGMouseButton::Right),
                    Some(_) => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
                    None => (CGEventType::MouseMoved, CGMouseButton::Left),
                };
                self.mouse(ty, button, None)
            }
            SafeEvent::MouseButton { button, down } => {
                if *down {
                    self.held.push(*button);
                } else {
                    self.held.retain(|b| b != button);
                }
                let (ty, cg, number) = match (button, down) {
                    (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, None),
                    (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, None),
                    (MouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, None),
                    (MouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, None),
                    (MouseButton::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, Some(2)),
                    (MouseButton::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, Some(2)),
                    (MouseButton::Back, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, Some(3)),
                    (MouseButton::Back, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, Some(3)),
                    (MouseButton::Forward, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, Some(4)),
                    (MouseButton::Forward, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, Some(4)),
                };
                self.mouse(ty, cg, number)
            }
            SafeEvent::Wheel { dx, dy } => {
                // DOM positive dy = content moves up (scroll down); CG is the inverse.
                let ev = CGEvent::new_scroll_event(Self::source()?, ScrollEventUnit::PIXEL, 2, -dy, -dx, 0)
                    .map_err(|()| InjectError::Os("CGEventCreateScrollWheelEvent2".into()))?;
                Self::post(ev);
                Ok(())
            }
            SafeEvent::Key { code, down } => {
                let ev = CGEvent::new_keyboard_event(Self::source()?, keycode_for(*code), *down)
                    .map_err(|()| InjectError::Os("CGEventCreateKeyboardEvent".into()))?;
                Self::post(ev);
                Ok(())
            }
            SafeEvent::Text(text) => {
                let units: Vec<u16> = text.encode_utf16().collect();
                // CGEvent carries at most 20 UTF-16 units per event.
                for chunk in units.chunks(20) {
                    for down in [true, false] {
                        let ev = CGEvent::new_keyboard_event(Self::source()?, 0, down)
                            .map_err(|()| InjectError::Os("CGEventCreateKeyboardEvent".into()))?;
                        ev.set_string_from_utf16_unchecked(chunk);
                        Self::post(ev);
                    }
                }
                Ok(())
            }
        }
    }
}

/// ANSI virtual key codes (`kVK_*` from HIToolbox/Events.h).
fn keycode_for(code: KeyCode) -> u16 {
    use KeyCode::*;
    match code {
        KeyA => 0x00,
        KeyS => 0x01,
        KeyD => 0x02,
        KeyF => 0x03,
        KeyH => 0x04,
        KeyG => 0x05,
        KeyZ => 0x06,
        KeyX => 0x07,
        KeyC => 0x08,
        KeyV => 0x09,
        KeyB => 0x0B,
        KeyQ => 0x0C,
        KeyW => 0x0D,
        KeyE => 0x0E,
        KeyR => 0x0F,
        KeyY => 0x10,
        KeyT => 0x11,
        KeyO => 0x1F,
        KeyU => 0x20,
        KeyI => 0x22,
        KeyP => 0x23,
        KeyL => 0x25,
        KeyJ => 0x26,
        KeyK => 0x28,
        KeyN => 0x2D,
        KeyM => 0x2E,
        Digit1 => 0x12,
        Digit2 => 0x13,
        Digit3 => 0x14,
        Digit4 => 0x15,
        Digit6 => 0x16,
        Digit5 => 0x17,
        Digit9 => 0x19,
        Digit7 => 0x1A,
        Digit8 => 0x1C,
        Digit0 => 0x1D,
        Equal => 0x18,
        Minus => 0x1B,
        BracketRight => 0x1E,
        BracketLeft => 0x21,
        Quote => 0x27,
        Semicolon => 0x29,
        Backslash => 0x2A,
        Comma => 0x2B,
        Slash => 0x2C,
        Period => 0x2F,
        Backquote => 0x32,
        Enter => 0x24,
        Tab => 0x30,
        Space => 0x31,
        Backspace => 0x33,
        Escape => 0x35,
        MetaRight => 0x36,
        MetaLeft => 0x37,
        ShiftLeft => 0x38,
        CapsLock => 0x39,
        AltLeft => 0x3A,
        ControlLeft => 0x3B,
        ShiftRight => 0x3C,
        AltRight => 0x3D,
        ControlRight => 0x3E,
        F1 => 0x7A,
        F2 => 0x78,
        F3 => 0x63,
        F4 => 0x76,
        F5 => 0x60,
        F6 => 0x61,
        F7 => 0x62,
        F8 => 0x64,
        F9 => 0x65,
        F10 => 0x6D,
        F11 => 0x67,
        F12 => 0x6F,
        Insert => 0x72,
        Home => 0x73,
        PageUp => 0x74,
        Delete => 0x75,
        End => 0x77,
        PageDown => 0x79,
        ArrowLeft => 0x7B,
        ArrowRight => 0x7C,
        ArrowDown => 0x7D,
        ArrowUp => 0x7E,
    }
}
