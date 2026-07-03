//! The authored particle-effect model and its compiled (LUT-baked) form.
//!
//! `ParticleEffect` is what the editor edits and RON round-trips (via the DTOs in
//! `floptle-scene`): tracks with clips, bursts, automation lanes, and value-or-curve
//! properties. `CompiledEffect` is what the sim runs: the same structure with every
//! curve baked to a LUT and derived values (capacities, folded lanes) precomputed.
//! Editors recompile on edit; the split keeps the hot loop branch-light and gives
//! the future GPU backend upload-ready data (proposal §4.4).

use crate::curve::{Prop1, Prop4, Value, ValueOrCurve, bake1, bake4};
use floptle_core::math::Vec3;

/// How an effect behaves when its lifetime elapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EndBehavior {
    /// One-shot effect that despawns itself once the last particle dies.
    #[default]
    Destroy,
    /// One-shot effect that persists (frozen) after its lifetime.
    Persist,
}

/// Whether the timeline wraps at `lifetime` or plays once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Playback {
    Looping,
    #[default]
    OneShot,
}

/// Alpha compositing mode for a track's particles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Blend {
    /// Classic transparency — depth-sorted back-to-front at draw time.
    #[default]
    Alpha,
    /// Light-accumulating (fire, sparks, glow) — order-independent, drawn unsorted.
    Additive,
}

/// How a particle is drawn.
#[derive(Debug, Clone, PartialEq)]
pub enum RenderMode {
    /// A camera-facing textured quad. `None` texture = plain tinted quad.
    Billboard { texture: Option<String> },
    /// An instanced mesh drawn through the raster pass (phase 4).
    Mesh { asset_path: String },
}

impl Default for RenderMode {
    fn default() -> Self {
        Self::Billboard { texture: None }
    }
}

/// Where particles are born, and the emit direction their velocity frame aligns to.
/// Convention: the velocity value's +Y is "along the emit direction" (a cone tilts
/// it, a sphere points it radially); X/Z are lateral. `Point` emits straight +Y.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum EmitShape {
    #[default]
    Point,
    /// Spread within `angle` degrees of +Y, born on a disc of `radius` in XZ.
    Cone { angle: f32, radius: f32 },
    /// Born inside (or on, if `shell`) a sphere; emit direction is radial.
    Sphere { radius: f32, shell: bool },
    /// A line along X of `length` — slash arcs. Emit direction +Z.
    Edge { length: f32 },
    /// A circle of `radius` in XZ; emit direction is radially outward.
    Ring { radius: f32 },
}

/// The simulation space of a track's particles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Space {
    /// Particles ride the emitter node (attached fire follows the torch).
    #[default]
    Local,
    /// Particles anchor where they were born (trails stay behind) — phase 4.
    World,
}

/// A ranged emission span on the timeline — the draggable clip. While the playhead
/// is inside, the track emits at its rate. Multiple clips = start, stop, start again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clip {
    pub start: f32,
    pub end: f32,
}

/// A hand-placed instant emit — the draggable diamond.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Burst {
    pub t: f32,
    pub count: u32,
}

/// What an automation lane modulates. Lanes curve over EFFECT time and shape what a
/// particle is *born* as; life curves shape how it *ages* (proposal §2, the one rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneTarget {
    /// Multiplies `Track::rate` — swells, ramps.
    Rate,
    /// Multiplies burst counts.
    Count,
    /// Multiplies birth velocity magnitude.
    Speed,
    /// Multiplies birth size.
    Size,
    /// Multiplies birth color (Rgba lane — gradient strip in the editor).
    Tint,
    /// Scales the emit shape (a cone widening over the effect).
    ShapeScale,
}

/// A DAW-style automation lane: one curve over effect time targeting a birth
/// parameter. Keys are authored in SECONDS along the timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Lane {
    pub target: LaneTarget,
    pub curve: crate::curve::Curve,
}

/// The rendered look of a track. Lighting and shadow casting are per-track opt-ins,
/// both OFF by default (classic crisp VFX costs nothing until asked).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Look {
    pub render: RenderMode,
    pub blend: Blend,
    /// Full scene lighting per particle: sun + point lights + field shadow + AO.
    pub lit: bool,
    /// The track's live cloud casts into the field shadow march (aggregate proxy).
    pub cast_shadows: bool,
}

