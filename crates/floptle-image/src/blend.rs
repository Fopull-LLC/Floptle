//! Blend modes — the ONE definition of what "Multiply" means.
//!
//! The 3D vertex/texture brush already speaks Mix / Multiply / Add / Subtract /
//! Lighten / Darken (`paint_ui.rs`). Those six keep their names and their maths
//! here, so a 2D layer set to Multiply and a 3D dab set to Multiply can never
//! drift apart; the rest of the Photoshop vocabulary extends the list.
//!
//! All maths is straight-alpha, channels normalised to 0..1. `mix` is the plain
//! "source over" everyone calls Normal.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Blend {
    /// Source over destination — the normal paint.
    #[default]
    Mix,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    /// Linear dodge — the 3D brush's "Add".
    Add,
    /// Linear burn toward black — the 3D brush's "Subtract".
    Subtract,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl Blend {
    pub const ALL: [Blend; 18] = [
        Blend::Mix,
        Blend::Multiply,
        Blend::Screen,
        Blend::Overlay,
        Blend::Darken,
        Blend::Lighten,
        Blend::ColorDodge,
        Blend::ColorBurn,
        Blend::HardLight,
        Blend::SoftLight,
        Blend::Difference,
        Blend::Exclusion,
        Blend::Add,
        Blend::Subtract,
        Blend::Hue,
        Blend::Saturation,
        Blend::Color,
        Blend::Luminosity,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Blend::Mix => "Mix",
            Blend::Multiply => "Multiply",
            Blend::Screen => "Screen",
            Blend::Overlay => "Overlay",
            Blend::Darken => "Darken",
            Blend::Lighten => "Lighten",
            Blend::ColorDodge => "Color dodge",
            Blend::ColorBurn => "Color burn",
            Blend::HardLight => "Hard light",
            Blend::SoftLight => "Soft light",
            Blend::Difference => "Difference",
            Blend::Exclusion => "Exclusion",
            Blend::Add => "Add",
            Blend::Subtract => "Subtract",
            Blend::Hue => "Hue",
            Blend::Saturation => "Saturation",
            Blend::Color => "Color",
            Blend::Luminosity => "Luminosity",
        }
    }
}

fn sat(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// Separable channel maths (Porter-Duff `B(cb, cs)`), 0..1 in and out.
fn chan(mode: Blend, b: f32, s: f32) -> f32 {
    match mode {
        Blend::Mix => s,
        Blend::Multiply => b * s,
        Blend::Screen => b + s - b * s,
        Blend::Overlay => chan(Blend::HardLight, s, b),
        Blend::Darken => b.min(s),
        Blend::Lighten => b.max(s),
        Blend::ColorDodge => {
            if b <= 0.0 {
                0.0
            } else if s >= 1.0 {
                1.0
            } else {
                sat(b / (1.0 - s))
            }
        }
        Blend::ColorBurn => {
            if b >= 1.0 {
                1.0
            } else if s <= 0.0 {
                0.0
            } else {
                1.0 - sat((1.0 - b) / s)
            }
        }
        Blend::HardLight => {
            if s <= 0.5 {
                b * (2.0 * s)
            } else {
                let s2 = 2.0 * s - 1.0;
                b + s2 - b * s2
            }
        }
        Blend::SoftLight => {
            // W3C compositing spec's D(cb).
            if s <= 0.5 {
                b - (1.0 - 2.0 * s) * b * (1.0 - b)
            } else {
                let d = if b <= 0.25 { ((16.0 * b - 12.0) * b + 4.0) * b } else { b.sqrt() };
                b + (2.0 * s - 1.0) * (d - b)
            }
        }
        Blend::Difference => (b - s).abs(),
        Blend::Exclusion => b + s - 2.0 * b * s,
        Blend::Add => sat(b + s),
        Blend::Subtract => sat(b - (1.0 - s)),
        // Non-separable modes never reach here (handled in `blend_rgb`).
        Blend::Hue | Blend::Saturation | Blend::Color | Blend::Luminosity => s,
    }
}

fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

fn clip_color(mut c: [f32; 3]) -> [f32; 3] {
    let l = lum(c);
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    if n < 0.0 {
        for v in &mut c {
            *v = l + (*v - l) * l / (l - n).max(1e-6);
        }
    }
    if x > 1.0 {
        for v in &mut c {
            *v = l + (*v - l) * (1.0 - l) / (x - l).max(1e-6);
        }
    }
    c
}

fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(c);
    clip_color([c[0] + d, c[1] + d, c[2] + d])
}

fn sat_of(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    // Rank the channels, stretch min..max to 0..s.
    let mut idx = [0usize, 1, 2];
    idx.sort_by(|&a, &b| c[a].partial_cmp(&c[b]).unwrap_or(std::cmp::Ordering::Equal));
    let (lo, mid, hi) = (idx[0], idx[1], idx[2]);
    let mut out = [0.0; 3];
    if c[hi] > c[lo] {
        out[mid] = (c[mid] - c[lo]) * s / (c[hi] - c[lo]);
        out[hi] = s;
    }
    out[lo] = 0.0;
    out
}

