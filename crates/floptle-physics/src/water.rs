//! Water as a composable scalar field `depth(p)`, built the way gravity is: a
//! body asks the world one question and gets one answer, whether the water it
//! is in is a planet's ocean, a lake or a fish tank.
//!
//! A volume answers a depth: metres below the surface, zero anywhere outside.
//! How much of a hull is under, how hard the water pushes back and whether the
//! camera is wet all derive from that one number, so there is one definition
//! of "in the water".
//!
//! Buoyancy is per shape, not per body. A hull that lands flat floats; the same
//! hull nose-down sinks its nose and rights itself. That difference is about
//! where the displaced volume is, so the force is applied at each shape's own
//! position and the compound's inertia tensor turns the asymmetry into torque.
//! One buoyant force at the centre of mass gives a craft that bobs but never
//! rights itself.
//!
//! Every value here is f32 arithmetic over a volume list built from the scene
//! in a fixed order, with no iteration-order-dependent accumulation and no
//! state carried between steps. A water volume is as static as a gravity
//! source, so `Sim::step_body_tick` stays bit-for-bit exact and rollback
//! resimulation is untouched.

use std::sync::Arc;

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::water::DeclaredWater;

/// The shape of a body of water.
#[derive(Clone, Debug, PartialEq)]
pub enum WaterShape {
    /// A planet's sea: everything within `radius` of `center` is submerged, and
    /// depth is measured along the radius. The sea's "up" is different at every
    /// point on it, which is the whole reason this is not a plane with a big
    /// number in it.
    Sphere { center: Vec3, radius: f32 },
    /// A lake, a tank, a flooded room: an oriented box. Depth is measured from
    /// its top face, so the surface is flat and the sides are walls — a pool
    /// does not spill sideways.
    Box { center: Vec3, half: Vec3, rot: Quat },
    /// Water a script declared (`water.addVolume`): a lake fitted to its
    /// basin on a round world, or a grid of levels, with its current. The
    /// shape is the one `water.at` answers from, evaluated in f64 world space,
    /// so the solver and the script cannot disagree about where its surface
    /// is. `origin` is the sim frame's, which every other shape has folded in.
    Declared { water: Arc<DeclaredWater>, origin: DVec3 },
}

/// One body of water in the sim.
#[derive(Clone, Debug)]
pub struct WaterVolume {
    pub shape: WaterShape,
    /// Density, kg/m³ (fresh water ≈ 1000). What decides whether a thing floats
    /// is this against the body's own density, so a lead ball still sinks in a
    /// sea and a wooden crate still bobs in a bath.
    pub density: f32,
    /// Quadratic drag coefficient. Quadratic on purpose: it is what makes a
    /// hull that touches down gently float and the same hull at 60 m/s stop
    /// hard, without either case being special-cased.
    pub drag: f32,
    /// Angular drag coefficient — what stops a dropped craft spinning forever
    /// underwater.
    pub angular_drag: f32,
    /// A frozen sea is not a fluid: it applies no buoyancy and no drag, and the
    /// scene is expected to carry a collider for its surface instead. Freezing
    /// is a state rather than a second system, so an ice world's sea is the
    /// same node with a flag flipped — a script can thaw it.
    pub frozen: bool,
    /// The node this volume belongs to (what a query names, and what the
    /// renderer tints by).
    pub entity: u32,
}

impl WaterVolume {
    /// Metres below this volume's surface at sim-frame point `p`; `0.0`
    /// anywhere outside it. Never negative — "how far above the water am I" is
    /// a different question, and a signed answer here would silently make
    /// every `depth > 0.0` test wrong for a body in the air.
    pub fn depth_at(&self, p: Vec3) -> f32 {
        if self.frozen {
            return 0.0;
        }
        match &self.shape {
            WaterShape::Sphere { center, radius } => (radius - (p - *center).length()).max(0.0),
            WaterShape::Box { center, half, rot } => {
                let local = rot.inverse() * (p - *center);
                // Outside the footprint at all → not in this lake, whatever the
                // height says. (A pool's sides are walls, not a level set.)
                if local.x.abs() > half.x || local.z.abs() > half.z || local.y < -half.y {
                    return 0.0;
                }
                (half.y - local.y).max(0.0)
            }
            WaterShape::Declared { water, origin } => water.at(*origin + p.as_dvec3()).depth as f32,
        }
    }

