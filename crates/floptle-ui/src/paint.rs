//! The paint box: everything an element can *look like*
//! (docs/ui-system-2-proposal.md §A).
//!
//! The first cut of the UI system gave an element one flat fill, one uniform
//! radius, one uniform border and one drop shadow. That is a small enough
//! vocabulary that every project converges on the same flat-slab look, and the
//! only way out was writing a `stage ui` `.flsl` — which is how Fofighter ended
//! up with `uiPanel.flsl` and `uiCursor.flsl`, two shaders that exist purely to
//! draw a gradient and an underline.
//!
//! This module widens that vocabulary. Everything here is *instance data* — it
//! packs into the one instanced UI pipeline and costs no extra draw calls — on
//! the principle that the common visual grammar should batch, and `.flsl` stays
//! the escape hatch for genuinely procedural faces (a navball, a guard meter).
//!
//! **Every addition is backwards compatible.** `radius: 14.0` and
//! `radius: (12.0, 12.0, 0.0, 0.0)` both parse (see [`Corners`]); a gradient is
//! an `Option` beside the existing flat `fill` rather than a replacement for
//! it. Scenes authored against the first cut load unchanged, which is checked
//! by tests at the bottom of this file.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Per-corner / per-side scalars
// ---------------------------------------------------------------------------

/// Serde shim: a single number OR four of them. Lets `radius: 14.0` and
/// `radius: (12.0, 12.0, 0.0, 0.0)` both parse into the same field, so no
/// existing scene has to be rewritten.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum OneOrFour {
    One(f32),
    Four([f32; 4]),
}

/// Corner radii, clockwise from the top-left: `[TL, TR, BR, BL]`.
///
/// A header whose bottom corners are square (`(12, 12, 0, 0)`) is the case
/// that motivated this — with a single scalar it is impossible, and Fofighter's
/// `Front Header` has the wrong corners today because of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "OneOrFour", into = "OneOrFour")]
pub struct Corners(pub [f32; 4]);

/// Edge widths, clockwise from the left: `[L, T, R, B]`.
///
/// A bottom-only border is a rule under a header — currently an extra node
/// (Fofighter's `Front Rule` is a 620×2 filled rect for exactly this).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "OneOrFour", into = "OneOrFour")]
pub struct Sides(pub [f32; 4]);

macro_rules! quad_scalar {
    ($t:ident) => {
        impl $t {
            /// The same value on all four.
            pub const fn all(v: f32) -> Self {
                $t([v; 4])
            }
            /// True when all four agree (serializes back to a bare scalar).
            pub fn is_uniform(&self) -> bool {
                self.0[1] == self.0[0] && self.0[2] == self.0[0] && self.0[3] == self.0[0]
            }
            /// The largest of the four — what a conservative bound wants.
            pub fn max(&self) -> f32 {
                self.0.iter().copied().fold(f32::MIN, f32::max)
            }
            /// Every entry scaled (design units → px).
            pub fn scaled(&self, k: f32) -> [f32; 4] {
                [self.0[0] * k, self.0[1] * k, self.0[2] * k, self.0[3] * k]
            }
            /// True when nothing would be drawn.
            pub fn is_zero(&self) -> bool {
                self.0.iter().all(|v| *v <= 0.0)
            }
        }

        impl From<OneOrFour> for $t {
            fn from(v: OneOrFour) -> Self {
                match v {
                    OneOrFour::One(x) => $t([x; 4]),
                    OneOrFour::Four(x) => $t(x),
                }
            }
        }

        impl From<$t> for OneOrFour {
            fn from(v: $t) -> Self {
                // Round-trip the common case as a scalar so existing scenes
                // re-save byte-identical instead of churning every diff.
                if v.is_uniform() {
                    OneOrFour::One(v.0[0])
                } else {
                    OneOrFour::Four(v.0)
                }
            }
        }

        impl From<f32> for $t {
            fn from(v: f32) -> Self {
                $t([v; 4])
            }
        }

        impl std::ops::Deref for $t {
            type Target = [f32; 4];
            fn deref(&self) -> &[f32; 4] {
                &self.0
            }
        }

        impl std::ops::DerefMut for $t {
            fn deref_mut(&mut self) -> &mut [f32; 4] {
                &mut self.0
            }
        }
    };
}

