//! A swell: the waves on a body of water, as a rule rather than a simulation.
//!
//! A game draws its sea with Gerstner waves, a handful of trochoids summed in
//! a vertex shader. A solver that floats things on a flat sea then disagrees
//! with the picture: the boat sits level while the water it is drawn in
//! heaves past it. So the waves belong to the water itself. A volume carries
//! its list, and everything that asks where the surface is asks here: the
//! solver that floats a hull, `water.at`, a splash, and the shader that moves
//! the surface's vertices (which is handed the same numbers, [`Swell::pack`]).
//!
//! **One wave.** A point of the still surface at `x` moves in a circle as the
//! wave passes: up by `amplitude · sin θ` and along the wave's direction by
//! `steepness · amplitude · cos θ`, where `θ = k·(direction · x) − ω·t +
//! phase`, `k = 2π / wavelength` and `ω = k · speed`. With no `speed` given
//! a wave travels as one does on deep water, `ω = √(g·k)`: long waves outrun
//! short ones.
//!
//! **Asking the other way.** The shader moves a vertex it knows the rest
//! place of. The solver has a point in the world and needs the water above
//! it, which came from a rest place a little to one side. [`Swell::over`]
//! finds that place by Newton's method, starting afresh each time, so the
//! answer is the same in every replay.
//!
//! **On a round world** the waves are laid about an axis: the lake's own
//! middle, or a sea's north. Directions are given on the tangent plane there
//! (x and z, as on a flat pool), and a wave's fronts are the circles a fixed
//! distance along the ground from where it comes from, so its wavelength is
//! the same everywhere on the sphere.
//!
//! Everything here is a pure function of the waves, the place and the time.
//! The time is the simulation's clock, so a dropped tick and a rollback's
//! replay read the surface they read the first time.

use glam::{DMat3, DVec3};

/// The most waves one body of water carries: what a shader's swell block has
/// room for.
pub const MAX_WAVES: usize = 16;

/// The pull deep-water waves travel under when a wave gives no speed, m/s².
pub const SWELL_GRAVITY: f64 = 9.81;

/// How near folding over itself a swell may come. The sum over the waves of
/// `k · steepness · amplitude` is 1 where crests turn to cusps and loops; a
/// set past this is eased back to it, every wave by the same factor.
pub const MAX_FOLD: f64 = 0.8;

/// `vec4`s in the block a shader reads a swell from: see [`Swell::pack`].
pub const SWELL_BLOCK: usize = 7 + 2 * MAX_WAVES;

/// One wave, as a game gives it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Wave {
    /// Metres from the still surface to the crest.
    pub amplitude: f32,
    /// Metres from crest to crest.
    pub wavelength: f32,
    /// The way it travels, along the water's x and z (on a round world, the
    /// tangent plane at the swell's axis). Any length.
    pub direction: [f32; 2],
    /// How far a point of the surface moves along the wave for each metre it
    /// rises: 0 is a rolling sine, 1 the circle a real deep-water wave turns,
    /// which sharpens the crests and flattens the troughs.
    pub steepness: f32,
    /// How fast its crests travel, m/s; `None` for a deep-water wave of its
    /// length.
    pub speed: Option<f32>,
    /// Where in its cycle it starts, radians.
    pub phase: f32,
}

impl Default for Wave {
    fn default() -> Self {
        Self { amplitude: 0.5, wavelength: 20.0, direction: [1.0, 0.0], steepness: 1.0, speed: None, phase: 0.0 }
    }
}

/// One wave, worked out once.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Train {
    /// Its direction in the water's frame, unit, at right angles to the axis.
    dir: DVec3,
    k: f64,
    omega: f64,
    amp: f64,
    /// How far the surface moves along the wave: `steepness · amplitude`,
    /// eased so the set does not fold.
    horiz: f64,
    phase: f64,
}

/// The surface a swell is laid on, where it is asked about.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Lay {
    /// A flat surface: the frame's x–z plane, `level` up its y.
    Flat { level: f64 },
    /// The sphere of `radius` about the frame's origin.
    Round { radius: f64 },
}

