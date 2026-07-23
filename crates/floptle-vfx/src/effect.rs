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

/// Compositing mode for a track's particles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Blend {
    /// Classic transparency — depth-sorted back-to-front at draw time.
    #[default]
    Alpha,
    /// Light-accumulating (fire, sparks, glow) — order-independent, drawn unsorted.
    Additive,
    /// Premultiplied-alpha over: glow that also occludes cleanly (order-dependent).
    Premultiplied,
    /// Screen — lightens toward white, order-independent (soft glows, light shafts).
    Screen,
    /// Multiply — darkens what's behind (smoke that occludes, stains; order-dependent).
    Multiply,
}

impl Blend {
    /// Order-dependent modes must be depth-sorted back-to-front; light-accumulating
    /// ones (additive / screen) composite the same in any order.
    pub fn needs_sort(self) -> bool {
        matches!(self, Blend::Alpha | Blend::Premultiplied | Blend::Multiply)
    }
}

/// How a particle is drawn.
#[derive(Debug, Clone, PartialEq)]
pub enum RenderMode {
    /// A textured quad. `None` texture = plain tinted quad. How the quad is
    /// oriented in the world is the track's [`Look::orient`] (default: face camera).
    Billboard { texture: Option<String> },
    /// An instanced mesh drawn through the raster pass.
    Mesh { asset_path: String },
}

impl Default for RenderMode {
    fn default() -> Self {
        Self::Billboard { texture: None }
    }
}

/// How a billboard quad is oriented in the world — the alignment of its plane.
/// Only meaningful for [`RenderMode::Billboard`]; meshes use their full 3D rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BillboardOrient {
    /// Always faces the camera (classic billboard). The `roll` rotation spins it
    /// in the screen plane.
    #[default]
    FaceCamera,
    /// Stretched along the particle's velocity (sprays, sparks, rain, speed lines).
    /// The quad still turns its flat side to the camera around the motion axis; its
    /// height scales with [`Look::stretch`]. Roll is ignored (motion defines up).
    Velocity,
    /// Upright: locked to the world up-axis, yawing to face the camera (flames,
    /// grass, upright smoke that shouldn't tip over when you look down on it).
    Vertical,
    /// Flat on the ground: the quad lies in the world's horizontal plane, its normal
    /// pointing up (decals, shockwaves, ripples, magic circles). `roll` spins it flat.
    Horizontal,
    /// Fixed to the particle's birth orientation (the emit direction) — debris and
    /// cards keep the pose they were fired with, independent of the camera.
    WorldFixed,
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

/// A steady force added to a track's particles' velocity each step. Directions and
/// centres are in the track's SIMULATION space — emitter-local for `Space::Local`,
/// anchor-relative for `Space::World` — so a `Point`/`Vortex` centre stays put
/// relative to the emitter and the whole thing is floating-origin-safe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Force {
    /// A constant push in a fixed direction (wind, updraft). `dir` need not be unit.
    Directional { dir: Vec3, strength: f32 },
    /// Pull toward (`strength > 0`) or push away from (`< 0`) a point (gravity well).
    Point { center: Vec3, strength: f32 },
    /// Swirl around an `axis` through `center` (tornado, whirlpool).
    Vortex { center: Vec3, axis: Vec3, strength: f32 },
    /// Smooth value-noise turbulence — a chaotic push that makes smoke/embers wander.
    /// `frequency` is the spatial scale (higher = finer), `strength` the magnitude.
    Turbulence { frequency: f32, strength: f32 },
}

/// The simulation space of a track's particles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Space {
    /// Particles ride the emitter node (attached fire follows the torch).
    #[default]
    Local,
    /// Particles anchor where they were born (trails stay behind), floating-origin-safe.
    World,
}

