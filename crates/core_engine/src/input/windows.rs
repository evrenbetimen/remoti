//! Windows `SendInput` wrapper.
//!
//! * Absolute mouse coordinates are mapped onto the primary display
//!   (`0..=65535`), matching the DXGI output we capture.
//! * Keys are sent as virtual-key + scan code with the extended flag where the
//!   key lives on the extended keypad, so games and RDP-aware apps see them.
//! * Every injected event carries a Remoti marker in `dwExtraInfo`.

use remoti_proto::input::{KeyCode, MouseButton};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MapVirtualKeyW, SendInput, VIRTUAL_KEY,
};

use super::{InjectError, InputInjector, SafeEvent};

/// "RMTI" — lets local tooling recognise Remoti-injected events.
const EXTRA_INFO: usize = 0x524D_5449;
const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;
const WHEEL_DELTA: f32 = 120.0;
/// DOM wheel pixels per Windows notch (Chromium uses 100px per notch).
const PIXELS_PER_NOTCH: f32 = 100.0;

pub struct SendInputInjector;

impl SendInputInjector {
    pub fn new() -> Self {
        Self
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: EXTRA_INFO },
        },
    }
}

fn key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: EXTRA_INFO },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<(), InjectError> {
    if inputs.is_empty() {
        return Ok(());
    }
    // SAFETY: `inputs` is a valid slice of initialised INPUT structures.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        // Blocked by UIPI (elevated foreground window) or the secure desktop.
        Err(InjectError::Os(format!(
            "SendInput injected {sent}/{} events: {}",
            inputs.len(),
            windows::core::Error::from_thread()
        )))
    }
}

fn to_absolute(v: f32) -> i32 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as i32
}

impl InputInjector for SendInputInjector {
    fn inject(&mut self, event: &SafeEvent) -> Result<(), InjectError> {
        match event {
            SafeEvent::MouseMove { x, y } => {
                send(&[mouse(to_absolute(*x), to_absolute(*y), 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE)])
            }
            SafeEvent::MouseButton { button, down } => {
                let (flags, data) = match (button, down) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                    (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                    (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                    (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
                };
                send(&[mouse(0, 0, data, flags)])
            }
            SafeEvent::Wheel { dx, dy } => {
                let mut inputs = Vec::with_capacity(2);
                let notch = |px: i32| ((px as f32 / PIXELS_PER_NOTCH) * WHEEL_DELTA).round() as i32;
                if *dy != 0 {
                    // DOM: positive = scroll down; Windows: positive = away from user.
                    inputs.push(mouse(0, 0, (-notch(*dy)) as u32, MOUSEEVENTF_WHEEL));
                }
                if *dx != 0 {
                    inputs.push(mouse(0, 0, notch(*dx) as u32, MOUSEEVENTF_HWHEEL));
                }
                send(&inputs)
            }
            SafeEvent::Key { code, down } => {
                let (vk, extended) = vk_for(*code);
                // SAFETY: pure lookup without pointers.
                let scan = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) } as u16;
                let mut flags = KEYBD_EVENT_FLAGS(0);
                if extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                if !down {
                    flags |= KEYEVENTF_KEYUP;
                }
                send(&[key(vk, scan, flags)])
            }
            SafeEvent::Text(text) => {
                let mut inputs = Vec::with_capacity(text.len() * 4);
                for unit in text.encode_utf16() {
                    inputs.push(key(0, unit, KEYEVENTF_UNICODE));
                    inputs.push(key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                }
                send(&inputs)
            }
        }
    }
}

/// Virtual-key code and whether it needs `KEYEVENTF_EXTENDEDKEY`.
fn vk_for(code: KeyCode) -> (u16, bool) {
    use KeyCode::*;
    match code {
        KeyA => (0x41, false),
        KeyB => (0x42, false),
        KeyC => (0x43, false),
        KeyD => (0x44, false),
        KeyE => (0x45, false),
        KeyF => (0x46, false),
        KeyG => (0x47, false),
        KeyH => (0x48, false),
        KeyI => (0x49, false),
        KeyJ => (0x4A, false),
        KeyK => (0x4B, false),
        KeyL => (0x4C, false),
        KeyM => (0x4D, false),
        KeyN => (0x4E, false),
        KeyO => (0x4F, false),
        KeyP => (0x50, false),
        KeyQ => (0x51, false),
        KeyR => (0x52, false),
        KeyS => (0x53, false),
        KeyT => (0x54, false),
        KeyU => (0x55, false),
        KeyV => (0x56, false),
        KeyW => (0x57, false),
        KeyX => (0x58, false),
        KeyY => (0x59, false),
        KeyZ => (0x5A, false),
        Digit0 => (0x30, false),
        Digit1 => (0x31, false),
        Digit2 => (0x32, false),
        Digit3 => (0x33, false),
        Digit4 => (0x34, false),
        Digit5 => (0x35, false),
        Digit6 => (0x36, false),
        Digit7 => (0x37, false),
        Digit8 => (0x38, false),
        Digit9 => (0x39, false),
        F1 => (0x70, false),
        F2 => (0x71, false),
        F3 => (0x72, false),
        F4 => (0x73, false),
        F5 => (0x74, false),
        F6 => (0x75, false),
        F7 => (0x76, false),
        F8 => (0x77, false),
        F9 => (0x78, false),
        F10 => (0x79, false),
        F11 => (0x7A, false),
        F12 => (0x7B, false),
        Enter => (0x0D, false),
        Escape => (0x1B, false),
        Backspace => (0x08, false),
        Tab => (0x09, false),
        Space => (0x20, false),
        Minus => (0xBD, false),
        Equal => (0xBB, false),
        BracketLeft => (0xDB, false),
        BracketRight => (0xDD, false),
        Backslash => (0xDC, false),
        Semicolon => (0xBA, false),
        Quote => (0xDE, false),
        Backquote => (0xC0, false),
        Comma => (0xBC, false),
        Period => (0xBE, false),
        Slash => (0xBF, false),
        CapsLock => (0x14, false),
        ArrowLeft => (0x25, true),
        ArrowUp => (0x26, true),
        ArrowRight => (0x27, true),
        ArrowDown => (0x28, true),
        Home => (0x24, true),
        End => (0x23, true),
        PageUp => (0x21, true),
        PageDown => (0x22, true),
        Insert => (0x2D, true),
        Delete => (0x2E, true),
        ShiftLeft => (0xA0, false),
        ShiftRight => (0xA1, false),
        ControlLeft => (0xA2, false),
        ControlRight => (0xA3, true),
        AltLeft => (0xA4, false),
        AltRight => (0xA5, true),
        MetaLeft => (0x5B, true),
        MetaRight => (0x5C, true),
    }
}
