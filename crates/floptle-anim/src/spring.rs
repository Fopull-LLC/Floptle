//! Spring bones: secondary motion for chains of bones (card 0693).
//!
//! Hair, a ponytail, a cable, an antenna or a strap follows its animated
//! pose loosely: it hangs under gravity, swings when the body turns or
//! speeds up, springs back toward where the clip puts it, and stays out of
//! the head, the neck and the shoulders. One Verlet particle rides each
//! segment's tip (the next bone down the chain), in the model's own space;
//! the solver steps them at a fixed rate whatever the frame rate, then
//! turns each bone so its child points at its particle, and the skin
//! follows from the pose as it always does.
//!
//! Pure math, like the rest of this crate: the editor binds names to
//! indices, hands in the frame's animated pose, the node's motion and the
//! world's gravity, and reads the pose back. See
//! `.internal/docs/spring-bones-plan.md`.

use crate::{Skeleton, TransformTRS};
use floptle_core::math::{Mat4, Quat, Vec3};

/// The solver's step: fixed, so the same motion swings the same at 30 fps
/// and at 144 fps.
pub const SPRING_STEP: f32 = 1.0 / 120.0;

/// Steps one frame may take at most; a frame longer than that (a hitch, a
/// load) drops the rest rather than spiralling.
const MAX_STEPS: u32 = 8;

/// How a chain moves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringParams {
    /// How hard it springs back toward its animated pose, in cycles a
    /// second: 0 hangs free, 2 is hair, 6 a stiff antenna.
    pub stiffness: f32,
    /// How much of the world's gravity pulls it: 1 is all of it, 0 none.
    pub gravity: f32,
    /// The share of its speed it loses each sixtieth of a second, 0..1: how
    /// quickly a swing dies down. 0.2, the default, settles a free chain
    /// dropped from level in about a second; more drags it down slower.
    pub drag: f32,
    /// Furthest each segment may turn from its animated direction, radians.
    pub max_angle: f32,
    /// The chain's own thickness, for colliders: metres.
    pub hit_radius: f32,
    /// Where the pull back to the pose is measured from, 0..1. At 0 (the
    /// default) each segment springs toward its animated direction from its
    /// parent's swung position, so a long chain's bend adds up down its
    /// length; at 1 each tip springs toward its own animated place, measured
    /// from the chain's root, so a long chain keeps its shape. Between, a
    /// blend.
    pub root_stiffness: f32,
}

impl Default for SpringParams {
    fn default() -> Self {
        Self { stiffness: 1.5, gravity: 1.0, drag: 0.2, max_angle: 60f32.to_radians(), hit_radius: 0.02, root_stiffness: 0.0 }
    }
}

/// One chain: bones from its root down through each child, at least two.
#[derive(Clone, Debug, PartialEq)]
pub struct SpringChain {
    pub bones: Vec<usize>,
    pub params: SpringParams,
    pub enabled: bool,
}

/// A sphere (or a capsule, with `end`) riding a bone, that chains stay
/// out of. `offset` and `end` are in the bone's space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringCollider {
    pub bone: usize,
    pub offset: Vec3,
    pub radius: f32,
    pub end: Option<Vec3>,
}

#[derive(Clone, Debug)]
struct ChainState {
    def: SpringChain,
    /// Each segment's particle (the child's position), and where it was a
    /// step ago, in model space.
    tips: Vec<Vec3>,
    prev: Vec<Vec3>,
    /// Each segment's length this frame, from the animated pose.
    len: Vec<f32>,
    fresh: bool,
}

/// Every chain of one animated model, and the time it owes.
#[derive(Clone, Debug, Default)]
pub struct SpringSet {
    chains: Vec<ChainState>,
    colliders: Vec<SpringCollider>,
    owed: f32,
    /// Collider centres (and capsule ends) in model space, this frame.
    placed: Vec<(Vec3, Vec3, f32)>,
    /// The node's own acceleration this frame, in model space: felt by the
    /// chains as its opposite.
    accel: Vec3,
}

impl SpringSet {
    /// The chains and colliders, bone indices resolved. A chain of fewer
    /// than two bones moves nothing and is kept only so its index stays.
    pub fn new(chains: Vec<SpringChain>, colliders: Vec<SpringCollider>) -> Self {
        let chains = chains
            .into_iter()
            .map(|def| {
                let n = def.bones.len().saturating_sub(1);
                ChainState { def, tips: vec![Vec3::ZERO; n], prev: vec![Vec3::ZERO; n], len: vec![0.0; n], fresh: true }
            })
            .collect();
        Self { chains, colliders, owed: 0.0, placed: Vec::new(), accel: Vec3::ZERO }
    }