/// Where an effect's per-track `gravity` pull points — the dev-facing knob for
/// particles near worlds with non-down gravity. `WorldDown` is the classic
/// constant −Y pull (unchanged default). `Field` samples the LIVE scene gravity
/// field at the emitter each tick (radial "planet" volumes + celestial µ/r²), so
/// debris, dust and embers fall toward the ground beneath them instead of toward
/// world −Y. The host supplies the sampled vector; the per-track `gravity` scalar
/// still scales it, so a game tunes strength exactly as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GravityMode {
    #[default]
    WorldDown,
    Field,
}

/// How a [`Clip`] releases its particles across its span.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Emit {
    /// A continuous stream: `rate` particles/second across the whole clip.
    Rate { rate: f32 },
    /// A pulse train: `pulses` bursts, the first at the clip's start and each subsequent
    /// one `interval` seconds after the last (± `interval_jitter`), every burst spawning
    /// `count` particles (± `count_jitter`). `pulses = 1` is a single burst.
    Burst {
        count: u32,
        /// `0..1` fraction of random variance on each pulse's count.
        count_jitter: f32,
        /// Number of repeats (≥ 1).
        pulses: u32,
        /// Seconds between pulses.
        interval: f32,
        /// `0..1` fraction of random variance on each gap between pulses.
        interval_jitter: f32,
    },
}

impl Default for Emit {
    fn default() -> Self {
        Emit::Rate { rate: 10.0 }
    }
}

/// A ranged emission on the timeline — the draggable clip, and the unit of "one
/// emission". Its LENGTH is the lifetime of the particles it releases: each particle
/// lives `end - start` seconds (± `lifetime_jitter`). A clip either streams
/// ([`Emit::Rate`], particles born across the whole span) or fires burst pulses
/// ([`Emit::Burst`]); multiple clips on a track = emit, stop, emit again. This is why
/// there is no separate track-level rate or lifetime.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clip {
    pub start: f32,
    pub end: f32,
    /// `0..1` symmetric variance on each particle's lifetime (= the clip length).
    pub lifetime_jitter: f32,
    pub emit: Emit,
}

impl Clip {
    /// The lifetime of particles this clip releases — its length on the timeline,
    /// floored to a tiny positive value so a zero-width clip still spawns visibly.
    pub fn lifetime(&self) -> f32 {
        (self.end - self.start).max(1e-3)
    }
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
    /// Multiplies the billboard [`Look::aspect`] (width:height) over effect time —
    /// e.g. round sparks that stretch into streaks partway through the effect.
    Aspect,
}

/// A DAW-style automation lane: one curve over effect time targeting a birth
/// parameter. Keys are authored in SECONDS along the timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Lane {
    pub target: LaneTarget,
    pub curve: crate::curve::Curve,
}

/// How a flipbook advances through its frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlipMode {
    /// Play the whole sheet once across the particle's life (birth → death).
    #[default]
    OverLife,
    /// Loop at a fixed `fps`, wrapping — animated fire/smoke sprites.
    LoopFps,
}

/// A sprite-sheet animation for a billboard: the texture is a `cols × rows` grid of
/// frames; each particle samples the frame for its age. `1 × 1` = no flipbook.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Flipbook {
    pub cols: u32,
    pub rows: u32,
    pub mode: FlipMode,
    /// Frames per second for [`FlipMode::LoopFps`] (ignored for `OverLife`).
    pub fps: f32,
}

impl Default for Flipbook {
    fn default() -> Self {
        Self { cols: 1, rows: 1, mode: FlipMode::default(), fps: 12.0 }
    }
}

