//! Compound rigid bodies: one dynamic body built from MANY oriented shapes
//! (spheres / capsules / boxes), with composed mass, center of mass and inertia,
//! full 6-DOF motion (translation + rotation), per-shape contact attribution,
//! and runtime **split** — the physics half of assemblies that come apart:
//! multi-part vehicles, decoupling rocket stages, cranes, breakable structures.
//!
//! Stays in this engine's idiom (see `world.rs`): shapes are depenetrated
//! against the SDF collider set by sample points, there is no body-vs-body
//! pass, and stepping one compound alone is exactly its trajectory inside a
//! full step (the netcode prediction contract). What compounds add over
//! [`crate::Body`] is the rigid-body layer: contacts apply POSITIONAL and
//! VELOCITY corrections through the inverse inertia, so an off-center touch
//! torques the assembly — a rocket landing on one leg tips over.

use floptle_core::math::{Mat3, Quat, Vec3};

/// The geometry of one shape in a compound, in the shape's own local frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShapeGeom {
    /// A sphere of `radius`.
    Sphere { radius: f32 },
    /// A capsule along the shape's local Y axis: `radius` thick, end-sphere
    /// centers at ±`half_height`.
    Capsule { radius: f32, half_height: f32 },
    /// A box with the given half-extents.
    Box { half: Vec3 },
}

impl ShapeGeom {
    /// Solid-body inertia (diagonal, in the shape's local frame) for `mass`,
    /// as the classic closed forms. The capsule splits its mass by volume
    /// between the cylinder and the end spheres.
    fn local_inertia_diag(&self, mass: f32) -> Vec3 {
        match *self {
            ShapeGeom::Sphere { radius } => Vec3::splat(0.4 * mass * radius * radius),
            ShapeGeom::Box { half } => {
                let d = half * 2.0;
                Vec3::new(
                    mass / 12.0 * (d.y * d.y + d.z * d.z),
                    mass / 12.0 * (d.x * d.x + d.z * d.z),
                    mass / 12.0 * (d.x * d.x + d.y * d.y),
                )
            }
            ShapeGeom::Capsule { radius, half_height } => {
                let (r, h) = (radius.max(1e-4), 2.0 * half_height);
                let vc = core::f32::consts::PI * r * r * h;
                let vs = 4.0 / 3.0 * core::f32::consts::PI * r * r * r;
                let mc = mass * vc / (vc + vs);
                let ms = mass - mc;
                let iy = mc * r * r * 0.5 + ms * 0.4 * r * r;
                let ixz = mc * (h * h / 12.0 + r * r * 0.25)
                    + ms * (0.4 * r * r + h * h * 0.25 + 0.375 * h * r);
                Vec3::new(ixz, iy, ixz)
            }
        }
    }
}

/// One shape of a compound: geometry + its pose in the body frame + its share
/// of the mass + a stable caller-owned tag.
#[derive(Clone, Copy, Debug)]
pub struct CompoundShape {
    pub geom: ShapeGeom,
    /// Shape center in the body frame. Callers author offsets from their
    /// assembly origin; [`Compound::new`] re-expresses them about the center
    /// of mass (the frame the solver integrates in).
    pub offset: Vec3,
    /// Shape orientation in the body frame (a capsule's axis is its local Y).
    pub rot: Quat,
    pub mass: f32,
    /// Stable identifier for attribution — which PART took a contact, which
    /// shapes to detach on a split. The engine never interprets it (games use
    /// entity indices, part ids…).
    pub id: u64,
}

/// A contact a compound resolved this step — attributed to the SHAPE that took
/// it, with the normal impulse magnitude applied (the raw material for damage
/// and structural-stress systems).
#[derive(Clone, Copy, Debug)]
pub struct CompoundContact {
    /// Index into `PhysicsWorld::compounds`.
    pub compound: usize,
    /// Index into that compound's `shapes`.
    pub shape: usize,
    /// The shape's stable tag (`CompoundShape::id`).
    pub shape_id: u64,
    /// Index of the static collider hit (into `PhysicsWorld::colliders`).
    pub collider: usize,
    /// Contact point, sim frame.
    pub point: Vec3,
    /// Contact normal (out of the collider).
    pub normal: Vec3,
    /// Normal impulse magnitude applied (kg·m/s in sim units) — 0 for a
    /// purely positional resolve of an already-separating contact.
    pub impulse: f32,
}