    pub fn chain_count(&self) -> usize {
        self.chains.len()
    }

    /// Turn chain `i` on or off. Off, it follows its animated pose exactly;
    /// back on, it starts from there with no swing.
    pub fn set_enabled(&mut self, i: usize, on: bool) {
        if let Some(c) = self.chains.get_mut(i)
            && c.def.enabled != on
        {
            c.def.enabled = on;
            c.fresh = true;
        }
    }

    /// Chain `i`'s bones and numbers, as bound.
    pub fn chain(&self, i: usize) -> Option<&SpringChain> {
        self.chains.get(i).map(|c| &c.def)
    }

    pub fn is_enabled(&self, i: usize) -> bool {
        self.chains.get(i).is_some_and(|c| c.def.enabled)
    }

    /// Start chain `i` (every chain, for `None`) from its animated pose with
    /// no speed: after a teleport, so it does not whip across the jump.
    pub fn reset(&mut self, i: Option<usize>) {
        // The jump itself is no acceleration the chain should feel.
        self.accel = Vec3::ZERO;
        for (k, c) in self.chains.iter_mut().enumerate() {
            if i.is_none_or(|i| i == k) {
                c.fresh = true;
            }
        }
    }

    /// How the node moved this frame, for the chains to feel. `turn` takes
    /// a direction in its model space last frame to the same world direction
    /// in its model space now (the rotation of `inv(now) * then`): the
    /// particles keep their place in the world as the body turns under them,
    /// so turning swings them. `accel` is the node's acceleration in the
    /// world, turned into model space: the chains feel its opposite, so
    /// speeding up swings them back and a steady speed, however fast (a
    /// rover, a planet's rails), leaves them hanging still.
    pub fn feel(&mut self, turn: Quat, accel: Vec3) {
        if turn != Quat::IDENTITY {
            for c in &mut self.chains {
                for p in c.tips.iter_mut().chain(c.prev.iter_mut()) {
                    *p = turn * *p;
                }
            }
        }
        self.accel = if accel.is_finite() { accel } else { Vec3::ZERO };
    }

    /// Where chain `i`'s last particle is, in model space.
    pub fn tip(&self, i: usize) -> Option<Vec3> {
        self.chains.get(i).and_then(|c| c.tips.last().copied())
    }

    /// Lay the chains where they are now into `pose` (and `world`, its
    /// model-space matrices) without stepping: so everything that reads a
    /// pose between two steps (scripts asking where a bone is, attachments)
    /// sees the same swung chain the picture shows. A chain not started yet
    /// is left as animated.
    pub fn apply(&self, skel: &Skeleton, pose: &mut [TransformTRS], world: &mut Vec<Mat4>) {
        if world.len() < skel.len() {
            return;
        }
        let mut wrote = false;
        for c in &self.chains {
            if c.def.enabled && !c.fresh && !c.tips.is_empty() {
                write_back(c, skel, pose, world);
                wrote = true;
            }
        }
        if wrote {
            skel.world_matrices(pose, world);
        }
    }

