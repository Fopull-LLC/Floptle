//! Adjustments — colour maths as pure functions over a straight-RGBA8 buffer.
//!
//! The same list serves two jobs: an **adjustment layer** (non-destructive,
//! re-evaluated forever, affects everything beneath it) and a one-shot
//! **Adjust ▸ …** menu item on the active layer. Same code, same result — the
//! only difference is where the parameters are stored.
//!
//! Position matters for dithering, so every entry takes the buffer's canvas
//! origin: an ordered dither must land on the same texels whether the compositor
//! redrew the whole canvas or one dirty tile.

use serde::{Deserialize, Serialize};

use crate::palette::Palette;
use crate::u8c;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum CurveChannel {
    #[default]
    Rgb,
    R,
    G,
    B,
    /// The alpha channel — a curve that shapes coverage rather than colour.
    A,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Dither {
    #[default]
    None,
    /// 4×4 Bayer — position-stable, so a partial recomposite matches a full one.
    Ordered,
    /// Floyd–Steinberg error diffusion. Better looking, but the result depends on
    /// the whole buffer, so the compositor recomposites the full canvas for it
    /// ([`Adjustment::needs_full_canvas`]).
    FloydSteinberg,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum Adjustment {
    /// Photoshop's Levels: remap `in_black..in_white` to `out_black..out_white`
    /// with a midtone `gamma`. All 0..1.
    Levels {
        in_black: f32,
        in_white: f32,
        gamma: f32,
        out_black: f32,
        out_white: f32,
    },
    /// A freeform response curve; `points` are (input, output) in 0..1, sorted.
    Curves { channel: CurveChannel, points: Vec<(f32, f32)> },
    /// Hue rotation in degrees, saturation and lightness as −1..1 offsets.
    Hsl { hue: f32, sat: f32, light: f32 },
    /// Both −1..1.
    BrightnessContrast { brightness: f32, contrast: f32 },
    /// Per-channel −1..1 offsets.
    ColorBalance { r: f32, g: f32, b: f32 },
    /// Quantize each channel to `levels` steps.
    Posterize { levels: u32 },
    /// Everything above `t` (0..1 luma) goes white, below goes black.
    Threshold { t: f32 },
    /// Snap every pixel to the nearest palette entry, optionally dithered.
    /// `amount` 0..1 blends between the original and the snapped result.
    Quantize { palette: Palette, dither: Dither, amount: f32 },
    /// Recolour by luma through a gradient — how a grey painted texture becomes
    /// a retro one in a single layer.
    GradientMap { stops: Vec<(f32, [u8; 4])> },
    Invert,
    /// Drop to greyscale, `amount` 0..1.
    Desaturate { amount: f32 },
}

impl Adjustment {
    pub fn label(&self) -> &'static str {
        match self {
            Adjustment::Levels { .. } => "Levels",
            Adjustment::Curves { .. } => "Curves",
            Adjustment::Hsl { .. } => "Hue / Saturation",
            Adjustment::BrightnessContrast { .. } => "Brightness / Contrast",
            Adjustment::ColorBalance { .. } => "Color balance",
            Adjustment::Posterize { .. } => "Posterize",
            Adjustment::Threshold { .. } => "Threshold",
            Adjustment::Quantize { .. } => "Palette quantize",
            Adjustment::GradientMap { .. } => "Gradient map",
            Adjustment::Invert => "Invert",
            Adjustment::Desaturate { .. } => "Desaturate",
        }
    }

    /// True when the result depends on pixels outside the rect being redrawn, so
    /// the compositor must not take its dirty-rect shortcut.
    pub fn needs_full_canvas(&self) -> bool {
        matches!(self, Adjustment::Quantize { dither: Dither::FloydSteinberg, .. })
    }

    /// Every adjustment, with sensible starting values (the Add menu).
    pub fn presets() -> Vec<Adjustment> {
        vec![
            Adjustment::Levels {
                in_black: 0.0,
                in_white: 1.0,
                gamma: 1.0,
                out_black: 0.0,
                out_white: 1.0,
            },
            Adjustment::Curves {
                channel: CurveChannel::Rgb,
                points: vec![(0.0, 0.0), (1.0, 1.0)],
            },
            Adjustment::Hsl { hue: 0.0, sat: 0.0, light: 0.0 },
            Adjustment::BrightnessContrast { brightness: 0.0, contrast: 0.0 },
            Adjustment::ColorBalance { r: 0.0, g: 0.0, b: 0.0 },
            Adjustment::Posterize { levels: 4 },
            Adjustment::Threshold { t: 0.5 },
            Adjustment::Quantize {
                palette: Palette::new("(pick one)"),
                dither: Dither::None,
                amount: 1.0,
            },
            Adjustment::GradientMap {
                stops: vec![(0.0, [20, 12, 40, 255]), (1.0, [255, 240, 200, 255])],
            },
            Adjustment::Invert,
            Adjustment::Desaturate { amount: 1.0 },
        ]
    }

    /// Apply in place to a tightly-packed RGBA8 buffer of `w`×`h`, whose top-left
    /// pixel sits at canvas (`ox`, `oy`).
    pub fn apply(&self, buf: &mut [u8], w: u32, h: u32, ox: i32, oy: i32) {
        match self {
            Adjustment::Levels { in_black, in_white, gamma, out_black, out_white } => {
                let span = (in_white - in_black).abs().max(1e-4);
                let g = gamma.max(0.01);
                per_pixel(buf, |c| {
                    for v in c.iter_mut().take(3) {
                        let t = ((*v / 255.0 - in_black) / span).clamp(0.0, 1.0);
                        let t = t.powf(1.0 / g);
                        *v = (out_black + t * (out_white - out_black)) * 255.0;
                    }
                });
            }
            Adjustment::Curves { channel, points } => {
                per_pixel(buf, |c| match channel {
                    CurveChannel::Rgb => {
                        for v in c.iter_mut().take(3) {
                            *v = eval_curve(points, *v / 255.0) * 255.0;
                        }
                    }
                    CurveChannel::R => c[0] = eval_curve(points, c[0] / 255.0) * 255.0,
                    CurveChannel::G => c[1] = eval_curve(points, c[1] / 255.0) * 255.0,
                    CurveChannel::B => c[2] = eval_curve(points, c[2] / 255.0) * 255.0,
                    CurveChannel::A => c[3] = eval_curve(points, c[3] / 255.0) * 255.0,
                });
            }
            Adjustment::Hsl { hue, sat, light } => {
                per_pixel(buf, |c| {
                    let (mut hh, mut ss, mut ll) = rgb_to_hsl(c[0] / 255.0, c[1] / 255.0, c[2] / 255.0);
                    hh = (hh + hue / 360.0).rem_euclid(1.0);
                    ss = (ss * (1.0 + sat)).clamp(0.0, 1.0);
                    ll = (ll + light * 0.5).clamp(0.0, 1.0);
                    let (r, g, b) = hsl_to_rgb(hh, ss, ll);
                    c[0] = r * 255.0;
                    c[1] = g * 255.0;
                    c[2] = b * 255.0;
                });
            }
            Adjustment::BrightnessContrast { brightness, contrast } => {
                // Contrast pivots on mid-grey, the way every slider labelled
                // "contrast" behaves; brightness is a plain offset.
                let k = (1.0 + contrast).max(0.0);
                per_pixel(buf, |c| {
                    for v in c.iter_mut().take(3) {
                        let t = *v / 255.0 + brightness;
                        *v = ((t - 0.5) * k + 0.5) * 255.0;
                    }
                });
            }
            Adjustment::ColorBalance { r, g, b } => {
                let off = [r * 255.0, g * 255.0, b * 255.0];
                per_pixel(buf, |c| {
                    for i in 0..3 {
                        c[i] += off[i];
                    }
                });
            }
            Adjustment::Posterize { levels } => {
                let n = (*levels).max(2) as f32;
                per_pixel(buf, |c| {
                    for v in c.iter_mut().take(3) {
                        let t = (*v / 255.0 * (n - 1.0)).round() / (n - 1.0);
                        *v = t * 255.0;
                    }
                });
            }
            Adjustment::Threshold { t } => {
                let cut = t * 255.0;
                per_pixel(buf, |c| {
                    let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
                    let v = if l >= cut { 255.0 } else { 0.0 };
                    c[0] = v;
                    c[1] = v;
                    c[2] = v;
                });
            }
            Adjustment::GradientMap { stops } => {
                per_pixel(buf, |c| {
                    let l = (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]) / 255.0;
                    let g = crate::vector::sample_stops(stops, l);
                    c[0] = g[0] as f32;
                    c[1] = g[1] as f32;
                    c[2] = g[2] as f32;
                    c[3] *= g[3] as f32 / 255.0;
                });
            }
            Adjustment::Invert => {
                per_pixel(buf, |c| {
                    for v in c.iter_mut().take(3) {
                        *v = 255.0 - *v;
                    }
                });
            }
            Adjustment::Desaturate { amount } => {
                per_pixel(buf, |c| {
                    let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
                    for v in c.iter_mut().take(3) {
                        *v += (l - *v) * amount.clamp(0.0, 1.0);
                    }
                });
            }
            Adjustment::Quantize { palette, dither, amount } => {
                quantize(buf, w, h, ox, oy, palette, *dither, *amount);
            }
        }
    }
}