/// A swell at a place in the water's frame: what [`Swell::over`] answers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Over {
    /// Metres the place is above the moving surface; negative under it.
    pub above: f64,
    /// Metres the surface stands above its still level here.
    pub height: f64,
    /// Out of the surface, unit: it leans with the wave's face.
    pub normal: DVec3,
    /// The water's own motion as the waves pass, m/s: the circle a point of
    /// the surface turns, fading with depth. For a place in the air it is
    /// the motion of the surface under it; whoever asks decides whether air
    /// has a flow.
    pub flow: DVec3,
}

/// The waves on one body of water.
#[derive(Clone, Debug, PartialEq)]
pub struct Swell {
    /// The waves as they were given.
    waves: Vec<Wave>,
    /// The axis they are laid about on a round world, unit.
    axis: DVec3,
    trains: Vec<Train>,
    /// The most the surface can stand above or below its still level.
    reach: f64,
    /// No wave moves the surface sideways: the rest place of a point is the
    /// point, and asking costs one pass over the waves.
    upright: bool,
    /// Tells this set from every other: see [`Swell::over`].
    stamp: u64,
}

/// What one pass over the waves at a rest place adds up to.
struct Sums {
    /// Up at the rest place.
    up: DVec3,
    /// The surface's move along the ground, and up.
    horiz: DVec3,
    height: f64,
    /// How the moved place changes as the rest place does: the identity less
    /// each wave's squeeze along its own direction.
    jac: DMat3,
    /// How the height changes as the rest place does.
    slope: DVec3,
    /// Each wave's sine and cosine here: what the water's own motion is
    /// made of, kept so it is not worked out twice.
    turn: [(f64, f64); MAX_WAVES],
}

thread_local! {
    /// The last answer [`Swell::over`] gave on this thread. A shape in the
    /// water is asked about several times a step (how deep, which way is up,
    /// how fast the water moves), each at the same place and time.
    static LAST: std::cell::Cell<Option<([u64; 6], Over)>> = const { std::cell::Cell::new(None) };
}

impl Swell {
    /// The swell `waves` make, laid about `axis` on a round world (any
    /// length; +Y for a flat one). `None` when there are no waves: water
    /// with no swell carries none, and costs nothing. Waves past
    /// [`MAX_WAVES`] are left out.
    pub fn new(waves: &[Wave], axis: DVec3) -> Option<Self> {
        if waves.is_empty() {
            return None;
        }
        let waves: Vec<Wave> = waves.iter().take(MAX_WAVES).copied().collect();
        let axis = axis.try_normalize().unwrap_or(DVec3::Y);
        let (e1, e2) = tangent_axes(axis);
        let mut trains: Vec<Train> = waves
            .iter()
            .map(|w| {
                let finite = |v: f32, or: f64| if v.is_finite() { v as f64 } else { or };
                let d = e1 * finite(w.direction[0], 0.0) + e2 * finite(w.direction[1], 0.0);
                let k = std::f64::consts::TAU / finite(w.wavelength, 20.0).max(0.01);
                let amp = finite(w.amplitude, 0.0).max(0.0);
                Train {
                    dir: d.try_normalize().unwrap_or(e1),
                    k,
                    omega: match w.speed {
                        Some(c) if c.is_finite() => c as f64 * k,
                        _ => (SWELL_GRAVITY * k).sqrt(),
                    },
                    amp,
                    horiz: finite(w.steepness, 0.0).max(0.0) * amp,
                    phase: finite(w.phase, 0.0),
                }
            })
            .collect();
        let fold: f64 = trains.iter().map(|t| t.k * t.horiz).sum();
        if fold > MAX_FOLD {
            for t in &mut trains {
                t.horiz *= MAX_FOLD / fold;
            }
        }
        let stamp = {
            use std::hash::Hasher;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for t in &trains {
                for v in [t.dir.x, t.dir.y, t.dir.z, t.k, t.omega, t.amp, t.horiz, t.phase] {
                    h.write_u64(v.to_bits());
                }
            }
            h.finish()
        };
        Some(Self {
            reach: trains.iter().map(|t| t.amp).sum(),
            upright: trains.iter().all(|t| t.horiz == 0.0),
            waves,
            axis,
            trains,
            stamp,
        })
    }