/// The rendered look of a track. Lighting and shadow casting are per-track opt-ins,
/// both OFF by default (classic crisp VFX costs nothing until asked).
#[derive(Debug, Clone, PartialEq)]
pub struct Look {
    pub render: RenderMode,
    pub blend: Blend,
    /// How a billboard quad is aligned in the world (ignored for meshes).
    pub orient: BillboardOrient,
    /// Billboard width-to-height ratio: rendered width = size × `aspect`, height =
    /// size. 1 = square; >1 = wide; <1 = tall. Lets one size curve drive non-square
    /// quads (embers, ground streaks, tall flames).
    pub aspect: f32,
    /// [`BillboardOrient::Velocity`] length multiplier: the quad's height is scaled
    /// by `stretch` along the motion axis. 1 = neutral; higher = longer speed lines.
    pub stretch: f32,
    /// Sprite-sheet flipbook (`None` = a plain single-frame texture).
    pub flipbook: Option<Flipbook>,
    /// Full scene lighting per particle: sun + point lights + field shadow + AO.
    pub lit: bool,
    /// The track's live cloud casts into the field shadow march (aggregate proxy).
    pub cast_shadows: bool,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            render: RenderMode::default(),
            blend: Blend::default(),
            orient: BillboardOrient::default(),
            aspect: 1.0,
            stretch: 1.0,
            flipbook: None,
            lit: false,
            cast_shadows: false,
        }
    }
}

/// One visual layer AND its timeline lane — the unit you select, drag, mute, copy.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub name: String,
    pub enabled: bool,
    pub look: Look,
    pub space: Space,

    // Timeline content (effect-time domain). Each clip carries its own emission mode
    // (stream or burst-train) and lifetime (its length) — there is no track-level rate
    // or lifetime any more.
    pub clips: Vec<Clip>,
    pub automation: Vec<Lane>,

    // Emission.
    pub shape: EmitShape,
    /// Pool capacity override; derived from the clips when `None`.
    pub max_alive: Option<u32>,

    // Per-particle properties: birth value × curve over the particle's life [0..1].
    /// Emitter-space birth velocity; +Y means "along the emit direction" (see
    /// [`EmitShape`]). A curve makes velocity kinematic over the particle's life.
    pub velocity: ValueOrCurve,
    pub size: ValueOrCurve,
    /// Euler rotation in radians `(x=pitch, y=yaw, z=roll)`. Billboards use only the
    /// roll (z, the screen-facing spin); meshes use all three.
    pub rotation: ValueOrCurve,
    /// Angular velocity in radians/sec `(x=pitch, y=yaw, z=roll)`, integrated over the
    /// particle's age — how fast it spins after birth (constant, random, or curved).
    pub angular_velocity: ValueOrCurve,
    pub color: ValueOrCurve,
    /// 0 = weightless, 1 = full gravity.
    pub gravity: f32,
    pub drag: f32,
    /// Fraction of the emitter's world velocity a newborn particle carries (Unity's
    /// "inherit velocity"). 0 = ignore the emitter's motion (classic); 1 = fully keep
    /// up with it. Fixes World-space trails (smoke, dust) spawned off a fast vessel
    /// getting "left behind in space" — they now ride its momentum, then drift as drag
    /// bleeds it off. Only meaningful for `Space::World` (Local tracks ride the node).
    pub inherit_velocity: f32,
    /// Force fields (wind / attractor / vortex / turbulence) added to velocity each
    /// step — the "make it feel alive" layer. Empty = none (zero cost).
    pub forces: Vec<Force>,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            name: "Track".into(),
            enabled: true,
            look: Look::default(),
            space: Space::default(),
            clips: Vec::new(),
            automation: Vec::new(),
            shape: EmitShape::default(),
            max_alive: None,
            velocity: ValueOrCurve::Const(Value::Vec3(Vec3::new(0.0, 1.0, 0.0))),
            size: ValueOrCurve::constant(0.25),
            rotation: ValueOrCurve::Const(Value::Vec3(Vec3::ZERO)),
            angular_velocity: ValueOrCurve::Const(Value::Vec3(Vec3::ZERO)),
            color: ValueOrCurve::Const(Value::Rgba([1.0; 4])),
            gravity: 0.0,
            drag: 0.0,
            inherit_velocity: 0.0,
            forces: Vec::new(),
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
    /// Where this effect's gravity points (constant world-down vs. the live scene field).
    pub gravity_mode: GravityMode,
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
            gravity_mode: GravityMode::default(),
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

    pub shape: EmitShape,
    /// Hard cap on live particles (derived or authored).
    pub capacity: u32,

    // Life-curve LUTs, domain = particle life [0..1].
    pub velocity: Prop4,
    /// True when `velocity` was authored as a curve (kinematic velocity-over-life).
    pub velocity_is_curve: bool,
    pub size: Prop1,
    /// Euler rotation `(x=pitch, y=yaw, z=roll)`; billboards use z, meshes use all.
    pub rotation: Prop4,
    /// Angular velocity `(x,y,z)` rad/sec, integrated over age.
    pub angular_velocity: Prop4,
    pub color: Prop4,
    pub gravity: f32,
    pub drag: f32,
    /// Fraction of the emitter's world velocity a newborn carries (see [`Track::inherit_velocity`]).
    pub inherit_velocity: f32,
    /// Force fields (copied straight from the authoring track — no baking needed).
    pub forces: Vec<Force>,

    // Automation lanes folded per target, domain = effect time normalized [0..1].
    pub lane_rate: Prop1,
    pub lane_count: Prop1,
    pub lane_speed: Prop1,
    pub lane_size: Prop1,
    pub lane_tint: Prop4,
    pub lane_shape: Prop1,
    pub lane_aspect: Prop1,
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
    pub gravity_mode: GravityMode,
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