quad_scalar!(Corners);
quad_scalar!(Sides);

// ---------------------------------------------------------------------------
// Gradients
// ---------------------------------------------------------------------------

/// How a [`Gradient`] sweeps across the element.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GradientKind {
    /// Straight sweep along `angle` (0° = left→right, 90° = top→bottom).
    #[default]
    Linear,
    /// Out from the rect's centre. `radius` is a fraction of the half-diagonal.
    Radial,
    /// Around the centre, starting at `angle` — conic sweeps, radial meters.
    Angular,
}

/// A two-stop gradient over the element's fill.
///
/// Deliberately two stops, not N: two covers panel depth, button sheen,
/// vignettes and meter ramps, and it packs into a fixed instance lane so
/// gradients stay in the one batched pipeline. `mid` biases where the two
/// colours meet, which is most of what a third stop would have bought.
///
/// Radial and angular gradients are **centred on the rect** in this cut. An
/// off-centre origin needs two more instance lanes and no real screen has
/// wanted it yet; the field can be added later without breaking data.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gradient {
    #[serde(default)]
    pub kind: GradientKind,
    /// The far colour. The near colour is the shape's own `fill`, so adding a
    /// gradient to an existing element keeps the look it already had at one end.
    pub to: [f32; 4],
    /// Degrees. Linear: sweep direction. Angular: where the sweep starts.
    #[serde(default)]
    pub angle: f32,
    /// Where the two colours meet, 0..1 (0.5 = even).
    #[serde(default = "half")]
    pub mid: f32,
    /// Radial only: extent as a fraction of the half-diagonal.
    #[serde(default = "one")]
    pub radius: f32,
}

fn half() -> f32 {
    0.5
}
fn one() -> f32 {
    1.0
}

impl Default for Gradient {
    fn default() -> Self {
        Gradient {
            kind: GradientKind::Linear,
            to: [0.0, 0.0, 0.0, 1.0],
            angle: 90.0,
            mid: 0.5,
            radius: 1.0,
        }
    }
}

impl Gradient {
    /// `[kind, angle radians, mid, radius]` — the shader's config lane.
    pub fn pack(&self) -> [f32; 4] {
        let kind = match self.kind {
            GradientKind::Linear => 1.0,
            GradientKind::Radial => 2.0,
            GradientKind::Angular => 3.0,
        };
        [kind, self.angle.to_radians(), self.mid.clamp(0.001, 0.999), self.radius.max(1e-4)]
    }
}

// ---------------------------------------------------------------------------
// Depth: shadows, glow, grain
// ---------------------------------------------------------------------------

/// A soft shadow. `inset` flips it inside the shape (a recessed well — the
/// thing a pressed button and a progress track both want).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShadowSpec {
    /// Shadow colour; alpha is the strength (multiplied by the element opacity).
    pub color: [f32; 4],
    /// Offset in design units — `+x` right, `+y` down (a light from top-left).
    pub offset: [f32; 2],
    /// Soft-edge width in design units (0 = a hard offset shape; bigger = softer).
    pub blur: f32,
    /// Grow the shadow beyond the element on every side (design units).
    pub spread: f32,
    /// Draw it INSIDE the shape instead of behind it.
    #[serde(default)]
    pub inset: bool,
}

impl Default for ShadowSpec {
    fn default() -> Self {
        ShadowSpec {
            color: [0.0, 0.0, 0.0, 0.5],
            offset: [0.0, 4.0],
            blur: 10.0,
            spread: 0.0,
            inset: false,
        }
    }
}

/// An outer bloom. Mechanically a shadow with no offset and a colour you can
/// see — kept separate because "glow" is what you are actually reaching for,
/// and because an element wants both at once (lifted off the page AND lit).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlowSpec {
    pub color: [f32; 4],
    /// Soft-edge width in design units.
    pub radius: f32,
    /// Grow the lit area beyond the element before the falloff starts.
    #[serde(default)]
    pub spread: f32,
}