    /// The waves as they were given.
    pub fn waves(&self) -> &[Wave] {
        &self.waves
    }

    /// The axis the waves are laid about on a round world.
    pub fn axis(&self) -> DVec3 {
        self.axis
    }

    /// The most the surface stands above or below its still level, metres.
    pub fn reach(&self) -> f64 {
        self.reach
    }

    /// How fast wave `i`'s crests travel, m/s.
    pub fn speed(&self, i: usize) -> f64 {
        self.trains.get(i).map_or(0.0, |t| t.omega / t.k)
    }

    /// One pass over the waves at the rest place `rest`.
    fn sums(&self, rest: DVec3, lay: Lay, t: f64) -> Sums {
        let round = match lay {
            Lay::Round { radius } => Some(radius),
            Lay::Flat { .. } => None,
        };
        let up = match round {
            Some(_) => rest.try_normalize().unwrap_or(self.axis),
            None => DVec3::Y,
        };
        let mut s = Sums {
            up,
            horiz: DVec3::ZERO,
            height: 0.0,
            jac: DMat3::IDENTITY,
            slope: DVec3::ZERO,
            turn: [(0.0, 0.0); MAX_WAVES],
        };
        for (i, w) in self.trains.iter().enumerate() {
            let theta = |along: f64| sin_cos(w.k * along - w.omega * t + w.phase);
            match round {
                // On a plane a wave runs its own way, every metre a metre.
                None => {
                    let (sin, cos) = theta(rest.dot(w.dir));
                    s.turn[i] = (sin, cos);
                    s.horiz += w.dir * (w.horiz * cos);
                    s.height += w.amp * sin;
                    if w.horiz != 0.0 {
                        let squeeze = w.k * w.horiz * sin;
                        s.jac.x_axis -= w.dir * (squeeze * w.dir.x);
                        s.jac.z_axis -= w.dir * (squeeze * w.dir.z);
                    }
                    s.slope += w.dir * (w.k * w.amp * cos);
                }
                // On a sphere: metres over the ground from where the wave
                // starts, the way it runs here, and how long that way is
                // (it shrinks to nothing where the wave comes from).
                Some(radius) => {
                    let c = up.dot(w.dir).clamp(-1.0, 1.0);
                    let (way, len) = (w.dir - up * c, (1.0 - c * c).sqrt());
                    let (sin, cos) = theta(radius * c.asin());
                    s.turn[i] = (sin, cos);
                    s.horiz += way * (w.horiz * cos);
                    s.height += w.amp * sin;
                    if len > 1e-9 {
                        let squeeze = w.k * w.horiz * sin / len;
                        s.jac.x_axis -= way * (squeeze * way.x);
                        s.jac.y_axis -= way * (squeeze * way.y);
                        s.jac.z_axis -= way * (squeeze * way.z);
                        s.slope += way * (w.k * w.amp * cos / len);
                    }
                }
            }
        }
        s
    }

    /// Out of the surface a pass over the waves describes: across the two
    /// ways the moved surface runs from the rest place.
    fn normal_of(s: &Sums) -> DVec3 {
        let (t1, t2) = tangent_axes(s.up);
        let a = s.jac * t1 + s.up * s.slope.dot(t1);
        let b = s.jac * t2 + s.up * s.slope.dot(t2);
        let n = a.cross(b).try_normalize().unwrap_or(s.up);
        if n.dot(s.up) < 0.0 { -n } else { n }
    }

    /// Where the point of the still surface at `rest` is at time `t`: how
    /// far it has moved, in the water's frame, and the surface's normal
    /// there. What a shader's `waterSwell` and `waterSwellNormal` compute,
    /// vertex by vertex. On a round world `rest` is taken out to the sphere
    /// along its own radius.
    pub fn offset(&self, rest: DVec3, lay: Lay, t: f64) -> (DVec3, DVec3) {
        let s = self.sums(rest, lay, t);
        (s.horiz + s.up * s.height, Self::normal_of(&s))
    }

