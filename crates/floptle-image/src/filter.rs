//! Destructive filters — the explicit, undoable menu items (§9).
//!
//! Every function takes a tightly-packed straight-RGBA8 buffer and edits it in
//! place. Nothing here knows about layers or selections; the caller extracts a
//! region, filters it, and writes it back through the selection mask, so "filter
//! inside the marquee only" is free.

use crate::u8c;

/// Three box-blur passes ≈ a Gaussian, at a fraction of the cost. `radius` is in
/// pixels; 0 is a no-op. `wrap` reads across the canvas edges, which is what a
/// tileable texture needs (a clamped blur builds a bright rim at the seam).
pub fn blur(buf: &mut [u8], w: u32, h: u32, radius: f32, wrap: bool) {
    if radius <= 0.0 || w == 0 || h == 0 {
        return;
    }
    // Boxes sized per Wells' approximation, simplified: three equal boxes.
    let r = ((radius * 0.9).round() as i32).max(1);
    for _ in 0..3 {
        box_pass(buf, w, h, r, true, wrap);
        box_pass(buf, w, h, r, false, wrap);
    }
}

fn box_pass(buf: &mut [u8], w: u32, h: u32, r: i32, horizontal: bool, wrap: bool) {
    let (w, h) = (w as i32, h as i32);
    let src = buf.to_vec();
    let sample = |x: i32, y: i32| -> [f32; 4] {
        let (x, y) = if wrap {
            (x.rem_euclid(w), y.rem_euclid(h))
        } else {
            (x.clamp(0, w - 1), y.clamp(0, h - 1))
        };
        let o = (y * w + x) as usize * 4;
        // Premultiply so a blur can't drag opaque colour out of transparent
        // texels (the classic dark-halo bug).
        let a = src[o + 3] as f32 / 255.0;
        [src[o] as f32 * a, src[o + 1] as f32 * a, src[o + 2] as f32 * a, src[o + 3] as f32]
    };
    let n = (2 * r + 1) as f32;
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0f32; 4];
            for k in -r..=r {
                let s = if horizontal { sample(x + k, y) } else { sample(x, y + k) };
                for i in 0..4 {
                    acc[i] += s[i];
                }
            }
            let o = (y * w + x) as usize * 4;
            let a = acc[3] / n;
            let inv = if a > 0.5 { 255.0 / a } else { 0.0 };
            for i in 0..3 {
                buf[o + i] = u8c(acc[i] / n * inv);
            }
            buf[o + 3] = u8c(a);
        }
    }
}

/// Unsharp mask: `amount` 0..2 is a sensible range.
pub fn sharpen(buf: &mut [u8], w: u32, h: u32, amount: f32, radius: f32) {
    if amount <= 0.0 {
        return;
    }
    let mut soft = buf.to_vec();
    blur(&mut soft, w, h, radius.max(0.5), false);
    for (dst, s) in buf.chunks_exact_mut(4).zip(soft.chunks_exact(4)) {
        for i in 0..3 {
            let v = dst[i] as f32 + (dst[i] as f32 - s[i] as f32) * amount;
            dst[i] = u8c(v);
        }
    }
}

/// Deterministic value noise (no `rand` dependency, and reproducible for tests).
fn hash(x: u32) -> u32 {
    let mut v = x.wrapping_mul(0x27d4_eb2d);
    v ^= v >> 15;
    v = v.wrapping_mul(0x8589_4a5d);
    v ^= v >> 13;
    v
}

/// Add ±`amount` (0..1) noise. `mono` shifts all channels together (film grain);
/// otherwise each channel is independent (colour static).
pub fn noise(buf: &mut [u8], w: u32, _h: u32, amount: f32, mono: bool, seed: u32) {
    if amount <= 0.0 {
        return;
    }
    for (i, px) in buf.chunks_exact_mut(4).enumerate() {
        let base = hash(i as u32 ^ seed.wrapping_mul(0x9e37_79b9) ^ w);
        if mono {
            let n = (base % 512) as f32 / 511.0 - 0.5;
            for v in px.iter_mut().take(3) {
                *v = u8c(*v as f32 + n * 255.0 * amount);
            }
        } else {
            for (k, v) in px.iter_mut().take(3).enumerate() {
                let n = (hash(base.wrapping_add(k as u32)) % 512) as f32 / 511.0 - 0.5;
                *v = u8c(*v as f32 + n * 255.0 * amount);
            }
        }
    }
}