impl Default for GlowSpec {
    fn default() -> Self {
        GlowSpec { color: [1.0, 0.85, 0.35, 0.6], radius: 12.0, spread: 0.0 }
    }
}

/// Per-pixel noise over the fill. The cheapest available cure for "flat slab" —
/// a couple of percent breaks the plastic look that reads as machine-made.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrainSpec {
    /// 0..1 — how far the noise pushes the colour.
    pub amount: f32,
    /// Noise cell size in px (1 = per-pixel static, higher = chunkier).
    pub scale: f32,
}

impl Default for GrainSpec {
    fn default() -> Self {
        GrainSpec { amount: 0.05, scale: 1.0 }
    }
}

/// How an element composites against what is already drawn. A batch key, not
/// instance data — each mode is its own pipeline, so mixing them costs one
/// extra draw call per switch (cheap, and the alternative is dual-source
/// blending nobody needs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Blend {
    /// Standard alpha compositing.
    #[default]
    Normal,
    /// Adds light — glows, energy, hit sparks.
    Additive,
    /// Darkens — shadow washes, stains.
    Multiply,
    /// Lightens without blowing out the way Additive does.
    Screen,
}

impl Blend {
    /// Every mode, for building the pipeline variants and the Inspector combo.
    pub const ALL: [Blend; 4] = [Blend::Normal, Blend::Additive, Blend::Multiply, Blend::Screen];

    pub fn label(self) -> &'static str {
        match self {
            Blend::Normal => "Normal",
            Blend::Additive => "Additive",
            Blend::Multiply => "Multiply",
            Blend::Screen => "Screen",
        }
    }
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// How a texture fills its element's rect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFit {
    /// Fill the rect, ignoring the source aspect ratio.
    #[default]
    Stretch,
    /// Fit inside the rect, letterboxing — the whole image is visible.
    Contain,
    /// Fill the rect, cropping the overflow — no bars, no distortion.
    Cover,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of `Corners`/`Sides`: scenes authored before per-corner
    /// radii existed must load byte-for-byte unchanged.
    #[test]
    fn scalar_radius_still_parses() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct S {
            radius: Corners,
            border: Sides,
        }
        let old: S = ron::from_str("(radius: 14.0, border: 2.0)").unwrap();
        assert_eq!(old.radius.0, [14.0; 4]);
        assert_eq!(old.border.0, [2.0; 4]);

        let new: S =
            ron::from_str("(radius: (12.0, 12.0, 0.0, 0.0), border: (0.0, 0.0, 0.0, 2.0))")
                .unwrap();
        assert_eq!(new.radius.0, [12.0, 12.0, 0.0, 0.0]);
        assert_eq!(new.border.0, [0.0, 0.0, 0.0, 2.0]);
    }

    /// A uniform value re-serializes as a bare scalar, so re-saving an old
    /// scene does not churn every shape in the diff.
    #[test]
    fn uniform_round_trips_as_a_scalar() {
        #[derive(Serialize, Deserialize)]
        struct S {
            radius: Corners,
        }
        let text = ron::to_string(&S { radius: Corners::all(8.0) }).unwrap();
        assert!(text.contains("radius:8"), "{text}");
        let text = ron::to_string(&S { radius: Corners([8.0, 8.0, 0.0, 0.0]) }).unwrap();
        assert!(text.contains('('), "{text}");
    }

    #[test]
    fn gradient_packs_kind_and_radians() {
        let g = Gradient { kind: GradientKind::Radial, angle: 180.0, ..Default::default() };
        let p = g.pack();
        assert_eq!(p[0], 2.0);
        assert!((p[1] - std::f32::consts::PI).abs() < 1e-5);
    }

    /// `mid` at exactly 0 or 1 would divide by zero in the shader's two-sided
    /// ramp; the pack clamps it into the open interval.
    #[test]
    fn gradient_mid_is_clamped_off_the_ends() {
        assert!(Gradient { mid: 0.0, ..Default::default() }.pack()[2] > 0.0);
        assert!(Gradient { mid: 1.0, ..Default::default() }.pack()[2] < 1.0);
    }
}
