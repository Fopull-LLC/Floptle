//! The value-or-curve property type and its baked lookup tables.
//!
//! Every particle property is a constant OR a hand-drawn curve (`ValueOrCurve`).
//! Curves are authored as keyframes with per-key interpolation (constant / linear /
//! bezier-tangent) and evaluated analytically only at **bake time**: the runtime
//! samples a fixed-size LUT (`LUT_N` entries), so the hot loop is one lerp per
//! property on the CPU — and, later, one texture/buffer fetch in the GPU compute
//! backend (the LUT *is* the interchange format; see the proposal §4.4).

use floptle_core::math::Vec3;

/// Samples per baked curve. 64 is indistinguishable from analytic for VFX-scale
/// curves and keeps a full track's property set under 1 KiB per channel group.
pub const LUT_N: usize = 64;

/// A keyed value: the scalar/vector/color payload of a curve key or constant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    F32(f32),
    Vec3(Vec3),
    Rgba([f32; 4]),
}

impl Value {
    /// Widen to 4 channels (unused channels zero) — the bake works channel-wise.
    fn channels(self) -> [f32; 4] {
        match self {
            Value::F32(v) => [v, 0.0, 0.0, 0.0],
            Value::Vec3(v) => [v.x, v.y, v.z, 0.0],
            Value::Rgba(v) => v,
        }
    }
}

/// How a key reaches the NEXT key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Interp {
    /// Hold this key's value until the next key (stepped).
    Constant,
    #[default]
    Linear,
    /// Cubic Hermite using this key's `out_tan` and the next key's `in_tan`
    /// (tangents are slopes in value-units per unit `t`, Unity-style).
    Bezier,
}

/// What a curve returns outside its keyed range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Extrapolate {
    #[default]
    Clamp,
    /// Wrap `t` back into the keyed range (loops the drawn shape).
    Repeat,
}

/// One drawn node on a curve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Key {
    /// Domain position. Life curves use the particle's normalized lifetime `[0,1]`;
    /// automation lanes use seconds along the effect timeline (normalized at compile).
    pub t: f32,
    pub v: Value,
    pub interp: Interp,
    /// Incoming tangent (slope), used when the PREVIOUS segment is `Bezier`.
    pub in_tan: f32,
    /// Outgoing tangent (slope), used when THIS segment is `Bezier`.
    pub out_tan: f32,
}

impl Key {
    pub fn new(t: f32, v: Value) -> Self {
        Self { t, v, interp: Interp::Linear, in_tan: 0.0, out_tan: 0.0 }
    }
}

/// An authored curve: the graph-editor backing data. Keys are kept sorted by `t`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Curve {
    pub keys: Vec<Key>,
    pub extrapolate: Extrapolate,
}

impl Curve {
    /// Evaluate analytically at `t` (bake/editor path — the sim samples LUTs).
    /// Empty curves return zero; a single key is a constant.
    pub fn eval(&self, t: f32) -> [f32; 4] {
        let (first, last) = match (self.keys.first(), self.keys.last()) {
            (Some(f), Some(l)) => (f, l),
            _ => return [0.0; 4],
        };
        if self.keys.len() == 1 {
            return first.v.channels();
        }
        let span = last.t - first.t;
        let t = match self.extrapolate {
            _ if span <= 0.0 => first.t,
            Extrapolate::Clamp => t.clamp(first.t, last.t),
            Extrapolate::Repeat => first.t + (t - first.t).rem_euclid(span),
        };
        // Segment lookup: index of the first key strictly past t, then step back.
        let hi = self.keys.partition_point(|k| k.t <= t).min(self.keys.len() - 1);
        let (a, b) = (&self.keys[hi.saturating_sub(1)], &self.keys[hi]);
        let dt = b.t - a.t;
        if dt <= 0.0 {
            return b.v.channels();
        }
        let u = ((t - a.t) / dt).clamp(0.0, 1.0);
        let (va, vb) = (a.v.channels(), b.v.channels());
        let mut out = [0.0f32; 4];
        match a.interp {
            // Hold `a` across the segment; exactly ON the next key returns it.
            Interp::Constant => out = if u >= 1.0 { vb } else { va },
            Interp::Linear => {
                for c in 0..4 {
                    out[c] = va[c] + (vb[c] - va[c]) * u;
                }
            }
            Interp::Bezier => {
                // Cubic Hermite with slope tangents scaled by the segment length.
                let (u2, u3) = (u * u, u * u * u);
                let h00 = 2.0 * u3 - 3.0 * u2 + 1.0;
                let h10 = u3 - 2.0 * u2 + u;
                let h01 = -2.0 * u3 + 3.0 * u2;
                let h11 = u3 - u2;
                for c in 0..4 {
                    out[c] = h00 * va[c]
                        + h10 * dt * a.out_tan
                        + h01 * vb[c]
                        + h11 * dt * b.in_tan;
                }
            }
        }
        out
    }
}