/// Average each `size`×`size` block — the deliberate chunky look, and the
/// honest way to preview how a texture reads at a lower resolution.
pub fn pixelate(buf: &mut [u8], w: u32, h: u32, size: u32) {
    let s = size.max(2) as usize;
    let (w, h) = (w as usize, h as usize);
    for by in (0..h).step_by(s) {
        for bx in (0..w).step_by(s) {
            let mut acc = [0f32; 4];
            let mut n = 0.0;
            for y in by..(by + s).min(h) {
                for x in bx..(bx + s).min(w) {
                    let o = (y * w + x) * 4;
                    let a = buf[o + 3] as f32 / 255.0;
                    acc[0] += buf[o] as f32 * a;
                    acc[1] += buf[o + 1] as f32 * a;
                    acc[2] += buf[o + 2] as f32 * a;
                    acc[3] += buf[o + 3] as f32;
                    n += 1.0;
                }
            }
            if n == 0.0 {
                continue;
            }
            let a = acc[3] / n;
            let inv = if a > 0.5 { 255.0 / a } else { 0.0 };
            let px = [u8c(acc[0] / n * inv), u8c(acc[1] / n * inv), u8c(acc[2] / n * inv), u8c(a)];
            for y in by..(by + s).min(h) {
                for x in bx..(bx + s).min(w) {
                    let o = (y * w + x) * 4;
                    buf[o..o + 4].copy_from_slice(&px);
                }
            }
        }
    }
}

/// Roll the image by (dx, dy), wrapping — Photoshop's `Offset`. Half the width
/// and half the height brings the tiling seams into the middle where you can
/// paint them out, which is the single cheapest tileable-texture tool there is.
pub fn offset_wrap(buf: &mut [u8], w: u32, h: u32, dx: i32, dy: i32) {
    if w == 0 || h == 0 {
        return;
    }
    let src = buf.to_vec();
    let (wi, hi) = (w as i32, h as i32);
    for y in 0..hi {
        for x in 0..wi {
            let sx = (x - dx).rem_euclid(wi);
            let sy = (y - dy).rem_euclid(hi);
            let so = (sy * wi + sx) as usize * 4;
            let o = (y * wi + x) as usize * 4;
            buf[o..o + 4].copy_from_slice(&src[so..so + 4]);
        }
    }
}

/// Make an image tile without a visible seam, by mirror-blending both edge bands
/// into each other.
///
/// Each pixel inside the band blends toward its MIRROR across the canvas, with a
/// weight that reaches ½ exactly at the edge — so the first and last columns end
/// up as the same average and the seam is arithmetically gone, not merely
/// softened. `width` is the band in pixels. Follow it with the clone stamp over
/// the middle and you have a tileable texture.
pub fn seamless(buf: &mut [u8], w: u32, h: u32, width: u32) {
    let (wi, hi) = (w as i32, h as i32);
    if wi < 2 || hi < 2 {
        return;
    }
    let band = (width as i32).clamp(1, (wi.min(hi) / 2).max(1));
    // Horizontal, then vertical, each against a snapshot of the previous pass.
    for axis in 0..2 {
        let src = buf.to_vec();
        let px = |x: i32, y: i32| -> [f32; 4] {
            let o = (y * wi + x) as usize * 4;
            [src[o] as f32, src[o + 1] as f32, src[o + 2] as f32, src[o + 3] as f32]
        };
        for y in 0..hi {
            for x in 0..wi {
                let (n, i) = if axis == 0 { (wi, x) } else { (hi, y) };
                let f = if i < band {
                    0.5 * (1.0 - i as f32 / band as f32)
                } else if i >= n - band {
                    0.5 * (1.0 - (n - 1 - i) as f32 / band as f32)
                } else {
                    0.0
                };
                if f <= 0.0 {
                    continue;
                }
                let m = if axis == 0 { px(wi - 1 - x, y) } else { px(x, hi - 1 - y) };
                let c = px(x, y);
                let o = (y * wi + x) as usize * 4;
                for k in 0..4 {
                    buf[o + k] = u8c(c[k] + (m[k] - c[k]) * f);
                }
            }
        }
    }
}