/// A compound rigid body. Positions/velocities are sim-frame (origin-relative,
/// ADR-0015) like every other body; `pos` is the CENTER OF MASS — the frame
/// rigid dynamics integrates in. The assembly origin the caller authored
/// shapes around sits at `local_origin` in the body frame ([`Self::origin`]
/// maps it back to sim space for transform writeback).
#[derive(Clone, Debug)]
pub struct Compound {
    /// Center of mass, sim frame.
    pub pos: Vec3,
    /// `pos` at the start of the most recent step (render interpolation).
    pub prev_pos: Vec3,
    pub vel: Vec3,
    /// Body orientation (body frame → sim frame).
    pub orient: Quat,
    /// `orient` at the start of the most recent step.
    pub prev_orient: Quat,
    /// Angular velocity, sim frame (rad/s).
    pub ang_vel: Vec3,
    pub mass: f32,
    /// Shapes with offsets re-expressed about the center of mass.
    pub shapes: Vec<CompoundShape>,
    /// The caller's assembly origin, in the (CoM-centered) body frame.
    pub local_origin: Vec3,
    /// Inverse inertia tensor in the body frame (about the CoM).
    pub inv_inertia_local: Mat3,
    pub restitution: f32,
    pub friction: f32,
    pub use_gravity: bool,
    /// Inactive compounds are skipped entirely (networked-client authority).
    pub active: bool,
    /// Collision-layer bit index (same matrix semantics as [`crate::Body`]).
    pub layer: u8,
    /// Set each step when any contact opposes gravity.
    pub grounded: bool,
    /// Force accumulator (sim frame, applied at the CoM), cleared each step.
    pub(crate) force: Vec3,
    /// Torque accumulator (sim frame), cleared each step.
    pub(crate) torque: Vec3,
}

impl Compound {
    /// Build a compound from shapes authored around an assembly origin at the
    /// given sim-frame pose. Computes total mass, center of mass and inertia,
    /// and re-centers the shape offsets about the CoM. Shapes with
    /// non-positive mass contribute geometry but a tiny epsilon mass (a
    /// massless sensor fin still needs a defined tensor).
    pub fn new(pos: Vec3, orient: Quat, shapes: Vec<CompoundShape>) -> Self {
        assert!(!shapes.is_empty(), "a compound needs at least one shape");
        let mut mass = 0.0f32;
        let mut com = Vec3::ZERO;
        for s in &shapes {
            let m = s.mass.max(1e-4);
            mass += m;
            com += s.offset * m;
        }
        com /= mass;

        let mut shapes = shapes;
        for s in &mut shapes {
            s.offset -= com;
        }
        let inertia = compose_inertia(&shapes);
        Self {
            // The authored `pos` was the assembly origin; the CoM sits at the
            // rotated com offset from it.
            pos: pos + orient * com,
            prev_pos: pos + orient * com,
            vel: Vec3::ZERO,
            orient,
            prev_orient: orient,
            ang_vel: Vec3::ZERO,
            mass,
            shapes,
            local_origin: -com,
            inv_inertia_local: inertia.inverse(),
            restitution: 0.0,
            friction: 0.4,
            use_gravity: true,
            active: true,
            layer: 0,
            grounded: false,
            force: Vec3::ZERO,
            torque: Vec3::ZERO,
        }
    }

    /// The assembly origin (the pose callers authored shapes around), sim frame.
    pub fn origin(&self) -> Vec3 {
        self.pos + self.orient * self.local_origin
    }

    /// Sim-frame world center of one shape.
    pub fn shape_center(&self, i: usize) -> Vec3 {
        self.pos + self.orient * self.shapes[i].offset
    }

    /// Inverse inertia tensor in the SIM frame for the current orientation.
    pub fn world_inv_inertia(&self) -> Mat3 {
        let r = Mat3::from_quat(self.orient);
        r * self.inv_inertia_local * r.transpose()
    }

    /// Velocity of the body-fixed point currently at sim-frame `p`.
    pub fn point_velocity(&self, p: Vec3) -> Vec3 {
        self.vel + self.ang_vel.cross(p - self.pos)
    }

    /// Accumulate a force (sim frame) acting through the CoM for the next step.
    pub fn apply_force(&mut self, f: Vec3) {
        self.force += f;
    }

    /// Accumulate a force (sim frame) acting at sim-frame point `at` — the
    /// off-center part becomes torque. This is what engines/RCS/aero push with.
    pub fn apply_force_at(&mut self, f: Vec3, at: Vec3) {
        self.force += f;
        self.torque += (at - self.pos).cross(f);
    }

