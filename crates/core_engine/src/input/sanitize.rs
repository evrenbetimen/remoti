use std::collections::BTreeSet;
use std::time::Instant;

use remoti_proto::input::{InputEvent, KeyCode, MouseButton};
use tracing::debug;

/// Modifier state used for chord matching (left/right collapsed).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

impl Modifiers {
    pub const CTRL: Modifiers = Modifiers { ctrl: true, alt: false, shift: false, meta: false };
    pub const CTRL_ALT: Modifiers = Modifiers { ctrl: true, alt: true, shift: false, meta: false };
    pub const META: Modifiers = Modifiers { ctrl: false, alt: false, shift: false, meta: true };

    fn from_pressed(keys: &BTreeSet<KeyCode>) -> Self {
        let has = |a, b| keys.contains(&a) || keys.contains(&b);
        Modifiers {
            ctrl: has(KeyCode::ControlLeft, KeyCode::ControlRight),
            alt: has(KeyCode::AltLeft, KeyCode::AltRight),
            shift: has(KeyCode::ShiftLeft, KeyCode::ShiftRight),
            meta: has(KeyCode::MetaLeft, KeyCode::MetaRight),
        }
    }
}

/// A key combination the host refuses to receive remotely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: Modifiers,
    pub key: KeyCode,
}

#[derive(Debug, Clone)]
pub struct InputPolicy {
    /// Host-side switch: view-only sessions drop all input.
    pub allow_control: bool,
    pub max_events_per_sec: u32,
    pub burst: u32,
    pub max_text_chars: usize,
    /// Maximum wheel delta per event, in pixels.
    pub max_wheel: f32,
    pub blocked_chords: Vec<Chord>,
}

impl Default for InputPolicy {
    fn default() -> Self {
        Self {
            allow_control: true,
            max_events_per_sec: 600,
            burst: 120,
            max_text_chars: 256,
            max_wheel: 1200.0,
            blocked_chords: vec![
                // Secure attention sequence: never forwarded.
                Chord { modifiers: Modifiers::CTRL_ALT, key: KeyCode::Delete },
                // Locking the host would blank the remote session.
                Chord { modifiers: Modifiers::META, key: KeyCode::KeyL },
            ],
        }
    }
}

/// Event that passed sanitization. Coordinates in `[0, 1]`, wheel in pixels.
#[derive(Debug, Clone, PartialEq)]
pub enum SafeEvent {
    MouseMove { x: f32, y: f32 },
    MouseButton { button: MouseButton, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { code: KeyCode, down: bool },
    Text(String),
}

struct TokenBucket {
    capacity: f64,
    tokens: f64,
    rate: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(rate: u32, burst: u32, now: Instant) -> Self {
        let capacity = f64::from(burst.max(1));
        Self { capacity, tokens: capacity, rate: f64::from(rate.max(1)), last: now }
    }

    fn take(&mut self, now: Instant) -> bool {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.rate).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub struct InputSanitizer {
    policy: InputPolicy,
    bucket: TokenBucket,
    keys: BTreeSet<KeyCode>,
    buttons: Vec<MouseButton>,
    dropped: u64,
}

impl InputSanitizer {
    pub fn new(policy: InputPolicy, now: Instant) -> Self {
        let bucket = TokenBucket::new(policy.max_events_per_sec, policy.burst, now);
        Self { policy, bucket, keys: BTreeSet::new(), buttons: Vec::new(), dropped: 0 }
    }