    /// Step `dt` seconds and write the chains into `pose`, then `world` (the
    /// model-space matrices of `pose`, as [`Skeleton::world_matrices`] makes
    /// them) again. `pose` and `world` arrive as the frame animated them;
    /// `gravity` is the world's, turned into model space.
    pub fn step(&mut self, skel: &Skeleton, pose: &mut [TransformTRS], world: &mut Vec<Mat4>, gravity: Vec3, dt: f32) {
        if self.chains.is_empty() || world.len() < skel.len() {
            return;
        }
        self.placed.clear();
        for c in &self.colliders {
            let Some(m) = world.get(c.bone) else { continue };
            let a = m.transform_point3(c.offset);
            let b = c.end.map_or(a, |e| m.transform_point3(e));
            let scale = m.to_scale_rotation_translation().0.max_element();
            self.placed.push((a, b, c.radius * scale));
        }
        for c in &mut self.chains {
            if !c.def.enabled || c.tips.is_empty() {
                continue;
            }
            for (i, w) in c.def.bones.windows(2).enumerate() {
                let (Some(a), Some(b)) = (world.get(w[0]), world.get(w[1])) else { continue };
                c.len[i] = (b.w_axis.truncate() - a.w_axis.truncate()).length();
            }
            if c.fresh {
                for (i, &b) in c.def.bones[1..].iter().enumerate() {
                    let p = world.get(b).map_or(Vec3::ZERO, |m| m.w_axis.truncate());
                    c.tips[i] = p;
                    c.prev[i] = p;
                }
                c.fresh = false;
            }
        }
        self.owed += dt.max(0.0);
        let mut steps = 0;
        while self.owed >= SPRING_STEP && steps < MAX_STEPS {
            for c in &mut self.chains {
                if c.def.enabled && !c.tips.is_empty() {
                    integrate(c, skel, pose, world, &self.placed, gravity - self.accel);
                }
            }
            self.owed -= SPRING_STEP;
            steps += 1;
        }
        if steps == MAX_STEPS {
            self.owed = 0.0;
        }
        let mut wrote = false;
        for c in &self.chains {
            if c.def.enabled && !c.tips.is_empty() {
                write_back(c, skel, pose, world);
                wrote = true;
            }
        }
        if wrote {
            skel.world_matrices(pose, world);
        }
    }
}

fn rotation_of(m: &Mat4) -> Quat {
    m.to_scale_rotation_translation().1
}

/// The world rotation of `bone`'s parent as the frame animated it.
fn parent_rotation(skel: &Skeleton, world: &[Mat4], bone: usize) -> Quat {
    skel.nodes.get(bone).and_then(|n| n.parent).and_then(|p| world.get(p)).map_or(Quat::IDENTITY, rotation_of)
}

/// Segment `i`'s animated direction, laid on its solved parent: the way it
/// would point if only the bones above it had moved.
fn animated_direction(pose: &[TransformTRS], c: &ChainState, i: usize, parent: Quat) -> (Quat, Vec3) {
    let (b, child) = (c.def.bones[i], c.def.bones[i + 1]);
    let target = parent * pose[b].r;
    let local = pose[child].t.try_normalize().unwrap_or(Vec3::Y);
    (target, (target * local).normalize_or(Vec3::NEG_Y))
}

fn integrate(c: &mut ChainState, skel: &Skeleton, pose: &[TransformTRS], world: &[Mat4], colliders: &[(Vec3, Vec3, f32)], gravity: Vec3) {
    let p = c.def.params;
    let h = SPRING_STEP;
    let k = (std::f32::consts::TAU * p.stiffness.max(0.0)).powi(2);
    let keep = (1.0 - p.drag.clamp(0.0, 1.0)).powf(h * 60.0);
    let mut parent = parent_rotation(skel, world, c.def.bones[0]);
    let mut head = world[c.def.bones[0]].w_axis.truncate();
    for i in 0..c.tips.len() {
        let (target, a) = animated_direction(pose, c, i, parent);
        let len = c.len[i];
        let (tip, prev) = (c.tips[i], c.prev[i]);
        // Where the pose wants this tip: along its animated direction from
        // where its parent swung to, or, measured from the root, its own
        // animated place (at this segment's length from the head), or a blend.
        let mut aim = head + a * len;
        if p.root_stiffness > 0.0 {
            let own = world[c.def.bones[i + 1]].w_axis.truncate();
            let from_root = head + (own - head).normalize_or(a) * len;
            aim = aim.lerp(from_root, p.root_stiffness.clamp(0.0, 1.0));
        }
        let pull = (aim - tip) * k + gravity * p.gravity;
        let mut next = tip + (tip - prev) * keep + pull * h * h;
        next = constrain(head, next, a, len, p.max_angle);
        for &(ca, cb, r) in colliders {
            let r = r + p.hit_radius;
            next = slide_out(next, ca, cb, r, aim);
            // The segment, not only its end: a midpoint inside the collider
            // means the segment crosses it, and the tip goes twice as far as
            // the midpoint must so the whole segment clears.
            let mid = (head + next) * 0.5;
            let cleared = push_out(mid, ca, cb, r);
            if cleared != mid {
                next += (cleared - mid) * 2.0;
            }
        }
        next = constrain(head, next, a, len, p.max_angle);
        c.prev[i] = tip;
        c.tips[i] = next;
        parent = Quat::from_rotation_arc(a, (next - head).normalize_or(a)) * target;
        head = next;
    }
}