    /// Accumulate a pure torque (sim frame) for the next step.
    pub fn apply_torque(&mut self, t: Vec3) {
        self.torque += t;
    }

    /// Instantaneous impulse (sim frame) at sim-frame point `at`.
    pub fn apply_impulse_at(&mut self, imp: Vec3, at: Vec3) {
        self.vel += imp / self.mass;
        self.ang_vel += self.world_inv_inertia() * (at - self.pos).cross(imp);
    }

    /// Detach every shape whose id is in `ids` into a NEW compound, leaving
    /// the rest in `self` — a decoupler firing, a link snapping. Both halves
    /// keep their world pose and exchange momentum correctly: each new CoM
    /// inherits the velocity that body-fixed point had (`v + ω×r`), and both
    /// keep the angular velocity (splitting exerts no impulse; add separation
    /// springs at the game layer). Returns `None` — and changes nothing — if
    /// the split would leave either side empty.
    pub fn split(&mut self, ids: &[u64]) -> Option<Compound> {
        let going: Vec<usize> = (0..self.shapes.len())
            .filter(|&i| ids.contains(&self.shapes[i].id))
            .collect();
        if going.is_empty() || going.len() == self.shapes.len() {
            return None;
        }
        let mut detached_shapes = Vec::with_capacity(going.len());
        for &i in going.iter().rev() {
            detached_shapes.push(self.shapes.remove(i));
        }
        // Re-express both halves' shapes in WORLD-authored frames and rebuild,
        // so each recomputes its own CoM/inertia. Offsets are currently about
        // the OLD CoM; that old CoM (sim-frame `self.pos`) is the shared
        // assembly origin both rebuilds use.
        let old_pos = self.pos;
        let old_orient = self.orient;
        let old_vel = self.vel;
        let old_ang = self.ang_vel;
        let old_origin = self.local_origin;

        let rebuilt = |shapes: Vec<CompoundShape>| {
            let mut c = Compound::new(old_pos, old_orient, shapes);
            c.vel = old_vel + old_ang.cross(c.pos - old_pos);
            c.ang_vel = old_ang;
            c.prev_pos = c.pos;
            c
        };
        let mut kept = rebuilt(std::mem::take(&mut self.shapes));
        // The kept half keeps tracking the ORIGINAL assembly origin (its node):
        // that point sat at `old_origin` in the old CoM frame, and the new CoM
        // frame is shifted from it by the body-frame CoM delta.
        kept.local_origin = old_origin - old_orient.inverse() * (kept.pos - old_pos);
        let mut detached = rebuilt(detached_shapes);
        // The detached half is a NEW assembly: its origin is its own CoM (the
        // game layer roots a fresh node there via `origin()`).
        detached.local_origin = Vec3::ZERO;
        detached.restitution = self.restitution;
        detached.friction = self.friction;
        detached.use_gravity = self.use_gravity;
        detached.layer = self.layer;
        kept.restitution = self.restitution;
        kept.friction = self.friction;
        kept.use_gravity = self.use_gravity;
        kept.layer = self.layer;
        kept.active = self.active;
        detached.active = self.active;
        *self = kept;
        Some(detached)
    }

    /// The shape sample points for collision, like `Body::sample_centers` but
    /// per shape and orientation-aware: sphere → its center (radius r),
    /// capsule → both end-sphere centers (radius r), box → 8 corners + center
    /// (radius 0). Returns (points, count, inflate radius) for shape `i`.
    pub(crate) fn shape_samples(&self, i: usize) -> ([Vec3; 9], usize, f32) {
        let s = &self.shapes[i];
        let center = self.shape_center(i);
        let world_rot = self.orient * s.rot;
        let mut a = [center; 9];
        match s.geom {
            ShapeGeom::Sphere { radius } => (a, 1, radius),
            ShapeGeom::Capsule { radius, half_height } => {
                let axis = world_rot * Vec3::Y;
                a[0] = center - axis * half_height;
                a[1] = center + axis * half_height;
                (a, 2, radius)
            }
            ShapeGeom::Box { half } => {
                let mut n = 0;
                for &sx in &[-1.0f32, 1.0] {
                    for &sy in &[-1.0f32, 1.0] {
                        for &sz in &[-1.0f32, 1.0] {
                            a[n] = center
                                + world_rot * Vec3::new(sx * half.x, sy * half.y, sz * half.z);
                            n += 1;
                        }
                    }
                }
                a[8] = center;
                (a, 9, 0.0)
            }
        }
    }
}

