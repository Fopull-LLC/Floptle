//! Contacts between moving things: a compound against another compound, or
//! against a plain dynamic body. Run once per step, after every body and
//! compound has taken its own step against the static world.
//!
//! Plain body against plain body is not resolved here; the sim still reports
//! those as touches without pushing either one.
//!
//! **Each side is pushed only if it resolves against the other.** A side is
//! pushed when it can move and its layer-matrix row includes the other's layer.
//! A driven body (the rollback driver steps it one tick at a time) and an
//! anchored compound never move here. Their trajectories stay exactly what
//! stepping them alone produces, which is what rollback replays depend on. The
//! other side still gets pushed off them. See ADR-0026.
//!
//! Shapes meet the way the static solver meets a collider: each side's sample
//! spheres are tested against the other side's exact signed distance, and each
//! penetrating sample gets a positional correction and an impulse through both
//! sides' inverse mass and inertia, so an off-centre hit spins both.

use floptle_core::math::{Mat3, Quat, Vec3};

use crate::body::BodyShape;
use crate::compound::{CompoundContact, ContactPeer, ShapeGeom};
use crate::world::PhysicsWorld;

/// One shape in world space, with an exact signed distance.
#[derive(Clone, Copy, Debug)]
enum Solid {
    Sphere { c: Vec3, r: f32 },
    Capsule { a: Vec3, b: Vec3, r: f32 },
    Box { c: Vec3, rot: Quat, half: Vec3 },
}

impl Solid {
    fn distance(&self, p: Vec3) -> f32 {
        match *self {
            Solid::Sphere { c, r } => (p - c).length() - r,
            Solid::Capsule { a, b, r } => {
                let ab = b - a;
                let t = ((p - a).dot(ab) / ab.length_squared().max(1e-12)).clamp(0.0, 1.0);
                (p - (a + ab * t)).length() - r
            }
            Solid::Box { c, rot, half } => {
                let q = (rot.conjugate() * (p - c)).abs() - half;
                q.max(Vec3::ZERO).length() + q.max_element().min(0.0)
            }
        }
    }

    /// Outward normal at `p`, by central differences, or `None` where the
    /// field is flat (the exact centre of a sphere).
    fn normal(&self, p: Vec3) -> Option<Vec3> {
        const E: f32 = 1e-3;
        let n = Vec3::new(
            self.distance(p + Vec3::X * E) - self.distance(p - Vec3::X * E),
            self.distance(p + Vec3::Y * E) - self.distance(p - Vec3::Y * E),
            self.distance(p + Vec3::Z * E) - self.distance(p - Vec3::Z * E),
        );
        n.try_normalize()
    }

    fn centre(&self) -> Vec3 {
        match *self {
            Solid::Sphere { c, .. } | Solid::Box { c, .. } => c,
            Solid::Capsule { a, b, .. } => (a + b) * 0.5,
        }
    }

    fn bound(&self) -> (Vec3, f32) {
        match *self {
            Solid::Sphere { c, r } => (c, r),
            Solid::Capsule { a, b, r } => ((a + b) * 0.5, (b - a).length() * 0.5 + r),
            Solid::Box { c, half, .. } => (c, half.length()),
        }
    }

    /// Sample centres tested against `other`, how many, and the radius around
    /// each. A sphere is its centre and a capsule its two end centres plus the
    /// point of its axis nearest `other`, so two capsules crossing mid-length
    /// still meet. A box is its 3×3×3 lattice: corners, edge midpoints, face
    /// centres and centre. Corners alone are what the static solver uses, and
    /// they miss two equal boxes meeting face to face: every corner then sits
    /// exactly on the other box's edge, at distance zero.
    fn samples(&self, other: &Solid) -> ([Vec3; 27], usize, f32) {
        let mut out = [self.centre(); 27];
        match *self {
            Solid::Sphere { c, r } => {
                out[0] = c;
                (out, 1, r)
            }
            Solid::Capsule { a, b, r } => {
                out[0] = a;
                out[1] = b;
                let d_at = |t: f32| other.distance(a.lerp(b, t));
                let mut best = (0.0f32, d_at(0.0));
                for i in 1..=8 {
                    let t = i as f32 / 8.0;
                    let d = d_at(t);
                    if d < best.1 {
                        best = (t, d);
                    }
                }
                if best.0 > 0.0 && best.0 < 1.0 {
                    out[2] = a.lerp(b, best.0);
                    (out, 3, r)
                } else {
                    (out, 2, r)
                }
            }
            Solid::Box { c, rot, half } => {
                let mut n = 0;
                for &sx in &[-1.0f32, 0.0, 1.0] {
                    for &sy in &[-1.0f32, 0.0, 1.0] {
                        for &sz in &[-1.0f32, 0.0, 1.0] {
                            out[n] = c + rot * Vec3::new(sx * half.x, sy * half.y, sz * half.z);
                            n += 1;
                        }
                    }
                }
                (out, 27, 0.0)
            }
        }
    }
}