    pub fn set_allow_control(&mut self, allowed: bool) -> Vec<SafeEvent> {
        self.policy.allow_control = allowed;
        if allowed { Vec::new() } else { self.release_all() }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    fn drop_event(&mut self, why: &'static str) -> Vec<SafeEvent> {
        self.dropped += 1;
        debug!(why, "remote input dropped");
        Vec::new()
    }

    /// Validates one remote event; returns the events to inject (possibly none).
    pub fn process(&mut self, event: InputEvent, now: Instant) -> Vec<SafeEvent> {
        if matches!(event, InputEvent::ReleaseAll) {
            return self.release_all();
        }
        if !self.policy.allow_control {
            return self.drop_event("control not permitted");
        }
        // Releases are never rate-limited: dropping one would leave keys stuck.
        let is_release =
            matches!(event, InputEvent::Key { down: false, .. } | InputEvent::MouseButton { down: false, .. });
        if !is_release && !self.bucket.take(now) {
            return self.drop_event("rate limit");
        }
        match event {
            InputEvent::MouseMove { x, y } => {
                if !x.is_finite() || !y.is_finite() {
                    return self.drop_event("non-finite coordinates");
                }
                vec![SafeEvent::MouseMove { x: x.clamp(0.0, 1.0), y: y.clamp(0.0, 1.0) }]
            }
            InputEvent::MouseButton { button, down } => {
                let held = self.buttons.contains(&button);
                match (down, held) {
                    (true, true) => self.drop_event("button already down"),
                    (false, false) => self.drop_event("button not down"),
                    (true, false) => {
                        self.buttons.push(button);
                        vec![SafeEvent::MouseButton { button, down }]
                    }
                    (false, true) => {
                        self.buttons.retain(|b| *b != button);
                        vec![SafeEvent::MouseButton { button, down }]
                    }
                }
            }
            InputEvent::Wheel { dx, dy } => {
                if !dx.is_finite() || !dy.is_finite() {
                    return self.drop_event("non-finite wheel");
                }
                let m = self.policy.max_wheel;
                let (dx, dy) = (dx.clamp(-m, m).round() as i32, dy.clamp(-m, m).round() as i32);
                if dx == 0 && dy == 0 {
                    return Vec::new();
                }
                vec![SafeEvent::Wheel { dx, dy }]
            }
            InputEvent::Key { code, down: true } => {
                let mut would_be = self.keys.clone();
                would_be.insert(code);
                let mods = Modifiers::from_pressed(&would_be);
                let blocked = !code.is_modifier()
                    && self.policy.blocked_chords.iter().any(|c| c.key == code && modifiers_cover(mods, c.modifiers));
                if blocked {
                    return self.drop_event("blocked key chord");
                }
                // Repeats of a held key are legitimate auto-repeat.
                self.keys.insert(code);
                vec![SafeEvent::Key { code, down: true }]
            }
            InputEvent::Key { code, down: false } => {
                if self.keys.remove(&code) {
                    vec![SafeEvent::Key { code, down: false }]
                } else {
                    self.drop_event("key not down")
                }
            }
            InputEvent::Text { text } => {
                let clean: String = text
                    .chars()
                    .filter(|c| !c.is_control() && !is_bidi_override(*c))
                    .take(self.policy.max_text_chars)
                    .collect();
                if clean.is_empty() {
                    return self.drop_event("empty text after filtering");
                }
                vec![SafeEvent::Text(clean)]
            }
            InputEvent::ReleaseAll => self.release_all(),
        }
    }

    /// Releases everything held (session end, focus loss, control revoked).
    pub fn release_all(&mut self) -> Vec<SafeEvent> {
        let mut out: Vec<SafeEvent> = std::mem::take(&mut self.buttons)
            .into_iter()
            .map(|button| SafeEvent::MouseButton { button, down: false })
            .collect();
        // Non-modifiers first, then modifiers, mirroring natural release order.
        let keys = std::mem::take(&mut self.keys);
        let (mods, plain): (Vec<_>, Vec<_>) = keys.into_iter().partition(|k| k.is_modifier());
        out.extend(plain.into_iter().chain(mods).map(|code| SafeEvent::Key { code, down: false }));
        out
    }
}

fn modifiers_cover(have: Modifiers, need: Modifiers) -> bool {
    (!need.ctrl || have.ctrl) && (!need.alt || have.alt) && (!need.shift || have.shift) && (!need.meta || have.meta)
}

/// Unicode bidi overrides can disguise typed text (e.g. in terminals).
fn is_bidi_override(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn san() -> (InputSanitizer, Instant) {
        let now = Instant::now();
        (InputSanitizer::new(InputPolicy::default(), now), now)
    }

    #[test]
    fn clamps_and_rejects_nan() {
        let (mut s, t) = san();
        assert_eq!(
            s.process(InputEvent::MouseMove { x: 2.0, y: -1.0 }, t),
            vec![SafeEvent::MouseMove { x: 1.0, y: 0.0 }]
        );
        assert!(s.process(InputEvent::MouseMove { x: f32::NAN, y: 0.5 }, t).is_empty());
    }

    #[test]
    fn blocks_ctrl_alt_delete() {
        let (mut s, t) = san();
        s.process(InputEvent::Key { code: KeyCode::ControlLeft, down: true }, t);
        s.process(InputEvent::Key { code: KeyCode::AltRight, down: true }, t);
        assert!(s.process(InputEvent::Key { code: KeyCode::Delete, down: true }, t).is_empty());
        // The blocked key was never recorded as held, so its release is dropped too.
        assert!(s.process(InputEvent::Key { code: KeyCode::Delete, down: false }, t).is_empty());
    }

    #[test]
    fn rate_limits_but_never_drops_releases() {
        let (mut s, t) = san();
        s.process(InputEvent::Key { code: KeyCode::KeyA, down: true }, t);
        for _ in 0..500 {
            s.process(InputEvent::MouseMove { x: 0.5, y: 0.5 }, t);
        }
        assert!(s.dropped() > 0);
        assert_eq!(s.process(InputEvent::Key { code: KeyCode::KeyA, down: false }, t).len(), 1);
        // Bucket refills over time.
        assert_eq!(s.process(InputEvent::MouseMove { x: 0.1, y: 0.1 }, t + Duration::from_secs(1)).len(), 1);
    }

    #[test]
    fn release_all_frees_held_state() {
        let (mut s, t) = san();
        s.process(InputEvent::Key { code: KeyCode::ShiftLeft, down: true }, t);
        s.process(InputEvent::Key { code: KeyCode::KeyX, down: true }, t);
        s.process(InputEvent::MouseButton { button: MouseButton::Left, down: true }, t);
        let released = s.process(InputEvent::ReleaseAll, t);
        assert_eq!(
            released,
            vec![
                SafeEvent::MouseButton { button: MouseButton::Left, down: false },
                SafeEvent::Key { code: KeyCode::KeyX, down: false },
                SafeEvent::Key { code: KeyCode::ShiftLeft, down: false },
            ]
        );
        assert!(s.release_all().is_empty());
    }

    #[test]
    fn view_only_drops_everything() {
        let (mut s, t) = san();
        s.process(InputEvent::Key { code: KeyCode::KeyA, down: true }, t);
        let released = s.set_allow_control(false);
        assert_eq!(released, vec![SafeEvent::Key { code: KeyCode::KeyA, down: false }]);
        assert!(s.process(InputEvent::MouseMove { x: 0.5, y: 0.5 }, t).is_empty());
    }

    #[test]
    fn text_is_filtered() {
        let (mut s, t) = san();
        let out = s.process(InputEvent::Text { text: "ab\u{0007}\u{202E}c\n".into() }, t);
        assert_eq!(out, vec![SafeEvent::Text("abc".into())]);
    }
}