/// Compose the body-frame inertia tensor (about the CoM — offsets must already
/// be CoM-relative) from per-shape solid inertias via rotation + parallel axis.
fn compose_inertia(shapes: &[CompoundShape]) -> Mat3 {
    let mut total = Mat3::ZERO;
    for s in shapes {
        let m = s.mass.max(1e-4);
        let diag = s.geom.local_inertia_diag(m);
        let r = Mat3::from_quat(s.rot);
        let local = r * Mat3::from_diagonal(diag) * r.transpose();
        let d = s.offset;
        let d2 = d.length_squared();
        // Parallel axis: m (|d|² E − d dᵀ).
        let pa = Mat3::from_cols(
            Vec3::new(m * (d2 - d.x * d.x), -m * d.x * d.y, -m * d.x * d.z),
            Vec3::new(-m * d.y * d.x, m * (d2 - d.y * d.y), -m * d.y * d.z),
            Vec3::new(-m * d.z * d.x, -m * d.z * d.y, m * (d2 - d.z * d.z)),
        );
        total = add_mat3(total, add_mat3(local, pa));
    }
    total
}

fn add_mat3(a: Mat3, b: Mat3) -> Mat3 {
    Mat3::from_cols(a.x_axis + b.x_axis, a.y_axis + b.y_axis, a.z_axis + b.z_axis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GravityField, PhysicsWorld, Plane};

    fn ball(offset: Vec3, radius: f32, mass: f32, id: u64) -> CompoundShape {
        CompoundShape { geom: ShapeGeom::Sphere { radius }, offset, rot: Quat::IDENTITY, mass, id }
    }

    fn boxs(offset: Vec3, half: Vec3, mass: f32, id: u64) -> CompoundShape {
        CompoundShape { geom: ShapeGeom::Box { half }, offset, rot: Quat::IDENTITY, mass, id }
    }

    fn ground_world() -> PhysicsWorld {
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(Plane::ground(0.0)));
        w
    }

    fn run(w: &mut PhysicsWorld, secs: f32) {
        let dt = 1.0 / 120.0;
        for _ in 0..(secs / dt) as usize {
            w.step(dt);
        }
    }

    #[test]
    fn single_sphere_inertia_matches_closed_form() {
        // One 2 kg sphere of radius 0.5 at the origin: I = 0.4·m·r² on every axis.
        let c = Compound::new(Vec3::ZERO, Quat::IDENTITY, vec![ball(Vec3::ZERO, 0.5, 2.0, 1)]);
        let want = 1.0 / (0.4 * 2.0 * 0.25);
        assert!((c.inv_inertia_local.x_axis.x - want).abs() < 1e-4);
        assert!((c.inv_inertia_local.y_axis.y - want).abs() < 1e-4);
        assert_eq!(c.mass, 2.0);
    }

    #[test]
    fn com_and_origin_bookkeeping() {
        // Two spheres, one heavy: the CoM sits toward the heavy one, and
        // `origin()` still maps back to the authored assembly origin.
        let authored = Vec3::new(10.0, 5.0, -2.0);
        let c = Compound::new(
            authored,
            Quat::IDENTITY,
            vec![ball(Vec3::new(-1.0, 0.0, 0.0), 0.5, 1.0, 1), ball(Vec3::new(1.0, 0.0, 0.0), 0.5, 3.0, 2)],
        );
        // CoM at (−1·1 + 1·3)/4 = +0.5 from the origin.
        assert!((c.pos - (authored + Vec3::new(0.5, 0.0, 0.0))).length() < 1e-5);
        assert!((c.origin() - authored).length() < 1e-5, "origin() must return the authored origin");
        // Parallel-axis: x-spin is cheap (masses on the axis), y-spin expensive.
        let ix = 1.0 / c.inv_inertia_local.x_axis.x;
        let iy = 1.0 / c.inv_inertia_local.y_axis.y;
        assert!(iy > ix * 2.0, "offset masses must dominate the perpendicular axes: ix={ix} iy={iy}");
    }

    #[test]
    fn compound_box_settles_on_ground() {
        let mut w = ground_world();
        let ci = w.add_compound(Compound::new(
            Vec3::new(0.0, 4.0, 0.0),
            Quat::IDENTITY,
            vec![boxs(Vec3::ZERO, Vec3::new(0.6, 0.4, 0.6), 5.0, 1)],
        ));
        run(&mut w, 4.0);
        let c = &w.compounds[ci];
        assert!(c.pos.is_finite(), "non-finite pos {:?}", c.pos);
        assert!((c.pos.y - 0.4).abs() < 0.1, "rests base-down, y={}", c.pos.y);
        assert!(c.vel.length() < 0.3, "at rest, vel={:?}", c.vel);
        assert!(c.grounded, "grounded");
        // Stays level: body Y still points up.
        let up = c.orient * Vec3::Y;
        assert!(up.y > 0.95, "stays level, up={up:?}");
    }

    #[test]
    fn lander_settles_on_its_legs() {
        // A hull box + four leg spheres below its corners: the assembly rests
        // ON THE LEGS (hull held above the ground), upright — the multi-shape
        // ground contact a rocket needs.
        let mut legs = vec![boxs(Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.5, 0.8, 0.5), 6.0, 100)];
        for (i, (sx, sz)) in [(-1.0f32, -1.0f32), (-1.0, 1.0), (1.0, -1.0), (1.0, 1.0)].iter().enumerate() {
            legs.push(ball(Vec3::new(sx * 0.7, 0.15, sz * 0.7), 0.15, 0.25, i as u64));
        }
        let mut w = ground_world();
        let ci = w.add_compound(Compound::new(Vec3::new(0.0, 3.0, 0.0), Quat::IDENTITY, legs));
        run(&mut w, 4.0);
        let c = &w.compounds[ci];
        let up = c.orient * Vec3::Y;
        assert!(up.y > 0.9, "lander stays upright, up={up:?}");
        // Origin (assembly base) hovers at leg-bottom height: legs at local y
        // 0.15 with r 0.15 → origin ~0 above ground contact height.
        let base = c.origin();
        assert!(base.y.abs() < 0.15, "base at leg height, y={}", base.y);
        // Contacts name the LEG shapes, not the hull.
        assert!(!w.compound_contacts.is_empty(), "legs must report contacts");
        assert!(
            w.compound_contacts.iter().all(|cc| cc.shape_id < 100),
            "only legs touch: {:?}",
            w.compound_contacts.iter().map(|cc| cc.shape_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn overhanging_box_tips_off_a_ledge() {
        // A platform ending at x=0; a box whose CoM overhangs the edge. Rigid
        // contact response must TORQUE it over the lip — it rotates and falls,
        // which no translation-only solver can do.
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(crate::BoxShape::new(
            Vec3::new(-2.5, 1.0, 0.0),
            Vec3::new(2.5, 1.0, 4.0),
            Quat::IDENTITY,
        )));
        let ci = w.add_compound(Compound::new(
            Vec3::new(0.35, 2.6, 0.0), // CoM past the x=0 edge
            Quat::IDENTITY,
            vec![boxs(Vec3::ZERO, Vec3::new(0.5, 0.3, 0.5), 3.0, 7)],
        ));
        run(&mut w, 3.0);
        let c = &w.compounds[ci];
        assert!(c.pos.is_finite());
        let up = c.orient * Vec3::Y;
        assert!(up.y < 0.9, "must have rotated off the lip, up={up:?}");
        assert!(c.pos.x > 0.5, "fell outward, x={}", c.pos.x);
        assert!(c.pos.y < 2.0, "fell below the platform top, y={}", c.pos.y);
    }

    #[test]
    fn offset_thrust_torques_the_assembly() {
        // Free space: a constant force applied off-center must accelerate AND
        // spin the compound — the honest CoT-vs-CoM physics ships fly by.
        let mut w = PhysicsWorld::new(GravityField::default());
        let ci = w.add_compound(Compound::new(
            Vec3::ZERO,
            Quat::IDENTITY,
            vec![boxs(Vec3::ZERO, Vec3::new(0.5, 1.0, 0.5), 4.0, 1)],
        ));
        let dt = 1.0 / 120.0;
        for _ in 0..120 {
            let at = w.compounds[ci].pos + w.compounds[ci].orient * Vec3::new(0.4, -1.0, 0.0);
            let f = w.compounds[ci].orient * Vec3::Y * 20.0;
            w.compounds[ci].apply_force_at(f, at);
            w.step(dt);
        }
        let c = &w.compounds[ci];
        assert!(c.vel.length() > 1.0, "thrust accelerates, vel={:?}", c.vel);
        // Off-center +Y push at +X lever arm → torque about −Z... r×F = (0.4,−1,0)×(0,20,0) = (0·0−0·20, 0−0, 0.4·20−0) = (0,0,8) initially → +Z spin.
        assert!(c.ang_vel.length() > 0.5, "offset thrust must spin it, ω={:?}", c.ang_vel);
        // A centered force must NOT spin it.
        let mut w2 = PhysicsWorld::new(GravityField::default());
        let c2 = w2.add_compound(Compound::new(
            Vec3::ZERO,
            Quat::IDENTITY,
            vec![boxs(Vec3::ZERO, Vec3::new(0.5, 1.0, 0.5), 4.0, 1)],
        ));
        for _ in 0..120 {
            let (pos, orient) = (w2.compounds[c2].pos, w2.compounds[c2].orient);
            w2.compounds[c2].apply_force_at(orient * Vec3::Y * 20.0, pos);
            w2.step(dt);
        }
        assert!(w2.compounds[c2].ang_vel.length() < 1e-4, "centered thrust is clean");
    }

    #[test]
    fn split_partitions_mass_and_conserves_point_velocity() {
        // A spinning, translating two-ball dumbbell splits: the detached ball's
        // new CoM velocity must equal the point velocity that spot had before.
        let mut c = Compound::new(
            Vec3::new(5.0, 0.0, 0.0),
            Quat::IDENTITY,
            vec![ball(Vec3::new(-1.0, 0.0, 0.0), 0.4, 2.0, 1), ball(Vec3::new(1.0, 0.0, 0.0), 0.4, 2.0, 2)],
        );
        c.vel = Vec3::new(0.0, 3.0, 0.0);
        c.ang_vel = Vec3::new(0.0, 0.0, 2.0); // spin about Z
        let predicted = c.point_velocity(Vec3::new(6.0, 0.0, 0.0)); // ball 2's center
        let detached = c.split(&[2]).expect("split succeeds");
        assert_eq!(detached.shapes.len(), 1);
        assert_eq!(detached.shapes[0].id, 2);
        assert!((detached.mass - 2.0).abs() < 1e-4);
        assert!((c.mass - 2.0).abs() < 1e-4);
        assert!((detached.pos - Vec3::new(6.0, 0.0, 0.0)).length() < 1e-4, "detached CoM at its ball");
        assert!((c.pos - Vec3::new(4.0, 0.0, 0.0)).length() < 1e-4, "kept CoM re-centers");
        assert!((detached.vel - predicted).length() < 1e-4, "momentum handoff: {:?} vs {predicted:?}", detached.vel);
        assert_eq!(detached.ang_vel, Vec3::new(0.0, 0.0, 2.0));
        // The kept half keeps tracking the original assembly origin.
        assert!((c.origin() - Vec3::new(5.0, 0.0, 0.0)).length() < 1e-4, "kept origin fixed: {:?}", c.origin());
        // Degenerate splits refuse.
        assert!(c.split(&[999]).is_none());
        assert!(c.split(&[1]).is_none(), "can't detach the last shape");
    }

    #[test]
    fn compound_step_is_deterministic() {
        let build = || {
            let mut w = ground_world();
            w.add_compound(Compound::new(
                Vec3::new(0.2, 3.0, -0.1),
                Quat::from_rotation_z(0.3),
                vec![
                    boxs(Vec3::ZERO, Vec3::new(0.5, 0.3, 0.4), 3.0, 1),
                    ball(Vec3::new(0.0, 0.6, 0.0), 0.3, 1.0, 2),
                ],
            ));
            w
        };
        let (mut a, mut b) = (build(), build());
        for _ in 0..480 {
            a.step(1.0 / 120.0);
            b.step(1.0 / 120.0);
        }
        assert_eq!(a.compounds[0].pos, b.compounds[0].pos, "bit-identical positions");
        assert_eq!(a.compounds[0].orient, b.compounds[0].orient, "bit-identical orientations");
        assert_eq!(a.compounds[0].vel, b.compounds[0].vel);
        assert!(a.compounds[0].pos.is_finite());
    }

    #[test]
    fn rebase_is_invisible_to_compounds() {
        let mut w = ground_world();
        let ci = w.add_compound(Compound::new(
            Vec3::new(0.0, 2.0, 0.0),
            Quat::IDENTITY,
            vec![boxs(Vec3::ZERO, Vec3::new(0.5, 0.4, 0.5), 2.0, 1)],
        ));
        run(&mut w, 3.0);
        let before = w.origin + w.compounds[ci].pos.as_dvec3();
        w.rebase(floptle_core::math::DVec3::new(4000.0, 0.0, -2000.0));
        run(&mut w, 1.0);
        let after = w.origin + w.compounds[ci].pos.as_dvec3();
        assert!((after - before).length() < 1e-2, "rebase moved the compound: {before:?} -> {after:?}");
    }
}
