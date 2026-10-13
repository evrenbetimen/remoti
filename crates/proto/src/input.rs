//! Remote input events. Coordinates are normalized to `[0, 1]` relative to the
//! captured display so the viewer never needs to know host resolution or DPI.
//! Key codes use DOM `KeyboardEvent.code` names; anything outside the
//! [`KeyCode`] set fails to deserialize and is therefore never injected.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Externally tagged so the same type works for postcard (wire) and JSON
/// (Electron IPC): `{"key":{"code":"KeyA","down":true}}`, `"releaseAll"`.
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum InputEvent {
    MouseMove {
        x: f32,
        y: f32,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    Wheel {
        dx: f32,
        dy: f32,
    },
    Key {
        code: KeyCode,
        down: bool,
    },
    /// Committed text (IME / paste-as-typing). Length-limited by the sanitizer.
    Text {
        text: String,
    },
    /// Release every key and button currently held (focus loss on the viewer).
    ReleaseAll,
}

macro_rules! key_codes {
    ($($variant:ident => $dom:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum KeyCode {
            $(#[serde(rename = $dom)] $variant,)+
        }

        impl KeyCode {
            pub const ALL: &'static [KeyCode] = &[$(KeyCode::$variant,)+];

            pub fn dom_code(self) -> &'static str {
                match self { $(KeyCode::$variant => $dom,)+ }
            }

            pub fn from_dom_code(code: &str) -> Option<KeyCode> {
                match code { $($dom => Some(KeyCode::$variant),)+ _ => None }
            }
        }
    };
}

key_codes! {
    KeyA => "KeyA", KeyB => "KeyB", KeyC => "KeyC", KeyD => "KeyD", KeyE => "KeyE",
    KeyF => "KeyF", KeyG => "KeyG", KeyH => "KeyH", KeyI => "KeyI", KeyJ => "KeyJ",
    KeyK => "KeyK", KeyL => "KeyL", KeyM => "KeyM", KeyN => "KeyN", KeyO => "KeyO",
    KeyP => "KeyP", KeyQ => "KeyQ", KeyR => "KeyR", KeyS => "KeyS", KeyT => "KeyT",
    KeyU => "KeyU", KeyV => "KeyV", KeyW => "KeyW", KeyX => "KeyX", KeyY => "KeyY",
    KeyZ => "KeyZ",
    Digit0 => "Digit0", Digit1 => "Digit1", Digit2 => "Digit2", Digit3 => "Digit3",
    Digit4 => "Digit4", Digit5 => "Digit5", Digit6 => "Digit6", Digit7 => "Digit7",
    Digit8 => "Digit8", Digit9 => "Digit9",
    F1 => "F1", F2 => "F2", F3 => "F3", F4 => "F4", F5 => "F5", F6 => "F6",
    F7 => "F7", F8 => "F8", F9 => "F9", F10 => "F10", F11 => "F11", F12 => "F12",
    Enter => "Enter", Escape => "Escape", Backspace => "Backspace", Tab => "Tab",
    Space => "Space", Minus => "Minus", Equal => "Equal", BracketLeft => "BracketLeft",
    BracketRight => "BracketRight", Backslash => "Backslash", Semicolon => "Semicolon",
    Quote => "Quote", Backquote => "Backquote", Comma => "Comma", Period => "Period",
    Slash => "Slash", CapsLock => "CapsLock",
    ArrowLeft => "ArrowLeft", ArrowRight => "ArrowRight", ArrowUp => "ArrowUp",
    ArrowDown => "ArrowDown", Home => "Home", End => "End", PageUp => "PageUp",
    PageDown => "PageDown", Insert => "Insert", Delete => "Delete",
    ShiftLeft => "ShiftLeft", ShiftRight => "ShiftRight", ControlLeft => "ControlLeft",
    ControlRight => "ControlRight", AltLeft => "AltLeft", AltRight => "AltRight",
    MetaLeft => "MetaLeft", MetaRight => "MetaRight",
}

impl KeyCode {
    pub fn is_modifier(self) -> bool {
        matches!(
            self,
            KeyCode::ShiftLeft
                | KeyCode::ShiftRight
                | KeyCode::ControlLeft
                | KeyCode::ControlRight
                | KeyCode::AltLeft
                | KeyCode::AltRight
                | KeyCode::MetaLeft
                | KeyCode::MetaRight
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dom_codes_roundtrip() {
        for k in KeyCode::ALL {
            assert_eq!(KeyCode::from_dom_code(k.dom_code()), Some(*k));
        }
    }

    #[test]
    fn unknown_key_is_rejected_at_parse_time() {
        let ok: Result<InputEvent, _> = serde_json::from_str(r#"{"key":{"code":"KeyA","down":true}}"#);
        assert!(ok.is_ok());
        let bad: Result<InputEvent, _> = serde_json::from_str(r#"{"key":{"code":"Power","down":true}}"#);
        assert!(bad.is_err());
    }

    #[test]
    fn postcard_roundtrip() {
        let ev = InputEvent::Key { code: KeyCode::Enter, down: true };
        let enc = postcard::to_stdvec(&ev).unwrap();
        let back: InputEvent = postcard::from_bytes(&enc).unwrap();
        assert_eq!(back, ev);
    }
}
