//! Layer effects — non-destructive decoration applied to a layer's own pixels
//! before it blends into the stack.
//!
//! Outline is the load-bearing one: a sprite that reads against any background
//! needs one, and doing it by hand in pixel art is miserable. Effects grow the
//! layer's footprint, so each reports the [`margin`](Effect::margin) the
//! compositor must render around the dirty rect for the result to be correct.

use serde::{Deserialize, Serialize};

use crate::{u8c, Blend};

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum Effect {
    /// A hard border of `width` pixels around the layer's alpha. `outside`
    /// grows outward (the sprite outline); otherwise it eats inward.
    Outline { color: [u8; 4], width: u32, outside: bool },
    /// Offset, blurred silhouette behind the layer.
    DropShadow { color: [u8; 4], dx: f32, dy: f32, blur: f32, opacity: f32 },
    /// A soft halo, outside or inside the alpha edge.
    Glow { color: [u8; 4], radius: f32, opacity: f32, inner: bool },
    /// Flood the layer's opaque pixels with one colour (keeping their alpha).
    ColorOverlay { color: [u8; 4], opacity: f32 },
}

impl Effect {
    pub fn label(&self) -> &'static str {
        match self {
            Effect::Outline { .. } => "Outline",
            Effect::DropShadow { .. } => "Drop shadow",
            Effect::Glow { .. } => "Glow",
            Effect::ColorOverlay { .. } => "Color overlay",
        }
    }

    /// Every effect with usable defaults (the Add menu).
    pub fn presets() -> Vec<Effect> {
        vec![
            Effect::Outline { color: [16, 16, 20, 255], width: 1, outside: true },
            Effect::DropShadow {
                color: [0, 0, 0, 255],
                dx: 2.0,
                dy: 2.0,
                blur: 2.0,
                opacity: 0.6,
            },
            Effect::Glow { color: [255, 230, 140, 255], radius: 4.0, opacity: 0.8, inner: false },
            Effect::ColorOverlay { color: [255, 255, 255, 255], opacity: 1.0 },
        ]
    }

    /// Pixels this effect can reach beyond the layer's own content.
    pub fn margin(&self) -> u32 {
        match self {
            Effect::Outline { width, outside, .. } => {
                if *outside {
                    *width
                } else {
                    0
                }
            }
            Effect::DropShadow { dx, dy, blur, .. } => {
                (dx.abs().max(dy.abs()) + blur * 2.0).ceil() as u32 + 1
            }
            Effect::Glow { radius, inner, .. } => {
                if *inner {
                    0
                } else {
                    (radius * 2.0).ceil() as u32 + 1
                }
            }
            Effect::ColorOverlay { .. } => 0,
        }
    }

    /// Apply in place to a layer's rendered RGBA8 buffer (`w`×`h`, already padded
    /// by at least [`margin`](Effect::margin)).
    pub fn apply(&self, buf: &mut [u8], w: u32, h: u32) {
        match self {
            Effect::ColorOverlay { color, opacity } => {
                let k = opacity.clamp(0.0, 1.0);
                for px in buf.chunks_exact_mut(4) {
                    if px[3] == 0 {
                        continue;
                    }
                    for i in 0..3 {
                        px[i] = u8c(px[i] as f32 + (color[i] as f32 - px[i] as f32) * k);
                    }
                }
            }
            Effect::Outline { color, width, outside } => {
                if *width == 0 {
                    return;
                }
                let r = *width as i32;
                let src = alpha_of(buf);
                let (wi, hi) = (w as i32, h as i32);
                let a_at = |x: i32, y: i32| -> u8 {
                    if x < 0 || y < 0 || x >= wi || y >= hi {
                        0
                    } else {
                        src[(y * wi + x) as usize]
                    }
                };
                let mut out = buf.to_vec();
                for y in 0..hi {
                    for x in 0..wi {
                        let here = a_at(x, y);
                        // Outside: paint where we're empty but something opaque is
                        // within `width`. Inside: paint where we're opaque but the
                        // edge is within `width`.
                        let want = if *outside { here < 255 } else { here > 0 };
                        if !want {
                            continue;
                        }
                        let mut near = false;
                        'k: for dy in -r..=r {
                            for dx in -r..=r {
                                if dx * dx + dy * dy > r * r {
                                    continue;
                                }
                                let n = a_at(x + dx, y + dy);
                                if (*outside && n > 128) || (!*outside && n <= 128) {
                                    near = true;
                                    break 'k;
                                }
                            }
                        }
                        if !near {
                            continue;
                        }
                        let o = (y * wi + x) as usize * 4;
                        let dst = [out[o], out[o + 1], out[o + 2], out[o + 3]];
                        let res = if *outside {
                            // Under the layer: outline first, existing pixels over it.
                            crate::blend::over(*color, dst, Blend::Mix, 1.0)
                        } else {
                            crate::blend::over(dst, *color, Blend::Mix, 1.0)
                        };
                        out[o..o + 4].copy_from_slice(&res);
                    }
                }
                buf.copy_from_slice(&out);
            }
            Effect::DropShadow { color, dx, dy, blur, opacity } => {
                let mut a: Vec<f32> = alpha_of(buf).iter().map(|&v| v as f32).collect();
                shift(&mut a, w, h, *dx, *dy);
                crate::filter::blur_alpha(&mut a, w, h, *blur);
                under(buf, &a, *color, opacity.clamp(0.0, 1.0));
            }
            Effect::Glow { color, radius, opacity, inner } => {
                let src = alpha_of(buf);
                let mut a: Vec<f32> = src.iter().map(|&v| v as f32).collect();
                if *inner {
                    // Invert, blur, and keep only what falls inside the shape.
                    for v in a.iter_mut() {
                        *v = 255.0 - *v;
                    }
                    crate::filter::blur_alpha(&mut a, w, h, *radius);
                    for (i, v) in a.iter_mut().enumerate() {
                        *v *= src[i] as f32 / 255.0;
                    }
                    over(buf, &a, *color, opacity.clamp(0.0, 1.0));
                } else {
                    crate::filter::blur_alpha(&mut a, w, h, *radius);
                    // A soft glow reads better with a little gain.
                    for v in a.iter_mut() {
                        *v = (*v * 1.6).min(255.0);
                    }
                    under(buf, &a, *color, opacity.clamp(0.0, 1.0));
                }
            }
        }
    }
}

