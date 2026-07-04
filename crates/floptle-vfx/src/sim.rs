//! The data-oriented particle simulation.
//!
//! Deterministic by construction: every particle's whole life derives from its birth
//! record (birth time, counter-hashed seed, lane-sampled birth values), so scrubbing
//! the editor playhead re-simulates from `t = 0` and lands on the exact same state,
//! and probe renders are bit-stable. RNG is a pure integer hash of `(seed, counter)`
//! — no state, bit-exact in Rust and WGSL alike.
//!
//! Layout is structure-of-arrays with vec4-aligned fields: the CPU loop is tight and
//! the arrays are std430-compatible, so the GPU compute backend (proposal §8 phase 5)
//! aliases this layout instead of inventing its own. Births stay CPU-side forever
//! (timeline logic — tiny and exact); aging is the part that will move on-device.

use crate::effect::{CompiledEffect, CompiledTrack, EmitShape, Playback, RenderMode, Space};
use floptle_core::math::{DVec3, Quat, Vec3, Vec4};
use floptle_core::transform::Transform;
use std::sync::Arc;

/// Fixed step for editor scrubbing / deterministic re-simulation (matches the
/// physics fixed-step discipline). In-game playback advances by real `dt`.
pub const SCRUB_STEP: f32 = 1.0 / 120.0;

// ---------------------------------------------------------------------------
// Deterministic RNG — integer hashing, WGSL-portable.
// ---------------------------------------------------------------------------

/// Lowbias32 integer hash — the per-particle RNG. Pure, stateless, exact.
#[inline]
pub fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

#[inline]
fn hash3(a: u32, b: u32, c: u32) -> u32 {
    hash(a ^ hash(b ^ hash(c)))
}

/// A uniform float in `[0, 1)` from a hash, salted so one particle seed yields
/// many independent values.
#[inline]
fn rand01(seed: u32, salt: u32) -> f32 {
    (hash(seed ^ hash(salt)) >> 8) as f32 / (1u32 << 24) as f32
}

/// Symmetric jitter multiplier: `1 ± amount` (clamped positive).
#[inline]
fn jitter_mul(seed: u32, salt: u32, amount: f32) -> f32 {
    (1.0 + amount * (rand01(seed, salt) * 2.0 - 1.0)).max(1e-3)
}

/// Per-property RNG salts for `ValueOrCurve::Range` birth values — distinct so a
/// particle's random size, speed, rotation, and tint are drawn independently (and
/// stay stable across its life, since they derive from the fixed birth seed).
const SALT_VELOCITY: u32 = 0x5EED_0001;
const SALT_SIZE: u32 = 0x5EED_0002;
const SALT_ROTATION: u32 = 0x5EED_0003;
const SALT_COLOR: u32 = 0x5EED_0004;
const SALT_ANGULAR: u32 = 0x5EED_0005;

// ---------------------------------------------------------------------------
// Force fields — deterministic, WGSL-portable acceleration added each step.
// ---------------------------------------------------------------------------

use crate::effect::Force;

/// A `[0,1)` float from a hashed `u32` (the noise-lattice value source).
#[inline]
fn hash_unit(x: u32) -> f32 {
    (hash(x) >> 8) as f32 / (1u32 << 24) as f32
}

/// One channel of trilinearly-interpolated 3D value noise in `[-1, 1]`, a pure
/// integer hash of the lattice corners (bit-identical in Rust and WGSL). `salt`
/// decorrelates the three channels of [`noise3`].
fn value_noise(p: Vec3, salt: u32) -> f32 {
    let pf = p.floor();
    let (ix, iy, iz) = (pf.x as i32, pf.y as i32, pf.z as i32);
    let f = p - pf;
    let u = f * f * (Vec3::splat(3.0) - 2.0 * f); // smoothstep fade
    let corner = |dx: i32, dy: i32, dz: i32| -> f32 {
        let h = hash3((ix + dx) as u32, (iy + dy) as u32, (iz + dz) as u32) ^ salt;
        hash_unit(h) * 2.0 - 1.0
    };
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let x00 = lerp(corner(0, 0, 0), corner(1, 0, 0), u.x);
    let x10 = lerp(corner(0, 1, 0), corner(1, 1, 0), u.x);
    let x01 = lerp(corner(0, 0, 1), corner(1, 0, 1), u.x);
    let x11 = lerp(corner(0, 1, 1), corner(1, 1, 1), u.x);
    let y0 = lerp(x00, x10, u.y);
    let y1 = lerp(x01, x11, u.y);
    lerp(y0, y1, u.z)
}

/// A smooth 3D value-noise vector in `[-1, 1]³`.
#[inline]
fn noise3(p: Vec3) -> Vec3 {
    Vec3::new(value_noise(p, 0x9E37_79B1), value_noise(p, 0x85EB_CA6B), value_noise(p, 0xC2B2_AE35))
}

/// The acceleration one [`Force`] applies to a particle at `pos` (simulation-space).
/// `world` is the particle's ABSOLUTE world position (anchor + pos for World tracks,
/// = pos for Local) — used only for turbulence so its noise field is fixed in the
/// world and rebase-invariant. Spatial forces use `pos` directly: for World tracks
/// both the authored centre and `pos` are anchor-relative, so `centre − pos` is
/// anchor-invariant.
fn force_accel(f: &Force, pos: Vec3, world: Vec3) -> Vec3 {
    match *f {
        Force::Directional { dir, strength } => dir.normalize_or_zero() * strength,
        Force::Point { center, strength } => {
            let d = center - pos;
            let l = d.length();
            if l < 1e-4 { Vec3::ZERO } else { d / l * strength }
        }
        Force::Vortex { center, axis, strength } => {
            axis.normalize_or_zero().cross(pos - center).normalize_or_zero() * strength
        }
        Force::Turbulence { frequency, strength } => noise3(world * frequency) * strength,
    }
}