/// A participant in the pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Body(usize),
    Compound(usize),
}

impl Side {
    fn peer(self) -> ContactPeer {
        match self {
            Side::Body(i) => ContactPeer::Body(i),
            Side::Compound(i) => ContactPeer::Compound(i),
        }
    }
}

/// How far one step may move a participant to separate it, in total, and how
/// far it may turn one. The same budgets the static pass gives a compound: a
/// deep overlap (two craft spawned inside each other) comes apart over a few
/// steps instead of launching both.
const PUSH_BUDGET: f32 = 0.35;
const ROT_BUDGET: f32 = 0.12;

/// Mass properties of one side at the moment of a contact.
struct Inertial {
    inv_mass: f32,
    /// Zero for a plain body: it does not rotate.
    inv_inertia: Mat3,
    pos: Vec3,
    vel: Vec3,
    ang_vel: Vec3,
    restitution: f32,
    friction: f32,
}

impl Inertial {
    fn point_vel(&self, p: Vec3) -> Vec3 {
        self.vel + self.ang_vel.cross(p - self.pos)
    }
    /// Generalised inverse mass along `n` at `p`: `1/m + ((I⁻¹(r×n))×r)·n`.
    fn w(&self, p: Vec3, n: Vec3) -> f32 {
        let r = p - self.pos;
        self.inv_mass + (self.inv_inertia * r.cross(n)).cross(r).dot(n)
    }
}

impl PhysicsWorld {
    fn pair_side_can_move(&self, s: Side) -> bool {
        match s {
            Side::Body(i) => !self.bodies[i].driven,
            Side::Compound(i) => !self.compounds[i].anchored,
        }
    }

    fn pair_layer(&self, s: Side) -> u8 {
        match s {
            Side::Body(i) => self.bodies[i].layer,
            Side::Compound(i) => self.compounds[i].layer,
        }
    }

    /// World-space solids of one side, current pose. A compound's carry the
    /// index of the shape they came from.
    fn pair_solids(&self, s: Side, out: &mut Vec<(usize, Solid)>) {
        out.clear();
        match s {
            Side::Body(i) => {
                let b = &self.bodies[i];
                let solid = match b.shape {
                    BodyShape::Sphere => Solid::Sphere { c: b.pos, r: b.radius },
                    BodyShape::Capsule { half_height } => Solid::Capsule {
                        a: b.pos - b.up * half_height,
                        b: b.pos + b.up * half_height,
                        r: b.radius,
                    },
                    BodyShape::Box { half } => Solid::Box { c: b.pos, rot: Quat::IDENTITY, half },
                };
                out.push((0, solid));
            }
            Side::Compound(ci) => {
                let c = &self.compounds[ci];
                for (si, s) in c.shapes.iter().enumerate() {
                    let centre = c.shape_center(si);
                    let rot = c.orient * s.rot;
                    let solid = match s.geom {
                        ShapeGeom::Sphere { radius } => Solid::Sphere { c: centre, r: radius },
                        ShapeGeom::Capsule { radius, half_height } => {
                            let axis = rot * Vec3::Y;
                            Solid::Capsule {
                                a: centre - axis * half_height,
                                b: centre + axis * half_height,
                                r: radius,
                            }
                        }
                        ShapeGeom::Box { half } => Solid::Box { c: centre, rot, half },
                    };
                    out.push((si, solid));
                }
            }
        }
    }

    /// A sphere around everything the side could touch.
    fn pair_bound(&self, s: Side, scratch: &mut Vec<(usize, Solid)>) -> (Vec3, f32) {
        self.pair_solids(s, scratch);
        let centre = match s {
            Side::Body(i) => self.bodies[i].pos,
            Side::Compound(i) => self.compounds[i].pos,
        };
        let reach = scratch
            .iter()
            .map(|(_, sol)| {
                let (c, r) = sol.bound();
                (c - centre).length() + r
            })
            .fold(0.0f32, f32::max);
        (centre, reach)
    }