fn per_pixel(buf: &mut [u8], mut f: impl FnMut(&mut [f32; 4])) {
    for px in buf.chunks_exact_mut(4) {
        // Fully transparent texels carry no colour worth adjusting, and touching
        // them turns invisible black into visible fringing when they're later
        // resampled. Leave them alone.
        if px[3] == 0 {
            continue;
        }
        let mut c = [px[0] as f32, px[1] as f32, px[2] as f32, px[3] as f32];
        f(&mut c);
        for i in 0..4 {
            px[i] = u8c(c[i]);
        }
    }
}

/// Piecewise-linear response curve through `points` (sorted by x), clamped at
/// both ends.
pub fn eval_curve(points: &[(f32, f32)], t: f32) -> f32 {
    if points.is_empty() {
        return t;
    }
    let t = t.clamp(0.0, 1.0);
    if t <= points[0].0 {
        return points[0].1;
    }
    let last = points[points.len() - 1];
    if t >= last.0 {
        return last.1;
    }
    for w in points.windows(2) {
        if t >= w[0].0 && t <= w[1].0 {
            let d = (w[1].0 - w[0].0).max(1e-6);
            let f = (t - w[0].0) / d;
            // Deliberately LINEAR between keys: a two-point 0→1 curve has to be
            // exactly the identity, or "Curves, untouched" would quietly regrade
            // the image. Add keys to get a curve.
            return w[0].1 + (w[1].1 - w[0].1) * f;
        }
    }
    last.1
}