/// `next` at `len` from `head`, turned no further than `max` from `a`.
fn constrain(head: Vec3, next: Vec3, a: Vec3, len: f32, max: f32) -> Vec3 {
    let mut dir = (next - head).normalize_or(a);
    let angle = dir.angle_between(a);
    if angle > max && angle.is_finite() {
        let axis = a.cross(dir).normalize_or(a.any_orthonormal_vector());
        dir = Quat::from_axis_angle(axis, max) * a;
    }
    head + dir * len
}

/// `p` pushed out of the capsule from `a` to `b` of radius `r`, then slid a
/// little round it toward `aim`, the place the pull wants it. Pushed straight
/// out alone, a chain swung past a shoulder stayed on the far side for good,
/// its pull going straight through the shoulder; sliding, it works its way
/// back round.
fn slide_out(p: Vec3, a: Vec3, b: Vec3, r: f32, aim: Vec3) -> Vec3 {
    let out = push_out(p, a, b, r);
    if out == p {
        return out;
    }
    let q = closest_on(out, a, b);
    let n = (out - q).normalize_or(Vec3::Y);
    let toward = aim - out;
    let along = toward - n * toward.dot(n);
    // A small step a solver tick, so the slide reads as motion, not a jump.
    let slid = out + along.clamp_length_max(r * 0.25);
    push_out(slid, a, b, r)
}

/// The point of the segment from `a` to `b` nearest `p`.
fn closest_on(p: Vec3, a: Vec3, b: Vec3) -> Vec3 {
    let ab = b - a;
    let t = if ab.length_squared() > 1e-12 { ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0) } else { 0.0 };
    a + ab * t
}

/// `p` pushed out of the capsule from `a` to `b` of radius `r` (a sphere
/// when `a == b`).
fn push_out(p: Vec3, a: Vec3, b: Vec3, r: f32) -> Vec3 {
    let ab = b - a;
    let t = if ab.length_squared() > 1e-12 { ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0) } else { 0.0 };
    let q = a + ab * t;
    let d = p - q;
    let dist = d.length();
    if dist >= r {
        return p;
    }
    q + d.normalize_or(Vec3::Y) * r
}