/// Turn a height field (luma) into a tangent-space normal map. `strength` scales
/// the slope; `wrap` samples across the edges for tileable textures.
pub fn normal_from_height(buf: &mut [u8], w: u32, h: u32, strength: f32, wrap: bool) {
    let (wi, hi) = (w as i32, h as i32);
    let src = buf.to_vec();
    let at = |x: i32, y: i32| -> f32 {
        let (x, y) = if wrap {
            (x.rem_euclid(wi), y.rem_euclid(hi))
        } else {
            (x.clamp(0, wi - 1), y.clamp(0, hi - 1))
        };
        let o = (y * wi + x) as usize * 4;
        (0.2126 * src[o] as f32 + 0.7152 * src[o + 1] as f32 + 0.0722 * src[o + 2] as f32) / 255.0
    };
    for y in 0..hi {
        for x in 0..wi {
            // Sobel.
            let dx = (at(x + 1, y - 1) + 2.0 * at(x + 1, y) + at(x + 1, y + 1))
                - (at(x - 1, y - 1) + 2.0 * at(x - 1, y) + at(x - 1, y + 1));
            let dy = (at(x - 1, y + 1) + 2.0 * at(x, y + 1) + at(x + 1, y + 1))
                - (at(x - 1, y - 1) + 2.0 * at(x, y - 1) + at(x + 1, y - 1));
            let n = [-dx * strength, -dy * strength, 1.0];
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-6);
            let o = (y * wi + x) as usize * 4;
            for i in 0..3 {
                buf[o + i] = u8c((n[i] / len * 0.5 + 0.5) * 255.0);
            }
        }
    }
}