    /// The water at `p` in its frame at time `t`: how far `p` is above the
    /// moving surface, how the surface stands and leans there, and how the
    /// water moves.
    ///
    /// The surface over `p` came from a rest place beside it, found by a
    /// few Newton steps from `p` itself, with nothing carried over from the
    /// ask before: the same answer whenever it is asked. The last answer is
    /// kept, per thread, so asking again about the same place and time is
    /// free; it is a pure function either way.
    pub fn over(&self, p: DVec3, lay: Lay, t: f64) -> Over {
        let (kind, size) = match lay {
            Lay::Flat { level } => (0u64, level),
            Lay::Round { radius } => (1, radius),
        };
        let key = [self.stamp ^ kind, p.x.to_bits(), p.y.to_bits(), p.z.to_bits(), size.to_bits(), t.to_bits()];
        if let Some((k, o)) = LAST.get()
            && k == key
        {
            return o;
        }
        let o = self.solve(p, lay, t);
        LAST.set(Some((key, o)));
        o
    }

    fn solve(&self, p: DVec3, lay: Lay, t: f64) -> Over {
        // The place `p` is over, on the still surface.
        let onto = |v: DVec3| match lay {
            Lay::Flat { level } => DVec3::new(v.x, level, v.z),
            Lay::Round { radius } => v.try_normalize().unwrap_or(self.axis) * radius,
        };
        let target = onto(p);
        let mut rest = target;
        let mut s = self.sums(rest, lay, t);
        if !self.upright {
            for _ in 0..NEWTON_STEPS {
                // How far the moved place is from over `p`, along the ground.
                let miss = match lay {
                    Lay::Flat { .. } => target - rest - s.horiz,
                    Lay::Round { radius } => {
                        let moved = s.up * (radius + s.height) + s.horiz;
                        target / radius * moved.length() - moved
                    }
                };
                if miss.length_squared() < NEWTON_NEAR * NEWTON_NEAR {
                    break;
                }
                rest = onto(rest + s.jac.inverse() * miss);
                s = self.sums(rest, lay, t);
            }
        }
        let (above, height, up) = match lay {
            Lay::Flat { level } => (p.y - level - s.height, s.height, DVec3::Y),
            Lay::Round { radius } => {
                let surface = (s.up * (radius + s.height) + s.horiz).length();
                (p.length() - surface, surface - radius, target / radius)
            }
        };
        let mut flow = DVec3::ZERO;
        let round = matches!(lay, Lay::Round { .. });
        for (w, (sin, cos)) in self.trains.iter().zip(&s.turn) {
            let way = if round { w.dir - s.up * s.up.dot(w.dir) } else { w.dir };
            // The circle shrinks with depth: a wave is felt about half its
            // length down.
            let fade = if above < 0.0 { exp_neg(w.k * above) } else { 1.0 };
            flow += (way * (w.horiz * sin) - up * (w.amp * cos)) * (fade * w.omega);
        }
        Over { above, height, normal: Self::normal_of(&s), flow }
    }

    /// The block a shader reads this swell from, [`SWELL_BLOCK`] `vec4`s:
    ///
    /// | lane | holds |
    /// |---|---|
    /// | 0 | the number of waves, the sphere's radius (0 on a plane), the still level on a plane |
    /// | 1–3 | the rows of the turn from the drawn node's space to the water's, and in `w` where the node is in the water's frame |
    /// | 4–6 | the rows of the turn from the water's frame to the world |
    /// | 7 + 2i | wave `i`'s direction and `k` |
    /// | 8 + 2i | its amplitude, its move along the ground, and `phase − ω·t` wrapped to a turn |
    ///
    /// The time is folded into each wave's phase here, in f64, so an hour
    /// into a game the shader is not multiplying a large time by a float.
    pub fn pack(&self, lay: Lay, t: f64, node_to_water: (DMat3, DVec3), water_to_world: DMat3) -> [[f32; 4]; SWELL_BLOCK] {
        let mut out = [[0.0f32; 4]; SWELL_BLOCK];
        out[0] = match lay {
            Lay::Flat { level } => [self.trains.len() as f32, 0.0, level as f32, 0.0],
            Lay::Round { radius } => [self.trains.len() as f32, radius as f32, 0.0, 0.0],
        };
        let (turn, at) = node_to_water;
        for r in 0..3 {
            let row = turn.row(r);
            out[1 + r] = [row.x as f32, row.y as f32, row.z as f32, at[r] as f32];
            let row = water_to_world.row(r);
            out[4 + r] = [row.x as f32, row.y as f32, row.z as f32, 0.0];
        }
        for (i, w) in self.trains.iter().enumerate() {
            out[7 + 2 * i] = [w.dir.x as f32, w.dir.y as f32, w.dir.z as f32, w.k as f32];
            let phase = (w.phase - w.omega * t).rem_euclid(std::f64::consts::TAU);
            out[8 + 2 * i] = [w.amp as f32, w.horiz as f32, phase as f32, 0.0];
        }
        out
    }
}