const BAYER4: [[f32; 4]; 4] = [
    [0.0, 8.0, 2.0, 10.0],
    [12.0, 4.0, 14.0, 6.0],
    [3.0, 11.0, 1.0, 9.0],
    [15.0, 7.0, 13.0, 5.0],
];

/// Reduce to `palette`, optionally dithered. `amount` blends with the original.
#[allow(clippy::too_many_arguments)]
pub fn quantize(
    buf: &mut [u8],
    w: u32,
    h: u32,
    ox: i32,
    oy: i32,
    palette: &Palette,
    dither: Dither,
    amount: f32,
) {
    if palette.colors.is_empty() {
        return;
    }
    let amount = amount.clamp(0.0, 1.0);
    match dither {
        Dither::None | Dither::Ordered => {
            // The ordered case nudges the sample by a per-texel threshold before
            // snapping, which is what breaks up banding.
            let step = 255.0 / palette.colors.len().max(2) as f32;
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let o = (y as usize * w as usize + x as usize) * 4;
                    if buf[o + 3] == 0 {
                        continue;
                    }
                    let mut c = [buf[o], buf[o + 1], buf[o + 2], buf[o + 3]];
                    if dither == Dither::Ordered {
                        let bx = (x + ox).rem_euclid(4) as usize;
                        let by = (y + oy).rem_euclid(4) as usize;
                        let d = (BAYER4[by][bx] / 16.0 - 0.5) * step;
                        for v in c.iter_mut().take(3) {
                            *v = u8c(*v as f32 + d);
                        }
                    }
                    let snapped = palette.snap(c);
                    for i in 0..3 {
                        buf[o + i] = u8c(
                            buf[o + i] as f32 + (snapped[i] as f32 - buf[o + i] as f32) * amount,
                        );
                    }
                }
            }
        }
        Dither::FloydSteinberg => {
            let mut err = vec![0f32; (w as usize + 2) * h as usize * 3];
            let stride = w as usize + 2;
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let o = (y * w as usize + x) * 4;
                    if buf[o + 3] == 0 {
                        continue;
                    }
                    let ei = (y * stride + x + 1) * 3;
                    let mut c = [0u8; 4];
                    let mut want = [0f32; 3];
                    for i in 0..3 {
                        want[i] = buf[o + i] as f32 + err[ei + i];
                        c[i] = u8c(want[i]);
                    }
                    c[3] = buf[o + 3];
                    let snapped = palette.snap(c);
                    for i in 0..3 {
                        let e = want[i] - snapped[i] as f32;
                        // 7/16 right, 3/16 down-left, 5/16 down, 1/16 down-right.
                        err[ei + 3 + i] += e * 7.0 / 16.0;
                        if y + 1 < h as usize {
                            let d = ((y + 1) * stride + x + 1) * 3 + i;
                            err[d - 3] += e * 3.0 / 16.0;
                            err[d] += e * 5.0 / 16.0;
                            err[d + 3] += e / 16.0;
                        }
                        buf[o + i] =
                            u8c(buf[o + i] as f32 + (snapped[i] as f32 - buf[o + i] as f32) * amount);
                    }
                }
            }
        }
    }
}