// ---------------------------------------------------------------------------
// SoA particle storage
// ---------------------------------------------------------------------------

/// Live particles of one track — structure-of-arrays, capacity-allocated once,
/// retired by swap-remove so the arrays stay dense. Field packing is std430-ready:
/// `pos_age` = position.xyz + age, `vel_life` = velocity.xyz + total lifetime,
/// `frame` = birth-orientation quaternion, `misc` = [birth_size, speed_mul, spare, spare].
pub struct TrackParticles {
    pub pos_age: Vec<Vec4>,
    pub vel_life: Vec<Vec4>,
    pub frame: Vec<Vec4>,
    pub misc: Vec<Vec4>,
    pub seed: Vec<u32>,
    pub count: usize,
}

impl TrackParticles {
    fn with_capacity(cap: usize) -> Self {
        Self {
            pos_age: Vec::with_capacity(cap),
            vel_life: Vec::with_capacity(cap),
            frame: Vec::with_capacity(cap),
            misc: Vec::with_capacity(cap),
            seed: Vec::with_capacity(cap),
            count: 0,
        }
    }

    fn clear(&mut self) {
        self.pos_age.clear();
        self.vel_life.clear();
        self.frame.clear();
        self.misc.clear();
        self.seed.clear();
        self.count = 0;
    }

    fn swap_remove(&mut self, i: usize) {
        self.pos_age.swap_remove(i);
        self.vel_life.swap_remove(i);
        self.frame.swap_remove(i);
        self.misc.swap_remove(i);
        self.seed.swap_remove(i);
        self.count -= 1;
    }

    #[allow(clippy::too_many_arguments)]
    fn push(&mut self, pos: Vec3, age: f32, vel: Vec3, life: f32, frame: Quat, misc: Vec4, seed: u32) {
        self.pos_age.push(pos.extend(age));
        self.vel_life.push(vel.extend(life));
        self.frame.push(Vec4::new(frame.x, frame.y, frame.z, frame.w));
        self.misc.push(misc);
        self.seed.push(seed);
        self.count += 1;
    }
}

/// Per-track live state inside an instance.
struct TrackState {
    particles: TrackParticles,
    /// Fractional-emission accumulator; resets when the playhead enters a clip.
    acc: f32,
    /// Monotonic birth counter — the RNG stream position. Never resets mid-play.
    emit_counter: u32,
}

// ---------------------------------------------------------------------------
// Effect instance
// ---------------------------------------------------------------------------

/// A live, playing copy of a compiled effect. Owns its particles; shares the
/// compiled data. Deterministic given (`effect.seed`, `instance_seed`, step sizes).
pub struct EffectInstance {
    pub effect: Arc<CompiledEffect>,
    /// Playhead in seconds. `prev_t` trails it for crossing tests.
    pub t: f32,
    prev_t: f32,
    pub playing: bool,
    instance_seed: u32,
    tracks: Vec<TrackState>,
    /// World anchor for `Space::World` tracks (the emitter's last world position).
    /// World particles are stored relative to it and it tracks the emitter, so they
    /// stay put in the world as the node moves — the f64 anchor keeps precision far
    /// from the origin. `anchored` guards the first advance (no shift on birth).
    anchor: DVec3,
    anchored: bool,
}

/// Epsilon the playhead starts *before*, so events placed exactly at `t = 0`
/// fire on the first step (crossings are half-open `(prev_t, t]`).
const START_EPS: f32 = -1.0e-6;

impl EffectInstance {
    pub fn new(effect: Arc<CompiledEffect>, instance_seed: u32) -> Self {
        let tracks = effect
            .tracks
            .iter()
            .map(|ct| TrackState {
                particles: TrackParticles::with_capacity(ct.capacity as usize),
                acc: 0.0,
                emit_counter: 0,
            })
            .collect();
        Self {
            effect,
            t: 0.0,
            prev_t: START_EPS,
            playing: true,
            instance_seed,
            tracks,
            anchor: DVec3::ZERO,
            anchored: false,
        }
    }

    /// The world anchor `Space::World` particles are stored relative to (the emitter's
    /// last world position). The render caller maps this to camera-relative space.
    pub fn anchor(&self) -> DVec3 {
        self.anchor
    }

    /// Back to `t = 0` with no live particles — the scrub / restart baseline.
    pub fn reset(&mut self) {
        self.t = 0.0;
        self.prev_t = START_EPS;
        self.anchored = false;
        for ts in &mut self.tracks {
            ts.particles.clear();
            ts.acc = 0.0;
            ts.emit_counter = 0;
        }
    }

    /// Total live particles across all tracks.
    pub fn alive(&self) -> usize {
        self.tracks.iter().map(|ts| ts.particles.count).sum()
    }

    /// Live particles of one track (render collection reads the SoA directly).
    pub fn track_particles(&self, track: usize) -> &TrackParticles {
        &self.tracks[track].particles
    }

    /// A one-shot that has finished emitting and drained its particles.
    pub fn is_done(&self) -> bool {
        self.effect.playback == Playback::OneShot
            && self.t >= self.effect.lifetime
            && self.alive() == 0
    }

