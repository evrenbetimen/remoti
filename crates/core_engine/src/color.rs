//! BGRA → YUV 4:2:0 conversion (BT.709, limited range) for encoders that need
//! planar input. Integer fixed-point math; chroma is the 2x2 box average.
//! Every encoder that consumes these buffers signals BT.709 limited range in
//! its bitstream so the viewer's decoder converts back with the same matrix.

use crate::frame::Frame;

#[inline]
fn luma(b: i32, g: i32, r: i32) -> u8 {
    ((47 * r + 157 * g + 16 * b + 128) >> 8).wrapping_add(16).clamp(16, 235) as u8
}

#[inline]
fn chroma(b: i32, g: i32, r: i32) -> (u8, u8) {
    let u = ((-26 * r - 86 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
    (u.clamp(16, 240) as u8, v.clamp(16, 240) as u8)
}

/// Sums the B, G, R components of the 2x2 block whose top-left pixel is at
/// byte offset `i` on row `row0` (next row at `row0 + stride`).
#[inline]
fn block_bgr(data: &[u8], i: usize, stride: usize) -> (i32, i32, i32) {
    let mut s = (0i32, 0i32, 0i32);
    for o in [i, i + 4, i + stride, i + stride + 4] {
        s.0 += i32::from(data[o]);
        s.1 += i32::from(data[o + 1]);
        s.2 += i32::from(data[o + 2]);
    }
    ((s.0 + 2) >> 2, (s.1 + 2) >> 2, (s.2 + 2) >> 2)
}

fn luma_plane(frame: &Frame, y: &mut [u8], y_stride: usize) {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let stride = frame.stride();
    for row in 0..h {
        let src = &frame.data[row * stride..row * stride + w * 4];
        let dst = &mut y[row * y_stride..row * y_stride + w];
        for (px, out) in src.chunks_exact(4).zip(dst.iter_mut()) {
            *out = luma(i32::from(px[0]), i32::from(px[1]), i32::from(px[2]));
        }
    }
}

/// Planar I420 buffers with tight strides (`width`, `width / 2`).
#[derive(Default)]
pub struct I420 {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    /// Converts `frame` (even dimensions, see [`Frame::copy_from_strided`]).
    pub fn convert(&mut self, frame: &Frame) {
        let (w, h) = (frame.width as usize, frame.height as usize);
        let (cw, ch) = (w / 2, h / 2);
        self.width = w;
        self.height = h;
        self.y.resize(w * h, 0);
        self.u.resize(cw * ch, 0);
        self.v.resize(cw * ch, 0);
        luma_plane(frame, &mut self.y, w);
        let stride = frame.stride();
        for cy in 0..ch {
            for cx in 0..cw {
                let (b, g, r) = block_bgr(&frame.data, 2 * cy * stride + 8 * cx, stride);
                let (u, v) = chroma(b, g, r);
                self.u[cy * cw + cx] = u;
                self.v[cy * cw + cx] = v;
            }
        }
    }
}

/// Writes NV12 (Y plane, then interleaved UV) into `dst` with the given
/// row pitch, as hardware encoders expect. `dst` must hold
/// `pitch * height * 3 / 2` bytes. Returns `false` when it is too small.
pub fn bgra_to_nv12(frame: &Frame, dst: &mut [u8], pitch: usize) -> bool {
    let (w, h) = (frame.width as usize, frame.height as usize);
    if pitch < w || dst.len() < pitch * h * 3 / 2 {
        return false;
    }
    let (y, uv) = dst.split_at_mut(pitch * h);
    luma_plane(frame, y, pitch);
    let stride = frame.stride();
    for cy in 0..h / 2 {
        let row = &mut uv[cy * pitch..cy * pitch + w];
        for cx in 0..w / 2 {
            let (b, g, r) = block_bgr(&frame.data, 2 * cy * stride + 8 * cx, stride);
            let (u, v) = chroma(b, g, r);
            row[2 * cx] = u;
            row[2 * cx + 1] = v;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, bgra: [u8; 4]) -> Frame {
        let mut f = Frame::new();
        let src: Vec<u8> = bgra.iter().copied().cycle().take((w * h * 4) as usize).collect();
        assert!(f.copy_from_strided(&src, w, h, (w * 4) as usize));
        f
    }

    #[test]
    fn reference_colours_bt709_limited() {
        let mut out = I420::default();
        for (bgra, yuv) in [
            ([0, 0, 0, 255], (16, 128, 128)),
            ([255, 255, 255, 255], (235, 128, 128)),
            ([0, 0, 255, 255], (63, 102, 240)),
            ([0, 255, 0, 255], (173, 42, 26)),
            ([255, 0, 0, 255], (32, 240, 118)),
        ] {
            out.convert(&solid(4, 2, bgra));
            let got = (out.y[0], out.u[0], out.v[0]);
            let close = |a: u8, b: u8| a.abs_diff(b) <= 2;
            assert!(close(got.0, yuv.0) && close(got.1, yuv.1) && close(got.2, yuv.2), "{bgra:?} -> {got:?}");
        }
    }

    #[test]
    fn nv12_matches_i420_and_respects_pitch() {
        let mut f = Frame::new();
        let src: Vec<u8> = (0..8 * 4 * 4).map(|i| (i * 7 % 251) as u8).collect();
        assert!(f.copy_from_strided(&src, 8, 4, 32));
        let mut i420 = I420::default();
        i420.convert(&f);
        let pitch = 16;
        let mut nv12 = vec![0u8; pitch * 4 * 3 / 2];
        assert!(bgra_to_nv12(&f, &mut nv12, pitch));
        for row in 0..4 {
            assert_eq!(&nv12[row * pitch..row * pitch + 8], &i420.y[row * 8..row * 8 + 8]);
        }
        let uv = &nv12[pitch * 4..];
        for cy in 0..2 {
            for cx in 0..4 {
                assert_eq!(uv[cy * pitch + 2 * cx], i420.u[cy * 4 + cx]);
                assert_eq!(uv[cy * pitch + 2 * cx + 1], i420.v[cy * 4 + cx]);
            }
        }
        assert!(!bgra_to_nv12(&f, &mut nv12[..10], pitch));
    }
}