/// A property that is a single constant OR a drawn curve. `Const` is the default;
/// the inspector's hover-corner graph icon promotes it (proposal §6.4).
#[derive(Clone, Debug, PartialEq)]
pub enum ValueOrCurve {
    Const(Value),
    Curve(Curve),
}

impl ValueOrCurve {
    pub fn constant(v: f32) -> Self {
        Self::Const(Value::F32(v))
    }
}

// ---------------------------------------------------------------------------
// Baked (compiled) properties — what the sim actually samples.
// ---------------------------------------------------------------------------

/// A baked scalar property: constant, or `LUT_N` samples over the domain `[0,1]`.
#[derive(Clone, Debug)]
pub enum Prop1 {
    Const(f32),
    Lut(Box<[f32; LUT_N]>),
}

/// A baked 4-channel property (Vec3 uses xyz, Rgba uses all four).
#[derive(Clone, Debug)]
pub enum Prop4 {
    Const([f32; 4]),
    Lut(Box<[[f32; 4]; LUT_N]>),
}

/// Map `u ∈ [0,1]` onto the LUT's fractional index space.
#[inline]
fn lut_pos(u: f32) -> (usize, usize, f32) {
    let x = (u.clamp(0.0, 1.0)) * (LUT_N - 1) as f32;
    let i = (x as usize).min(LUT_N - 2);
    (i, i + 1, x - i as f32)
}

impl Prop1 {
    #[inline]
    pub fn sample(&self, u: f32) -> f32 {
        match self {
            Prop1::Const(v) => *v,
            Prop1::Lut(s) => {
                let (i, j, f) = lut_pos(u);
                s[i] + (s[j] - s[i]) * f
            }
        }
    }
}

impl Prop4 {
    #[inline]
    pub fn sample(&self, u: f32) -> [f32; 4] {
        match self {
            Prop4::Const(v) => *v,
            Prop4::Lut(s) => {
                let (i, j, f) = lut_pos(u);
                let (a, b) = (s[i], s[j]);
                [
                    a[0] + (b[0] - a[0]) * f,
                    a[1] + (b[1] - a[1]) * f,
                    a[2] + (b[2] - a[2]) * f,
                    a[3] + (b[3] - a[3]) * f,
                ]
            }
        }
    }

    #[inline]
    pub fn sample_vec3(&self, u: f32) -> Vec3 {
        let v = self.sample(u);
        Vec3::new(v[0], v[1], v[2])
    }
}

/// Bake a value-or-curve to a scalar property. `domain` rescales key times
/// (life curves pass 1.0; automation lanes pass the effect lifetime so lane keys
/// authored in seconds land on the shared `[0,1]` LUT domain).
pub fn bake1(p: &ValueOrCurve, domain: f32) -> Prop1 {
    match p {
        ValueOrCurve::Const(v) => Prop1::Const(v.channels()[0]),
        ValueOrCurve::Curve(c) => {
            let mut s = Box::new([0.0f32; LUT_N]);
            for (i, out) in s.iter_mut().enumerate() {
                let u = i as f32 / (LUT_N - 1) as f32;
                *out = c.eval(u * domain)[0];
            }
            Prop1::Lut(s)
        }
    }
}

