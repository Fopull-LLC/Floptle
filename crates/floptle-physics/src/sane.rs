//! Keeping a bad number out of the step.
//!
//! A body whose speed or spin runs away (an uncapped drag torque that doubles
//! a piece of debris's spin every tick, say) passes through every finite value
//! and then overflows. Past that point its numbers are infinite or not a
//! number at all, and every query asked about it (a box of the ground ahead,
//! a march along its move, a cell walk round it) is asked about a box of
//! infinite size. One such walk never ended, and took the game with it.
//!
//! So the physics refuses a bad number three times over:
//! - at the door: [`crate::Sim`] refuses a non-finite force, impulse, torque,
//!   velocity or position and says so, naming the node;
//! - in the step: a body or an assembly whose state is non-finite, or past
//!   one of the bounds below, is stopped where it last was finite and put to
//!   sleep, and the step says so (see [`crate::PhysicsWorld::quarantined`]);
//! - in every walk: each loop whose length comes from geometry or time has a
//!   hard cap of its own, so a value that slipped past both of the above
//!   still costs a bounded step.
//!
//! The bounds are far beyond anything a game legitimately does, and are not
//! a speed limit: they only say where a number has stopped meaning anything
//! to an `f32` simulation.

use floptle_core::math::{Quat, Vec3};

/// Fastest a body or an assembly may move, metres a second, relative to the
/// simulation's floating origin: three times the speed of light. A craft in
/// low orbit moves at 7.8 km/s, a planet round its star at tens of km/s and
/// the fastest probe ever flown at under 200 km/s; planets on rails are
/// colliders the rails move, not bodies. A billion metres a second is 16,000
/// km in one 60 Hz tick, and its square (1e18) is still far inside an `f32`,
/// so every product the step forms from it stays finite.
pub const MAX_SPEED: f32 = 1.0e9;

/// Fastest a body or an assembly may turn, radians a second: about 160,000
/// turns a second. A car's wheel turns at around 200 rad/s, a turbine at a
/// few thousand, the fastest man-made rotors at under 100,000. A turn of a
/// million radians a second is 16,000 radians, 2,600 whole turns, in a tick,
/// which no frame can show and no contact can follow.
pub const MAX_SPIN: f32 = 1.0e6;

/// Farthest a body or an assembly may be from the floating origin, metres:
/// about 6,700 times the distance from the Earth to the Sun. A craft left
/// by Neptune while the origin follows a player at Earth is 4.5e12 m away
/// and still simulated; at 1e15 an `f32` position is a hundred thousand
/// kilometres between one value and the next, and its square (1e30) is the
/// largest the step forms that stays well inside an `f32`.
pub const MAX_REACH: f32 = 1.0e15;

/// Whether `v` is finite and no longer than `bound`. One comparison: the
/// squared length of anything holding a NaN or an infinity is NaN or
/// infinite, and neither is at most a finite bound.
#[inline]
pub(crate) fn within(v: Vec3, bound: f32) -> bool {
    v.length_squared() <= bound * bound
}

/// Whether `p`, `q` is a pose the step can work from.
#[inline]
pub(crate) fn pose_ok(p: Vec3, q: Quat) -> bool {
    within(p, MAX_REACH) && q.is_finite()
}