pub fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        ((g - b) / d + if g < b { 6.0 } else { 0.0 }) / 6.0
    } else if max == g {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    (h, s, l)
}

pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s <= 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |mut t: f32| {
        t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(px: &[[u8; 4]]) -> Vec<u8> {
        px.iter().flatten().copied().collect()
    }

    #[test]
    fn invert_is_its_own_inverse() {
        let mut b = buf(&[[10, 200, 30, 255]]);
        Adjustment::Invert.apply(&mut b, 1, 1, 0, 0);
        assert_eq!(&b[..3], &[245, 55, 225]);
        Adjustment::Invert.apply(&mut b, 1, 1, 0, 0);
        assert_eq!(&b[..3], &[10, 200, 30]);
    }

    #[test]
    fn transparent_pixels_are_left_alone() {
        let mut b = buf(&[[10, 200, 30, 0]]);
        Adjustment::Invert.apply(&mut b, 1, 1, 0, 0);
        assert_eq!(b, vec![10, 200, 30, 0]);
    }

    #[test]
    fn levels_stretch_the_range() {
        let mut b = buf(&[[128, 128, 128, 255]]);
        Adjustment::Levels {
            in_black: 0.25,
            in_white: 0.75,
            gamma: 1.0,
            out_black: 0.0,
            out_white: 1.0,
        }
        .apply(&mut b, 1, 1, 0, 0);
        assert!((b[0] as i32 - 128).abs() <= 2, "mid stays mid: {}", b[0]);
        let mut b = buf(&[[40, 40, 40, 255]]);
        Adjustment::Levels {
            in_black: 0.25,
            in_white: 0.75,
            gamma: 1.0,
            out_black: 0.0,
            out_white: 1.0,
        }
        .apply(&mut b, 1, 1, 0, 0);
        assert_eq!(b[0], 0, "below in_black clips to black");
    }

    #[test]
    fn posterize_snaps_to_levels() {
        let mut b = buf(&[[100, 100, 100, 255]]);
        Adjustment::Posterize { levels: 2 }.apply(&mut b, 1, 1, 0, 0);
        assert!(b[0] == 0 || b[0] == 255);
    }

    #[test]
    fn hsl_round_trips() {
        for c in [[200u8, 30, 30], [10, 240, 120], [70, 70, 200], [128, 128, 128]] {
            let (h, s, l) = rgb_to_hsl(c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0);
            let (r, g, b) = hsl_to_rgb(h, s, l);
            assert!((r * 255.0 - c[0] as f32).abs() < 1.5, "{c:?} -> {r}");
            assert!((g * 255.0 - c[1] as f32).abs() < 1.5);
            assert!((b * 255.0 - c[2] as f32).abs() < 1.5);
        }
    }

    #[test]
    fn quantize_lands_on_palette_entries() {
        let pal = Palette { name: "x".into(), colors: vec![[0, 0, 0, 255], [255, 255, 255, 255]] };
        let mut b = buf(&[[200, 200, 200, 255], [20, 20, 20, 255]]);
        Adjustment::Quantize { palette: pal, dither: Dither::None, amount: 1.0 }
            .apply(&mut b, 2, 1, 0, 0);
        assert_eq!(&b[..4], &[255, 255, 255, 255]);
        assert_eq!(&b[4..], &[0, 0, 0, 255]);
    }

    /// An ORDERED dither must depend only on absolute canvas position, so a
    /// dirty-rect recomposite matches the full-canvas one exactly.
    #[test]
    fn ordered_dither_is_position_stable() {
        let pal = Palette { name: "x".into(), colors: vec![[0, 0, 0, 255], [255, 255, 255, 255]] };
        let adj = Adjustment::Quantize { palette: pal, dither: Dither::Ordered, amount: 1.0 };
        let row: Vec<[u8; 4]> = (0..8).map(|_| [128, 128, 128, 255]).collect();
        let mut full = buf(&row);
        adj.apply(&mut full, 8, 1, 0, 0);
        // The same texels rendered as a 4-wide sub-rect starting at x=4.
        let mut part = buf(&row[4..]);
        adj.apply(&mut part, 4, 1, 4, 0);
        assert_eq!(&full[16..], &part[..], "sub-rect dither must match the full render");
        assert!(!adj.needs_full_canvas());
    }

    #[test]
    fn floyd_steinberg_declares_it_needs_the_whole_canvas() {
        let pal = Palette { name: "x".into(), colors: vec![[0, 0, 0, 255], [255, 255, 255, 255]] };
        let adj = Adjustment::Quantize { palette: pal, dither: Dither::FloydSteinberg, amount: 1.0 };
        assert!(adj.needs_full_canvas());
        // And it still produces only palette colours.
        let mut b: Vec<u8> = (0..64).flat_map(|i| [i * 4, i * 4, i * 4, 255]).collect();
        adj.apply(&mut b, 8, 8, 0, 0);
        assert!(b.as_chunks::<4>().0.iter().all(|p| p[0] == 0 || p[0] == 255));
    }

    #[test]
    fn gradient_map_recolours_by_luma() {
        let adj = Adjustment::GradientMap {
            stops: vec![(0.0, [255, 0, 0, 255]), (1.0, [0, 0, 255, 255])],
        };
        let mut b = buf(&[[0, 0, 0, 255], [255, 255, 255, 255]]);
        adj.apply(&mut b, 2, 1, 0, 0);
        assert_eq!(&b[..3], &[255, 0, 0]);
        assert_eq!(&b[4..7], &[0, 0, 255]);
    }

    #[test]
    fn curves_are_identity_by_default() {
        let adj = Adjustment::Curves {
            channel: CurveChannel::Rgb,
            points: vec![(0.0, 0.0), (1.0, 1.0)],
        };
        let mut b = buf(&[[33, 180, 250, 255]]);
        adj.apply(&mut b, 1, 1, 0, 0);
        assert!((b[0] as i32 - 33).abs() <= 1 && (b[1] as i32 - 180).abs() <= 1);
    }

    #[test]
    fn every_preset_runs_without_panicking() {
        for a in Adjustment::presets() {
            let mut b: Vec<u8> = (0..16).flat_map(|i| [i * 16, 255 - i * 16, 128, 255]).collect();
            a.apply(&mut b, 4, 4, 0, 0);
            assert_eq!(b.len(), 64);
        }
    }
}