    /// The water's own velocity at `p`, m/s in the sim frame: a river's
    /// current, zero in still water and out of it. Drag acts on the velocity
    /// relative to this, so a current carries what floats in it.
    pub fn flow_at(&self, p: Vec3) -> Vec3 {
        match &self.shape {
            WaterShape::Declared { water, origin } if !self.frozen => water.at(*origin + p.as_dvec3()).flow.as_vec3(),
            _ => Vec3::ZERO,
        }
    }

    /// The direction "up" out of the water at `p` — radially outward for a sea,
    /// the box's own +Y for a lake. Buoyancy pushes along this.
    pub fn up_at(&self, p: Vec3) -> Vec3 {
        match &self.shape {
            WaterShape::Sphere { center, .. } => (p - *center).try_normalize().unwrap_or(Vec3::Y),
            WaterShape::Box { rot, .. } => *rot * Vec3::Y,
            WaterShape::Declared { water, origin } => water.at(*origin + p.as_dvec3()).up.as_vec3(),
        }
    }

    /// Signed metres above this volume's surface at `p` (infinite where it
    /// has no surface over `p`).
    fn height_above(&self, p: Vec3) -> f32 {
        match &self.shape {
            WaterShape::Sphere { center, radius } => (p - *center).length() - radius,
            WaterShape::Box { center, half, rot } => {
                let local = rot.inverse() * (p - *center);
                if local.x.abs() > half.x || local.z.abs() > half.z {
                    f32::INFINITY
                } else {
                    local.y - half.y
                }
            }
            WaterShape::Declared { water, origin } => water.at(*origin + p.as_dvec3()).above as f32,
        }
    }
}

/// Every body of water in the sim, sampled together.
#[derive(Default, Clone)]
pub struct WaterField {
    pub volumes: Vec<WaterVolume>,
}

/// What a point in the world is doing about water.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WaterSample {
    /// Metres below the surface (0 = dry).
    pub depth: f32,
    /// Out of the water — radial on a sea, +Y in a lake.
    pub up: Vec3,
    /// The volume's density, kg/m³ (0 when dry).
    pub density: f32,
    /// The node whose volume this is (`None` when dry).
    pub entity: Option<u32>,
    /// The water's velocity there, m/s (zero in still water).
    pub flow: Vec3,
}

impl WaterField {
    /// The deepest volume containing `p`, and how deep. Deepest rather than
    /// first so a tank sitting inside an ocean answers as the tank — the same
    /// "innermost wins" rule patched conics use for gravity, and for the same
    /// reason: overlapping volumes must not depend on scene order.
    pub fn sample(&self, p: Vec3) -> WaterSample {
        let mut best = WaterSample::default();
        for v in &self.volumes {
            let d = v.depth_at(p);
            if d > best.depth {
                best = WaterSample {
                    depth: d,
                    up: v.up_at(p),
                    density: v.density,
                    entity: Some(v.entity),
                    flow: v.flow_at(p),
                };
            }
        }
        best
    }

    /// Metres below the surface at `p` — the one number everything else is
    /// derived from. `0.0` in air.
    pub fn depth_at(&self, p: Vec3) -> f32 {
        self.sample(p).depth
    }

    pub fn is_empty(&self) -> bool {
        self.volumes.is_empty()
    }

    /// The fraction (0..1) of a sphere of `radius` centred at `p` that is under
    /// the surface, as the exact spherical-cap volume rather than a step at the
    /// centre.
    ///
    /// This is what stops a floating hull juddering. A binary in/out test makes
    /// buoyancy switch fully on and fully off across one tick as the waterline
    /// crosses each shape's centre, and a craft at rest on a sea then vibrates
    /// at the tick rate forever. The cap fraction is continuous, so a body
    /// settles at the depth where buoyancy equals weight and stays there.
    pub fn submersion(&self, p: Vec3, radius: f32) -> f32 {
        let r = radius.max(1e-4);
        let d = self.depth_at(p);
        // Fully dry: the centre is above water and the sphere's top half can't
        // reach down to it.
        if d <= 0.0 {
            // `depth_at` clamps at zero, so the only way to know how far above
            // the surface the centre is, is to ask each volume for its own
            // signed answer. Cheap, and only on the dry path.
            let above = self.height_above(p);
            if above >= r {
                return 0.0;
            }
            // Cap of height (r - above) submerged.
            return cap_fraction(r, r - above);
        }
        if d >= r {
            return 1.0;
        }
        // Centre is under: everything except the dry cap of height (r - d).
        1.0 - cap_fraction(r, r - d)
    }