fn alpha_of(buf: &[u8]) -> Vec<u8> {
    buf.as_chunks::<4>().0.iter().map(|p| p[3]).collect()
}

/// Shift an alpha field by a (possibly fractional) offset, bilinear.
fn shift(a: &mut [f32], w: u32, h: u32, dx: f32, dy: f32) {
    let (wi, hi) = (w as i32, h as i32);
    let src = a.to_vec();
    let at = |x: i32, y: i32| -> f32 {
        if x < 0 || y < 0 || x >= wi || y >= hi { 0.0 } else { src[(y * wi + x) as usize] }
    };
    for y in 0..hi {
        for x in 0..wi {
            let fx = x as f32 - dx;
            let fy = y as f32 - dy;
            let (x0, y0) = (fx.floor() as i32, fy.floor() as i32);
            let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
            let v = at(x0, y0) * (1.0 - tx) * (1.0 - ty)
                + at(x0 + 1, y0) * tx * (1.0 - ty)
                + at(x0, y0 + 1) * (1.0 - tx) * ty
                + at(x0 + 1, y0 + 1) * tx * ty;
            a[(y * wi + x) as usize] = v;
        }
    }
}

/// Composite a coloured alpha field UNDER the buffer.
fn under(buf: &mut [u8], a: &[f32], color: [u8; 4], opacity: f32) {
    for (i, px) in buf.chunks_exact_mut(4).enumerate() {
        let cov = (a[i] / 255.0).clamp(0.0, 1.0) * opacity * (color[3] as f32 / 255.0);
        if cov <= 0.0 {
            continue;
        }
        let shade = [color[0], color[1], color[2], u8c(cov * 255.0)];
        let dst = [px[0], px[1], px[2], px[3]];
        let res = crate::blend::over(shade, dst, Blend::Mix, 1.0);
        px.copy_from_slice(&res);
    }
}