    /// Deterministic scrub: re-simulate from zero to `target` in fixed steps.
    pub fn simulate_to(&mut self, target: f32, gravity: Vec3) {
        self.simulate_to_at(target, gravity, Transform::IDENTITY);
    }

    /// [`simulate_to`] with the emitter's world transform (for `Space::World` tracks).
    pub fn simulate_to_at(&mut self, target: f32, gravity: Vec3, emitter: Transform) {
        self.reset();
        let mut sim_t = 0.0;
        while sim_t + SCRUB_STEP <= target {
            self.advance_at(SCRUB_STEP, gravity, emitter);
            sim_t += SCRUB_STEP;
        }
        if target > sim_t {
            self.advance_at(target - sim_t, gravity, emitter);
        }
    }

    /// Advance with the emitter at the world origin — for tests and static previews.
    pub fn advance(&mut self, dt: f32, gravity: Vec3) {
        self.advance_at(dt, gravity, Transform::IDENTITY);
    }

    /// Advance the playhead by `dt`, firing crossed clips/bursts and aging every
    /// particle. `gravity` is the scene's gravity (world down); `emitter` is the node's
    /// world transform — `Space::World` particles bake into world orientation at birth
    /// and re-anchor to it, so they stay put in the world as the node moves.
    pub fn advance_at(&mut self, dt: f32, gravity: Vec3, emitter: Transform) {
        if !self.playing || dt <= 0.0 {
            return;
        }
        // Re-anchor World tracks: hold their absolute world positions fixed while the
        // anchor follows the emitter (so f32 offsets stay small near the action).
        let anchor_now = emitter.translation;
        if self.anchored {
            let delta = (anchor_now - self.anchor).as_vec3();
            if delta != Vec3::ZERO {
                let effect = Arc::clone(&self.effect);
                for (ti, ct) in effect.tracks.iter().enumerate() {
                    if ct.space == Space::World {
                        for pa in &mut self.tracks[ti].particles.pos_age {
                            let np = pa.truncate() - delta;
                            *pa = np.extend(pa.w);
                        }
                    }
                }
            }
        }
        self.anchor = anchor_now;
        self.anchored = true;

        // Age existing particles first; newborns then age only their partial step.
        self.integrate(dt, gravity);

        let lifetime = self.effect.lifetime;
        let mut remaining = dt;
        while remaining > 0.0 {
            let step_end = (self.t + remaining).min(lifetime);
            let seg = remaining.min(lifetime - self.t);
            let (prev, now) = (self.prev_t, step_end);
            self.emit_segment(prev, now, gravity, &emitter);
            self.t = step_end;
            self.prev_t = step_end;
            remaining -= seg.max(0.0);
            if self.t >= lifetime {
                match self.effect.playback {
                    Playback::Looping => {
                        self.t = 0.0;
                        self.prev_t = START_EPS;
                        // Guard: a dt many lifetimes long must still terminate.
                        if seg <= 0.0 {
                            break;
                        }
                    }
                    Playback::OneShot => break,
                }
            }
        }
    }

    /// Fire every clip overlap and burst crossing in `(prev, now]`.
    fn emit_segment(&mut self, prev: f32, now: f32, gravity: Vec3, emitter: &Transform) {
        let effect = Arc::clone(&self.effect);
        let inst_seed = hash(self.instance_seed ^ hash(effect.seed));
        let lifetime = effect.lifetime;
        for (ti, ct) in effect.tracks.iter().enumerate() {
            if !ct.enabled {
                continue;
            }
            let ts = &mut self.tracks[ti];

            for b in &ct.bursts {
                if prev < b.t && b.t <= now {
                    let mul = ct.lane_count.sample(b.t / lifetime);
                    let n = (b.count as f32 * mul).round().max(0.0) as u32;
                    for _ in 0..n {
                        spawn(ts, ct, ti as u32, inst_seed, b.t, now, lifetime, gravity, emitter);
                    }
                }
            }

            for clip in &ct.clips {
                // Entering a clip resets the fractional accumulator: each span
                // starts its emission phase fresh (deterministic across scrubs).
                if prev < clip.start && clip.start <= now {
                    ts.acc = 0.0;
                }
                let (s, e) = (prev.max(clip.start), now.min(clip.end));
                if e <= s || ct.rate <= 0.0 {
                    continue;
                }
                let mid = 0.5 * (s + e);
                let rate_eff = ct.rate * ct.lane_rate.sample(mid / lifetime);
                if rate_eff <= 0.0 {
                    continue;
                }
                let acc0 = ts.acc;
                ts.acc += rate_eff * (e - s);
                let n = ts.acc.floor() as u32;
                ts.acc -= n as f32;
                for k in 1..=n {
                    // Reconstruct the exact accumulator-crossing time so birth
                    // spacing is even regardless of frame boundaries.
                    let tau = (s + (k as f32 - acc0) / rate_eff).clamp(s, e);
                    spawn(ts, ct, ti as u32, inst_seed, tau, now, lifetime, gravity, emitter);
                }
            }
        }
    }

