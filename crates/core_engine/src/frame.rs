//! Frame buffers and delta detection.
//!
//! Capture backends write into a reusable, tightly packed BGRA [`Frame`]
//! (stride removed, dimensions cropped to even numbers for 4:2:0 encoding).
//! [`DeltaTracker`] hashes 64x64 tiles with XXH3 and reports which tiles changed
//! since the previous frame, re-hashing only regions the OS flagged as dirty
//! when such hints exist. Unchanged frames are never encoded or sent.

use std::time::Instant;

use xxhash_rust::xxh3::xxh3_64;

pub const BYTES_PER_PIXEL: usize = 4;
pub const TILE: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn right(&self) -> u32 {
        self.x.saturating_add(self.width)
    }

    pub fn bottom(&self) -> u32 {
        self.y.saturating_add(self.height)
    }

    pub fn union(&self, o: &Rect) -> Rect {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        Rect { x, y, width: self.right().max(o.right()) - x, height: self.bottom().max(o.bottom()) - y }
    }
}

/// Tightly packed BGRA frame. The allocation is reused across captures.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub captured_at: Instant,
}

impl Frame {
    pub fn new() -> Self {
        Self { width: 0, height: 0, data: Vec::new(), captured_at: Instant::now() }
    }

    pub fn stride(&self) -> usize {
        self.width as usize * BYTES_PER_PIXEL
    }

    /// Copies a strided source image, cropping to even dimensions.
    /// Returns `false` (leaving the frame untouched) if `src` is too small.
    pub fn copy_from_strided(&mut self, src: &[u8], width: u32, height: u32, src_stride: usize) -> bool {
        let w = width & !1;
        let h = height & !1;
        let row = w as usize * BYTES_PER_PIXEL;
        if w == 0 || h == 0 || src_stride < row {
            return false;
        }
        let needed = src_stride * (h as usize - 1) + row;
        if src.len() < needed {
            return false;
        }
        self.width = w;
        self.height = h;
        self.data.resize(row * h as usize, 0);
        if src_stride == row {
            self.data.copy_from_slice(&src[..row * h as usize]);
        } else {
            for (y, dst) in self.data.chunks_exact_mut(row).enumerate() {
                let off = y * src_stride;
                dst.copy_from_slice(&src[off..off + row]);
            }
        }
        self.captured_at = Instant::now();
        true
    }
}

impl Default for Frame {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delta {
    pub dirty_tiles: u32,
    pub total_tiles: u32,
    /// Bounding box of all changed tiles.
    pub bounds: Option<Rect>,
    /// Dimensions changed (or first frame): the encoder must restart.
    pub resized: bool,
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.dirty_tiles == 0 && !self.resized
    }

    pub fn dirty_ratio(&self) -> f32 {
        if self.total_tiles == 0 { 0.0 } else { self.dirty_tiles as f32 / self.total_tiles as f32 }
    }
}

pub struct DeltaTracker {
    width: u32,
    height: u32,
    cols: u32,
    rows: u32,
    hashes: Vec<u64>,
}

impl DeltaTracker {
    pub fn new() -> Self {
        Self { width: 0, height: 0, cols: 0, rows: 0, hashes: Vec::new() }
    }

    /// Forgets history so the next frame is reported as fully changed.
    pub fn reset(&mut self) {
        self.width = 0;
        self.height = 0;
        self.hashes.clear();
    }