    fn pair_inertial(&self, s: Side, moves: bool) -> Inertial {
        match s {
            Side::Body(i) => {
                let b = &self.bodies[i];
                Inertial {
                    inv_mass: if moves { 1.0 / b.mass.max(1e-4) } else { 0.0 },
                    inv_inertia: Mat3::ZERO,
                    pos: b.pos,
                    vel: b.vel,
                    ang_vel: Vec3::ZERO,
                    restitution: b.restitution,
                    friction: b.friction,
                }
            }
            Side::Compound(i) => {
                let c = &self.compounds[i];
                Inertial {
                    inv_mass: if moves { 1.0 / c.mass.max(1e-4) } else { 0.0 },
                    inv_inertia: if moves { c.world_inv_inertia() } else { Mat3::ZERO },
                    pos: c.pos,
                    vel: c.vel,
                    ang_vel: c.ang_vel,
                    restitution: c.restitution,
                    friction: c.friction,
                }
            }
        }
    }

    /// Move one side by a positional correction and a velocity impulse, both
    /// applied at `p`. `budget` is what is left of this step's push and turn.
    fn pair_apply(
        &mut self,
        s: Side,
        p: Vec3,
        shift: Vec3,
        impulse: Vec3,
        budget: &mut (f32, f32),
    ) {
        match s {
            Side::Body(i) => {
                let b = &mut self.bodies[i];
                let inv_m = 1.0 / b.mass.max(1e-4);
                let mut d = shift * inv_m;
                let len = d.length();
                if len > budget.0 {
                    d *= budget.0 / len.max(1e-9);
                }
                budget.0 -= d.length();
                let mut dv = impulse * inv_m;
                for axis in 0..3 {
                    if b.lock_pos[axis] {
                        crate::body::set_axis(&mut d, axis, 0.0);
                        crate::body::set_axis(&mut dv, axis, 0.0);
                    }
                }
                b.pos += d;
                b.vel += dv;
                if (d.length_squared() > 1e-12 || dv.length_squared() > 1e-12) && b.asleep {
                    b.asleep = false;
                    b.sleep_time = 0.0;
                }
            }
            Side::Compound(i) => {
                let c = &mut self.compounds[i];
                let inv_m = 1.0 / c.mass.max(1e-4);
                let inv_i = c.world_inv_inertia();
                let r = p - c.pos;
                let mut d = shift * inv_m;
                let len = d.length();
                if len > budget.0 {
                    d *= budget.0 / len.max(1e-9);
                }
                budget.0 -= d.length();
                c.pos += d;
                let mut rot = inv_i * r.cross(shift);
                let rl = rot.length();
                if rl > budget.1 {
                    rot *= budget.1 / rl.max(1e-9);
                }
                budget.1 -= rot.length();
                if rot.length_squared() > 1e-14 {
                    c.orient = (Quat::from_scaled_axis(rot) * c.orient).normalize();
                }
                c.vel += impulse * inv_m;
                c.ang_vel += inv_i * r.cross(impulse);
            }
        }
    }

    fn pair_mark_grounded(&mut self, s: Side, n: Vec3) {
        let pos = match s {
            Side::Body(i) => self.bodies[i].pos,
            Side::Compound(i) => self.compounds[i].pos,
        };
        let g = self.gravity.accel_at(pos, &self.colliders);
        let Some(up) = (-g).try_normalize() else { return };
        if n.dot(up) <= 0.5 {
            return;
        }
        match s {
            Side::Body(i) => {
                let b = &mut self.bodies[i];
                b.grounded = true;
                b.ground_normal = Some(n);
            }
            Side::Compound(i) => self.compounds[i].grounded = true,
        }
    }