    /// Age, gravity, drag, forces, retire. Kinematic-velocity tracks sample their LUT.
    fn integrate(&mut self, dt: f32, gravity: Vec3) {
        // World-track turbulence samples noise at the absolute world position; the
        // anchor (as f32) shifts it back into world space from the anchor-relative store.
        let anchor = self.anchor.as_vec3();
        for (ti, ct) in self.effect.tracks.iter().enumerate() {
            let p = &mut self.tracks[ti].particles;
            let damp = (-ct.drag * dt).exp();
            let g = gravity * ct.gravity * dt;
            let is_world = ct.space == Space::World;
            let mut i = 0;
            while i < p.count {
                let age = p.pos_age[i].w + dt;
                let life = p.vel_life[i].w;
                if age >= life {
                    p.swap_remove(i);
                    continue;
                }
                let mut vel = p.vel_life[i].truncate();
                vel = vel * damp + g;
                // Force fields accelerate the CARRIED velocity (kinematic tracks below
                // only replace the base term, so forces survive the re-sample).
                if !ct.forces.is_empty() {
                    let pos = p.pos_age[i].truncate();
                    let world = if is_world { anchor + pos } else { pos };
                    let mut acc = Vec3::ZERO;
                    for force in &ct.forces {
                        acc += force_accel(force, pos, world);
                    }
                    vel += acc * dt;
                }
                let base = if ct.velocity_is_curve {
                    let q = p.frame[i];
                    let frame = Quat::from_xyzw(q.x, q.y, q.z, q.w);
                    frame * ct.velocity.sample_vec3(age / life) * p.misc[i].y
                } else {
                    Vec3::ZERO
                };
                let pos = p.pos_age[i].truncate() + (vel + base) * dt;
                p.pos_age[i] = pos.extend(age);
                p.vel_life[i] = vel.extend(life);
                i += 1;
            }
        }
    }
}

/// Birth one particle at effect-time `tau`, aged forward to `now`.
#[allow(clippy::too_many_arguments)]
fn spawn(
    ts: &mut TrackState,
    ct: &CompiledTrack,
    track_idx: u32,
    inst_seed: u32,
    tau: f32,
    now: f32,
    lifetime: f32,
    gravity: Vec3,
    emitter: &Transform,
) {
    if ts.particles.count as u32 >= ct.capacity {
        return; // pool full — drop, never reallocate mid-play
    }
    let counter = ts.emit_counter;
    ts.emit_counter = ts.emit_counter.wrapping_add(1);
    let seed = hash3(inst_seed, track_idx, counter);

    let un = tau / lifetime; // normalized effect time for lane sampling
    let shape_scale = ct.lane_shape.sample(un);
    let (mut offset, mut dir) = sample_shape(ct.shape, shape_scale, seed);
    // A World-space track bakes the birth offset + emit direction into WORLD
    // orientation (the emitter's rotation/scale); the anchor carries translation, so
    // the particle stops riding the node. Local tracks stay emitter-local (the node
    // matrix is applied at render).
    if ct.space == Space::World {
        offset = emitter.rotation * (emitter.scale * offset);
        dir = emitter.rotation * dir;
    }
    let frame = Quat::from_rotation_arc(Vec3::Y, dir);

    let life = ct.particle_lifetime * jitter_mul(seed, 0xA11FE, ct.lifetime_jitter);
    let age0 = (now - tau).max(0.0);
    if age0 >= life {
        return; // born and expired within one (enormous) step
    }

    let speed_mul = ct.lane_speed.sample(un);
    let birth_size = ct.lane_size.sample(un);
    // Birth velocity: value at life 0, resolving a per-particle `Range` from the seed.
    let v0 = frame * ct.velocity.sample_vec3_rand(0.0, rand01(seed, SALT_VELOCITY)) * speed_mul;

    // Constant-velocity particles carry their full velocity in the integrated
    // state; kinematic (curve) ones carry only the gravity-accumulated part and
    // re-sample their base velocity each step.
    let g0 = gravity * ct.gravity * age0;
    let (vel, carried) = if ct.velocity_is_curve { (g0, v0) } else { (v0 + g0, v0) };
    let pos = offset + carried * age0;

    let misc = Vec4::new(birth_size, speed_mul, 0.0, 0.0);
    ts.particles.push(pos, age0, vel, life, frame, misc, seed);
}

/// Deterministic shape sample: birth offset (emitter space) + unit emit direction.
/// The velocity value's +Y aligns to the returned direction (proposal §3).
fn sample_shape(shape: EmitShape, scale: f32, seed: u32) -> (Vec3, Vec3) {
    let (r0, r1, r2) = (rand01(seed, 1), rand01(seed, 2), rand01(seed, 3));
    match shape {
        EmitShape::Point => (Vec3::ZERO, Vec3::Y),
        EmitShape::Cone { angle, radius } => {
            let a = r0 * std::f32::consts::TAU;
            let rad = radius * scale * r1.sqrt();
            let offset = Vec3::new(a.cos() * rad, 0.0, a.sin() * rad);
            // Uniform direction within `angle` degrees of +Y.
            let half = angle.to_radians().clamp(0.0, std::f32::consts::PI);
            let cos_t = 1.0 - r2 * (1.0 - half.cos());
            let sin_t = (1.0 - cos_t * cos_t).max(0.0).sqrt();
            let phi = rand01(seed, 4) * std::f32::consts::TAU;
            let dir = Vec3::new(sin_t * phi.cos(), cos_t, sin_t * phi.sin());
            (offset, dir.normalize())
        }
        EmitShape::Sphere { radius, shell } => {
            // Uniform direction via z + azimuth; radius by cube-root for volume.
            let z = r0 * 2.0 - 1.0;
            let a = r1 * std::f32::consts::TAU;
            let xy = (1.0 - z * z).max(0.0).sqrt();
            let dir = Vec3::new(xy * a.cos(), z, xy * a.sin());
            let rad = radius * scale * if shell { 1.0 } else { r2.cbrt() };
            (dir * rad, dir)
        }
        EmitShape::Edge { length } => {
            (Vec3::new((r0 - 0.5) * length * scale, 0.0, 0.0), Vec3::Z)
        }
        EmitShape::Ring { radius } => {
            let a = r0 * std::f32::consts::TAU;
            let dir = Vec3::new(a.cos(), 0.0, a.sin());
            (dir * radius * scale, dir)
        }
    }
}