/// Peak value of a folded scalar lane over the whole timeline (the max the emission
/// multiplier can reach), floored at 1 so a lane that only attenuates never shrinks the
/// pool budget below the un-automated size.
fn prop1_peak(p: &Prop1) -> f32 {
    let m = match p {
        Prop1::Const(v) => *v,
        Prop1::Range(a, b) => a.max(*b),
        Prop1::Lut(s) => s.iter().copied().fold(0.0f32, f32::max),
    };
    m.max(1.0)
}

/// Upper bound on simultaneously live particles, summed over clips: a stream can hold
/// `rate × lifetime`; a burst-train can hold `count × pulses` (all pulses' particles
/// alive at once in the worst case). Jitter widens both, and the peak Rate/Count
/// automation multipliers scale them so a lane swell above 1× isn't silently clipped.
fn derive_capacity(clips: &[Clip], rate_peak: f32, count_peak: f32) -> u32 {
    let mut total = 0.0f32;
    for c in clips {
        let life = c.lifetime() * (1.0 + c.lifetime_jitter.clamp(0.0, 1.0));
        total += match c.emit {
            Emit::Rate { rate } => rate.max(0.0) * rate_peak * life,
            Emit::Burst { count, count_jitter, pulses, .. } => {
                count as f32 * (1.0 + count_jitter.clamp(0.0, 1.0)) * count_peak * pulses.max(1) as f32
            }
        };
    }
    (total.ceil() as u32).clamp(1, 65_536)
}