#[cfg(test)]
thread_local! {
    /// Leave every state as it is, however bad: the reference the guard that
    /// the checks change nothing for a sane body compares with. Test-only.
    pub(crate) static CHECKS_OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The first few of each kind of report are said in full, then one in this
/// many with the running count, so a script writing NaN every tick tells the
/// log once, not sixty times a second.
const SAID_IN_FULL: u64 = 8;
const SAID_EVERY: u64 = 1024;

fn say_this_one(n: u64) -> bool {
    n <= SAID_IN_FULL || n.is_multiple_of(SAID_EVERY)
}

/// One line the physics has to say about a node: written to the terminal as
/// it happens, and kept for the host to show where the game's developer
/// reads (the editor's Console, under the node's name).
#[derive(Clone, Debug, PartialEq)]
pub struct SanityReport {
    /// The body's node, or the assembly's root, by entity index.
    pub node: u32,
    /// What happened, as a sentence without the node in it.
    pub text: String,
}

/// What the physics refused and stopped, counted since it was built.
#[derive(Clone, Debug, Default)]
pub struct Sanity {
    refused: u64,
    quarantined: u64,
    /// The owners (entity indices) stopped since [`Self::take_quarantined`]
    /// last asked, in the order they were stopped.
    recent: Vec<u32>,
    /// What was said since [`Self::take_reports`] last asked: as rate-limited
    /// as the terminal, so a host showing every one floods nothing.
    reports: Vec<SanityReport>,
}

impl Sanity {
    /// A write of `what` to node `owner` was refused: `value` was not a
    /// finite number, or (a position) not one within [`MAX_REACH`] of the
    /// floating origin.
    pub(crate) fn refuse(&mut self, owner: u32, what: &str, value: impl std::fmt::Debug) {
        self.refused += 1;
        if say_this_one(self.refused) {
            let text = format!(
                "refused a {what} of {value:?}: not a finite number the step can use, so the body keeps what it had{}",
                self.tally(self.refused, "refused")
            );
            self.say(owner, text);
        }
    }

    /// Node `owner`'s body or assembly was found with a state the step cannot
    /// work from (`why`) and was stopped (`how`).
    pub(crate) fn quarantine(&mut self, owner: u32, why: &str, how: &str) {
        self.quarantined += 1;
        self.recent.push(owner);
        if say_this_one(self.quarantined) {
            let text = format!("{why}, so it was {how}{}", self.tally(self.quarantined, "stopped"));
            self.say(owner, text);
        }
    }

    fn say(&mut self, node: u32, text: String) {
        floptle_say::say_err!("physics: node {node}: {text}");
        self.reports.push(SanityReport { node, text });
    }

    fn tally(&self, n: u64, verb: &str) -> String {
        if n > SAID_IN_FULL {
            format!(" ({n} {verb} so far; from here on only every {SAID_EVERY}th is logged)")
        } else {
            String::new()
        }
    }

    /// What was said since the last call, oldest first.
    pub fn take_reports(&mut self) -> Vec<SanityReport> {
        std::mem::take(&mut self.reports)
    }

    /// Writes refused so far.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// Bodies and assemblies stopped so far.
    pub fn quarantined(&self) -> u64 {
        self.quarantined
    }

    /// The nodes stopped since the last call, oldest first.
    pub fn take_quarantined(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.recent)
    }
}

