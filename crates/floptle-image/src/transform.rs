//! Resampling and the free transform.
//!
//! `nearest` is not a quality setting, it's a correctness one: rotating or
//! scaling a pixel sprite through a bilinear filter turns it to mush and no
//! sampler downstream can recover it (the same trap `load_texture_sized_filtered`
//! documents on the import side).

use crate::u8c;

/// Sample a straight-RGBA8 buffer with bilinear filtering, premultiplied so
/// transparent neighbours can't drag their stale colour in.
fn bilinear(src: &[u8], w: u32, h: u32, x: f32, y: f32) -> [u8; 4] {
    let (wi, hi) = (w as i32, h as i32);
    let at = |x: i32, y: i32| -> [f32; 4] {
        if x < 0 || y < 0 || x >= wi || y >= hi {
            return [0.0; 4];
        }
        let o = (y * wi + x) as usize * 4;
        let a = src[o + 3] as f32 / 255.0;
        [src[o] as f32 * a, src[o + 1] as f32 * a, src[o + 2] as f32 * a, src[o + 3] as f32]
    };
    let (x0, y0) = (x.floor() as i32, y.floor() as i32);
    let (tx, ty) = (x - x0 as f32, y - y0 as f32);
    let mut acc = [0f32; 4];
    for (dx, dy, wgt) in [
        (0, 0, (1.0 - tx) * (1.0 - ty)),
        (1, 0, tx * (1.0 - ty)),
        (0, 1, (1.0 - tx) * ty),
        (1, 1, tx * ty),
    ] {
        let s = at(x0 + dx, y0 + dy);
        for i in 0..4 {
            acc[i] += s[i] * wgt;
        }
    }
    let a = acc[3];
    let inv = if a > 0.5 { 255.0 / a } else { 0.0 };
    [u8c(acc[0] * inv), u8c(acc[1] * inv), u8c(acc[2] * inv), u8c(a)]
}

fn nearest_px(src: &[u8], w: u32, h: u32, x: f32, y: f32) -> [u8; 4] {
    let xi = x.round() as i32;
    let yi = y.round() as i32;
    if xi < 0 || yi < 0 || xi >= w as i32 || yi >= h as i32 {
        return [0, 0, 0, 0];
    }
    let o = (yi as usize * w as usize + xi as usize) * 4;
    [src[o], src[o + 1], src[o + 2], src[o + 3]]
}

/// Scale a buffer to `dw`×`dh`.
pub fn resample(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32, nearest: bool) -> Vec<u8> {
    let mut out = vec![0u8; dw as usize * dh as usize * 4];
    if sw == 0 || sh == 0 {
        return out;
    }
    let fx = sw as f32 / dw as f32;
    let fy = sh as f32 / dh as f32;
    // Downscaling by more than 2× with point sampling drops whole rows; box-average
    // instead unless the caller explicitly wants nearest (pixel art).
    let box_down = !nearest && (fx > 1.5 || fy > 1.5);
    for y in 0..dh {
        for x in 0..dw {
            let px = if box_down {
                let x0 = (x as f32 * fx).floor() as i32;
                let x1 = (((x + 1) as f32 * fx).ceil() as i32).min(sw as i32);
                let y0 = (y as f32 * fy).floor() as i32;
                let y1 = (((y + 1) as f32 * fy).ceil() as i32).min(sh as i32);
                let mut acc = [0f32; 4];
                let mut n = 0.0;
                for sy in y0..y1.max(y0 + 1) {
                    for sx in x0..x1.max(x0 + 1) {
                        let o = (sy as usize * sw as usize + sx as usize) * 4;
                        if o + 4 > src.len() {
                            continue;
                        }
                        let a = src[o + 3] as f32 / 255.0;
                        acc[0] += src[o] as f32 * a;
                        acc[1] += src[o + 1] as f32 * a;
                        acc[2] += src[o + 2] as f32 * a;
                        acc[3] += src[o + 3] as f32;
                        n += 1.0;
                    }
                }
                if n == 0.0 {
                    [0, 0, 0, 0]
                } else {
                    let a = acc[3] / n;
                    let inv = if a > 0.5 { 255.0 / a } else { 0.0 };
                    [u8c(acc[0] / n * inv), u8c(acc[1] / n * inv), u8c(acc[2] / n * inv), u8c(a)]
                }
            } else {
                let sx = (x as f32 + 0.5) * fx - 0.5;
                let sy = (y as f32 + 0.5) * fy - 0.5;
                if nearest {
                    nearest_px(src, sw, sh, sx, sy)
                } else {
                    bilinear(src, sw, sh, sx, sy)
                }
            };
            let o = (y as usize * dw as usize + x as usize) * 4;
            out[o..o + 4].copy_from_slice(&px);
        }
    }
    out
}

