//! Adaptive bitrate + frame-rate controller (delay- and loss-based, in the
//! spirit of WebRTC's GCC).
//!
//! Inputs every sampling tick: smoothed RTT, packet loss, video backlog on the
//! sender, frames dropped at the source and the viewer's decode queue.
//! Output: target encoder bitrate and capture FPS. Decreases are
//! multiplicative and fast; increases are additive and slow, so the stream
//! backs off quickly when Wi-Fi degrades and probes upward gently.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct RateConfig {
    pub min_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub start_bitrate_bps: u32,
    pub min_fps: u16,
    pub max_fps: u16,
}

impl Default for RateConfig {
    fn default() -> Self {
        Self {
            min_bitrate_bps: 300_000,
            max_bitrate_bps: 20_000_000,
            start_bitrate_bps: 4_000_000,
            min_fps: 10,
            max_fps: 60,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RateSample {
    pub rtt: Duration,
    pub loss: f32,
    pub video_backlog: usize,
    pub frames_dropped: u32,
    pub decode_queue: u16,
    /// Bitrate actually produced by the encoder over the last interval.
    pub sent_bps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    pub bitrate_bps: u32,
    pub fps: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Overuse,
    Normal,
    Underuse,
}

pub struct RateController {
    cfg: RateConfig,
    bitrate: f64,
    fps: u16,
    base_rtt: Option<Duration>,
    base_rtt_set_at: Instant,
    last_decrease: Option<Instant>,
    last_update: Instant,
    published: RateDecision,
}

const BASE_RTT_WINDOW: Duration = Duration::from_secs(10);
const DECREASE_HOLDOFF: Duration = Duration::from_millis(300);
const INCREASE_PER_SEC: f64 = 0.08;
const HIGH_LOSS: f32 = 0.10;
const LOW_LOSS: f32 = 0.02;
const APP_LIMITED_RATIO: f64 = 0.5;
const FPS_STEPS: [u16; 6] = [10, 15, 24, 30, 45, 60];

impl RateController {
    pub fn new(cfg: RateConfig, now: Instant) -> Self {
        let cfg = RateConfig {
            max_bitrate_bps: cfg.max_bitrate_bps.max(cfg.min_bitrate_bps),
            min_fps: cfg.min_fps.max(1),
            max_fps: cfg.max_fps.max(cfg.min_fps.max(1)),
            ..cfg
        };
        let bitrate = f64::from(cfg.start_bitrate_bps.clamp(cfg.min_bitrate_bps, cfg.max_bitrate_bps));
        let fps = fps_for(&cfg, bitrate);
        Self {
            cfg,
            bitrate,
            fps,
            base_rtt: None,
            base_rtt_set_at: now,
            last_decrease: None,
            last_update: now,
            published: RateDecision { bitrate_bps: bitrate as u32, fps },
        }
    }

    pub fn current(&self) -> RateDecision {
        self.published
    }

    fn classify(&mut self, s: &RateSample, now: Instant) -> Signal {
        if !s.rtt.is_zero() {
            let expired = now.duration_since(self.base_rtt_set_at) > BASE_RTT_WINDOW;
            match self.base_rtt {
                Some(b) if !expired && s.rtt >= b => {}
                _ => {
                    self.base_rtt = Some(s.rtt);
                    self.base_rtt_set_at = now;
                }
            }
        }
        let queuing = match self.base_rtt {
            Some(base) if !s.rtt.is_zero() => {
                let allowance = (base / 2).max(Duration::from_millis(30));
                s.rtt > base + allowance
            }
            _ => false,
        };
        if s.loss > HIGH_LOSS || queuing || s.video_backlog >= 2 || s.frames_dropped > 0 || s.decode_queue > 3 {
            Signal::Overuse
        } else if s.loss < LOW_LOSS && s.video_backlog == 0 && s.decode_queue <= 1 {
            Signal::Underuse
        } else {
            Signal::Normal
        }
    }

    /// Feeds one sample; returns a new decision when it changed meaningfully.
    pub fn on_sample(&mut self, s: RateSample, now: Instant) -> Option<RateDecision> {
        let elapsed = now.duration_since(self.last_update).as_secs_f64().min(1.0);
        self.last_update = now;
        let (min, max) = (f64::from(self.cfg.min_bitrate_bps), f64::from(self.cfg.max_bitrate_bps));

        match self.classify(&s, now) {
            Signal::Overuse => {
                let can_decrease = self.last_decrease.is_none_or(|t| now.duration_since(t) >= DECREASE_HOLDOFF);
                if can_decrease {
                    let factor = if s.loss > HIGH_LOSS { 1.0 - 0.5 * f64::from(s.loss.min(0.8)) } else { 0.85 };
                    self.bitrate = (self.bitrate * factor).max(min);
                    self.last_decrease = Some(now);
                }
            }
            // Application-limited (static screen): probing higher would only
            // cause a burst later, so increase only while the budget is used.
            Signal::Underuse if f64::from(s.sent_bps) >= self.bitrate * APP_LIMITED_RATIO => {
                // Additive increase relative to max so small rates recover too.
                self.bitrate = (self.bitrate + max * INCREASE_PER_SEC * elapsed).min(max);
            }
            Signal::Underuse => {}
            Signal::Normal => {}
        }
        self.fps = fps_for(&self.cfg, self.bitrate);

        let candidate = RateDecision { bitrate_bps: self.bitrate as u32, fps: self.fps };
        let prev = self.published;
        let rel =
            (f64::from(candidate.bitrate_bps) - f64::from(prev.bitrate_bps)).abs() / f64::from(prev.bitrate_bps.max(1));
        let at_bound = candidate.bitrate_bps != prev.bitrate_bps
            && (candidate.bitrate_bps == self.cfg.min_bitrate_bps || candidate.bitrate_bps == self.cfg.max_bitrate_bps);
        if candidate.fps != prev.fps || rel >= 0.10 || at_bound {
            self.published = candidate;
            Some(candidate)
        } else {
            None
        }
    }
}

/// Lower bitrates get fewer, sharper frames: text legibility matters more
/// than motion smoothness for remote desktop.
fn fps_for(cfg: &RateConfig, bitrate: f64) -> u16 {
    let range = f64::from(cfg.max_bitrate_bps - cfg.min_bitrate_bps).max(1.0);
    let ratio = ((bitrate - f64::from(cfg.min_bitrate_bps)) / range).clamp(0.0, 1.0);
    // Reach full frame rate at ~25% of the bitrate range.
    let wanted =
        f64::from(cfg.min_fps) + (f64::from(cfg.max_fps) - f64::from(cfg.min_fps)) * (ratio * 4.0).min(1.0).sqrt();
    let stepped = FPS_STEPS.iter().copied().filter(|&f| f64::from(f) <= wanted + 0.5).max().unwrap_or(cfg.min_fps);
    stepped.clamp(cfg.min_fps, cfg.max_fps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(rtt_ms: u64, loss: f32, backlog: usize) -> RateSample {
        RateSample {
            rtt: Duration::from_millis(rtt_ms),
            loss,
            video_backlog: backlog,
            sent_bps: u32::MAX,
            ..Default::default()
        }
    }

    #[test]
    fn backs_off_on_loss_and_recovers() {
        let t0 = Instant::now();
        let mut rc = RateController::new(RateConfig::default(), t0);
        let start = rc.current().bitrate_bps;
        let mut t = t0;
        for _ in 0..15 {
            t += Duration::from_millis(400);
            rc.on_sample(sample(40, 0.25, 0), t);
        }
        let low = rc.current();
        assert!(low.bitrate_bps < start / 4, "{low:?}");
        assert!(low.fps < 60);
        for _ in 0..200 {
            t += Duration::from_millis(250);
            rc.on_sample(sample(40, 0.0, 0), t);
        }
        assert!(rc.current().bitrate_bps > start);
        assert_eq!(rc.current().fps, 60);
    }

    #[test]
    fn detects_queuing_delay() {
        let t0 = Instant::now();
        let mut rc = RateController::new(RateConfig::default(), t0);
        let mut t = t0;
        for _ in 0..4 {
            t += Duration::from_millis(250);
            rc.on_sample(sample(30, 0.0, 0), t);
        }
        let before = rc.current().bitrate_bps;
        t += Duration::from_millis(400);
        rc.on_sample(sample(150, 0.0, 0), t);
        assert!(rc.current().bitrate_bps < before);
    }

    #[test]
    fn does_not_probe_when_application_limited() {
        let t0 = Instant::now();
        let mut rc = RateController::new(RateConfig::default(), t0);
        let start = rc.current().bitrate_bps;
        let mut t = t0;
        for _ in 0..100 {
            t += Duration::from_millis(250);
            rc.on_sample(RateSample { rtt: Duration::from_millis(20), sent_bps: 100_000, ..Default::default() }, t);
        }
        assert_eq!(rc.current().bitrate_bps, start);
    }

    #[test]
    fn respects_bounds() {
        let cfg = RateConfig {
            min_bitrate_bps: 500_000,
            max_bitrate_bps: 2_000_000,
            start_bitrate_bps: 1_000_000,
            min_fps: 15,
            max_fps: 30,
        };
        let t0 = Instant::now();
        let mut rc = RateController::new(cfg, t0);
        let mut t = t0;
        for _ in 0..100 {
            t += Duration::from_millis(400);
            rc.on_sample(sample(20, 0.9, 5), t);
        }
        assert_eq!(rc.current().bitrate_bps, 500_000);
        assert_eq!(rc.current().fps, 15);
        for _ in 0..500 {
            t += Duration::from_millis(400);
            rc.on_sample(sample(20, 0.0, 0), t);
        }
        assert_eq!(rc.current().bitrate_bps, 2_000_000);
        assert_eq!(rc.current().fps, 30);
    }
}