/// Composite a coloured alpha field OVER the buffer.
fn over(buf: &mut [u8], a: &[f32], color: [u8; 4], opacity: f32) {
    for (i, px) in buf.chunks_exact_mut(4).enumerate() {
        let cov = (a[i] / 255.0).clamp(0.0, 1.0) * opacity * (color[3] as f32 / 255.0);
        if cov <= 0.0 {
            continue;
        }
        let src = [color[0], color[1], color[2], u8c(cov * 255.0)];
        let dst = [px[0], px[1], px[2], px[3]];
        let res = crate::blend::over(dst, src, Blend::Mix, 1.0);
        px.copy_from_slice(&res);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 9×9 buffer with a 3×3 opaque white square in the middle.
    fn square() -> Vec<u8> {
        let mut b = vec![0u8; 9 * 9 * 4];
        for y in 3..6 {
            for x in 3..6 {
                let o = (y * 9 + x) * 4;
                b[o..o + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        b
    }

    fn px(b: &[u8], x: usize, y: usize) -> [u8; 4] {
        let o = (y * 9 + x) * 4;
        [b[o], b[o + 1], b[o + 2], b[o + 3]]
    }

    #[test]
    fn outline_rings_the_shape_without_covering_it() {
        let mut b = square();
        Effect::Outline { color: [255, 0, 0, 255], width: 1, outside: true }.apply(&mut b, 9, 9);
        assert_eq!(px(&b, 4, 4), [255, 255, 255, 255], "interior untouched");
        assert_eq!(px(&b, 2, 4), [255, 0, 0, 255], "ring painted outside");
        assert_eq!(px(&b, 0, 0), [0, 0, 0, 0], "beyond the ring stays empty");
    }

    #[test]
    fn inner_outline_eats_inward() {
        let mut b = square();
        Effect::Outline { color: [255, 0, 0, 255], width: 1, outside: false }.apply(&mut b, 9, 9);
        assert_eq!(px(&b, 3, 3), [255, 0, 0, 255], "edge recoloured");
        assert_eq!(px(&b, 2, 3), [0, 0, 0, 0], "nothing added outside");
    }

    #[test]
    fn drop_shadow_falls_behind_and_offset() {
        let mut b = square();
        Effect::DropShadow { color: [0, 0, 0, 255], dx: 2.0, dy: 2.0, blur: 0.0, opacity: 1.0 }
            .apply(&mut b, 9, 9);
        assert_eq!(px(&b, 4, 4), [255, 255, 255, 255], "the layer stays on top");
        let s = px(&b, 7, 7);
        assert!(s[3] > 200 && s[0] < 40, "shadow at the offset: {s:?}");
    }

    #[test]
    fn color_overlay_keeps_alpha() {
        let mut b = square();
        Effect::ColorOverlay { color: [10, 200, 30, 255], opacity: 1.0 }.apply(&mut b, 9, 9);
        assert_eq!(px(&b, 4, 4), [10, 200, 30, 255]);
        assert_eq!(px(&b, 0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn glow_reaches_outward_and_margins_are_honest() {
        let mut b = square();
        let e = Effect::Glow { color: [255, 255, 0, 255], radius: 2.0, opacity: 1.0, inner: false };
        assert!(e.margin() >= 4);
        e.apply(&mut b, 9, 9);
        assert!(px(&b, 1, 4)[3] > 0, "glow should reach two pixels out");
    }

    #[test]
    fn presets_all_apply_cleanly() {
        for e in Effect::presets() {
            let mut b = square();
            e.apply(&mut b, 9, 9);
            assert_eq!(b.len(), 9 * 9 * 4, "{}", e.label());
        }
    }
}