pub fn flip_h(buf: &mut [u8], w: u32, h: u32) {
    for y in 0..h as usize {
        for x in 0..(w as usize / 2) {
            let a = (y * w as usize + x) * 4;
            let b = (y * w as usize + (w as usize - 1 - x)) * 4;
            for i in 0..4 {
                buf.swap(a + i, b + i);
            }
        }
    }
}

pub fn flip_v(buf: &mut [u8], w: u32, h: u32) {
    for y in 0..(h as usize / 2) {
        for x in 0..w as usize {
            let a = (y * w as usize + x) * 4;
            let b = ((h as usize - 1 - y) * w as usize + x) * 4;
            for i in 0..4 {
                buf.swap(a + i, b + i);
            }
        }
    }
}

/// Rotate by a multiple of 90° (positive = clockwise), returning the new
/// `(buffer, w, h)`.
pub fn rotate_quarter(src: &[u8], w: u32, h: u32, turns: i32) -> (Vec<u8>, u32, u32) {
    let t = turns.rem_euclid(4);
    if t == 0 {
        return (src.to_vec(), w, h);
    }
    let (nw, nh) = if t % 2 == 1 { (h, w) } else { (w, h) };
    let mut out = vec![0u8; nw as usize * nh as usize * 4];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let (dx, dy) = match t {
                1 => (h as usize - 1 - y, x),
                2 => (w as usize - 1 - x, h as usize - 1 - y),
                _ => (y, w as usize - 1 - x),
            };
            let so = (y * w as usize + x) * 4;
            let dofs = (dy * nw as usize + dx) * 4;
            out[dofs..dofs + 4].copy_from_slice(&src[so..so + 4]);
        }
    }
    (out, nw, nh)
}

/// A free transform: scale, rotate and translate about a pivot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Xform {
    pub scale: (f32, f32),
    /// Radians, clockwise in a y-down space.
    pub rotate: f32,
    pub translate: (f32, f32),
    pub pivot: (f32, f32),
}

impl Default for Xform {
    fn default() -> Self {
        Xform { scale: (1.0, 1.0), rotate: 0.0, translate: (0.0, 0.0), pivot: (0.0, 0.0) }
    }
}

impl Xform {
    pub fn is_identity(&self) -> bool {
        self.scale == (1.0, 1.0) && self.rotate == 0.0 && self.translate == (0.0, 0.0)
    }

    /// Map a source point to its destination.
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        let (px, py) = (x - self.pivot.0, y - self.pivot.1);
        let (sx, sy) = (px * self.scale.0, py * self.scale.1);
        let (c, s) = (self.rotate.cos(), self.rotate.sin());
        (
            self.pivot.0 + sx * c - sy * s + self.translate.0,
            self.pivot.1 + sx * s + sy * c + self.translate.1,
        )
    }

    /// Map a destination point back to the source (what the sampler needs).
    pub fn inverse(&self, x: f32, y: f32) -> (f32, f32) {
        let (dx, dy) = (x - self.translate.0 - self.pivot.0, y - self.translate.1 - self.pivot.1);
        let (c, s) = ((-self.rotate).cos(), (-self.rotate).sin());
        let (rx, ry) = (dx * c - dy * s, dx * s + dy * c);
        let sx = if self.scale.0.abs() < 1e-6 { 0.0 } else { rx / self.scale.0 };
        let sy = if self.scale.1.abs() < 1e-6 { 0.0 } else { ry / self.scale.1 };
        (sx + self.pivot.0, sy + self.pivot.1)
    }
}