/// One visual layer AND its timeline lane — the unit you select, drag, mute, copy.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub name: String,
    pub enabled: bool,
    pub look: Look,
    pub space: Space,

    // Timeline content (effect-time domain).
    pub clips: Vec<Clip>,
    pub bursts: Vec<Burst>,
    pub automation: Vec<Lane>,

    // Emission.
    pub rate: f32,
    pub shape: EmitShape,
    pub particle_lifetime: f32,
    /// `0..1` symmetric variance on each particle's lifetime.
    pub lifetime_jitter: f32,
    /// Pool capacity override; derived from rate/bursts/lifetime when `None`.
    pub max_alive: Option<u32>,

    // Per-particle properties: birth value × curve over the particle's life [0..1].
    /// Emitter-space birth velocity; +Y means "along the emit direction" (see
    /// [`EmitShape`]). A curve makes velocity kinematic over the particle's life.
    pub velocity: ValueOrCurve,
    pub size: ValueOrCurve,
    /// Billboard spin in radians (a 0→2π curve is one revolution per lifetime).
    pub rotation: ValueOrCurve,
    pub color: ValueOrCurve,
    /// 0 = weightless, 1 = full gravity.
    pub gravity: f32,
    pub drag: f32,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            name: "Track".into(),
            enabled: true,
            look: Look::default(),
            space: Space::default(),
            clips: Vec::new(),
            bursts: Vec::new(),
            automation: Vec::new(),
            rate: 10.0,
            shape: EmitShape::default(),
            particle_lifetime: 1.0,
            lifetime_jitter: 0.0,
            max_alive: None,
            velocity: ValueOrCurve::Const(Value::Vec3(Vec3::new(0.0, 1.0, 0.0))),
            size: ValueOrCurve::constant(0.25),
            rotation: ValueOrCurve::constant(0.0),
            color: ValueOrCurve::Const(Value::Rgba([1.0; 4])),
            gravity: 0.0,
            drag: 0.0,
        }
    }
}

/// The reusable, named effect the designer spawns — a lifetime plus tracks.
#[derive(Debug, Clone, PartialEq)]
pub struct ParticleEffect {
    pub name: String,
    /// Seconds the timeline runs (one loop period for `Looping`).
    pub lifetime: f32,
    pub playback: Playback,
    /// OneShot only; hidden in the UI for `Looping`.
    pub end: EndBehavior,
    pub tracks: Vec<Track>,
    /// Base seed; instances offset it so two campfires don't march in lockstep.
    pub seed: u32,
}

