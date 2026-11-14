//! Things entering and leaving water: one event per crossing, and the slam a
//! fast entry meets.
//!
//! The solver already knows each shape's submerged fraction every tick (it is
//! what buoyancy is made of). Once a tick, after the step, each body and each
//! part of an assembly is asked again: a shape whose fraction went from none
//! to some has entered, one whose fraction went back to none has left. A
//! crossing slower than [`SPLASH_SPEED`] along the water's up is a shape
//! bobbing at the surface and is not reported, so a floating crate does not
//! splash sixty times a second. What a game draws and plays for a splash, a
//! ripple, wet looks and impact damage all read the one list.

use std::collections::{HashMap, VecDeque};

use floptle_core::math::{DVec3, Vec3};

use crate::water::WaterField;

/// Crossings slower than this along the water's up, m/s, are bobbing, not
/// splashes.
pub const SPLASH_SPEED: f32 = 0.5;
/// Events kept for scripts to read: the oldest go first.
pub const SPLASH_RING: usize = 64;

/// One thing entering or leaving water.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Splash {
    /// The node of the body, or of the assembly's part, that crossed.
    pub node: u32,
    /// The water it crossed: a Water Volume node's index, or `u32::MAX - id`
    /// for water a script declared.
    pub water: u32,
    /// Where it crossed, on the surface, in the world.
    pub pos: DVec3,
    /// The water's up there.
    pub normal: Vec3,
    /// Its speed along `-normal`, m/s: positive going in, negative coming
    /// out.
    pub speed: f32,
    pub mass: f32,
    /// The face it met the water with, m².
    pub area: f32,
    /// The water it displaces after the step, m³.
    pub displaced: f32,
    pub entering: bool,
}

/// What the splash pass remembers between ticks.
#[derive(Default, Clone)]
pub struct Crossings {
    /// Shapes in water at the last tick, by node.
    wet: HashMap<u32, ()>,
    /// The tick clock when each node was last in water, s.
    pub last_wet: HashMap<u32, f64>,
    pub clock: f64,
    pub events: VecDeque<Splash>,
}

/// One shape asked about water: where it is, how big, how fast, how heavy.
pub struct Probe {
    pub node: u32,
    pub pos: Vec3,
    pub radius: f32,
    /// Its velocity now, and as the tick began: a crossing reports the
    /// speed it met the surface at, before the tick's water slowed it.
    pub vel: Vec3,
    pub was_vel: Vec3,
    pub mass: f32,
}

impl Crossings {
    /// Ask each probe about the water after a tick of `dt`; record its
    /// crossings and return, per probe, the velocity change its water's slam
    /// puts on it (zero for most).
    pub fn pass(&mut self, water: &WaterField, origin: DVec3, probes: &[Probe], dt: f32, slam: impl Fn(u32) -> f32) -> Vec<Vec3> {
        self.clock += dt as f64;
        let mut out = vec![Vec3::ZERO; probes.len()];
        if water.is_empty() && self.wet.is_empty() {
            return out;
        }
        let mut wet = HashMap::with_capacity(self.wet.len());
        for (k, p) in probes.iter().enumerate() {
            let f = water.submersion(p.pos, p.radius);
            let now = f > 0.0;
            let was = self.wet.contains_key(&p.node);
            if now {
                wet.insert(p.node, ());
                self.last_wet.insert(p.node, self.clock);
            }
            if !now && !was {
                continue;
            }
            let Some(v) = water.nearest(p.pos) else { continue };
            let up = v.up_at(p.pos);
            let into = -p.was_vel.dot(up);
            let area = core::f32::consts::PI * p.radius * p.radius;
            if now != was && into.abs() >= SPLASH_SPEED && (into > 0.0) == now {
                let depth = v.depth_at(p.pos);
                let above = if depth > 0.0 { -depth } else { water.height_above(p.pos) };
                let surface = p.pos - up * above;
                let volume = 4.0 / 3.0 * core::f32::consts::PI * p.radius * p.radius * p.radius;
                if self.events.len() >= SPLASH_RING {
                    self.events.pop_front();
                }
                self.events.push_back(Splash {
                    node: p.node,
                    water: v.entity,
                    pos: origin + surface.as_dvec3(),
                    normal: up,
                    speed: into,
                    mass: p.mass,
                    area,
                    displaced: f * volume,
                    entering: now,
                });
            }
            // The slam: a shape going in fast meets the water's inertia,
            // `k·ρ·v²·A·dt` against its motion while it is going under, and
            // never more than stops it.
            let hard = slam(v.entity);
            // A small, fast shape can go from dry to under in one tick.
            if now && (f < 1.0 || !was) && -p.vel.dot(up) > 0.0 && hard > 0.0 {
                let speed = p.vel.length();
                let dv = (hard * v.density * speed * speed * area * dt / p.mass.max(1e-4)).min(speed);
                out[k] = -p.vel / speed.max(1e-6) * dv;
            }
        }
        self.wet = wet;
        out
    }

    /// A tick with nothing to ask: the clock runs on.
    pub fn tick(&mut self, dt: f32) {
        self.clock += dt as f64;
    }

    /// Was anything in water at the last tick?
    pub fn any_wet(&self) -> bool {
        !self.wet.is_empty()
    }

    /// `node` is in water now (an assembly whose part is).
    pub fn mark_wet(&mut self, node: u32) {
        self.last_wet.insert(node, self.clock);
    }

    /// The clock, and every node in water at it.
    pub fn wet_now(&self) -> (f64, Vec<u32>) {
        (self.clock, self.last_wet.iter().filter(|(_, t)| **t == self.clock).map(|(n, _)| *n).collect())
    }

    /// Seconds since `node` was last in water; `None` if it never was.
    pub fn dry_for(&self, node: u32) -> Option<f64> {
        self.last_wet.get(&node).map(|t| self.clock - t)
    }
}