    /// Resolve every compound-vs-compound and compound-vs-body overlap left
    /// after this step's solo moves. Contacts are reported through
    /// `compound_contacts`, one row per compound side, naming the other side
    /// as its peer.
    pub(crate) fn resolve_pairs(&mut self) {
        if self.compounds.is_empty() {
            return;
        }
        let mut sides: Vec<Side> = Vec::new();
        for (ci, c) in self.compounds.iter().enumerate() {
            if c.active && !c.shapes.is_empty() {
                sides.push(Side::Compound(ci));
            }
        }
        if sides.is_empty() {
            return;
        }
        let n_compounds = sides.len();
        for (bi, b) in self.bodies.iter().enumerate() {
            if b.active && !b.sensor && !b.kinematic && !b.pushbox_only {
                sides.push(Side::Body(bi));
            }
        }
        let mut scratch = Vec::new();
        let bounds: Vec<(Vec3, f32)> = sides.iter().map(|&s| self.pair_bound(s, &mut scratch)).collect();
        // Every pair has a compound in it, so the first side runs over the
        // compounds only: compounds x (compounds + bodies) sphere tests.
        let mut pairs: Vec<(usize, usize)> = Vec::new();
        for i in 0..n_compounds {
            for j in (i + 1)..sides.len() {
                let (ci, ri) = bounds[i];
                let (cj, rj) = bounds[j];
                if (ci - cj).length_squared() > (ri + rj) * (ri + rj) {
                    continue;
                }
                pairs.push((i, j));
            }
        }
        if pairs.is_empty() {
            return;
        }
        let mut budgets: Vec<(f32, f32)> = vec![(PUSH_BUDGET, ROT_BUDGET); sides.len()];
        let mut sa = Vec::new();
        let mut sb = Vec::new();
        for _pass in 0..2 {
            for &(i, j) in &pairs {
                let (a, b) = (sides[i], sides[j]);
                let (la, lb) = (self.pair_layer(a), self.pair_layer(b));
                let a_moves = self.pair_side_can_move(a) && (self.matrix[la as usize] >> lb) & 1 == 1;
                let b_moves = self.pair_side_can_move(b) && (self.matrix[lb as usize] >> la) & 1 == 1;
                if !a_moves && !b_moves {
                    continue;
                }
                self.pair_solids(a, &mut sa);
                self.pair_solids(b, &mut sb);
                for &(shape_a, solid_a) in &sa {
                    let (ca, ra) = solid_a.bound();
                    for &(shape_b, solid_b) in &sb {
                        let (cb, rb) = solid_b.bound();
                        if (ca - cb).length_squared() > (ra + rb) * (ra + rb) {
                            continue;
                        }
                        // A's samples into B, then B's into A. `n` always
                        // points from B towards A.
                        let (pa, na, rad_a) = solid_a.samples(&solid_b);
                        for &p in &pa[..na] {
                            let pen = rad_a - solid_b.distance(p);
                            if pen > 0.0
                                && let Some(n) = solid_b.normal(p)
                            {
                                let cp = p - n * rad_a;
                                self.pair_resolve((a, shape_a, a_moves, i), (b, shape_b, b_moves, j), cp, n, pen, &mut budgets);
                            }
                        }
                        let (pb, nb, rad_b) = solid_b.samples(&solid_a);
                        for &p in &pb[..nb] {
                            let pen = rad_b - solid_a.distance(p);
                            if pen > 0.0
                                && let Some(n) = solid_a.normal(p)
                            {
                                let cp = p - n * rad_b;
                                self.pair_resolve((a, shape_a, a_moves, i), (b, shape_b, b_moves, j), cp, -n, pen, &mut budgets);
                            }
                        }
                    }
                }
            }
        }
    }