/// Turn each bone of the chain so its child points at its particle.
fn write_back(c: &ChainState, skel: &Skeleton, pose: &mut [TransformTRS], world: &[Mat4]) {
    let mut parent = parent_rotation(skel, world, c.def.bones[0]);
    let mut head = world[c.def.bones[0]].w_axis.truncate();
    for i in 0..c.tips.len() {
        let (target, a) = animated_direction(pose, c, i, parent);
        let solved = Quat::from_rotation_arc(a, (c.tips[i] - head).normalize_or(a)) * target;
        pose[c.def.bones[i]].r = (parent.inverse() * solved).normalize();
        parent = solved;
        head = c.tips[i];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SkelNode;

    /// A root at `top` with a chain of `n` segments of `len` hanging off it
    /// along `along` (its rest direction), each bone the next one's parent.
    fn chain(n: usize, len: f32, top: Vec3, along: Vec3) -> Skeleton {
        let mut nodes = vec![SkelNode {
            name: "root".into(),
            parent: None,
            rest: TransformTRS { t: top, ..TransformTRS::IDENTITY },
            pivot: Vec3::ZERO,
        }];
        for i in 0..=n {
            nodes.push(SkelNode {
                name: format!("b{i}"),
                parent: Some(i),
                rest: TransformTRS { t: if i == 0 { Vec3::ZERO } else { along * len }, ..TransformTRS::IDENTITY },
                pivot: Vec3::ZERO,
            });
        }
        Skeleton::new(nodes)
    }

    fn set_for(skel: &Skeleton, params: SpringParams, colliders: Vec<SpringCollider>) -> SpringSet {
        let bones: Vec<usize> = (1..skel.len()).collect();
        SpringSet::new(vec![SpringChain { bones, params, enabled: true }], colliders)
    }

    /// One frame: the animated pose (root turned by `turn`), stepped.
    fn frame(skel: &Skeleton, set: &mut SpringSet, turn: Quat, dt: f32) -> (Vec<TransformTRS>, Vec<Mat4>) {
        let mut pose = skel.rest_pose();
        pose[0].r = turn;
        let mut world = Vec::new();
        skel.world_matrices(&pose, &mut world);
        set.step(skel, &mut pose, &mut world, Vec3::new(0.0, -9.81, 0.0), dt);
        (pose, world)
    }

    fn free() -> SpringParams {
        SpringParams { stiffness: 0.0, max_angle: std::f32::consts::PI, ..Default::default() }
    }

    /// **A chain held out sideways, free of its pose, falls under gravity
    /// and hangs straight down, settled within a second.**
    #[test]
    fn a_free_chain_hangs_under_gravity_and_settles_within_a_second() {
        let skel = chain(5, 0.1, Vec3::ZERO, Vec3::X);
        let mut set = set_for(&skel, free(), vec![]);
        let mut last = Vec3::ZERO;
        let mut moved_late = 0.0f32;
        for f in 0..60 {
            frame(&skel, &mut set, Quat::IDENTITY, 1.0 / 60.0);
            let tip = set.tip(0).unwrap();
            if f >= 58 {
                moved_late = moved_late.max((tip - last).length());
            }
            last = tip;
        }
        let tip = set.tip(0).unwrap();
        let hang = tip.normalize().angle_between(Vec3::NEG_Y).to_degrees();
        assert!(hang < 6.0, "after a second the chain hangs {hang:.1}° off straight down (tip {tip})");
        // Settled: under 3 mm a frame (0.18 m/s) of sway left, from the
        // hardest start there is, dropped from level.
        assert!(moved_late < 0.003, "the tip still moved {moved_late:.4} m a frame after a second");
    }

    /// **The same motion settles in the same place at 30 fps and at 144.**
    #[test]
    fn the_swing_is_the_same_at_any_frame_rate() {
        let skel = chain(5, 0.1, Vec3::ZERO, Vec3::X);
        let run = |fps: f32| {
            let mut set = set_for(&skel, SpringParams { stiffness: 0.8, ..free() }, vec![]);
            let frames = (fps * 3.0).round() as usize;
            for _ in 0..frames {
                frame(&skel, &mut set, Quat::IDENTITY, 1.0 / fps);
            }
            set.tip(0).unwrap()
        };
        let (slow, fast) = (run(30.0), run(144.0));
        assert!((slow - fast).length() < 0.003, "30 fps settled at {slow}, 144 fps at {fast}");
    }

    /// **Hair hanging over a head stays outside it while the head turns.**
    /// A chain from the crown of a 0.11 m head, the head turning a quarter
    /// turn in a third of a second: no particle is ever inside the head.
    #[test]
    fn a_chain_stays_out_of_the_head_as_it_turns() {
        // The head is the root bone at the origin; the chain starts at its
        // crown and lies along +Z over its back.
        let mut skel = chain(6, 0.1, Vec3::ZERO, Vec3::Z);
        skel.nodes[1].rest.t = Vec3::new(0.0, 0.12, 0.0);
        let head = SpringCollider { bone: 0, offset: Vec3::ZERO, radius: 0.11, end: None };
        let mut set = set_for(&skel, SpringParams { stiffness: 0.5, ..Default::default() }, vec![head]);
        let mut closest = f32::INFINITY;
        for f in 0..90 {
            let turn = Quat::from_rotation_y((f as f32 / 20.0).min(1.0) * std::f32::consts::FRAC_PI_2);
            let (_, world) = frame(&skel, &mut set, turn, 1.0 / 60.0);
            for m in &world[2..] {
                closest = closest.min(m.w_axis.truncate().length());
            }
        }
        assert!(closest >= 0.11 + 0.02 - 1e-3, "a bone came within {closest:.4} m of the head's centre (radius 0.11)");
    }

    /// **A capsule pushes a chain out along its whole length.**
    #[test]
    fn a_capsule_holds_a_chain_off_along_its_length() {
        let p = push_out(Vec3::new(0.05, 0.5, 0.0), Vec3::ZERO, Vec3::Y, 0.1);
        assert!((p - Vec3::new(0.1, 0.5, 0.0)).length() < 1e-5, "{p}");
        let past = push_out(Vec3::new(0.0, 1.05, 0.0), Vec3::ZERO, Vec3::Y, 0.1);
        assert!((past - Vec3::new(0.0, 1.1, 0.0)).length() < 1e-5, "{past}");
    }

    /// **A reset starts the chain from its animated pose with no swing.**
    /// Thrown hard sideways (the frame a teleport would read as a huge
    /// acceleration), the chain is far off its pose; reset, it is back on
    /// its pose the next frame and stays there.
    #[test]
    fn a_reset_after_a_teleport_leaves_no_whip() {
        let skel = chain(4, 0.1, Vec3::ZERO, Vec3::NEG_Y);
        let mut set = set_for(&skel, SpringParams::default(), vec![]);
        for _ in 0..30 {
            frame(&skel, &mut set, Quat::IDENTITY, 1.0 / 60.0);
        }
        for _ in 0..3 {
            set.feel(Quat::IDENTITY, Vec3::new(2000.0, 0.0, 0.0));
            frame(&skel, &mut set, Quat::IDENTITY, 1.0 / 60.0);
        }
        assert!(set.tip(0).unwrap().x < -0.1, "the fixture: the throw did not swing the chain: {}", set.tip(0).unwrap());
        set.reset(None);
        let (_, world) = frame(&skel, &mut set, Quat::IDENTITY, 1.0 / 60.0);
        let want = world[skel.len() - 1].w_axis.truncate();
        let got = set.tip(0).unwrap();
        assert!((got - want).length() < 0.002, "after a reset the tip is at {got}, the pose at {want}");
        frame(&skel, &mut set, Quat::IDENTITY, 1.0 / 60.0);
        assert!(set.tip(0).unwrap().x.abs() < 0.002, "after the reset it swung on: {}", set.tip(0).unwrap());
    }

    /// **Speeding up swings the chain back; a steady speed leaves it
    /// hanging; a turn swings it.** The node moves along +X at a steady
    /// speed for a second (no acceleration), then speeds up for a frame.
    #[test]
    fn a_change_of_speed_swings_the_chain_and_a_steady_one_does_not() {
        let skel = chain(4, 0.1, Vec3::ZERO, Vec3::NEG_Y);
        let dt = 1.0 / 60.0;
        let mut set = set_for(&skel, SpringParams { stiffness: 0.5, ..Default::default() }, vec![]);
        let mut steady = 0.0f32;
        for f in 0..60 {
            set.feel(Quat::IDENTITY, Vec3::ZERO);
            frame(&skel, &mut set, Quat::IDENTITY, dt);
            if f >= 30 {
                steady = steady.max(set.tip(0).unwrap().x.abs());
            }
        }
        assert!(steady < 1e-4, "at a steady speed the tip still swung {steady:.5} m");
        // From standstill to 6 m/s in one frame: the chain trails (-X).
        set.feel(Quat::IDENTITY, Vec3::new(6.0 / dt, 0.0, 0.0));
        frame(&skel, &mut set, Quat::IDENTITY, dt);
        for _ in 0..5 {
            set.feel(Quat::IDENTITY, Vec3::ZERO);
            frame(&skel, &mut set, Quat::IDENTITY, dt);
        }
        assert!(set.tip(0).unwrap().x < -0.01, "after speeding up the tip did not trail: {}", set.tip(0).unwrap());
        // A body turning a quarter turn about Y under a chain held out along
        // +X: the tip keeps its place in the world for a moment, so in the
        // body's frame it swings the other way.
        let skel = chain(4, 0.1, Vec3::ZERO, Vec3::X);
        let mut held = set_for(&skel, SpringParams { stiffness: 3.0, gravity: 0.0, ..Default::default() }, vec![]);
        for _ in 0..30 {
            held.feel(Quat::IDENTITY, Vec3::ZERO);
            frame(&skel, &mut held, Quat::IDENTITY, dt);
        }
        held.feel(Quat::from_rotation_y(-0.3), Vec3::ZERO);
        frame(&skel, &mut held, Quat::IDENTITY, dt);
        let z = held.tip(0).unwrap().z;
        let want = (Quat::from_rotation_y(-0.3) * Vec3::X).z;
        assert!(z * want > 0.0 && z.abs() > 0.05, "a turn did not swing the tip: z {z}");
    }

    /// **Measured from the root, a long chain keeps its shape under gravity**
    /// (card 0700). An eight-segment chain held out level by its pose sags
    /// under gravity; springing each tip toward its own animated place
    /// (`root_stiffness` 1) it sags far less than springing each segment from
    /// its parent's swung position, where the bend adds up down the chain.
    #[test]
    fn root_relative_stiffness_keeps_a_long_chain_from_sagging() {
        let skel = chain(8, 0.1, Vec3::ZERO, Vec3::X);
        let sag = |root: f32| {
            let mut set = set_for(&skel, SpringParams { stiffness: 2.0, root_stiffness: root, max_angle: std::f32::consts::PI, ..Default::default() }, vec![]);
            for _ in 0..240 {
                frame(&skel, &mut set, Quat::IDENTITY, 1.0 / 60.0);
            }
            -set.tip(0).unwrap().y
        };
        let (parent, root) = (sag(0.0), sag(1.0));
        assert!(parent > 0.05, "the fixture: the chain hardly sagged at all ({parent:.3} m)");
        assert!(root < parent * 0.6, "measured from the root it sagged {root:.3} m against {parent:.3} m");
    }

    /// **A collider slides a particle round toward where it is pulled, not
    /// only straight out** (card 0700). A particle pressed into a sphere with
    /// its aim on the far side and off to one side: pushed straight out it
    /// lands on the near surface and stays there, its pull going through the
    /// sphere; slid, it moves round the surface toward the aim, closer to it
    /// and still outside, a step each solver tick: the way out for a chain
    /// pinned behind a shoulder by its own pull.
    #[test]
    fn a_collider_slides_a_particle_round_toward_its_aim() {
        let (c, r) = (Vec3::ZERO, 0.1);
        let pressed = Vec3::new(-0.09, 0.0, 0.0);
        let aim = Vec3::new(0.2, 0.05, 0.0);
        let straight = push_out(pressed, c, c, r);
        let slid = slide_out(pressed, c, c, r, aim);
        assert!(straight.length() >= r - 1e-5 && slid.length() >= r - 1e-5, "both must end outside the sphere: {straight} {slid}");
        assert!(slid.distance(aim) < straight.distance(aim) - 0.001, "sliding did not move it toward its aim: {slid} vs {straight}");
        // Pushed straight out again from where it landed, it makes no
        // progress at all: its pull is straight into the sphere.
        assert_eq!(push_out(straight, c, c, r), straight);
    }

    /// **A chain switched off follows its pose exactly.**
    #[test]
    fn a_disabled_chain_follows_its_pose() {
        let skel = chain(4, 0.1, Vec3::ZERO, Vec3::X);
        let mut set = set_for(&skel, free(), vec![]);
        set.set_enabled(0, false);
        let (pose, _) = frame(&skel, &mut set, Quat::IDENTITY, 0.5);
        assert!(pose.iter().zip(skel.rest_pose()).all(|(a, b)| a == &b), "a disabled chain moved its bones");
    }

    /// **The cost grows with the bones, no faster.** 144 bones (24 chains
    /// of 6) against 24 (4 of 6), the same frames: within 8×, not the 36×
    /// a per-pair cost would read.
    #[test]
    fn the_cost_grows_with_the_bones() {
        let time = |chains: usize| {
            let mut nodes = vec![SkelNode { name: "head".into(), parent: None, rest: TransformTRS::IDENTITY, pivot: Vec3::ZERO }];
            let mut defs = Vec::new();
            for c in 0..chains {
                let mut bones = Vec::new();
                for b in 0..7 {
                    let parent = if b == 0 { 0 } else { nodes.len() - 1 };
                    let a = c as f32 / chains as f32 * std::f32::consts::TAU;
                    let t = if b == 0 { Vec3::new(a.cos() * 0.1, 0.1, a.sin() * 0.1) } else { Vec3::new(0.0, -0.1, 0.0) };
                    bones.push(nodes.len());
                    nodes.push(SkelNode { name: format!("c{c}b{b}"), parent: Some(parent), rest: TransformTRS { t, ..TransformTRS::IDENTITY }, pivot: Vec3::ZERO });
                }
                defs.push(SpringChain { bones, params: SpringParams::default(), enabled: true });
            }
            let skel = Skeleton::new(nodes);
            let colliders = vec![SpringCollider { bone: 0, offset: Vec3::ZERO, radius: 0.11, end: None }; 6];
            let mut set = SpringSet::new(defs, colliders);
            let t = floptle_core::time::Instant::now();
            for _ in 0..200 {
                frame(&skel, &mut set, Quat::IDENTITY, 1.0 / 60.0);
            }
            t.elapsed().as_secs_f64()
        };
        let _ = time(4);
        let (small, big) = (time(4), time(24));
        assert!(big < small * 8.0, "24 chains took {:.2} ms, 4 took {:.2} ms", big * 1e3, small * 1e3);
    }
}