/// The most Newton steps [`Swell::over`] takes to find the rest place, and
/// how near, in metres along the ground, is near enough to stop sooner. A
/// calm sea is found in one or two; a set at [`MAX_FOLD`] takes them all.
const NEWTON_STEPS: usize = 4;
const NEWTON_NEAR: f64 = 1e-4;

/// The sine and cosine of `theta`, radians, by a fixed polynomial: the same
/// bits on every machine, where a platform's own `sin` is whatever its maths
/// library says, and a few times quicker. Good to 1e-11.
fn sin_cos(theta: f64) -> (f64, f64) {
    // In turns, folded to a quarter either side of zero.
    let turns = theta * std::f64::consts::FRAC_1_PI * 0.5;
    // The nearest whole turn below, without a call into the maths library.
    let whole = (turns + 0.5) as i64 as f64;
    let x = turns - if whole > turns + 0.5 { whole - 1.0 } else { whole };
    let (x, flip) = if x > 0.25 {
        (0.5 - x, -1.0)
    } else if x < -0.25 {
        (-0.5 - x, -1.0)
    } else {
        (x, 1.0)
    };
    let y = x * std::f64::consts::TAU;
    let y2 = y * y;
    // Taylor's series, far enough that the last term is under 1e-11 at π/2.
    let sin = y
        * (1.0
            + y2 * (-1.0 / 6.0
                + y2 * (1.0 / 120.0
                    + y2 * (-1.0 / 5040.0
                        + y2 * (1.0 / 362_880.0
                            + y2 * (-1.0 / 39_916_800.0 + y2 * (1.0 / 6_227_020_800.0 + y2 * (-1.0 / 1_307_674_368_000.0))))))));
    let cos = 1.0
        + y2 * (-0.5
            + y2 * (1.0 / 24.0
                + y2 * (-1.0 / 720.0
                    + y2 * (1.0 / 40_320.0
                        + y2 * (-1.0 / 3_628_800.0
                            + y2 * (1.0 / 479_001_600.0 + y2 * (-1.0 / 87_178_291_200.0 + y2 * (1.0 / 20_922_789_888_000.0))))))));
    (sin, cos * flip)
}

/// `e^x` for `x` of 0 or less, the same bits on every machine: how a wave's
/// stir fades with depth. Good to a few parts in a million, which is more
/// than a drag needs.
fn exp_neg(x: f64) -> f64 {
    if x < -40.0 {
        return 0.0;
    }
    if x >= 0.0 {
        return 1.0;
    }
    // 2^n times e^r, with r between -ln 2 and 0.
    let n = (x * std::f64::consts::LOG2_E) as i64 as f64;
    let r = x - n * std::f64::consts::LN_2;
    // Taylor's series by Horner's rule: its last term is under 2e-6.
    const INV: [f64; 8] = [1.0, 1.0 / 2.0, 1.0 / 6.0, 1.0 / 24.0, 1.0 / 120.0, 1.0 / 720.0, 1.0 / 5040.0, 1.0 / 40_320.0];
    let mut sum = 0.0;
    for c in INV.iter().rev() {
        sum = (sum + c) * r;
    }
    let sum = sum + 1.0;
    sum * f64::from_bits(((1023 + n as i64) as u64) << 52)
}

