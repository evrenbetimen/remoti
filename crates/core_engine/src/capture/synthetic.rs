use std::time::{Duration, Instant};

use super::{CaptureError, CaptureStatus, ScreenCapturer};
use crate::frame::{BYTES_PER_PIXEL, Frame, Rect};

/// Static gradient with a moving bar: exercises delta detection (only the bar
/// region changes) and the full encode/transport path without OS APIs.
pub struct SyntheticCapturer {
    width: u32,
    height: u32,
    canvas: Vec<u8>,
    bar_x: u32,
    last: Option<Instant>,
}

const BAR_WIDTH: u32 = 48;
const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

impl SyntheticCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        let mut canvas = vec![0u8; (width * height) as usize * BYTES_PER_PIXEL];
        for y in 0..height {
            for x in 0..width {
                let i = ((y * width + x) as usize) * BYTES_PER_PIXEL;
                canvas[i] = (x * 255 / width.max(1)) as u8;
                canvas[i + 1] = (y * 255 / height.max(1)) as u8;
                canvas[i + 2] = 96;
                canvas[i + 3] = 255;
            }
        }
        Self { width, height, canvas, bar_x: 0, last: None }
    }

    fn paint_bar(&mut self, x0: u32, on: bool) {
        for y in 0..self.height {
            for x in x0..(x0 + BAR_WIDTH).min(self.width) {
                let i = ((y * self.width + x) as usize) * BYTES_PER_PIXEL;
                if on {
                    self.canvas[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
                } else {
                    self.canvas[i] = (x * 255 / self.width.max(1)) as u8;
                    self.canvas[i + 1] = (y * 255 / self.height.max(1)) as u8;
                    self.canvas[i + 2] = 96;
                    self.canvas[i + 3] = 255;
                }
            }
        }
    }
}

impl ScreenCapturer for SyntheticCapturer {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn capture(&mut self, timeout: Duration, frame: &mut Frame) -> Result<CaptureStatus, CaptureError> {
        // Emulate a 60 Hz display: block until the next vsync-like tick.
        if let Some(last) = self.last {
            let due = last + FRAME_INTERVAL;
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

        let old = self.bar_x;
        self.paint_bar(old, false);
        self.bar_x = (self.bar_x + 8) % self.width.saturating_sub(BAR_WIDTH).max(1);
        self.paint_bar(self.bar_x, true);

        let stride = self.width as usize * BYTES_PER_PIXEL;
        if !frame.copy_from_strided(&self.canvas, self.width, self.height, stride) {
            return Err(CaptureError::Os("synthetic canvas size mismatch".into()));
        }
        let dirty = vec![
            Rect { x: old, y: 0, width: BAR_WIDTH, height: self.height },
            Rect { x: self.bar_x, y: 0, width: BAR_WIDTH, height: self.height },
        ];
        Ok(CaptureStatus::Updated { dirty: Some(dirty) })
    }
}