/// Blend one RGB triple (0..1) over another with `mode`, ignoring alpha.
pub fn blend_rgb(mode: Blend, b: [f32; 3], s: [f32; 3]) -> [f32; 3] {
    match mode {
        Blend::Hue => set_lum(set_sat(s, sat_of(b)), lum(b)),
        Blend::Saturation => set_lum(set_sat(b, sat_of(s)), lum(b)),
        Blend::Color => set_lum(s, lum(b)),
        Blend::Luminosity => set_lum(b, lum(s)),
        _ => [chan(mode, b[0], s[0]), chan(mode, b[1], s[1]), chan(mode, b[2], s[2])],
    }
}

/// Composite straight-RGBA8 `src` over straight-RGBA8 `dst` with `mode` and an
/// extra `alpha` multiplier (layer opacity × mask × brush flow), all 0..1.
///
/// This is the one function the compositor and the brush both call, so "what
/// Multiply does" has exactly one implementation.
pub fn over(dst: [u8; 4], src: [u8; 4], mode: Blend, alpha: f32) -> [u8; 4] {
    let sa = (src[3] as f32 / 255.0) * alpha.clamp(0.0, 1.0);
    if sa <= 0.0 {
        return dst;
    }
    let da = dst[3] as f32 / 255.0;
    let b = [dst[0] as f32 / 255.0, dst[1] as f32 / 255.0, dst[2] as f32 / 255.0];
    let s = [src[0] as f32 / 255.0, src[1] as f32 / 255.0, src[2] as f32 / 255.0];
    // W3C: the blended colour only applies where the backdrop exists; where it
    // doesn't, the source shows through unblended. Without this a Multiply layer
    // over empty canvas paints black instead of nothing.
    let bl = blend_rgb(mode, b, s);
    let cs = [
        s[0] + da * (bl[0] - s[0]),
        s[1] + da * (bl[1] - s[1]),
        s[2] + da * (bl[2] - s[2]),
    ];
    let oa = sa + da * (1.0 - sa);
    if oa <= 0.0 {
        return [0, 0, 0, 0];
    }
    let mut out = [0u8; 4];
    for i in 0..3 {
        let c = (cs[i] * sa + b[i] * da * (1.0 - sa)) / oa;
        out[i] = crate::u8c(sat(c) * 255.0);
    }
    out[3] = crate::u8c(oa * 255.0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_is_source_over() {
        // Opaque red over opaque blue = red.
        assert_eq!(over([0, 0, 255, 255], [255, 0, 0, 255], Blend::Mix, 1.0), [255, 0, 0, 255]);
        // Half-alpha red over opaque black = half red, still opaque.
        let r = over([0, 0, 0, 255], [255, 0, 0, 128], Blend::Mix, 1.0);
        assert_eq!(r[3], 255);
        assert!((r[0] as i32 - 128).abs() <= 1, "{r:?}");
    }

    #[test]
    fn alpha_multiplier_scales_coverage() {
        let r = over([0, 0, 0, 255], [255, 255, 255, 255], Blend::Mix, 0.25);
        assert!((r[0] as i32 - 64).abs() <= 1, "{r:?}");
    }

    #[test]
    fn multiply_darkens_and_screen_lightens() {
        let m = over([128, 128, 128, 255], [128, 128, 128, 255], Blend::Multiply, 1.0);
        assert!(m[0] < 128, "multiply must darken: {m:?}");
        let s = over([128, 128, 128, 255], [128, 128, 128, 255], Blend::Screen, 1.0);
        assert!(s[0] > 128, "screen must lighten: {s:?}");
    }

    /// The transparency gotcha: a Multiply layer over EMPTY canvas must show its
    /// own colour, not black. (W3C's `cs = s + da*(B(b,s) - s)`.)
    #[test]
    fn blend_over_empty_backdrop_is_the_source() {
        let r = over([0, 0, 0, 0], [200, 40, 40, 255], Blend::Multiply, 1.0);
        assert_eq!(r, [200, 40, 40, 255]);
    }

    #[test]
    fn nonseparable_modes_keep_backdrop_luma() {
        let b = [0.2, 0.5, 0.8];
        let s = [0.9, 0.1, 0.3];
        let out = blend_rgb(Blend::Color, b, s);
        assert!((lum(out) - lum(b)).abs() < 1e-3, "Color must keep backdrop luminosity");
        let out = blend_rgb(Blend::Luminosity, b, s);
        assert!((lum(out) - lum(s)).abs() < 1e-3, "Luminosity must take source luminosity");
    }

    /// Every mode must produce FINITE channel maths for the awkward inputs
    /// (fully transparent backdrop, zero and full channels). A NaN here would
    /// clamp silently to 0 and show up as a black speck nobody could explain.
    #[test]
    fn every_mode_is_finite_at_the_extremes() {
        for m in Blend::ALL {
            for &(b, s) in &[
                ([0.0f32, 0.0, 0.0], [1.0f32, 1.0, 1.0]),
                ([1.0, 1.0, 1.0], [0.0, 0.0, 0.0]),
                ([0.5, 0.0, 1.0], [1.0, 0.5, 0.0]),
            ] {
                let o = blend_rgb(m, b, s);
                assert!(o.iter().all(|c| c.is_finite()), "{m:?} produced {o:?}");
            }
            // …and the u8 path stays deterministic.
            let a = over([255, 0, 128, 200], [3, 250, 9, 40], m, 0.7);
            let b = over([255, 0, 128, 200], [3, 250, 9, 40], m, 0.7);
            assert_eq!(a, b, "{m:?}");
        }
    }
}