/// Two unit directions along the ground at `up`, at right angles to it and
/// to each other: the frame's x and z wherever that is possible, so waves on
/// a round world whose axis is +Y run as they would on a flat pool.
pub fn tangent_axes(up: DVec3) -> (DVec3, DVec3) {
    let z = DVec3::Z - up * up.z;
    let e2 = if z.length_squared() > 1e-6 { z.normalize() } else { (DVec3::X - up * up.x).normalize() };
    (up.cross(e2).normalize(), e2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wave(amplitude: f32, wavelength: f32, direction: [f32; 2], steepness: f32) -> Wave {
        Wave { amplitude, wavelength, direction, steepness, speed: None, phase: 0.0 }
    }

    /// Eight waves of mixed lengths and headings, as a game's sea has.
    pub(crate) fn sea() -> Vec<Wave> {
        (0..8)
            .map(|i| {
                let a = 0.4 + i as f32 * 0.83;
                Wave {
                    amplitude: 0.9 / (1.0 + i as f32),
                    wavelength: 60.0 / (1.0 + i as f32 * 0.7),
                    direction: [a.cos(), a.sin()],
                    steepness: 0.7,
                    speed: None,
                    phase: i as f32 * 1.3,
                }
            })
            .collect()
    }

    /// **One wave is the textbook trochoid.** A point of the surface rises
    /// by `A sin θ` and moves along the wave by `s·A cos θ`, and a wave
    /// given no speed travels as deep water carries it.
    #[test]
    fn one_wave_is_the_formula() {
        let s = Swell::new(&[wave(1.5, 40.0, [1.0, 0.0], 0.5)], DVec3::Y).unwrap();
        let k = std::f64::consts::TAU / 40.0;
        let omega = (9.81 * k).sqrt();
        assert!((s.speed(0) - omega / k).abs() < 1e-12, "a deep-water wave's speed");
        for (x, t) in [(0.0, 0.0), (7.0, 1.25), (-33.0, 9.5), (120.0, 61.0)] {
            let theta = k * x - omega * t;
            let (off, _) = s.offset(DVec3::new(x, 0.0, 3.0), Lay::Flat { level: 0.0 }, t);
            assert!((off.y - 1.5 * theta.sin()).abs() < 1e-12, "height at {x}, {t}");
            assert!((off.x - 0.75 * theta.cos()).abs() < 1e-12, "sway at {x}, {t}");
            assert!(off.z.abs() < 1e-12);
        }
        // A given speed is the speed.
        let fixed = Swell::new(&[Wave { speed: Some(3.0), ..wave(1.0, 10.0, [0.0, 1.0], 0.0) }], DVec3::Y).unwrap();
        assert!((fixed.speed(0) - 3.0).abs() < 1e-12);
        let (off, _) = fixed.offset(DVec3::new(0.0, 0.0, 2.5 + 3.0 * 4.0), Lay::Flat { level: 0.0 }, 4.0);
        assert!((off.y - 1.0).abs() < 1e-9, "the crest at 2.5 m has travelled 12 m in 4 s: {}", off.y);
    }

    /// **The water over a point is the surface the shader draws there.**
    /// Move a rest point as the shader does, ask what is over where it
    /// landed, and the answer is that point: on the surface, to well under a
    /// millimetre, with the same normal.
    #[test]
    fn over_undoes_offset() {
        let s = Swell::new(&sea(), DVec3::Y).unwrap();
        let mut worst: f64 = 0.0;
        for i in 0..400 {
            let rest = DVec3::new((i as f64 * 7.31) % 190.0 - 95.0, 2.0, (i as f64 * 3.77) % 170.0 - 85.0);
            let t = i as f64 * 0.37;
            let lay = Lay::Flat { level: 2.0 };
            let (off, n) = s.offset(rest, lay, t);
            let o = s.over(rest + off, lay, t);
            worst = worst.max(o.above.abs());
            assert!((o.normal - n).length() < 1e-3, "the normal over the moved point is the moved point's: {:?} {n:?}", o.normal);
            assert!((o.height - off.y).abs() < 1e-4);
        }
        assert!(worst < 1e-4, "a point of the drawn surface is {worst} m off the solver's");
    }

    /// The same on a sphere, small enough that the curve matters.
    #[test]
    fn over_undoes_offset_on_a_round_world() {
        let axis = DVec3::new(0.3, 1.0, -0.2).normalize();
        let s = Swell::new(&sea(), axis).unwrap();
        let (e1, e2) = tangent_axes(axis);
        let lay = Lay::Round { radius: 805.0 };
        let mut worst: f64 = 0.0;
        for i in 0..400 {
            let (a, b) = ((i as f64 * 7.31) % 300.0 - 150.0, (i as f64 * 3.77) % 300.0 - 150.0);
            let rest = (axis * 805.0 + e1 * a + e2 * b).normalize() * 805.0;
            let t = i as f64 * 0.37;
            let (off, n) = s.offset(rest, lay, t);
            let o = s.over(rest + off, lay, t);
            worst = worst.max(o.above.abs());
            assert!((o.normal - n).length() < 1e-3);
        }
        assert!(worst < 1e-4, "a point of the drawn surface is {worst} m off the solver's");
    }

    /// **Near its axis a round world's swell is the flat one.** A lake on a
    /// planet and a pool on a table, given the same waves, heave alike.
    #[test]
    fn a_round_swell_is_the_flat_one_near_its_axis() {
        let waves = sea();
        let flat = Swell::new(&waves, DVec3::Y).unwrap();
        let round = Swell::new(&waves, DVec3::Y).unwrap();
        let r = 600_000.0;
        for (x, z, t) in [(0.0, 0.0, 0.0), (30.0, -12.0, 4.0), (-80.0, 55.0, 17.5)] {
            let (f, _) = flat.offset(DVec3::new(x, 0.0, z), Lay::Flat { level: 0.0 }, t);
            let on = DVec3::new(x, (r * r - x * x - z * z).sqrt(), z);
            let (o, _) = round.offset(on, Lay::Round { radius: r }, t);
            assert!((f - o).length() < 0.01, "at {x}, {z}: flat {f:?} round {o:?}");
        }
    }

    /// **A wavelength is the same all over a sphere.** A quarter of the way
    /// round from the axis, across the wave, crests are still one wavelength
    /// apart along the ground.
    #[test]
    fn a_wavelength_holds_round_the_sphere() {
        let s = Swell::new(&[wave(1.0, 50.0, [1.0, 0.0], 0.0)], DVec3::Y).unwrap();
        let r = 2000.0;
        let lay = Lay::Round { radius: r };
        // Walk along the wave's own great circle from 40° off the axis.
        let at = |metres: f64| {
            let a: f64 = 40f64.to_radians() + metres / r;
            DVec3::new(a.sin(), a.cos(), 0.0) * r
        };
        let h0 = s.offset(at(0.0), lay, 0.0).0.dot(at(0.0).normalize());
        let h1 = s.offset(at(50.0), lay, 0.0).0.dot(at(50.0).normalize());
        let half = s.offset(at(25.0), lay, 0.0).0.dot(at(25.0).normalize());
        assert!((h0 - h1).abs() < 1e-9, "one wavelength on, the same height: {h0} {h1}");
        assert!((h0 + half).abs() < 1e-9, "half a wavelength on, the opposite: {h0} {half}");
    }

    /// **The surface never folds.** Waves too steep to sum are eased back,
    /// so the water over any point has one answer.
    #[test]
    fn a_swell_too_steep_is_eased_short_of_folding() {
        let s = Swell::new(&[wave(4.0, 10.0, [1.0, 0.0], 1.0), wave(3.0, 8.0, [0.6, 0.8], 1.0)], DVec3::Y).unwrap();
        let fold: f64 = s.trains.iter().map(|t| t.k * t.horiz).sum();
        assert!((fold - MAX_FOLD).abs() < 1e-12, "{fold}");
        let lay = Lay::Flat { level: 0.0 };
        let mut worst: f64 = 0.0;
        for i in 0..300 {
            let rest = DVec3::new(i as f64 * 0.173, 0.0, i as f64 * 0.091);
            let (off, _) = s.offset(rest, lay, 2.0);
            worst = worst.max(s.over(rest + off, lay, 2.0).above.abs());
        }
        assert!(worst < 1e-3, "{worst}");
    }

    /// **The same time is the same surface.** Asked twice, asked after other
    /// questions, asked of a copy: bit for bit.
    #[test]
    fn the_same_time_gives_the_same_surface() {
        let s = Swell::new(&sea(), DVec3::Y).unwrap();
        let lay = Lay::Flat { level: 0.0 };
        let p = DVec3::new(12.5, -0.3, -40.25);
        let first = s.over(p, lay, 123.456);
        let _ = s.over(DVec3::new(1.0, 0.0, 2.0), lay, 5.0);
        assert_eq!(first, s.over(p, lay, 123.456));
        assert_eq!(first, s.clone().over(p, lay, 123.456));
        assert_eq!(first, s.solve(p, lay, 123.456), "the kept answer is the worked one");
        assert_ne!(first.above, s.over(p, lay, 123.5).above, "and it moves with the clock");
    }

    /// **Water under a wave turns in a circle, and less with depth.** At the
    /// crest it runs with the wave, in the trough against it, and a
    /// wavelength down it is all but still.
    #[test]
    fn the_water_moves_as_the_wave_passes() {
        let s = Swell::new(&[wave(1.0, 40.0, [1.0, 0.0], 1.0)], DVec3::Y).unwrap();
        let k = std::f64::consts::TAU / 40.0;
        let omega = (9.81 * k).sqrt();
        let lay = Lay::Flat { level: 0.0 };
        // The crest is where θ = π/2: its rest place is x = 10, and a
        // trochoid's crest does not move sideways.
        let crest = s.over(DVec3::new(10.0, 0.9, 0.0), lay, 0.0);
        assert!((crest.height - 1.0).abs() < 1e-6, "{}", crest.height);
        assert!((crest.flow.x - omega * (-k * 0.1f64).exp()).abs() < 1e-3, "{:?}", crest.flow);
        let trough = s.over(DVec3::new(30.0, -1.1, 0.0), lay, 0.0);
        assert!(trough.flow.x < -omega * 0.9, "{:?}", trough.flow);
        let deep = s.over(DVec3::new(10.0, -40.0, 0.0), lay, 0.0);
        assert!(deep.flow.length() < omega * 0.01, "{:?}", deep.flow);
        // Over the crest, in the air: the surface's own motion, unfaded.
        let over = s.over(DVec3::new(10.0, 3.0, 0.0), lay, 0.0);
        assert!((over.flow.x - omega).abs() < 1e-3 && over.above > 0.0, "{over:?}");
    }

    /// **The swell's own sine, cosine and fade are the real ones.** They are
    /// worked out by fixed polynomials so every machine gets the same bits.
    #[test]
    fn the_fixed_maths_is_the_maths() {
        for i in -4000..4000 {
            let theta = i as f64 * 0.0137 + (i as f64 * 0.731).sin() * 300.0;
            let (s, c) = sin_cos(theta);
            assert!((s - theta.sin()).abs() < 1e-9 && (c - theta.cos()).abs() < 1e-9, "at {theta}: {s} {c}");
        }
        assert_eq!(sin_cos(0.0), (0.0, 1.0));
        for i in 0..3600 {
            let x = -(i as f64) * 0.011;
            assert!((exp_neg(x) - x.exp()).abs() <= 1e-5 * x.exp(), "at {x}: {} against {}", exp_neg(x), x.exp());
        }
        assert_eq!(exp_neg(0.0), 1.0);
        assert_eq!(exp_neg(-1000.0), 0.0);
    }

    /// **No waves is no swell.**
    #[test]
    fn no_waves_is_no_swell() {
        assert!(Swell::new(&[], DVec3::Y).is_none());
    }
}