    /// How far above the nearest surface `p` is (0 when submerged). The signed
    /// other half of `depth_at`, split out because a positive-only depth is the
    /// right default everywhere else.
    pub fn height_above(&self, p: Vec3) -> f32 {
        let mut best = f32::INFINITY;
        for v in &self.volumes {
            if v.frozen {
                continue;
            }
            let h = v.height_above(p);
            if h < best {
                best = h;
            }
        }
        best.max(0.0)
    }
}

/// The fraction of a sphere of radius `r` occupied by a cap of height `h`
/// (0 ≤ h ≤ 2r): `V_cap / V_sphere = h²(3r − h) / (4r³)`.
fn cap_fraction(r: f32, h: f32) -> f32 {
    let h = h.clamp(0.0, 2.0 * r);
    (h * h * (3.0 * r - h) / (4.0 * r * r * r)).clamp(0.0, 1.0)
}

/// How many times denser the water is than a body of `radius` and `mass`
/// whose centre is at `p`, with the volume it is in: `(volume entity,
/// water density, ratio)`. `None` when the centre is dry.
///
/// Above 1 it floats, below 1 it sinks. A ratio in the hundreds is not a light
/// body; it is a volume whose `density` was written in different mass units
/// from the body's (a sea at 1025 kg/m³ in a game whose masses are tonnes).
pub fn density_ratio(field: &WaterField, p: Vec3, radius: f32, mass: f32) -> Option<(u32, f32, f32)> {
    let s = field.sample(p);
    let entity = s.entity?;
    let r = radius.max(1e-4);
    let volume = 4.0 / 3.0 * core::f32::consts::PI * r * r * r;
    Some((entity, s.density, s.density * volume / mass.max(1e-4)))
}

/// The acceleration water applies to a sphere of `radius` and `mass` moving at
/// `vel`, centred at `p`: buoyancy up, quadratic drag against the motion.
///
/// Returned as an acceleration rather than a force so the caller can apply it
/// the way it applies gravity, and returns `None` when the sphere is dry — the
/// hot path for every body in a scene that has water somewhere in it.
///
/// `g` is the local gravity, because buoyancy is *displaced weight*: on a
/// planet whose gravity varies with altitude, so does the push, and passing it
/// in is what keeps the two consistent instead of assuming 9.81.
pub fn buoyancy_accel(
    field: &WaterField,
    p: Vec3,
    radius: f32,
    mass: f32,
    vel: Vec3,
    g: Vec3,
    dt: f32,
) -> Option<Vec3> {
    if field.is_empty() {
        return None;
    }
    let f = field.submersion(p, radius);
    if f <= 0.0 {
        return None;
    }
    let s = field.sample(p);
    // A shape whose centre is above water still has a wet cap, and `sample`
    // reports dry there — take the nearest volume's properties instead of
    // giving a half-submerged hull no push at all.
    let (density, drag, ang, up, flow) = match s.entity {
        Some(_) => {
            let v = field.volumes.iter().find(|v| Some(v.entity) == s.entity).unwrap_or(&field.volumes[0]);
            (v.density, v.drag, v.angular_drag, s.up, s.flow)
        }
        None => {
            let v = nearest_volume(field, p)?;
            // A wet cap above the surface rides the current just under it.
            let below = p - v.up_at(p) * radius.max(1e-4);
            (v.density, v.drag, v.angular_drag, v.up_at(p), v.flow_at(below))
        }
    };
    let _ = ang;
    let mass = mass.max(1e-4);
    let r = radius.max(1e-4);
    let volume = 4.0 / 3.0 * core::f32::consts::PI * r * r * r * f;
    // Archimedes: the weight of the fluid displaced, pushed along the water's
    // own up (not −gravity — on a sea they agree, in a tilted tank they do not).
    let buoyant = up * (density * volume * g.length()) / mass;
    // Quadratic drag over the wet cross-section, on the velocity relative to
    // the water, so a current carries a floating crate downstream. Capped at
    // the acceleration that would bring the body exactly to the water's
    // speed this step: an explicit quadratic term is unstable at high speed
    // and would otherwise fling a fast-entering hull back out harder than it
    // arrived.
    let rel = vel - flow;
    let speed = rel.length();
    let drag_acc = if speed > 1e-5 {
        let area = core::f32::consts::PI * r * r * f;
        let mag = (0.5 * density * drag * area * speed * speed) / mass;
        let max = speed / dt.max(1e-5);
        -rel / speed * mag.min(max)
    } else {
        Vec3::ZERO
    };
    Some(buoyant + drag_acc)
}