/// What is wrong with a state, for the log: the first of its parts that is
/// out of bounds, with its value, or `None` when nothing is.
pub(crate) fn fault(pos: Vec3, orient: Quat, vel: Vec3, spin: Vec3, pushes: bool) -> Option<String> {
    Some(if !within(pos, MAX_REACH) {
        if pos.is_finite() {
            format!("went {:e} m from the origin, past the farthest a body may be", pos.length())
        } else {
            format!("had a position of {pos:?}, not a finite number")
        }
    } else if !orient.is_finite() {
        format!("had an orientation of {orient:?}, not a finite number")
    } else if !within(vel, MAX_SPEED) {
        if vel.is_finite() {
            format!("ran away at {:e} m/s, past the fastest a body may move", vel.length())
        } else {
            format!("had a velocity of {vel:?}, not a finite number")
        }
    } else if !within(spin, MAX_SPIN) {
        if spin.is_finite() {
            format!("ran away at {:e} rad/s, past the fastest a body may turn", spin.length())
        } else {
            format!("had a spin of {spin:?}, not a finite number")
        }
    } else if !pushes {
        "had a force or torque that is not a finite number".to_string()
    } else {
        return None;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AnchoredCollider, Body, GravityField, PhysicsWorld};
    use floptle_core::math::DVec3;
    use floptle_core::{BodyKind, Parent, RigidBody, Transform, World};
    use std::time::Duration;

    /// Run `f` on its own thread; fail if it has not finished within `secs`,
    /// so a walk that never ends fails its test instead of hanging the suite.
    fn within(secs: u64, what: &str, f: impl FnOnce() + Send + 'static) {
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            f();
            let _ = tx.send(());
        });
        match rx.recv_timeout(Duration::from_secs(secs)) {
            Ok(()) => h.join().expect("the case panicked"),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Err(e) = h.join() {
                    std::panic::resume_unwind(e);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("{what}: did not return within {secs} s"),
        }
    }

    #[test]
    fn within_refuses_nan_and_both_infinities_and_takes_the_bound_itself() {
        assert!(within_bound(Vec3::new(3.0, 4.0, 0.0), 5.0));
        assert!(!within_bound(Vec3::new(3.0, 4.1, 0.0), 5.0));
        for x in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(!within_bound(Vec3::new(0.0, x, 0.0), 5.0), "{x}");
        }
        // Finite, but its square overflows: still refused, not wrapped.
        assert!(!within_bound(Vec3::splat(1e30), MAX_REACH));
    }

    fn within_bound(v: Vec3, b: f32) -> bool {
        super::within(v, b)
    }

    #[test]
    fn a_spam_of_reports_is_said_a_few_times_then_seldom() {
        let said = (1..=10_000u64).filter(|&n| say_this_one(n)).count();
        assert_eq!(said, SAID_IN_FULL as usize + 9);
    }

    /// Typical play on terrain: crates and balls dropped to tumble and roll
    /// down a slope, a two-part assembly thrown tumbling across it, a fast
    /// one flying in at 300 m/s, and a capsule walking. Every body's and
    /// assembly's state, every tick, as bits.
    fn typical(ticks: usize) -> Vec<Vec<u32>> {
        let tilt = floptle_core::math::Quat::from_rotation_z(0.25);
        let up = tilt * Vec3::Y;
        let mut field = floptle_field::ChunkField::new(0.75);
        field.fill_with(Vec3::new(-40.0, -12.0, -20.0), Vec3::new(40.0, 10.0, 20.0), |p| p.dot(up), |_| [0.5; 3]);
        let field: &'static floptle_field::ChunkField = Box::leak(Box::new(field));
        let mut ecs = World::default();
        for i in 0..6 {
            let e = ecs.spawn();
            let x = i as f64 * 1.7 - 5.0;
            ecs.insert(e, Transform::from_translation(DVec3::new(x, 3.0 + i as f64 * 0.4, 0.3 * i as f64)));
            let rb = if i % 2 == 0 {
                RigidBody { kind: BodyKind::Box, half_extents: [0.3, 0.2, 0.4], mass: 2.0, rotates: true, ..Default::default() }
            } else {
                RigidBody { kind: BodyKind::Sphere, radius: 0.35, mass: 1.0, rotates: true, ..Default::default() }
            };
            ecs.insert(e, rb);
        }
        let walker = ecs.spawn();
        ecs.insert(walker, Transform::from_translation(DVec3::new(6.0, 4.0, -3.0)));
        ecs.insert(walker, RigidBody { kind: BodyKind::Capsule, radius: 0.3, height: 1.8, mass: 70.0, ..Default::default() });
        let mut craft = Vec::new();
        for (at, half) in [(DVec3::new(-2.0, 5.0, 4.0), 0.4f32), (DVec3::new(10.0, 30.0, -6.0), 0.6)] {
            let root = ecs.spawn();
            ecs.insert(root, Transform::from_translation(at));
            ecs.insert(root, RigidBody { assembly: true, ..Default::default() });
            for dx in [-1.0f64, 1.0] {
                let part = ecs.spawn();
                ecs.insert(part, Transform::from_translation(DVec3::new(dx * half as f64, 0.0, 0.0)));
                ecs.insert(part, Parent(root));
                ecs.insert(part, RigidBody { kind: BodyKind::Box, half_extents: [half; 3], mass: 5.0, ..Default::default() });
            }
            craft.push(root.index());
        }
        let mut sim = crate::Sim::build_layered(
            &ecs,
            &[crate::TerrainVolume::new(DVec3::ZERO, field)],
            GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)),
            DVec3::ZERO,
            floptle_core::Layers::default(),
        );
        sim.set_compound_velocity(craft[0], Vec3::new(4.0, 1.0, -2.0));
        sim.set_compound_angular_velocity(craft[0], Vec3::new(3.0, 5.0, -2.0));
        sim.set_compound_velocity(craft[1], Vec3::new(-150.0, -260.0, 20.0));
        let mut out = Vec::new();
        for tick in 0..ticks {
            if tick < 120 {
                sim.set_body_velocity(walker.index(), Vec3::new(-3.0, 0.0, 1.0));
            }
            sim.step_tick(1.0 / 60.0, None);
            let w = &sim.world;
            let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
            let mut row = Vec::new();
            for b in &w.bodies {
                row.extend(bits(&[b.pos.to_array(), b.vel.to_array(), b.ang_vel.to_array()].concat()));
                row.extend(bits(&b.orient.to_array()));
                row.push(b.asleep as u32);
            }
            for c in &w.compounds {
                row.extend(bits(&[c.pos.to_array(), c.vel.to_array(), c.ang_vel.to_array()].concat()));
                row.extend(bits(&c.orient.to_array()));
                row.push(c.asleep as u32);
            }
            out.push(row);
        }
        assert_eq!(sim.world.sanity.quarantined(), 0, "a sane body was stopped");
        assert_eq!(sim.world.sanity.refused(), 0, "a sane write was refused");
        out
    }

    /// The checks never touch a body whose state is sane: typical play runs
    /// to the bit what it runs with them switched off. The comparison is
    /// only worth something if the scene moves, so it is made to.
    #[test]
    fn the_checks_change_nothing_for_sane_bodies() {
        CHECKS_OFF.with(|o| o.set(true));
        let off = typical(360);
        CHECKS_OFF.with(|o| o.set(false));
        let on = typical(360);
        assert_ne!(on.first(), on.last(), "fixture: nothing moved");
        for (tick, (a, b)) in off.iter().zip(&on).enumerate() {
            assert!(a == b, "tick {tick}: the checks changed where something went");
        }
    }

    /// A collider whose normal has no length (a field flat where a body sits
    /// in it): the contact pass grouped hits by facing the first one's
    /// normal, which a normal of no length never does, so the first hit
    /// never left the list and the step never returned.
    struct NoNormalFloor;
    impl crate::CollisionShape for NoNormalFloor {
        fn distance(&self, p: Vec3) -> f32 {
            p.y
        }
        fn normal(&self, _: Vec3) -> Vec3 {
            Vec3::ZERO
        }
    }

    #[test]
    fn a_contact_with_a_normal_of_no_length_still_ends() {
        within(20, "no-length normal", || {
            let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
            w.add_collider(Box::new(NoNormalFloor));
            let mut b = Body::sphere(Vec3::new(0.0, 0.2, 0.0), 0.5);
            b.rotates = true;
            w.add_body(b);
            for _ in 0..10 {
                w.step(1.0 / 120.0);
            }
        });
    }

    /// The ground a fast mover is heading for, asked ahead of it for a box
    /// of infinite size: 2^27 chunks a side once its corners saturate, whose
    /// count overflowed, wrapped to zero, passed the cap and walked 2^81
    /// chunks. And one far too big but finite, and one not a number.
    #[test]
    fn meshing_ahead_for_an_endless_box_returns() {
        within(20, "mesh ahead", || {
            let mut field = floptle_field::ChunkField::new(0.75);
            field.fill_slab(Vec3::new(-24.0, -6.0, -24.0), Vec3::new(24.0, 0.0, 24.0), 0.0, [0.5; 3]);
            let t = crate::ChunkTerrain::new(field);
            let inf = Vec3::splat(f32::INFINITY);
            for b in [(-inf, inf), (Vec3::ZERO, inf), (-inf, Vec3::ZERO), (Vec3::splat(-1e30), Vec3::splat(1e30)), (Vec3::NAN, Vec3::NAN)] {
                t.mesh_ahead(&[b]);
            }
        });
    }

    /// The faces in a box (what a decal is laid on) for a box of endless
    /// size, or one far too big, over terrain: a gather meshes every chunk
    /// in its box, so it would have tried to mesh 2^81 of them.
    #[test]
    fn a_face_gather_of_endless_size_returns() {
        within(20, "face gather", || {
            let mut field = floptle_field::ChunkField::new(0.75);
            field.fill_slab(Vec3::new(-24.0, -6.0, -24.0), Vec3::new(24.0, 0.0, 24.0), 0.0, [0.5; 3]);
            let cols = vec![AnchoredCollider::world(Box::new(crate::ChunkTerrain::new(field)))];
            let inf = Vec3::splat(f32::INFINITY);
            for (lo, hi) in [(-inf, inf), (Vec3::splat(-1e30), Vec3::splat(1e30)), (Vec3::NAN, Vec3::NAN)] {
                let _ = crate::triangles_in_box(&cols, lo, hi, !0);
                let _ = crate::unmeshed_ground_in_box(&cols, lo, hi, !0);
            }
            // A mark of ordinary size still gets its faces.
            let (tris, _) = crate::triangles_in_box(&cols, Vec3::new(-1.0, -1.0, -1.0), Vec3::new(1.0, 1.0, 1.0), !0);
            assert!(!tris.is_empty(), "a small box over the ground gave no faces");
        });
    }

    /// A ray along no direction at all, with no end, through a mesh: its walk
    /// crossed no cell wall, never left the grid and never ended.
    #[test]
    fn a_mesh_ray_that_is_not_a_number_ends() {
        within(20, "mesh ray", || {
            let verts = [Vec3::new(-5.0, 0.0, -5.0), Vec3::new(5.0, 0.0, -5.0), Vec3::new(0.0, 0.0, 5.0)];
            let m = crate::TriMeshCollider::new(&verts, &[0, 1, 2]);
            use crate::CollisionShape;
            for rd in [Vec3::NAN, Vec3::ZERO, Vec3::new(f32::NAN, -1.0, 0.0)] {
                let _ = m.ray_cast(Vec3::new(0.0, 1.0, 0.0), rd, f32::INFINITY);
            }
        });
    }

    /// A 2D box cast and overlap of endless size: a quarter unit between
    /// points asked for endless points.
    #[test]
    fn a_box_cast_of_endless_size_returns() {
        within(20, "box cast", || {
            let cols = vec![AnchoredCollider::world(Box::new(crate::Plane::ground(0.0)))];
            for half in [[f32::INFINITY; 2], [f32::NAN; 2], [1e30, 1e30]] {
                let _ = crate::boxcast(&cols, &[], Vec3::new(0.0, 5.0, 0.0), -Vec3::Y, half, 10.0, &[], !0, false);
                let _ = crate::overlap_box_2d(&cols, &[], Vec3::new(0.0, 5.0, 0.0), half, &[], !0);
            }
        });
    }
}