/// Blur only the alpha channel — the shared primitive behind glow/shadow effects.
pub fn blur_alpha(alpha: &mut [f32], w: u32, h: u32, radius: f32) {
    if radius <= 0.0 {
        return;
    }
    let r = (radius.round() as i32).max(1);
    let (wi, hi) = (w as i32, h as i32);
    for pass in 0..2 {
        let src = alpha.to_vec();
        let n = (2 * r + 1) as f32;
        for y in 0..hi {
            for x in 0..wi {
                let mut acc = 0.0;
                for k in -r..=r {
                    let (sx, sy) = if pass == 0 { (x + k, y) } else { (x, y + k) };
                    let sx = sx.clamp(0, wi - 1);
                    let sy = sy.clamp(0, hi - 1);
                    acc += src[(sy * wi + sx) as usize];
                }
                alpha[(y * wi + x) as usize] = acc / n;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, c: [u8; 4]) -> Vec<u8> {
        (0..w * h).flat_map(|_| c).collect()
    }

    #[test]
    fn blur_of_a_flat_image_changes_nothing() {
        let mut b = solid(16, 16, [40, 90, 200, 255]);
        let before = b.clone();
        blur(&mut b, 16, 16, 3.0, false);
        assert_eq!(b, before);
    }

    #[test]
    fn blur_spreads_a_dot() {
        let mut b = solid(21, 21, [0, 0, 0, 255]);
        let mid = (10 * 21 + 10) * 4;
        b[mid..mid + 4].copy_from_slice(&[255, 255, 255, 255]);
        blur(&mut b, 21, 21, 3.0, false);
        assert!(b[mid] < 255, "centre should soften");
        assert!(b[(10 * 21 + 12) * 4] > 0, "energy should reach the neighbour");
    }

    /// Premultiplied blur: an opaque red dot on a transparent field must not
    /// leave a dark halo (the classic straight-alpha blur bug).
    #[test]
    fn blur_does_not_darken_transparent_neighbours() {
        let mut b = solid(9, 9, [0, 0, 0, 0]);
        let mid = (4 * 9 + 4) * 4;
        b[mid..mid + 4].copy_from_slice(&[255, 0, 0, 255]);
        blur(&mut b, 9, 9, 2.0, false);
        let n = (4 * 9 + 5) * 4;
        assert!(b[n + 3] > 0, "alpha should spread");
        assert!(b[n] > 100, "the spread colour must stay red, not go dark: {:?}", &b[n..n + 4]);
    }

    #[test]
    fn offset_wrap_is_reversible() {
        let mut b: Vec<u8> = (0..(8 * 8)).flat_map(|i| [i as u8, 0, 0, 255]).collect();
        let before = b.clone();
        offset_wrap(&mut b, 8, 8, 4, 4);
        assert_ne!(b, before);
        offset_wrap(&mut b, 8, 8, -4, -4);
        assert_eq!(b, before);
    }

    #[test]
    fn offset_by_half_moves_the_corner_to_the_middle() {
        let mut b = solid(8, 8, [0, 0, 0, 255]);
        b[0..4].copy_from_slice(&[255, 255, 255, 255]); // corner pixel
        offset_wrap(&mut b, 8, 8, 4, 4);
        let mid = (4 * 8 + 4) * 4;
        assert_eq!(&b[mid..mid + 4], &[255, 255, 255, 255]);
    }

    #[test]
    fn pixelate_makes_uniform_blocks() {
        let mut b: Vec<u8> = (0..(8 * 8)).flat_map(|i| [(i * 4) as u8, 0, 0, 255]).collect();
        pixelate(&mut b, 8, 8, 4);
        let a = &b[0..4];
        let c = &b[(3 * 8 + 3) * 4..(3 * 8 + 3) * 4 + 4];
        assert_eq!(a, c, "the whole 4×4 block should share one colour");
    }

    #[test]
    fn seamless_matches_opposite_edges() {
        // A left-to-right ramp has a hard seam when tiled; seamless should soften it.
        let mut b: Vec<u8> = (0..(32 * 32))
            .flat_map(|i| {
                let x = (i % 32) as u8;
                [x * 8, x * 8, x * 8, 255]
            })
            .collect();
        let gap_before = (b[0] as i32 - b[31 * 4] as i32).abs();
        seamless(&mut b, 32, 32, 8);
        let gap_after = (b[0] as i32 - b[31 * 4] as i32).abs();
        assert!(gap_after < gap_before, "seam gap {gap_before} → {gap_after}");
    }

    #[test]
    fn normal_map_of_a_flat_height_is_straight_up() {
        let mut b = solid(8, 8, [128, 128, 128, 255]);
        normal_from_height(&mut b, 8, 8, 2.0, false);
        assert_eq!(&b[0..3], &[128, 128, 255]);
    }

    #[test]
    fn noise_is_deterministic() {
        let mut a = solid(8, 8, [128, 128, 128, 255]);
        let mut b = a.clone();
        noise(&mut a, 8, 8, 0.5, false, 7);
        noise(&mut b, 8, 8, 0.5, false, 7);
        assert_eq!(a, b);
        let mut c = solid(8, 8, [128, 128, 128, 255]);
        noise(&mut c, 8, 8, 0.5, false, 8);
        assert_ne!(a, c, "a different seed must give a different field");
    }

    #[test]
    fn sharpen_increases_local_contrast() {
        let mut b = solid(9, 9, [100, 100, 100, 255]);
        let mid = (4 * 9 + 4) * 4;
        b[mid..mid + 3].copy_from_slice(&[160, 160, 160]);
        sharpen(&mut b, 9, 9, 1.0, 1.0);
        assert!(b[mid] > 160, "the peak should get peakier: {}", b[mid]);
    }
}