fn nearest_volume(field: &WaterField, p: Vec3) -> Option<&WaterVolume> {
    let distance = |v: &WaterVolume| match &v.shape {
        WaterShape::Sphere { center, radius } => ((p - *center).length() - radius).abs(),
        WaterShape::Box { center, .. } => (p - *center).length(),
        WaterShape::Declared { .. } => v.height_above(p).abs(),
    };
    field.volumes.iter().filter(|v| !v.frozen).min_by(|a, b| distance(a).total_cmp(&distance(b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sea(radius: f32) -> WaterField {
        WaterField {
            volumes: vec![WaterVolume {
                shape: WaterShape::Sphere { center: Vec3::ZERO, radius },
                density: 1000.0,
                drag: 1.0,
                angular_drag: 1.0,
                frozen: false,
                entity: 7,
            }],
        }
    }

    fn lake() -> WaterField {
        WaterField {
            volumes: vec![WaterVolume {
                shape: WaterShape::Box {
                    center: Vec3::new(0.0, 0.0, 0.0),
                    half: Vec3::new(10.0, 5.0, 10.0),
                    rot: Quat::IDENTITY,
                },
                density: 1000.0,
                drag: 1.0,
                angular_drag: 1.0,
                frozen: false,
                entity: 3,
            }],
        }
    }

    /// Depth is measured from the surface and never goes negative — a body in
    /// the air must not read as "shallowly submerged", which is what a signed
    /// answer would do to every `depth > 0` test in a game.
    #[test]
    fn depth_is_zero_outside_and_measured_from_the_surface() {
        let f = sea(100.0);
        assert_eq!(f.depth_at(Vec3::new(0.0, 150.0, 0.0)), 0.0, "well above");
        assert_eq!(f.depth_at(Vec3::new(0.0, 100.0, 0.0)), 0.0, "exactly at the surface");
        assert!((f.depth_at(Vec3::new(0.0, 90.0, 0.0)) - 10.0).abs() < 1e-3, "10 m down");
        assert!((f.depth_at(Vec3::ZERO) - 100.0).abs() < 1e-3, "the seabed centre");
    }

    /// A lake's sides are walls. Standing next to a pool at the same height as
    /// its water is not standing in it — a level-set answer would drown the
    /// whole map at the waterline.
    #[test]
    fn a_lake_has_edges_not_just_a_level() {
        let f = lake();
        assert!(f.depth_at(Vec3::new(0.0, 0.0, 0.0)) > 0.0, "in the middle");
        assert_eq!(f.depth_at(Vec3::new(40.0, 0.0, 0.0)), 0.0, "beside it, same height");
        assert_eq!(f.depth_at(Vec3::new(0.0, 9.0, 0.0)), 0.0, "above the surface");
        assert_eq!(f.depth_at(Vec3::new(0.0, -9.0, 0.0)), 0.0, "below the bottom");
    }

    /// Submersion is continuous across the waterline. A step function makes
    /// buoyancy switch fully on and off within one tick as a shape's centre
    /// crosses the surface, and a craft at rest then vibrates at the tick rate
    /// forever — the bug this whole spherical-cap term exists to avoid.
    #[test]
    fn submersion_crosses_the_waterline_smoothly() {
        let f = sea(100.0);
        let r = 2.0;
        let at = |y: f32| f.submersion(Vec3::new(0.0, y, 0.0), r);
        assert_eq!(at(103.0), 0.0, "clear of the water");
        assert_eq!(at(97.0), 1.0, "fully under");
        assert!((at(100.0) - 0.5).abs() < 0.02, "half in at the surface, got {}", at(100.0));

        // …and monotone, with no jump bigger than a smooth curve would make.
        let mut prev = 0.0;
        let mut max_jump: f32 = 0.0;
        for i in 0..=40 {
            let y = 103.0 - i as f32 * 0.15;
            let s = at(y);
            assert!(s >= prev - 1e-6, "submersion must not decrease going down");
            max_jump = max_jump.max(s - prev);
            prev = s;
        }
        assert!(max_jump < 0.12, "a step, not a ramp: biggest jump was {max_jump}");
    }

    /// A hull that lands flat floats.
    /// Integrate a light sphere dropped into a sea and it must settle at a
    /// steady depth rather than sinking or being spat out.
    #[test]
    fn a_light_body_settles_at_a_waterline_instead_of_sinking() {
        let f = sea(100.0);
        let g = Vec3::new(0.0, -9.81, 0.0);
        let (r, dt) = (1.0, 1.0 / 60.0);
        // Density 400 kg/m³ — driftwood. Mass = ρ·V.
        let mass = 400.0 * 4.0 / 3.0 * core::f32::consts::PI * r * r * r;
        let mut p = Vec3::new(0.0, 104.0, 0.0);
        let mut v = Vec3::ZERO;
        for _ in 0..1500 {
            let a = g + buoyancy_accel(&f, p, r, mass, v, g, dt).unwrap_or(Vec3::ZERO);
            v += a * dt;
            p += v * dt;
        }
        let depth = 100.0 - p.y;
        assert!(v.length() < 0.2, "it should have settled, still moving at {}", v.length());
        // Floating at ρ_body/ρ_water = 0.4 submerged: the centre sits slightly
        // below the surface, but the body is nowhere near the seabed.
        assert!(depth > -r && depth < r, "should straddle the waterline, sits at depth {depth}");
        assert!(p.y > 95.0, "it sank: {}", p.y);
    }

    /// A script's lake on a round world, 805 m from the planet's centre and 60 m
    /// across, flowing east at `current` m/s.
    fn planet_lake(current: Vec3) -> WaterField {
        use floptle_core::water::{DeclaredWater, Surface};
        let water = DeclaredWater {
            id: 1,
            anchor: None,
            frame: floptle_core::scatter::Frame::IDENTITY,
            surface: Surface::Lake { radius: 805.0, axis: DVec3::Y, reach: 60.0 },
            density: 1000.0,
            drag: 1.0,
            angular_drag: 1.0,
            current,
            floor: f64::INFINITY,
            look: None,
            simulated: None,
        };
        WaterField {
            volumes: vec![WaterVolume {
                shape: WaterShape::Declared { water: Arc::new(water), origin: DVec3::new(0.0, 800.0, 0.0) },
                density: 1000.0,
                drag: 1.0,
                angular_drag: 1.0,
                frozen: false,
                entity: 9,
            }],
        }
    }

    /// **A crate floats on a script's lake, at the lake's own level.** Its
    /// surface is part of the planet's sphere, so the crate settles where the
    /// script's `water.at` says the surface is, in the sim's frame.
    #[test]
    fn a_crate_floats_on_a_declared_lake() {
        let f = planet_lake(Vec3::ZERO);
        let g = Vec3::new(0.0, -9.81, 0.0);
        let (r, dt) = (0.5, 1.0 / 60.0);
        let mass = 400.0 * 4.0 / 3.0 * core::f32::consts::PI * r * r * r;
        // The sim frame sits 800 m up the planet's +Y: the surface is at y = 5.
        let (mut p, mut v) = (Vec3::new(0.0, 7.0, 0.0), Vec3::ZERO);
        for _ in 0..1500 {
            v += (g + buoyancy_accel(&f, p, r, mass, v, g, dt).unwrap_or(Vec3::ZERO)) * dt;
            p += v * dt;
        }
        assert!(v.length() < 0.1, "still moving at {}", v.length());
        assert!((p.y - 5.0).abs() < r, "floats at {} against a surface at 5", p.y);
        // Past the farthest shore it is dry, however low.
        assert_eq!(f.depth_at(Vec3::new(80.0, 0.0, 0.0)), 0.0);
    }

    /// **A current carries what floats in it.** Drag acts on the velocity
    /// relative to the water, so a 1 kg crate dropped still into a 1 m/s river
    /// is going nearly 1 m/s downstream within five seconds. Against still water
    /// the same drag would only ever slow it down.
    #[test]
    fn a_current_carries_a_floating_crate_downstream() {
        let f = planet_lake(Vec3::new(1.0, 0.0, 0.0));
        let dt = 1.0 / 60.0;
        let (mass, r) = (1.0, 0.1);
        let (mut p, mut v) = (Vec3::new(0.0, 5.0, 0.0), Vec3::ZERO);
        for _ in 0..300 {
            // The planet's own gravity, toward its centre 800 m below.
            let g = -(p + Vec3::new(0.0, 800.0, 0.0)).normalize() * 9.81;
            v += (g + buoyancy_accel(&f, p, r, mass, v, g, dt).unwrap_or(Vec3::ZERO)) * dt;
            p += v * dt;
        }
        assert!(v.x >= 0.8, "only {} m/s downstream after 5 s", v.x);
        assert!(v.x <= 1.0 + 1e-3, "faster than the water: {}", v.x);
        assert_eq!(f.sample(Vec3::new(0.0, 4.0, 0.0)).flow, Vec3::new(1.0, 0.0, 0.0));
    }

    /// …and the other half of the same line: a dense body sinks. Buoyancy that
    /// floats everything is just an upward force.
    #[test]
    fn a_dense_body_still_sinks() {
        let f = sea(100.0);
        let g = Vec3::new(0.0, -9.81, 0.0);
        let (r, dt) = (1.0, 1.0 / 60.0);
        let mass = 8000.0 * 4.0 / 3.0 * core::f32::consts::PI * r * r * r; // steel
        let mut p = Vec3::new(0.0, 99.0, 0.0);
        let mut v = Vec3::ZERO;
        for _ in 0..600 {
            let a = g + buoyancy_accel(&f, p, r, mass, v, g, dt).unwrap_or(Vec3::ZERO);
            v += a * dt;
            p += v * dt;
        }
        assert!(p.y < 95.0, "steel should be well down by now, at {}", p.y);
    }

    /// Drag is capped at the acceleration that brings the body to rest this
    /// step. Without the cap an explicit quadratic term overshoots at high
    /// speed and spits a fast-entering hull back out faster than it arrived —
    /// energy from nowhere, and it looks like the sea is a trampoline.
    #[test]
    fn fast_entry_is_stopped_hard_but_never_reversed() {
        let f = sea(100.0);
        let g = Vec3::new(0.0, -9.81, 0.0);
        let (r, dt) = (1.0, 1.0 / 60.0);
        let mass = 500.0;
        let mut p = Vec3::new(0.0, 99.0, 0.0);
        let mut v = Vec3::new(0.0, -60.0, 0.0); // splashdown
        for _ in 0..240 {
            let a = g + buoyancy_accel(&f, p, r, mass, v, g, dt).unwrap_or(Vec3::ZERO);
            v += a * dt;
            p += v * dt;
            assert!(v.y < 30.0, "the water threw it back out at {} m/s", v.y);
        }
        assert!(v.length() < 60.0, "60 m/s should not survive the water");
    }

    /// A frozen sea is not a fluid. Nothing floats in it and nothing is dragged
    /// by it — the scene carries a collider for the surface instead, which is
    /// what makes an ice world walkable rather than a place you fall through.
    #[test]
    fn a_frozen_sea_applies_nothing() {
        let mut f = sea(100.0);
        f.volumes[0].frozen = true;
        assert_eq!(f.depth_at(Vec3::new(0.0, 50.0, 0.0)), 0.0, "no depth inside a frozen sea");
        let a = buoyancy_accel(
            &f,
            Vec3::new(0.0, 50.0, 0.0),
            1.0,
            100.0,
            Vec3::ZERO,
            Vec3::new(0.0, -9.81, 0.0),
            1.0 / 60.0,
        );
        assert!(a.is_none(), "frozen water must not push");
    }

    /// Overlapping volumes resolve by depth, not scene order — a tank inside an
    /// ocean answers as the tank. Order-dependence here would make a scene's
    /// physics depend on the order nodes happen to be listed in a file.
    #[test]
    fn the_deepest_volume_wins_regardless_of_order() {
        let tank = WaterVolume {
            shape: WaterShape::Box {
                center: Vec3::new(0.0, 0.0, 0.0),
                half: Vec3::splat(2.0),
                rot: Quat::IDENTITY,
            },
            density: 1300.0,
            drag: 1.0,
            angular_drag: 1.0,
            frozen: false,
            entity: 42,
        };
        let ocean = WaterVolume {
            shape: WaterShape::Sphere { center: Vec3::ZERO, radius: 3.0 },
            density: 1000.0,
            drag: 1.0,
            angular_drag: 1.0,
            frozen: false,
            entity: 9,
        };
        let p = Vec3::new(0.0, -1.0, 0.0);
        let a = WaterField { volumes: vec![tank.clone(), ocean.clone()] };
        let b = WaterField { volumes: vec![ocean, tank] };
        assert_eq!(a.sample(p).entity, b.sample(p).entity, "order changed the answer");
        assert_eq!(a.sample(p).density, 1300.0, "the deeper (tank) volume should win");
    }
}