/// Resample `src` through `xf` into a fresh buffer of the same size.
pub fn transform(src: &[u8], w: u32, h: u32, xf: &Xform, nearest: bool) -> Vec<u8> {
    let mut out = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = xf.inverse(x as f32 + 0.5, y as f32 + 0.5);
            let px = if nearest {
                nearest_px(src, w, h, sx - 0.5, sy - 0.5)
            } else {
                bilinear(src, w, h, sx - 0.5, sy - 0.5)
            };
            let o = (y as usize * w as usize + x as usize) * 4;
            out[o..o + 4].copy_from_slice(&px);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(w: u32, h: u32) -> Vec<u8> {
        (0..w * h).flat_map(|i| [(i % 256) as u8, 0, 0, 255]).collect()
    }

    #[test]
    fn nearest_doubling_replicates_pixels() {
        let src = vec![255, 0, 0, 255, 0, 255, 0, 255]; // 2×1
        let out = resample(&src, 2, 1, 4, 1, true);
        assert_eq!(&out[0..4], &[255, 0, 0, 255]);
        assert_eq!(&out[4..8], &[255, 0, 0, 255]);
        assert_eq!(&out[8..12], &[0, 255, 0, 255]);
    }

    #[test]
    fn bilinear_doubling_interpolates() {
        let src = vec![0, 0, 0, 255, 255, 255, 255, 255];
        let out = resample(&src, 2, 1, 4, 1, false);
        let mid = out[4];
        assert!(mid > 0 && mid < 255, "should ramp, got {mid}");
    }

    #[test]
    fn flips_are_involutions() {
        let mut b = ramp(6, 4);
        let before = b.clone();
        flip_h(&mut b, 6, 4);
        assert_ne!(b, before);
        flip_h(&mut b, 6, 4);
        assert_eq!(b, before);
        flip_v(&mut b, 6, 4);
        flip_v(&mut b, 6, 4);
        assert_eq!(b, before);
    }

    #[test]
    fn four_quarter_turns_return_the_original() {
        let src = ramp(5, 3);
        let (a, w, h) = rotate_quarter(&src, 5, 3, 1);
        assert_eq!((w, h), (3, 5));
        let (b, w, h) = rotate_quarter(&a, w, h, 1);
        let (c, w, h) = rotate_quarter(&b, w, h, 1);
        let (d, w, h) = rotate_quarter(&c, w, h, 1);
        assert_eq!((w, h), (5, 3));
        assert_eq!(d, src);
    }

    #[test]
    fn xform_inverse_undoes_apply() {
        let xf = Xform {
            scale: (1.7, 0.6),
            rotate: 0.6,
            translate: (12.0, -4.0),
            pivot: (8.0, 8.0),
        };
        let (x, y) = xf.apply(3.0, 11.0);
        let (bx, by) = xf.inverse(x, y);
        assert!((bx - 3.0).abs() < 1e-3 && (by - 11.0).abs() < 1e-3, "{bx},{by}");
    }

    #[test]
    fn identity_transform_is_a_copy() {
        let src = ramp(8, 8);
        let out = transform(&src, 8, 8, &Xform::default(), true);
        assert_eq!(out, src);
    }

    #[test]
    fn a_half_turn_transform_moves_the_corner_across() {
        let mut src = vec![0u8; 8 * 8 * 4];
        src[0..4].copy_from_slice(&[255, 0, 0, 255]);
        let xf = Xform {
            rotate: std::f32::consts::PI,
            pivot: (4.0, 4.0),
            ..Default::default()
        };
        let out = transform(&src, 8, 8, &xf, true);
        let o = (7 * 8 + 7) * 4;
        assert_eq!(&out[o..o + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn downscaling_averages_rather_than_dropping_rows() {
        // A 1-px checker downscaled 4× should go grey, not pick one phase.
        let src: Vec<u8> = (0..16 * 16)
            .flat_map(|i| {
                let on = ((i / 16) + (i % 16)) % 2 == 0;
                if on { [255u8, 255, 255, 255] } else { [0, 0, 0, 255] }
            })
            .collect();
        let out = resample(&src, 16, 16, 4, 4, false);
        assert!((out[0] as i32 - 128).abs() < 40, "expected grey, got {}", out[0]);
    }
}