// ---------------------------------------------------------------------------
// Render-facing sampling
// ---------------------------------------------------------------------------

/// One particle's drawable state, sampled from the SoA + LUTs at collect time.
#[derive(Clone, Copy, Debug)]
pub struct ParticleSample {
    /// Emitter-space position (`Space::Local`; the caller applies the node matrix).
    pub pos: Vec3,
    /// Emitter-space instantaneous velocity — the motion axis for velocity-aligned
    /// billboards. The caller rotates it into world/camera-relative space.
    pub velocity: Vec3,
    /// Birth orientation (the emit-direction frame): the plane of a `WorldFixed`
    /// billboard, so debris keeps the pose it was fired with.
    pub frame: Quat,
    pub size: f32,
    /// Euler rotation in radians `(x=pitch, y=yaw, z=roll)` — base rotation plus the
    /// angular velocity integrated over the particle's age.
    pub rotation: Vec3,
    pub color: [f32; 4],
    /// Age in seconds (for fixed-fps flipbooks).
    pub age: f32,
    /// Normalized life `age/lifetime` in `[0,1]` (for over-life flipbooks + effects).
    pub age01: f32,
}

impl EffectInstance {
    /// Sample every live particle of `track` for drawing. Size/rotation/color come
    /// from the life-curve LUTs at `age/life`, scaled by the birth-lane snapshot.
    pub fn sample_track(&self, track: usize, mut f: impl FnMut(ParticleSample)) {
        let ct = &self.effect.tracks[track];
        let p = &self.tracks[track].particles;
        let tint = ct.lane_tint.sample(self.t / self.effect.lifetime);
        for i in 0..p.count {
            let seed = p.seed[i];
            let age = p.pos_age[i].w;
            let u = age / p.vel_life[i].w;
            // Per-particle `Range` randoms resolve from the birth seed (stable over
            // life); `Const`/`Lut` properties ignore the random argument.
            let mut color = ct.color.sample_rand(u, rand01(seed, SALT_COLOR));
            for c in 0..4 {
                color[c] *= tint[c];
            }
            // Rotation = base Euler over life + angular velocity integrated over age.
            let rotation = ct.rotation.sample_vec3_rand(u, rand01(seed, SALT_ROTATION))
                + ct.angular_velocity.sample_vec3_rand(u, rand01(seed, SALT_ANGULAR)) * age;
            // Instantaneous velocity for velocity-aligned billboards: the integrated
            // component plus, for kinematic tracks, the re-sampled base (mirrors
            // `integrate` so the drawn motion axis matches the actual motion).
            let frame = {
                let q = p.frame[i];
                Quat::from_xyzw(q.x, q.y, q.z, q.w)
            };
            let mut velocity = p.vel_life[i].truncate();
            if ct.velocity_is_curve {
                velocity += frame * ct.velocity.sample_vec3(u) * p.misc[i].y;
            }
            f(ParticleSample {
                pos: p.pos_age[i].truncate(),
                velocity,
                frame,
                size: ct.size.sample_rand(u, rand01(seed, SALT_SIZE)) * p.misc[i].x,
                rotation,
                color,
                age,
                age01: u,
            });
        }
    }