impl Track {
    fn compile(&self, lifetime: f32) -> CompiledTrack {
        let mut clips = self.clips.clone();
        clips.sort_by(|a, b| a.start.total_cmp(&b.start));
        // Sanitize each clip: a positive length (so lifetime > 0), clamped jitters, ≥ 1
        // pulse, non-negative rate/interval.
        for c in &mut clips {
            if c.end < c.start + 1e-3 {
                c.end = c.start + 1e-3;
            }
            c.lifetime_jitter = c.lifetime_jitter.clamp(0.0, 1.0);
            match &mut c.emit {
                Emit::Rate { rate } => *rate = rate.max(0.0),
                Emit::Burst { count_jitter, pulses, interval, interval_jitter, .. } => {
                    *count_jitter = count_jitter.clamp(0.0, 1.0);
                    *pulses = (*pulses).max(1);
                    *interval = interval.max(0.0);
                    *interval_jitter = interval_jitter.clamp(0.0, 1.0);
                }
            }
        }
        // Fold the emission-scaling lanes first so the derived pool is sized for their
        // PEAK multiplier (a Rate/Count swell above 1× would otherwise overflow and drop
        // spawns silently).
        let lane_rate = fold_lanes1(&self.automation, LaneTarget::Rate, lifetime);
        let lane_count = fold_lanes1(&self.automation, LaneTarget::Count, lifetime);
        let capacity = self
            .max_alive
            .unwrap_or_else(|| derive_capacity(&clips, prop1_peak(&lane_rate), prop1_peak(&lane_count)))
            .max(1);
        CompiledTrack {
            name: self.name.clone(),
            enabled: self.enabled,
            look: self.look.clone(),
            space: self.space,
            clips,
            shape: self.shape,
            capacity,
            velocity: bake4(&self.velocity, 1.0),
            velocity_is_curve: matches!(self.velocity, ValueOrCurve::Curve(_)),
            size: bake1(&self.size, 1.0),
            rotation: bake4(&self.rotation, 1.0),
            angular_velocity: bake4(&self.angular_velocity, 1.0),
            color: bake4(&self.color, 1.0),
            gravity: self.gravity,
            drag: self.drag.max(0.0),
            inherit_velocity: self.inherit_velocity,
            forces: self.forces.clone(),
            lane_rate,
            lane_count,
            lane_speed: fold_lanes1(&self.automation, LaneTarget::Speed, lifetime),
            lane_size: fold_lanes1(&self.automation, LaneTarget::Size, lifetime),
            lane_tint: fold_lanes_tint(&self.automation, lifetime),
            lane_shape: fold_lanes1(&self.automation, LaneTarget::ShapeScale, lifetime),
            lane_aspect: fold_lanes1(&self.automation, LaneTarget::Aspect, lifetime),
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
            gravity_mode: self.gravity_mode,
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
            clips: vec![
                // A 0.5 s-long stream at 20/s = 10 alive; a 12-count burst clip = 12.
                Clip { start: 0.0, end: 0.5, lifetime_jitter: 0.0, emit: Emit::Rate { rate: 20.0 } },
                Clip {
                    start: 0.1,
                    end: 0.6,
                    lifetime_jitter: 0.0,
                    emit: Emit::Burst { count: 12, count_jitter: 0.0, pulses: 1, interval: 0.0, interval_jitter: 0.0 },
                },
            ],
            ..Track::default()
        };
        assert_eq!(t.compile(2.0).capacity, 22);
    }

    #[test]
    fn capacity_accounts_for_rate_and_count_lane_swells() {
        // A 3× Count lane must triple the burst pool budget so the swell isn't clipped.
        let lane = |target| Lane {
            target,
            curve: Curve {
                keys: vec![Key::new(0.0, Value::F32(3.0)), Key::new(1.0, Value::F32(3.0))],
                extrapolate: Default::default(),
            },
        };
        let burst = Track {
            clips: vec![Clip {
                start: 0.0,
                end: 1.0,
                lifetime_jitter: 0.0,
                emit: Emit::Burst { count: 100, count_jitter: 0.0, pulses: 1, interval: 0.0, interval_jitter: 0.0 },
            }],
            automation: vec![lane(LaneTarget::Count)],
            ..Track::default()
        };
        assert_eq!(burst.compile(1.0).capacity, 300, "3x Count lane sizes the pool for 300");
        // A rate stream with a 3× Rate lane: 10/s × 3 × 2 s life = 60.
        let stream = Track {
            clips: vec![Clip { start: 0.0, end: 2.0, lifetime_jitter: 0.0, emit: Emit::Rate { rate: 10.0 } }],
            automation: vec![lane(LaneTarget::Rate)],
            ..Track::default()
        };
        assert_eq!(stream.compile(1.0).capacity, 60);
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