/// Bake a value-or-curve to a 4-channel property (see [`bake1`] for `domain`).
pub fn bake4(p: &ValueOrCurve, domain: f32) -> Prop4 {
    match p {
        ValueOrCurve::Const(v) => Prop4::Const(v.channels()),
        ValueOrCurve::Curve(c) => {
            let mut s = Box::new([[0.0f32; 4]; LUT_N]);
            for (i, out) in s.iter_mut().enumerate() {
                let u = i as f32 / (LUT_N - 1) as f32;
                *out = c.eval(u * domain);
            }
            Prop4::Lut(s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(t: f32, v: f32) -> Key {
        Key::new(t, Value::F32(v))
    }

    #[test]
    fn linear_curve_lerps_and_clamps() {
        let c = Curve { keys: vec![key(0.0, 1.0), key(1.0, 3.0)], extrapolate: Extrapolate::Clamp };
        assert_eq!(c.eval(0.5)[0], 2.0);
        assert_eq!(c.eval(-1.0)[0], 1.0);
        assert_eq!(c.eval(2.0)[0], 3.0);
    }

    #[test]
    fn repeat_extrapolation_wraps() {
        let c =
            Curve { keys: vec![key(0.0, 0.0), key(1.0, 4.0)], extrapolate: Extrapolate::Repeat };
        assert!((c.eval(1.25)[0] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn constant_interp_steps() {
        let mut a = key(0.0, 5.0);
        a.interp = Interp::Constant;
        let c = Curve { keys: vec![a, key(1.0, 9.0)], extrapolate: Extrapolate::Clamp };
        assert_eq!(c.eval(0.99)[0], 5.0);
        assert_eq!(c.eval(1.0)[0], 9.0);
    }

    #[test]
    fn bezier_hits_endpoints_and_flat_tangents_ease() {
        let mut a = key(0.0, 0.0);
        a.interp = Interp::Bezier;
        let c = Curve { keys: vec![a, key(1.0, 1.0)], extrapolate: Extrapolate::Clamp };
        assert_eq!(c.eval(0.0)[0], 0.0);
        assert_eq!(c.eval(1.0)[0], 1.0);
        // Zero tangents at both ends = smoothstep: midpoint is 0.5, but eased.
        assert!((c.eval(0.5)[0] - 0.5).abs() < 1e-5);
        assert!(c.eval(0.25)[0] < 0.25); // slow start
        assert!(c.eval(0.75)[0] > 0.75); // slow end
    }

    #[test]
    fn lut_bake_matches_analytic_for_linear() {
        let c = Curve { keys: vec![key(0.0, 2.0), key(1.0, 6.0)], extrapolate: Extrapolate::Clamp };
        let baked = bake1(&ValueOrCurve::Curve(c.clone()), 1.0);
        for i in 0..=20 {
            let u = i as f32 / 20.0;
            assert!(
                (baked.sample(u) - c.eval(u)[0]).abs() < 1e-4,
                "LUT diverged from analytic at u={u}"
            );
        }
    }

    #[test]
    fn lane_domain_rescale_normalizes_seconds() {
        // A lane keyed in seconds over a 2 s effect: value 0 → 8 across the timeline.
        let c = Curve { keys: vec![key(0.0, 0.0), key(2.0, 8.0)], extrapolate: Extrapolate::Clamp };
        let baked = bake1(&ValueOrCurve::Curve(c), 2.0);
        assert!((baked.sample(0.5) - 4.0).abs() < 1e-4);
    }

    #[test]
    fn rgba_curve_interpolates_all_channels() {
        let c = Curve {
            keys: vec![
                Key::new(0.0, Value::Rgba([1.0, 0.0, 0.0, 1.0])),
                Key::new(1.0, Value::Rgba([0.0, 1.0, 0.0, 0.0])),
            ],
            extrapolate: Extrapolate::Clamp,
        };
        let mid = c.eval(0.5);
        assert_eq!(mid, [0.5, 0.5, 0.0, 0.5]);
    }
}