    /// One contact between A and B at `cp`, with `n` pointing from B towards
    /// A and `pen` the overlap along it. Each side tuple is `(side, shape
    /// index, moves, slot in the budget list)`.
    fn pair_resolve(
        &mut self,
        (a, shape_a, a_moves, ia): (Side, usize, bool, usize),
        (b, shape_b, b_moves, ib): (Side, usize, bool, usize),
        cp: Vec3,
        n: Vec3,
        pen: f32,
        budgets: &mut [(f32, f32)],
    ) {
        let ma = self.pair_inertial(a, a_moves);
        let mb = self.pair_inertial(b, b_moves);
        let w = ma.w(cp, n) + mb.w(cp, n);
        if !(w.is_finite() && w > 1e-9) {
            return;
        }
        let v_rel = ma.point_vel(cp) - mb.point_vel(cp);
        let vn = v_rel.dot(n);
        let speed = (-vn).max(0.0);
        let speed_abs = v_rel.length();
        let lambda = pen / w;
        let mut j = 0.0;
        let mut impulse = Vec3::ZERO;
        if vn < 0.0 {
            let e = ma.restitution.max(mb.restitution);
            j = -(1.0 + e) * vn / w;
            impulse = n * j;
            // Friction against what is left of the sliding speed once the
            // normal impulse has landed.
            let va = ma.point_vel(cp) + n * (j * ma.inv_mass);
            let vb = mb.point_vel(cp) - n * (j * mb.inv_mass);
            let vr = va - vb;
            let vt = vr - n * vr.dot(n);
            let vt_len = vt.length();
            if vt_len > 1e-6 {
                let t = vt / vt_len;
                let wt = ma.w(cp, t) + mb.w(cp, t);
                if wt.is_finite() && wt > 1e-9 {
                    let mu = (ma.friction * mb.friction).max(0.0).sqrt();
                    let jt = (vt_len / wt).min(mu * j);
                    impulse -= t * jt;
                }
            }
        }
        if a_moves {
            let mut bud = budgets[ia];
            self.pair_apply(a, cp, n * lambda, impulse, &mut bud);
            budgets[ia] = bud;
            self.pair_mark_grounded(a, n);
        }
        if b_moves {
            let mut bud = budgets[ib];
            self.pair_apply(b, cp, -n * lambda, -impulse, &mut bud);
            budgets[ib] = bud;
            self.pair_mark_grounded(b, -n);
        }
        // One row per compound side, each naming the other as its peer, with
        // the normal pointing into that side.
        for (me, shape, other, normal) in [(a, shape_a, b, n), (b, shape_b, a, -n)] {
            if let Side::Compound(ci) = me {
                let shape_id = self.compounds[ci].shapes[shape].id;
                self.compound_contacts.push(CompoundContact {
                    compound: ci,
                    shape,
                    shape_id,
                    peer: other.peer(),
                    point: cp,
                    normal,
                    impulse: j,
                    speed,
                    speed_abs,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::compound::{Compound, CompoundShape};
    use crate::gravity::GravityField;

    fn boxs(offset: Vec3, half: Vec3, mass: f32, id: u64) -> CompoundShape {
        CompoundShape { geom: ShapeGeom::Box { half }, offset, rot: Quat::IDENTITY, mass, id }
    }

    fn space() -> PhysicsWorld {
        PhysicsWorld::new(GravityField::uniform(Vec3::ZERO))
    }

    fn crate_at(pos: Vec3, half: f32, mass: f32, id: u64) -> Compound {
        let mut c = Compound::new(pos, Quat::IDENTITY, vec![boxs(Vec3::ZERO, Vec3::splat(half), mass, id)]);
        c.use_gravity = false;
        c
    }

    fn run(w: &mut PhysicsWorld, secs: f32) {
        let dt = 1.0 / 120.0;
        for _ in 0..(secs / dt) as usize {
            w.step(dt);
        }
    }

    #[test]
    fn two_compounds_meeting_head_on_stop_each_other_and_keep_their_momentum() {
        // Unequal masses and speeds, so a solver that pushed both sides
        // equally, or only one, would give a different momentum.
        let mut w = space();
        let a = w.add_compound(crate_at(Vec3::new(-3.0, 0.0, 0.0), 0.5, 2.0, 1));
        let b = w.add_compound(crate_at(Vec3::new(3.0, 0.0, 0.0), 0.5, 6.0, 2));
        w.compounds[a].vel = Vec3::new(4.0, 0.0, 0.0);
        w.compounds[b].vel = Vec3::new(-1.0, 0.0, 0.0);
        let before = w.compounds[a].vel * 2.0 + w.compounds[b].vel * 6.0;
        run(&mut w, 3.0);
        let (ca, cb) = (&w.compounds[a], &w.compounds[b]);
        assert!(ca.pos.x < cb.pos.x, "passed through: a at {}, b at {}", ca.pos.x, cb.pos.x);
        assert!(cb.pos.x - ca.pos.x > 0.95, "still overlapping by {}", 1.0 - (cb.pos.x - ca.pos.x));
        let after = ca.vel * 2.0 + cb.vel * 6.0;
        assert!((after - before).length() < 0.05, "momentum {before:?} became {after:?}");
        // Compounds default to no restitution, so the two leave together at
        // the shared velocity (2·4 − 6·1) / 8 = 0.25.
        assert!((ca.vel.x - 0.25).abs() < 0.02, "light crate at {:?}, expected 0.25", ca.vel);
        assert!((cb.vel.x - 0.25).abs() < 0.02, "heavy crate at {:?}, expected 0.25", cb.vel);
    }

    #[test]
    fn an_off_centre_hit_spins_both_about_the_same_axis() {
        let mut w = space();
        let a = w.add_compound(crate_at(Vec3::new(-3.0, 0.0, 0.6), 0.5, 3.0, 1));
        let b = w.add_compound(crate_at(Vec3::new(0.0, 0.0, 0.0), 0.5, 3.0, 2));
        w.compounds[a].vel = Vec3::new(5.0, 0.0, 0.0);
        run(&mut w, 1.0);
        let (wa, wb) = (w.compounds[a].ang_vel.y, w.compounds[b].ang_vel.y);
        // A moving +x strikes B's +z half. B takes +x at r = (−0.5, 0, +z), a
        // torque of +y; A takes −x at r = (+0.5, 0, −z), also +y. Opposite
        // forces at opposite arms turn both the same way, as the +y angular
        // momentum A brought in says they must.
        assert!(wb > 0.05, "the struck crate should spin +y, ang_vel.y {wb}");
        assert!(wa > 0.05, "the striking crate should spin +y, ang_vel.y {wa}");
    }

    #[test]
    fn a_body_does_not_pass_through_a_parked_compound() {
        let mut w = space();
        let mut ship = crate_at(Vec3::ZERO, 1.0, 50.0, 1);
        ship.anchored = true;
        let ci = w.add_compound(ship);
        let mut ball = Body::sphere(Vec3::new(-4.0, 0.0, 0.0), 0.3);
        ball.use_gravity = false;
        ball.vel = Vec3::new(6.0, 0.0, 0.0);
        let bi = w.add_body(ball);
        run(&mut w, 2.0);
        let b = &w.bodies[bi];
        assert!(b.pos.x < -1.0 + 0.3 + 1e-3 + 0.02, "the ball went into the ship: x {}", b.pos.x);
        assert!(b.vel.x <= 0.0, "the ball is still driving into the ship: {:?}", b.vel);
        assert_eq!(w.compounds[ci].pos, Vec3::ZERO, "an anchored ship moved");
    }

    #[test]
    fn a_body_stands_on_a_compound_deck() {
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        let mut deck = Compound::new(
            Vec3::ZERO,
            Quat::IDENTITY,
            vec![boxs(Vec3::ZERO, Vec3::new(3.0, 0.5, 3.0), 100.0, 1)],
        );
        deck.anchored = true;
        w.add_compound(deck);
        let bi = w.add_body(Body::capsule(Vec3::new(0.0, 3.0, 0.0), 0.3, 1.8));
        run(&mut w, 3.0);
        let b = &w.bodies[bi];
        let feet = b.pos.y - b.height() * 0.5;
        assert!((feet - 0.5).abs() < 0.05, "feet at {feet}, deck top at 0.5");
        assert!(b.grounded, "standing on the deck should read as grounded");
    }

    #[test]
    fn a_light_body_moves_a_heavy_compound_only_a_little() {
        let mut w = space();
        let ci = w.add_compound(crate_at(Vec3::ZERO, 1.0, 100.0, 1));
        let mut ball = Body::sphere(Vec3::new(-3.0, 0.0, 0.0), 0.3);
        ball.use_gravity = false;
        ball.mass = 1.0;
        ball.vel = Vec3::new(5.0, 0.0, 0.0);
        w.add_body(ball);
        run(&mut w, 1.0);
        let v = w.compounds[ci].vel.x;
        // A perfectly inelastic 1 kg at 5 m/s into 100 kg gives ~0.05 m/s.
        assert!(v > 0.02 && v < 0.12, "the ship took {v} m/s from a 1 kg ball");
    }

    #[test]
    fn a_blow_near_the_end_of_a_rod_turns_it_rather_than_shoving_it() {
        // One point contact, so the impulse has a closed form. A 1 kg ball at
        // 4 m/s strikes a free 1 kg rod 1.8 m from its middle. With no
        // restitution the ball leaves at the speed of the point it struck:
        //   w = 1/m_ball + 1/m_rod + (r×n)·I⁻¹(r×n) = 1 + 1 + 1.8²/I_y
        //   I_y = (0.2² + 4²)/12, so w ≈ 4.42 and the ball keeps 4·(1 − 1/w) ≈ 3.10.
        // A solver that ignored the rod's rotation would use w = 2 and leave
        // the ball at 2.0, as if it had hit the rod's middle.
        let mut w = space();
        let mut rod = Compound::new(Vec3::ZERO, Quat::IDENTITY, vec![boxs(Vec3::ZERO, Vec3::new(0.1, 0.1, 2.0), 1.0, 1)]);
        rod.use_gravity = false;
        let ci = w.add_compound(rod);
        let mut ball = Body::sphere(Vec3::new(-2.0, 0.0, 1.8), 0.2);
        ball.use_gravity = false;
        ball.mass = 1.0;
        ball.vel = Vec3::new(4.0, 0.0, 0.0);
        let bi = w.add_body(ball);
        run(&mut w, 0.6);
        let v = w.bodies[bi].vel.x;
        assert!((v - 3.10).abs() < 0.2, "the ball kept {v} m/s, expected about 3.10");
        assert!(w.compounds[ci].ang_vel.y > 0.5, "the rod should turn +y, ang_vel {:?}", w.compounds[ci].ang_vel);
    }

    #[test]
    fn a_body_sliding_on_a_deck_is_slowed_by_friction() {
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        let mut deck = Compound::new(Vec3::ZERO, Quat::IDENTITY, vec![boxs(Vec3::ZERO, Vec3::new(4.0, 0.5, 4.0), 100.0, 1)]);
        deck.anchored = true;
        w.add_compound(deck);
        let mut crate_ = Body::boxx(Vec3::new(-2.0, 0.8, 0.0), Vec3::splat(0.3));
        crate_.vel = Vec3::new(3.0, 0.0, 0.0);
        let bi = w.add_body(crate_);
        run(&mut w, 2.0);
        // μ = √(0.3 · 0.4) ≈ 0.35 stops 3 m/s in under a second.
        let v = w.bodies[bi].vel.x;
        assert!(v.abs() < 0.2, "the crate is still sliding at {v} m/s");
        assert!(w.bodies[bi].pos.x > -1.5, "the crate never slid at all");
    }

    #[test]
    fn a_driven_body_is_never_moved_by_a_compound() {
        // The ADR-0026 certification case: a compound rams a driven body.
        // The driven body must land exactly where stepping it alone puts it,
        // and the compound must be the one that gives way.
        let make = |with_ship: bool| {
            let mut w = space();
            let mut fighter = Body::sphere(Vec3::ZERO, 0.5);
            fighter.use_gravity = false;
            fighter.driven = true;
            fighter.vel = Vec3::new(-0.5, 0.0, 0.0);
            let bi = w.add_body(fighter);
            let ci = with_ship.then(|| {
                let mut ship = crate_at(Vec3::new(-3.0, 0.0, 0.0), 0.5, 1.0, 1);
                ship.vel = Vec3::new(5.0, 0.0, 0.0);
                w.add_compound(ship)
            });
            (w, bi, ci)
        };
        let (mut alone, bi_alone, _) = make(false);
        let (mut rammed, bi, ci) = make(true);
        let dt = 1.0 / 120.0;
        let mut touched = false;
        for _ in 0..240 {
            alone.step_body(bi_alone, dt);
            rammed.step_body(bi, dt);
            rammed.step(dt);
            touched |= !rammed.compound_contacts.is_empty();
            assert_eq!(rammed.bodies[bi].pos, alone.bodies[bi_alone].pos, "the ship moved a driven body");
            assert_eq!(rammed.bodies[bi].vel, alone.bodies[bi_alone].vel, "the ship changed a driven body's velocity");
        }
        assert!(touched, "the ship never reached the fighter, so this proved nothing");
        let ship = &rammed.compounds[ci.unwrap()];
        assert!(ship.vel.x < 0.0, "the ship should rebound off the fighter, vel {:?}", ship.vel);
        assert!(ship.pos.x < rammed.bodies[bi].pos.x - 0.99, "the ship is inside the fighter");
    }

    #[test]
    fn the_layer_matrix_decides_who_is_pushed() {
        let setup = |a_row_has_b: bool, b_row_has_a: bool| {
            let mut w = space();
            let mut a = crate_at(Vec3::new(-2.0, 0.0, 0.0), 0.5, 2.0, 1);
            a.layer = 1;
            a.vel = Vec3::new(3.0, 0.0, 0.0);
            let mut b = crate_at(Vec3::ZERO, 0.5, 2.0, 2);
            b.layer = 2;
            let (ai, bi) = (w.add_compound(a), w.add_compound(b));
            w.matrix[1] = if a_row_has_b { !0 } else { !(1 << 2) };
            w.matrix[2] = if b_row_has_a { !0 } else { !(1 << 1) };
            run(&mut w, 1.5);
            (w.compounds[ai].pos.x, w.compounds[bi].pos.x)
        };
        let (a, b) = setup(false, false);
        assert!(a > b, "masked off both ways, they should pass through: a {a}, b {b}");
        let (a, b) = setup(true, false);
        assert!(b.abs() < 1e-6, "only A resolves against B, so B must not move: b {b}");
        assert!(a < b, "A resolves against B and should stop short of it: a {a}");
        let (a, b) = setup(false, true);
        assert!(b > 0.1, "only B resolves against A, so B is the one shoved: b {b}");
        assert!(a < b, "B should be pushed ahead of A: a {a}, b {b}");
    }

    #[test]
    fn contacts_name_the_part_that_was_hit_and_what_hit_it() {
        let mut w = space();
        // A two-part craft: the nose (id 10) leads, the tail (id 11) trails.
        let mut craft = Compound::new(
            Vec3::new(-4.0, 0.0, 0.0),
            Quat::IDENTITY,
            vec![
                boxs(Vec3::new(0.6, 0.0, 0.0), Vec3::splat(0.3), 1.0, 10),
                boxs(Vec3::new(-0.6, 0.0, 0.0), Vec3::splat(0.3), 1.0, 11),
            ],
        );
        craft.use_gravity = false;
        craft.vel = Vec3::new(4.0, 0.0, 0.0);
        let craft = w.add_compound(craft);
        let mut wall = crate_at(Vec3::ZERO, 0.5, 10.0, 20);
        wall.anchored = true;
        let wall = w.add_compound(wall);
        let mut rows = Vec::new();
        for _ in 0..240 {
            w.step(1.0 / 120.0);
            rows.extend(w.compound_contacts.iter().copied());
        }
        let on_craft: Vec<_> = rows.iter().filter(|r| r.compound == craft).collect();
        let on_wall: Vec<_> = rows.iter().filter(|r| r.compound == wall).collect();
        assert!(!on_craft.is_empty() && !on_wall.is_empty(), "both sides should report the hit");
        assert!(on_craft.iter().all(|r| r.shape_id == 10), "only the nose hit the wall");
        assert!(on_craft.iter().all(|r| r.peer == ContactPeer::Compound(wall)));
        assert!(on_wall.iter().all(|r| r.peer == ContactPeer::Compound(craft)));
        assert!(on_craft.iter().any(|r| r.speed > 3.0), "the hit should report its closing speed");
        // Each row's normal points into its own side.
        assert!(on_craft.iter().all(|r| r.normal.x < -0.9), "craft normals should face -x");
        assert!(on_wall.iter().all(|r| r.normal.x > 0.9), "wall normals should face +x");
    }

    #[test]
    fn far_apart_compounds_do_no_shape_work() {
        // Parked craft far apart must cost a sphere test each, not their
        // shape counts squared: nothing should be reported and nothing moved.
        let mut w = space();
        let parts: Vec<_> = (0..40).map(|i| boxs(Vec3::new(i as f32 * 0.5, 0.0, 0.0), Vec3::splat(0.2), 1.0, i)).collect();
        let mut rocket = Compound::new(Vec3::ZERO, Quat::IDENTITY, parts.clone());
        rocket.use_gravity = false;
        let mut other = Compound::new(Vec3::new(0.0, 100.0, 0.0), Quat::IDENTITY, parts);
        other.use_gravity = false;
        w.add_compound(rocket);
        w.add_compound(other);
        let before: Vec<_> = w.compounds.iter().map(|c| c.pos).collect();
        run(&mut w, 0.5);
        assert!(w.compound_contacts.is_empty());
        let after: Vec<_> = w.compounds.iter().map(|c| c.pos).collect();
        assert_eq!(before, after);
    }
}