impl Default for ParticleEffect {
    fn default() -> Self {
        Self {
            name: "Effect".into(),
            lifetime: 1.0,
            playback: Playback::default(),
            end: EndBehavior::default(),
            tracks: Vec::new(),
            seed: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Compiled form
// ---------------------------------------------------------------------------

/// A track with every curve baked and derived values precomputed — what the sim runs.
#[derive(Debug, Clone)]
pub struct CompiledTrack {
    pub name: String,
    pub enabled: bool,
    pub look: Look,
    pub space: Space,

    pub clips: Vec<Clip>,
    pub bursts: Vec<Burst>,

    pub rate: f32,
    pub shape: EmitShape,
    pub particle_lifetime: f32,
    pub lifetime_jitter: f32,
    /// Hard cap on live particles (derived or authored).
    pub capacity: u32,

    // Life-curve LUTs, domain = particle life [0..1].
    pub velocity: Prop4,
    /// True when `velocity` was authored as a curve (kinematic velocity-over-life).
    pub velocity_is_curve: bool,
    pub size: Prop1,
    pub rotation: Prop1,
    pub color: Prop4,
    pub gravity: f32,
    pub drag: f32,

    // Automation lanes folded per target, domain = effect time normalized [0..1].
    pub lane_rate: Prop1,
    pub lane_count: Prop1,
    pub lane_speed: Prop1,
    pub lane_size: Prop1,
    pub lane_tint: Prop4,
    pub lane_shape: Prop1,
}

/// A compiled effect: share via `Arc` across every live instance.
#[derive(Debug, Clone)]
pub struct CompiledEffect {
    pub name: String,
    pub lifetime: f32,
    pub playback: Playback,
    pub end: EndBehavior,
    pub seed: u32,
    pub tracks: Vec<CompiledTrack>,
}

/// Fold every lane targeting `which` into one scalar multiplier LUT (lanes stack
/// multiplicatively; no lanes = constant 1, i.e. zero cost at sample time).
fn fold_lanes1(lanes: &[Lane], which: LaneTarget, lifetime: f32) -> Prop1 {
    let hits: Vec<&Lane> = lanes.iter().filter(|l| l.target == which).collect();
    if hits.is_empty() {
        return Prop1::Const(1.0);
    }
    let mut s = Box::new([1.0f32; crate::curve::LUT_N]);
    for lane in hits {
        let baked = bake1(&ValueOrCurve::Curve(lane.curve.clone()), lifetime);
        for (i, out) in s.iter_mut().enumerate() {
            *out *= baked.sample(i as f32 / (crate::curve::LUT_N - 1) as f32);
        }
    }
    Prop1::Lut(s)
}

/// Fold every `Tint` lane into one Rgba multiplier LUT.
fn fold_lanes_tint(lanes: &[Lane], lifetime: f32) -> Prop4 {
    let hits: Vec<&Lane> = lanes.iter().filter(|l| l.target == LaneTarget::Tint).collect();
    if hits.is_empty() {
        return Prop4::Const([1.0; 4]);
    }
    let mut s = Box::new([[1.0f32; 4]; crate::curve::LUT_N]);
    for lane in hits {
        let baked = bake4(&ValueOrCurve::Curve(lane.curve.clone()), lifetime);
        for (i, out) in s.iter_mut().enumerate() {
            let v = baked.sample(i as f32 / (crate::curve::LUT_N - 1) as f32);
            for c in 0..4 {
                out[c] *= v[c];
            }
        }
    }
    Prop4::Lut(s)
}

impl Track {
    /// Upper bound on simultaneously live particles: continuous emission fills
    /// `rate × lifetime`, plus every burst could still be alive at once.
    fn derive_capacity(&self) -> u32 {
        let life = self.particle_lifetime * (1.0 + self.lifetime_jitter);
        let clip_secs: f32 = self.clips.iter().map(|c| (c.end - c.start).max(0.0)).sum();
        let from_rate = (self.rate * life.min(clip_secs.max(life))).ceil() as u32;
        let from_bursts: u32 = self.bursts.iter().map(|b| b.count).sum();
        (from_rate + from_bursts).clamp(1, 65_536)
    }

    fn compile(&self, lifetime: f32) -> CompiledTrack {
        let mut clips = self.clips.clone();
        clips.sort_by(|a, b| a.start.total_cmp(&b.start));
        let mut bursts = self.bursts.clone();
        bursts.sort_by(|a, b| a.t.total_cmp(&b.t));
        CompiledTrack {
            name: self.name.clone(),
            enabled: self.enabled,
            look: self.look.clone(),
            space: self.space,
            clips,
            bursts,
            rate: self.rate.max(0.0),
            shape: self.shape,
            particle_lifetime: self.particle_lifetime.max(1e-3),
            lifetime_jitter: self.lifetime_jitter.clamp(0.0, 1.0),
            capacity: self.max_alive.unwrap_or_else(|| self.derive_capacity()).max(1),
            velocity: bake4(&self.velocity, 1.0),
            velocity_is_curve: matches!(self.velocity, ValueOrCurve::Curve(_)),
            size: bake1(&self.size, 1.0),
            rotation: bake1(&self.rotation, 1.0),
            color: bake4(&self.color, 1.0),
            gravity: self.gravity,
            drag: self.drag.max(0.0),
            lane_rate: fold_lanes1(&self.automation, LaneTarget::Rate, lifetime),
            lane_count: fold_lanes1(&self.automation, LaneTarget::Count, lifetime),
            lane_speed: fold_lanes1(&self.automation, LaneTarget::Speed, lifetime),
            lane_size: fold_lanes1(&self.automation, LaneTarget::Size, lifetime),
            lane_tint: fold_lanes_tint(&self.automation, lifetime),
            lane_shape: fold_lanes1(&self.automation, LaneTarget::ShapeScale, lifetime),
        }
    }
}

impl ParticleEffect {
    /// Bake every curve and derive capacities. Called on asset load and after edits.
    pub fn compile(&self) -> CompiledEffect {
        let lifetime = self.lifetime.max(1e-3);
        CompiledEffect {
            name: self.name.clone(),
            lifetime,
            playback: self.playback,
            end: self.end,
            seed: self.seed,
            tracks: self.tracks.iter().map(|t| t.compile(lifetime)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{Curve, Key};

    #[test]
    fn capacity_covers_rate_and_bursts() {
        let t = Track {
            rate: 20.0,
            particle_lifetime: 0.5,
            clips: vec![Clip { start: 0.0, end: 2.0 }],
            bursts: vec![Burst { t: 0.1, count: 12 }],
            ..Track::default()
        };
        // 20/s × 0.5 s alive = 10 continuous + 12 burst.
        assert_eq!(t.derive_capacity(), 22);
    }

    #[test]
    fn lanes_fold_multiplicatively() {
        let mk = |v0: f32, v1: f32| Lane {
            target: LaneTarget::Rate,
            curve: Curve {
                keys: vec![Key::new(0.0, Value::F32(v0)), Key::new(1.0, Value::F32(v1))],
                extrapolate: Default::default(),
            },
        };
        let t = Track { automation: vec![mk(2.0, 2.0), mk(3.0, 3.0)], ..Track::default() };
        let c = t.compile(1.0);
        assert!((c.lane_rate.sample(0.5) - 6.0).abs() < 1e-4);
        // Untouched targets stay free constant-1 multipliers.
        assert!(matches!(c.lane_speed, Prop1::Const(v) if v == 1.0));
    }
}