    /// Compares `frame` with the previous one. `hints` are OS-provided dirty
    /// rectangles; tiles outside them are assumed unchanged.
    pub fn update(&mut self, frame: &Frame, hints: Option<&[Rect]>) -> Delta {
        let resized = frame.width != self.width || frame.height != self.height;
        if resized {
            self.width = frame.width;
            self.height = frame.height;
            self.cols = frame.width.div_ceil(TILE);
            self.rows = frame.height.div_ceil(TILE);
            self.hashes.clear();
            self.hashes.resize((self.cols * self.rows) as usize, 0);
        }
        let total_tiles = self.cols * self.rows;
        let mut dirty_tiles = 0;
        let mut bounds: Option<Rect> = None;

        let mut visit = |tx: u32, ty: u32, hashes: &mut Vec<u64>| {
            let idx = (ty * self.cols + tx) as usize;
            let h = hash_tile(frame, tx, ty);
            if resized || hashes[idx] != h {
                hashes[idx] = h;
                dirty_tiles += 1;
                let r = tile_rect(frame, tx, ty);
                bounds = Some(bounds.map_or(r, |b| b.union(&r)));
            }
        };

        match hints.filter(|_| !resized) {
            Some(rects) => {
                let mut seen = vec![false; total_tiles as usize];
                for r in rects {
                    let x0 = (r.x / TILE).min(self.cols);
                    let y0 = (r.y / TILE).min(self.rows);
                    let x1 = r.right().div_ceil(TILE).min(self.cols);
                    let y1 = r.bottom().div_ceil(TILE).min(self.rows);
                    for ty in y0..y1 {
                        for tx in x0..x1 {
                            let idx = (ty * self.cols + tx) as usize;
                            if !seen[idx] {
                                seen[idx] = true;
                                visit(tx, ty, &mut self.hashes);
                            }
                        }
                    }
                }
            }
            None => {
                for ty in 0..self.rows {
                    for tx in 0..self.cols {
                        visit(tx, ty, &mut self.hashes);
                    }
                }
            }
        }
        Delta { dirty_tiles, total_tiles, bounds, resized }
    }
}

impl Default for DeltaTracker {
    fn default() -> Self {
        Self::new()
    }
}

fn tile_rect(frame: &Frame, tx: u32, ty: u32) -> Rect {
    let x = tx * TILE;
    let y = ty * TILE;
    Rect { x, y, width: TILE.min(frame.width - x), height: TILE.min(frame.height - y) }
}

fn hash_tile(frame: &Frame, tx: u32, ty: u32) -> u64 {
    let r = tile_rect(frame, tx, ty);
    let stride = frame.stride();
    let row_bytes = r.width as usize * BYTES_PER_PIXEL;
    let mut acc: u64 = 0x9E37_79B9_7F4A_7C15;
    for y in r.y..r.bottom() {
        let off = y as usize * stride + r.x as usize * BYTES_PER_PIXEL;
        let h = xxh3_64(&frame.data[off..off + row_bytes]);
        acc = (acc ^ h).wrapping_mul(0x0000_0100_0000_01B3).rotate_left(23);
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, fill: u8) -> Frame {
        let mut f = Frame::new();
        let src = vec![fill; (w * h * 4) as usize];
        assert!(f.copy_from_strided(&src, w, h, (w * 4) as usize));
        f
    }

    #[test]
    fn crops_to_even_and_removes_stride() {
        let mut f = Frame::new();
        let stride = 7 * 4 + 12;
        let src = vec![1u8; stride * 5];
        assert!(f.copy_from_strided(&src, 7, 5, stride));
        assert_eq!((f.width, f.height), (6, 4));
        assert_eq!(f.data.len(), 6 * 4 * 4);
        assert!(!f.copy_from_strided(&src[..10], 7, 5, stride));
    }

    #[test]
    fn detects_changed_tiles_only() {
        let mut t = DeltaTracker::new();
        let mut f = frame(256, 128, 0);
        let first = t.update(&f, None);
        assert!(first.resized);
        assert_eq!(first.dirty_tiles, 8);
        assert!(t.update(&f, None).is_empty());

        // Touch one pixel in tile (2, 1).
        let off = (70 * f.stride()) + 130 * 4;
        f.data[off] = 255;
        let d = t.update(&f, None);
        assert_eq!(d.dirty_tiles, 1);
        assert_eq!(d.bounds, Some(Rect { x: 128, y: 64, width: 64, height: 64 }));
    }

    #[test]
    fn hints_limit_scanning() {
        let mut t = DeltaTracker::new();
        let mut f = frame(256, 128, 0);
        t.update(&f, None);
        f.data[0] = 9; // tile (0,0) changes, but hint points elsewhere
        let d = t.update(&f, Some(&[Rect { x: 200, y: 100, width: 10, height: 10 }]));
        assert_eq!(d.dirty_tiles, 0);
        let d = t.update(&f, Some(&[Rect { x: 0, y: 0, width: 1, height: 1 }]));
        assert_eq!(d.dirty_tiles, 1);
    }
}