    /// Iterate the tracks that draw as billboards, with their look data.
    pub fn billboard_tracks(&self) -> impl Iterator<Item = (usize, &CompiledTrack)> {
        self.effect
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, ct)| {
                ct.enabled && matches!(ct.look.render, RenderMode::Billboard { .. })
            })
    }

    /// Iterate the tracks that draw as instanced meshes.
    pub fn mesh_tracks(&self) -> impl Iterator<Item = (usize, &CompiledTrack)> {
        self.effect
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, ct)| ct.enabled && matches!(ct.look.render, RenderMode::Mesh { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{Curve, Key, Value, ValueOrCurve};
    use crate::effect::{Burst, Clip, Force, ParticleEffect, Playback, Track};

    const NO_G: Vec3 = Vec3::ZERO;

    fn one_track_effect(track: Track, lifetime: f32, playback: Playback) -> Arc<CompiledEffect> {
        Arc::new(
            ParticleEffect {
                lifetime,
                playback,
                tracks: vec![track],
                ..ParticleEffect::default()
            }
            .compile(),
        )
    }

    #[test]
    fn burst_at_zero_fires_exactly_once() {
        let fx = one_track_effect(
            Track {
                rate: 0.0,
                bursts: vec![Burst { t: 0.0, count: 7 }],
                particle_lifetime: 10.0,
                ..Track::default()
            },
            1.0,
            Playback::OneShot,
        );
        let mut inst = EffectInstance::new(fx, 42);
        inst.advance(0.1, NO_G);
        assert_eq!(inst.alive(), 7);
        inst.advance(0.1, NO_G);
        assert_eq!(inst.alive(), 7, "burst must not re-fire");
    }

    #[test]
    fn clip_gated_rate_emits_exact_count() {
        // 10/s inside a [0.0, 0.55] clip = exactly 5 particles, none outside it
        // (crossings at 0.1..0.5; the clip edge is deliberately NOT a crossing so
        // float accumulation can't fencepost the count).
        let fx = one_track_effect(
            Track {
                rate: 10.0,
                clips: vec![Clip { start: 0.0, end: 0.55 }],
                particle_lifetime: 10.0,
                ..Track::default()
            },
            1.0,
            Playback::OneShot,
        );
        let mut inst = EffectInstance::new(fx, 1);
        for _ in 0..100 {
            inst.advance(0.01, NO_G);
        }
        assert_eq!(inst.alive(), 5);
    }

    #[test]
    fn looping_rearms_clips_and_keeps_live_particles_across_wrap() {
        let fx = one_track_effect(
            Track {
                rate: 0.0,
                bursts: vec![Burst { t: 0.5, count: 3 }],
                particle_lifetime: 0.9, // outlives the 1 s wrap when born at 0.5
                ..Track::default()
            },
            1.0,
            Playback::Looping,
        );
        let mut inst = EffectInstance::new(fx, 9);
        inst.simulate_to(0.6, NO_G);
        assert_eq!(inst.alive(), 3);
        // Cross the wrap to 1.3: originals (age 0.8) still alive, no re-fire yet.
        inst.simulate_to(1.3, NO_G);
        assert_eq!(inst.alive(), 3);
        // At 1.6 the loop's second burst fired; originals (age 1.1 > 0.9) retired.
        inst.simulate_to(1.6, NO_G);
        assert_eq!(inst.alive(), 3);
        assert!((inst.t - 0.6).abs() < 1e-4, "playhead wraps into the next loop");
    }

    #[test]
    fn oneshot_finishes_and_reports_done() {
        let fx = one_track_effect(
            Track {
                rate: 0.0,
                bursts: vec![Burst { t: 0.1, count: 2 }],
                particle_lifetime: 0.2,
                ..Track::default()
            },
            0.5,
            Playback::OneShot,
        );
        let mut inst = EffectInstance::new(fx, 3);
        inst.simulate_to(0.2, NO_G);
        assert!(!inst.is_done());
        inst.simulate_to(2.0, NO_G);
        assert!(inst.is_done());
    }

    #[test]
    fn scrub_is_bit_deterministic() {
        let fx = one_track_effect(
            Track {
                rate: 40.0,
                clips: vec![Clip { start: 0.0, end: 1.0 }],
                bursts: vec![Burst { t: 0.33, count: 5 }],
                particle_lifetime: 0.6,
                lifetime_jitter: 0.5,
                ..Track::default()
            },
            1.0,
            Playback::Looping,
        );
        let mut a = EffectInstance::new(Arc::clone(&fx), 77);
        let mut b = EffectInstance::new(fx, 77);
        a.simulate_to(0.85, Vec3::new(0.0, -9.81, 0.0));
        b.simulate_to(0.85, Vec3::new(0.0, -9.81, 0.0));
        assert_eq!(a.alive(), b.alive());
        let (pa, pb) = (a.track_particles(0), b.track_particles(0));
        for i in 0..pa.count {
            assert_eq!(pa.pos_age[i], pb.pos_age[i], "particle {i} diverged");
            assert_eq!(pa.seed[i], pb.seed[i]);
        }
        assert!(pa.count > 10, "test should exercise a real population");
    }

    #[test]
    fn different_instance_seeds_diverge() {
        let fx = one_track_effect(
            Track {
                rate: 30.0,
                clips: vec![Clip { start: 0.0, end: 1.0 }],
                particle_lifetime: 1.0,
                shape: crate::effect::EmitShape::Sphere { radius: 1.0, shell: false },
                ..Track::default()
            },
            1.0,
            Playback::OneShot,
        );
        let mut a = EffectInstance::new(Arc::clone(&fx), 1);
        let mut b = EffectInstance::new(fx, 2);
        a.simulate_to(0.5, NO_G);
        b.simulate_to(0.5, NO_G);
        let (pa, pb) = (a.track_particles(0), b.track_particles(0));
        assert_eq!(pa.count, pb.count, "emission timing is seed-independent");
        assert_ne!(pa.pos_age[0], pb.pos_age[0], "positions must differ by seed");
    }

    #[test]
    fn capacity_caps_live_particles_without_realloc() {
        let fx = one_track_effect(
            Track {
                rate: 10_000.0,
                clips: vec![Clip { start: 0.0, end: 1.0 }],
                particle_lifetime: 5.0,
                max_alive: Some(64),
                ..Track::default()
            },
            1.0,
            Playback::OneShot,
        );
        let mut inst = EffectInstance::new(fx, 5);
        inst.simulate_to(0.9, NO_G);
        assert_eq!(inst.alive(), 64);
        assert_eq!(inst.track_particles(0).pos_age.capacity(), 64);
    }

    #[test]
    fn velocity_curve_drives_kinematic_motion() {
        // Speed 2 → 0 over life, direction +Y (Point shape): a particle born at 0
        // decelerates; its Y strictly increases but by less each step.
        let vel = ValueOrCurve::Curve(Curve {
            keys: vec![
                Key::new(0.0, Value::Vec3(Vec3::new(0.0, 2.0, 0.0))),
                Key::new(1.0, Value::Vec3(Vec3::ZERO)),
            ],
            extrapolate: Default::default(),
        });
        let fx = one_track_effect(
            Track {
                rate: 0.0,
                bursts: vec![Burst { t: 0.0, count: 1 }],
                particle_lifetime: 1.0,
                velocity: vel,
                ..Track::default()
            },
            1.0,
            Playback::OneShot,
        );
        let mut inst = EffectInstance::new(fx, 8);
        inst.simulate_to(0.25, NO_G);
        let y1 = inst.track_particles(0).pos_age[0].y;
        inst.simulate_to(0.5, NO_G);
        let y2 = inst.track_particles(0).pos_age[0].y;
        inst.simulate_to(0.75, NO_G);
        let y3 = inst.track_particles(0).pos_age[0].y;
        assert!(y1 > 0.0 && y2 > y1 && y3 > y2, "keeps rising: {y1} {y2} {y3}");
        assert!((y2 - y1) > (y3 - y2), "but decelerates");
    }

    #[test]
    fn range_property_varies_per_particle_but_is_deterministic() {
        // A burst of 32 with a random birth size in [0.1, 1.0]: sizes must spread
        // across the range, stay inside it, and reproduce exactly for the same seed.
        let mk = || {
            one_track_effect(
                Track {
                    rate: 0.0,
                    bursts: vec![Burst { t: 0.0, count: 32 }],
                    particle_lifetime: 10.0,
                    size: ValueOrCurve::Range(Value::F32(0.1), Value::F32(1.0)),
                    ..Track::default()
                },
                1.0,
                Playback::OneShot,
            )
        };
        let mut a = EffectInstance::new(mk(), 7);
        a.simulate_to(0.1, NO_G);
        let mut sizes_a = Vec::new();
        a.sample_track(0, |s| sizes_a.push(s.size));
        assert_eq!(sizes_a.len(), 32);
        assert!(sizes_a.iter().all(|&s| (0.1..=1.0).contains(&s)), "all inside the range");
        let first = sizes_a[0];
        assert!(sizes_a.iter().any(|&s| (s - first).abs() > 1e-3), "sizes must genuinely vary");

        let mut b = EffectInstance::new(mk(), 7);
        b.simulate_to(0.1, NO_G);
        let mut sizes_b = Vec::new();
        b.sample_track(0, |s| sizes_b.push(s.size));
        assert_eq!(sizes_a, sizes_b, "same seed reproduces the same random sizes");
    }

    #[test]
    fn angular_velocity_spins_particles_over_age() {
        // A single particle with angular velocity (0, 2, 0) rad/s: at age t its yaw = 2t.
        let track = Track {
            rate: 0.0,
            bursts: vec![Burst { t: 0.0, count: 1 }],
            particle_lifetime: 10.0,
            angular_velocity: ValueOrCurve::Const(Value::Vec3(Vec3::new(0.0, 2.0, 0.0))),
            ..Track::default()
        };
        let fx = one_track_effect(track, 1.0, Playback::OneShot);
        let mut inst = EffectInstance::new(fx, 1);
        inst.simulate_to(0.5, NO_G);
        let mut rot = Vec3::ZERO;
        inst.sample_track(0, |s| rot = s.rotation);
        assert!((rot.y - 1.0).abs() < 0.05, "yaw ~1.0 at age 0.5, got {}", rot.y);
        assert_eq!((rot.x, rot.z), (0.0, 0.0), "only the y axis spins");
    }

    #[test]
    fn world_space_particles_reanchor_when_the_emitter_moves() {
        use crate::effect::Space;
        // A World-space burst with zero velocity/gravity: once born, moving the emitter
        // must NOT drag the particles along — their stored offset re-anchors by the
        // emitter delta so the absolute world position stays fixed.
        let mut track = Track {
            rate: 0.0,
            bursts: vec![Burst { t: 0.0, count: 4 }],
            particle_lifetime: 100.0,
            velocity: ValueOrCurve::Const(Value::Vec3(Vec3::ZERO)),
            ..Track::default()
        };
        track.space = Space::World;
        let fx = one_track_effect(track, 1.0, Playback::Looping);
        let mut inst = EffectInstance::new(fx, 1);
        inst.advance_at(0.05, NO_G, Transform::IDENTITY); // birth at the origin
        let p0 = inst.track_particles(0).pos_age[0].truncate();
        inst.advance_at(0.05, NO_G, Transform::from_translation(DVec3::new(10.0, 0.0, 0.0)));
        let p1 = inst.track_particles(0).pos_age[0].truncate();
        assert!((p1.x - (p0.x - 10.0)).abs() < 1e-3, "offset must re-anchor by the delta: {p0} -> {p1}");
    }

    #[test]
    fn local_space_particles_are_untouched_by_emitter_motion() {
        // The Local counterpart: the sim never re-anchors them (the node matrix moves
        // them at render), so their stored positions are identical whatever the emitter.
        let track = Track {
            rate: 0.0,
            bursts: vec![Burst { t: 0.0, count: 4 }],
            particle_lifetime: 100.0,
            velocity: ValueOrCurve::Const(Value::Vec3(Vec3::ZERO)),
            ..Track::default() // Space::Local by default
        };
        let fx = one_track_effect(track, 1.0, Playback::Looping);
        let mut inst = EffectInstance::new(fx, 1);
        inst.advance_at(0.05, NO_G, Transform::IDENTITY);
        let p0 = inst.track_particles(0).pos_age[0].truncate();
        inst.advance_at(0.05, NO_G, Transform::from_translation(DVec3::new(10.0, 0.0, 0.0)));
        let p1 = inst.track_particles(0).pos_age[0].truncate();
        assert_eq!(p0, p1, "local particles' stored positions don't move with the emitter");
    }

    #[test]
    fn directional_force_accelerates_particles() {
        // Zero velocity + zero gravity; a +X wind (strength 4) must push the particle
        // along +X only.
        let track = Track {
            rate: 0.0,
            bursts: vec![Burst { t: 0.0, count: 1 }],
            particle_lifetime: 10.0,
            velocity: ValueOrCurve::Const(Value::Vec3(Vec3::ZERO)),
            forces: vec![Force::Directional { dir: Vec3::X, strength: 4.0 }],
            ..Track::default()
        };
        let fx = one_track_effect(track, 1.0, Playback::OneShot);
        let mut inst = EffectInstance::new(fx, 1);
        inst.simulate_to(0.5, NO_G);
        let p = inst.track_particles(0).pos_age[0].truncate();
        assert!(p.x > 0.3, "wind should push +X, got {p}");
        assert!(p.y.abs() < 1e-3 && p.z.abs() < 1e-3, "only +X, got {p}");
    }

    #[test]
    fn point_force_attracts_toward_center() {
        // A particle offset at +X, pulled toward the origin, must move −X (toward it).
        let track = Track {
            rate: 0.0,
            bursts: vec![Burst { t: 0.0, count: 1 }],
            particle_lifetime: 10.0,
            shape: crate::effect::EmitShape::Edge { length: 4.0 }, // spread along X
            velocity: ValueOrCurve::Const(Value::Vec3(Vec3::ZERO)),
            forces: vec![Force::Point { center: Vec3::ZERO, strength: 5.0 }],
            ..Track::default()
        };
        let fx = one_track_effect(track, 1.0, Playback::OneShot);
        let mut inst = EffectInstance::new(fx, 3);
        let x0 = {
            inst.simulate_to(0.01, NO_G);
            inst.track_particles(0).pos_age[0].x
        };
        inst.simulate_to(0.4, NO_G);
        let x1 = inst.track_particles(0).pos_age[0].x;
        // Whichever side it was born, attraction moves |x| toward 0.
        assert!(x1.abs() < x0.abs(), "attractor should pull toward center: {x0} -> {x1}");
    }

    #[test]
    fn turbulence_is_world_fixed_and_rebase_safe() {
        use crate::effect::Space;
        // A World-space particle pushed only by turbulence: its ABSOLUTE world
        // trajectory must be identical whether or not the emitter drifts (which
        // re-anchors it every step). The noise field is fixed in the world.
        let mk = || {
            let mut t = Track {
                rate: 0.0,
                bursts: vec![Burst { t: 0.0, count: 1 }],
                particle_lifetime: 100.0,
                velocity: ValueOrCurve::Const(Value::Vec3(Vec3::ZERO)),
                forces: vec![Force::Turbulence { frequency: 0.5, strength: 3.0 }],
                ..Track::default()
            };
            t.space = Space::World;
            one_track_effect(t, 1.0, Playback::Looping)
        };
        let mut a = EffectInstance::new(mk(), 1);
        let mut b = EffectInstance::new(mk(), 1);
        a.advance_at(0.02, NO_G, Transform::IDENTITY); // birth both at the origin
        b.advance_at(0.02, NO_G, Transform::IDENTITY);
        let mut bx = 0.0;
        for _ in 0..20 {
            a.advance_at(0.02, NO_G, Transform::IDENTITY);
            bx += 1.0; // B's emitter drifts → re-anchors every step
            b.advance_at(0.02, NO_G, Transform::from_translation(DVec3::new(bx, 0.0, 0.0)));
        }
        let pa = a.anchor().as_vec3() + a.track_particles(0).pos_age[0].truncate();
        let pb = b.anchor().as_vec3() + b.track_particles(0).pos_age[0].truncate();
        assert!((pa - pb).length() < 0.05, "turbulence must be rebase-invariant: {pa} vs {pb}");
        assert!(pa.length() > 1e-3, "turbulence should actually displace the particle");
    }

    #[test]
    fn gravity_pulls_particles_down() {
        let fx = one_track_effect(
            Track {
                rate: 0.0,
                bursts: vec![Burst { t: 0.0, count: 1 }],
                particle_lifetime: 2.0,
                velocity: ValueOrCurve::Const(Value::Vec3(Vec3::ZERO)),
                gravity: 1.0,
                ..Track::default()
            },
            2.0,
            Playback::OneShot,
        );
        let mut inst = EffectInstance::new(fx, 4);
        inst.simulate_to(1.0, Vec3::new(0.0, -10.0, 0.0));
        let y = inst.track_particles(0).pos_age[0].y;
        // Analytic: −½·g·t² = −5; fixed-step integration lands close.
        assert!((-5.5..=-4.5).contains(&y), "fell to {y}");
    }

    #[test]
    fn sample_track_applies_life_curves_and_birth_size() {
        let size = ValueOrCurve::Curve(Curve {
            keys: vec![Key::new(0.0, Value::F32(1.0)), Key::new(1.0, Value::F32(0.0))],
            extrapolate: Default::default(),
        });
        let fx = one_track_effect(
            Track {
                rate: 0.0,
                bursts: vec![Burst { t: 0.0, count: 1 }],
                particle_lifetime: 1.0,
                size,
                ..Track::default()
            },
            1.0,
            Playback::OneShot,
        );
        let mut inst = EffectInstance::new(fx, 2);
        inst.simulate_to(0.5, NO_G);
        let mut got = None;
        inst.sample_track(0, |s| got = Some(s));
        let s = got.expect("one live particle");
        assert!((s.size - 0.5).abs() < 0.03, "size halfway through life, got {}", s.size);
        assert_eq!(s.color[3], 1.0);
    }
}
