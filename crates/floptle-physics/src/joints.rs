//! Joints between bodies, solved once per step after every body has moved.
//!
//! Hard links (a pin, a rod, a rope at full stretch, a weld, a slider's rail)
//! are positional: each pass moves the two ends back together through both
//! bodies' mass and inertia, and the same correction goes into their speed,
//! so a chain hangs still rather than ringing. Soft ones (a spring, a wheel's
//! suspension, a target pull, a motor) act on speed, in a form that stays
//! stable at any stiffness the step can show.

use floptle_core::joint::JointKind;
use floptle_core::math::{Quat, Vec3};

use crate::world::PhysicsWorld;

/// What one end of a joint holds: a body, or an assembly's compound (the
/// whole rigid craft, so a hinge between two assemblies articulates a vessel
/// built from sub-assemblies), by index into the world's lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Body(usize),
    Compound(usize),
}

/// One joint in the simulation. Anchors are in their body's own frame (the
/// body's `pos`, before its turn; a compound's centre of mass and turn); a
/// joint to the world keeps its far end in `world`, sim frame.
#[derive(Clone, Debug)]
pub struct JointRt {
    /// The node the joint is on (entity index).
    pub owner: u32,
    pub kind: JointKind,
    pub a: Link,
    pub b: Option<Link>,
    pub anchor_a: Vec3,
    pub anchor_b: Vec3,
    /// The far end when there is no other body.
    pub world: Vec3,
    /// Axis in B's own frame (the world's, with no B).
    pub axis: Vec3,
    /// B's orientation relative to A's when play started, for welds,
    /// sliders and hinge limits.
    pub rel0: Quat,
    pub length: f32,
    pub limit: bool,
    /// Radians (hinge) or units (slider).
    pub min: f32,
    pub max: f32,
    pub motor: bool,
    /// Radians a second (hinge, wheel) or units a second (slider).
    pub motor_speed: f32,
    pub max_force: f32,
    pub stiffness: f32,
    pub damping: f32,
    pub break_force: f32,
    pub broken: bool,
    /// The break has been written back to the node's component.
    pub reported: bool,
    /// Sim-frame point a target joint pulls toward.
    pub target: Option<Vec3>,
    /// The largest force the joint carried last step, for breaking and for
    /// scripts to read.
    pub force: f32,
    /// Where the joint is, after the last step: a hinge's or a wheel's turn
    /// (radians), a slider's travel along its rail, a distance joint's
    /// length. And how fast that is changing, a second.
    pub position: f32,
    pub speed: f32,
    /// Drive the motor to `goal` (a hinge's turn in radians, a slider's
    /// travel) instead of at a speed: a servo or a piston. `motor_speed` is
    /// how fast it may go getting there.
    pub servo: bool,
    pub goal: f32,
    /// A wheel's steering, radians: its axle turned about the suspension
    /// axis from the other end's Z.
    pub steer: f32,
    /// The servo's error, summed over time: what holds it at its goal against
    /// a steady load (an arm's own weight), which the error alone cannot.
    pub servo_sum: f32,
    /// Made by a script (`assembly.grip`) rather than from a node's `Joint`:
    /// a rebuild of the component joints keeps it.
    pub scripted: bool,
    /// The far end rides this collider (by index) when there is no other
    /// body: `world` is then relative to the collider's anchor, so a grip on
    /// the ground of an orbiting planet moves with the planet.
    pub on_collider: Option<usize>,
    /// The node that collider belongs to, so the grip finds it again when
    /// the collider list is rebuilt (a chunk streaming in reorders it).
    pub collider_eid: Option<u32>,
    /// A braked wheel's parking spot: the collider it rests on and where on
    /// it, relative to its anchor. See `PhysicsWorld::tyre_grip`.
    pub park: Option<(usize, Vec3)>,
    /// What the joint was built from: its kind, the other node's name, the
    /// anchors, the axis and whether the pair collides. A component that no
    /// longer matches is rebuilt.
    pub built_from: JointShape,
}

/// The parts of a joint the simulation is built from, as opposed to the
/// settings it reads every step.
#[derive(Clone, Debug, PartialEq)]
pub struct JointShape {
    pub kind: JointKind,
    pub other: String,
    pub other_id: Option<u32>,
    pub anchor: [f32; 3],
    pub other_anchor: [f32; 3],
    pub axis: [f32; 3],
    pub collide_connected: bool,
}

impl JointRt {
    /// Whether this joint moves what it holds of its own accord, so that
    /// nothing it holds may sleep: a turning motor, a servo, a target pull,
    /// or a far end that rides a collider (a grip on the ground of a planet
    /// carried on its rails, where the two ends are carried separately and
    /// only the joint keeps them together).
    pub(crate) fn drives(&self) -> bool {
        matches!(self.kind, JointKind::Target)
            || (self.motor && (self.motor_speed != 0.0 || self.servo))
            || (self.b.is_none() && self.on_collider.is_some())
    }
}

impl JointShape {
    pub fn of(j: &floptle_core::joint::Joint) -> Self {
        Self {
            kind: j.kind,
            other: j.other.clone(),
            other_id: j.other_id,
            anchor: j.anchor,
            other_anchor: j.other_anchor,
            axis: j.axis,
            collide_connected: j.collide_connected,
        }
    }
}

/// One end of a joint, as the solver sees it this pass.
#[derive(Clone, Copy)]
struct End {
    /// World anchor point.
    p: Vec3,
    /// From the centre of mass to the anchor.
    r: Vec3,
    inv_m: f32,
    inv_i: floptle_core::math::Mat3,
}

impl End {
    /// Inverse mass at the anchor along `n`.
    fn w(&self, n: Vec3) -> f32 {
        let rn = self.r.cross(n);
        self.inv_m + rn.dot(self.inv_i * rn)
    }
    fn w_ang(&self, n: Vec3) -> f32 {
        n.dot(self.inv_i * n)
    }
}

/// Passes over the hard links each step. Sixteen, not eight: a heavy load at
/// the end of a chain of light links (a robotic arm holding cargo) rested
/// past its limits and shivered there at eight, and settles still at this.
const ITERATIONS: usize = 16;

impl PhysicsWorld {
    fn joint_end(&self, bi: Option<Link>, local: Vec3, world: Vec3) -> End {
        match bi {
            Some(Link::Body(i)) => {
                let b = &self.bodies[i];
                let p = b.pos + b.orient * local;
                let moves = b.live() && !b.kinematic && !b.driven && !b.pushbox_only;
                End {
                    p,
                    r: p - b.com(),
                    inv_m: if moves { 1.0 / b.mass.max(1e-4) } else { 0.0 },
                    inv_i: if moves { b.world_inv_inertia() } else { floptle_core::math::Mat3::ZERO },
                }
            }
            Some(Link::Compound(i)) => {
                let c = &self.compounds[i];
                let p = c.pos + c.orient * local;
                let moves = c.live() && !c.anchored;
                End {
                    p,
                    r: p - c.pos,
                    inv_m: if moves { 1.0 / c.mass.max(1e-4) } else { 0.0 },
                    inv_i: if moves { c.world_inv_inertia() } else { floptle_core::math::Mat3::ZERO },
                }
            }
            None => End { p: world, r: Vec3::ZERO, inv_m: 0.0, inv_i: floptle_core::math::Mat3::ZERO },
        }
    }

    /// The far end of joint `j` when it has no other body: its fixed world
    /// point, or a point riding the collider it was gripped to.
    fn world_end(&self, j: usize) -> Vec3 {
        let jt = &self.joints[j];
        match jt.on_collider.and_then(|ci| self.colliders.get(ci)) {
            Some(col) => col.offset() + jt.world,
            None => jt.world,
        }
    }

    fn orient_of(&self, bi: Option<Link>) -> Quat {
        match bi {
            Some(Link::Body(i)) => self.bodies[i].orient,
            Some(Link::Compound(i)) => self.compounds[i].orient,
            None => Quat::IDENTITY,
        }
    }

    /// Whether a compound end takes the solver's corrections.
    fn compound_moves(&self, i: usize) -> bool {
        let c = &self.compounds[i];
        c.live() && !c.anchored
    }

    /// Move body `bi` by `lin` at the point offset `r` from its centre of mass
    /// (a positional impulse), turning it as that implies, and give it the
    /// matching speed over `dt`.
    fn joint_shift(&mut self, bi: Option<Link>, r: Vec3, lin: Vec3, dt: f32) {
        let i = match bi {
            Some(Link::Body(i)) => i,
            Some(Link::Compound(i)) => {
                if !self.compound_moves(i) {
                    return;
                }
                let c = &mut self.compounds[i];
                let d = lin / c.mass.max(1e-4);
                let rot = c.world_inv_inertia() * r.cross(lin);
                c.pos += d;
                c.vel += d / dt;
                if rot.length_squared() > 1e-14 {
                    c.orient = (Quat::from_scaled_axis(rot) * c.orient).normalize();
                }
                c.ang_vel += rot / dt;
                // Only a sleeper is woken: an awake one's count is its
                // motion's to judge (see `PhysicsWorld::settle_group`).
                if c.asleep && (d.length_squared() > 1e-12 || rot.length_squared() > 1e-12) {
                    c.wake(crate::body::WakeReason::Joint);
                }
                return;
            }
            None => return,
        };
        let b = &mut self.bodies[i];
        if !b.live() || b.kinematic || b.driven || b.pushbox_only {
            return;
        }
        let inv_m = 1.0 / b.mass.max(1e-4);
        let inv_i = b.world_inv_inertia();
        let d = lin * inv_m;
        let mut lock = d;
        for axis in 0..3 {
            if b.lock_pos[axis] {
                crate::body::set_axis(&mut lock, axis, 0.0);
            }
        }
        b.pos += lock;
        b.vel += lock / dt;
        let rot = b.free_spin(inv_i * r.cross(lin));
        b.turn(rot);
        b.ang_vel += rot / dt;
        if b.asleep && (lock.length_squared() > 1e-12 || rot.length_squared() > 1e-12) {
            b.wake_for(crate::body::WakeReason::Joint);
        }
    }

    /// Turn body `bi` by the angular positional impulse `ang`.
    fn joint_turn(&mut self, bi: Option<Link>, ang: Vec3, dt: f32) {
        let i = match bi {
            Some(Link::Body(i)) => i,
            Some(Link::Compound(i)) => {
                if !self.compound_moves(i) {
                    return;
                }
                let c = &mut self.compounds[i];
                let rot = c.world_inv_inertia() * ang;
                if rot.length_squared() > 1e-14 {
                    c.orient = (Quat::from_scaled_axis(rot) * c.orient).normalize();
                }
                c.ang_vel += rot / dt;
                if c.asleep && rot.length_squared() > 1e-12 {
                    c.wake(crate::body::WakeReason::Joint);
                }
                return;
            }
            None => return,
        };
        let b = &mut self.bodies[i];
        if !b.live() || b.kinematic || b.driven || b.pushbox_only || !b.rotates {
            return;
        }
        let rot = b.free_spin(b.world_inv_inertia() * ang);
        b.turn(rot);
        b.ang_vel += rot / dt;
        if b.asleep && rot.length_squared() > 1e-12 {
            b.wake_for(crate::body::WakeReason::Joint);
        }
    }

    /// Change body `bi`'s speed by the impulse `j` at offset `r`.
    fn joint_impulse(&mut self, bi: Option<Link>, r: Vec3, j: Vec3) {
        let i = match bi {
            Some(Link::Body(i)) => i,
            Some(Link::Compound(i)) => {
                if !self.compound_moves(i) {
                    return;
                }
                let c = &mut self.compounds[i];
                let dv = j / c.mass.max(1e-4);
                let dw = c.world_inv_inertia() * r.cross(j);
                c.vel += dv;
                c.ang_vel += dw;
                if c.asleep && (dv.length_squared() > 1e-10 || dw.length_squared() > 1e-10) {
                    c.wake(crate::body::WakeReason::Joint);
                }
                return;
            }
            None => return,
        };
        let b = &mut self.bodies[i];
        if !b.live() || b.kinematic || b.driven || b.pushbox_only {
            return;
        }
        let inv_m = 1.0 / b.mass.max(1e-4);
        let mut dv = j * inv_m;
        for axis in 0..3 {
            if b.lock_pos[axis] {
                crate::body::set_axis(&mut dv, axis, 0.0);
            }
        }
        b.vel += dv;
        let dw = b.free_spin(b.world_inv_inertia() * r.cross(j));
        b.ang_vel += dw;
        if b.asleep && (dv.length_squared() > 1e-10 || dw.length_squared() > 1e-10) {
            b.wake_for(crate::body::WakeReason::Joint);
        }
    }

    /// Speed of an end's anchor point.
    fn point_vel(&self, bi: Option<Link>, r: Vec3) -> Vec3 {
        match bi {
            Some(Link::Body(i)) => self.bodies[i].vel + self.bodies[i].ang_vel.cross(r),
            Some(Link::Compound(i)) => self.compounds[i].vel + self.compounds[i].ang_vel.cross(r),
            None => Vec3::ZERO,
        }
    }

    fn ang_vel_of(&self, bi: Option<Link>) -> Vec3 {
        match bi {
            Some(Link::Body(i)) => self.bodies[i].ang_vel,
            Some(Link::Compound(i)) => self.compounds[i].ang_vel,
            None => Vec3::ZERO,
        }
    }

    /// Bring two anchor points together along `c` (B minus A) as far as
    /// `amount` of it, returning the positional impulse spent.
    fn close_gap(&mut self, j: usize, dir: Vec3, gap: f32, dt: f32) -> f32 {
        let (a, b) = (self.joints[j].a, self.joints[j].b);
        let ea = self.joint_end(Some(a), self.joints[j].anchor_a, Vec3::ZERO);
        let eb = self.joint_end(b, self.joints[j].anchor_b, self.world_end(j));
        let w = ea.w(dir) + eb.w(dir);
        if !w.is_finite() || w <= 1e-9 {
            return 0.0;
        }
        let lambda = gap / w;
        self.joint_shift(Some(a), ea.r, dir * lambda, dt);
        self.joint_shift(b, eb.r, -dir * lambda, dt);
        lambda
    }

    /// Turn B by the rotation vector `err` (and A the other way), shared by
    /// their inertia, returning the angular impulse spent.
    fn close_turn(&mut self, j: usize, err: Vec3, dt: f32) -> f32 {
        let len = err.length();
        if len < 1e-7 {
            return 0.0;
        }
        let n = err / len;
        let (a, b) = (self.joints[j].a, self.joints[j].b);
        let ea = self.joint_end(Some(a), self.joints[j].anchor_a, Vec3::ZERO);
        let eb = self.joint_end(b, self.joints[j].anchor_b, self.world_end(j));
        let w = ea.w_ang(n) + eb.w_ang(n);
        if !w.is_finite() || w <= 1e-9 {
            return 0.0;
        }
        let lambda = len / w;
        self.joint_turn(Some(a), -n * lambda, dt);
        self.joint_turn(b, n * lambda, dt);
        lambda
    }

    /// The rotation vector that takes B's orientation to where the joint
    /// wants it relative to A's: zero when they are as they started.
    fn weld_error(&self, j: usize) -> Vec3 {
        let jt = &self.joints[j];
        let qa = self.orient_of(Some(jt.a));
        let qb = self.orient_of(jt.b);
        let want = qa * jt.rel0;
        let mut d = want * qb.inverse();
        if d.w < 0.0 {
            d = -d;
        }
        Vec3::new(d.x, d.y, d.z) * 2.0
    }

    /// How far B has turned relative to A about the joint's axis since play
    /// started, in radians.
    /// A ball joint's lean: the angle between this node's axis (as it sat
    /// against the other's when play started) and the other's, and the axis
    /// that turns the first onto the second.
    fn ball_lean(&self, j: usize) -> (f32, Vec3) {
        let jt = &self.joints[j];
        let ax = jt.axis.normalize_or(Vec3::Z);
        let axis_a = self.orient_of(Some(jt.a)) * (jt.rel0 * ax);
        let axis_b = self.orient_of(jt.b) * ax;
        let lean = axis_a.dot(axis_b).clamp(-1.0, 1.0).acos();
        (lean, axis_a.cross(axis_b).normalize_or_zero())
    }

    /// A wheel's axle, sim frame: Z of what it hangs from, turned about the
    /// suspension axis by its steering.
    fn wheel_axle(&self, jt: &JointRt) -> Vec3 {
        let q = self.orient_of(jt.b);
        let axle = q * Vec3::Z;
        if jt.steer == 0.0 {
            return axle;
        }
        let about = q * jt.axis.normalize_or(Vec3::Y);
        Quat::from_axis_angle(about, jt.steer) * axle
    }

    fn hinge_angle(&self, j: usize) -> f32 {
        let jt = &self.joints[j];
        let qa = self.orient_of(Some(jt.a));
        let qb = self.orient_of(jt.b);
        let rel = (qa * jt.rel0).inverse() * qb;
        let ax = jt.axis.normalize_or(Vec3::Z);
        let s = Vec3::new(rel.x, rel.y, rel.z).dot(ax);
        // The turn of A against B is what the limit holds, so it is negated.
        -2.0 * s.atan2(rel.w)
    }

    /// Solve every live joint: the hard links positionally over a few passes,
    /// then the springs and motors on speed. A joint that carried more than
    /// its `break_force` breaks.
    pub(crate) fn solve_joints(&mut self, dt: f32) {
        if self.joints.is_empty() || dt <= 0.0 {
            return;
        }
        // A joint whose ends are both at rest, one of them asleep, rests
        // too: nothing on either side has moved since they fell asleep
        // together (see `PhysicsWorld::settle`), and what the joint would
        // correct is only what was left when they did, which woke them every
        // step. It keeps the force it last carried. A joint that drives (a
        // turning motor, a servo, a target pull, a grip riding a collider)
        // wakes what it drives, as its first correction always did.
        let resting: Vec<bool> = if !crate::world::reference_settle() && self.joints.iter().any(|j| self.joint_end_asleep(Some(j.a)) || self.joint_end_asleep(j.b)) {
            (0..self.joints.len()).map(|j| self.joint_rests(j)).collect()
        } else {
            Vec::new()
        };
        let rests = |j: usize| resting.get(j).copied().unwrap_or(false);
        for j in 0..self.joints.len() {
            if !rests(j) {
                self.joints[j].force = 0.0;
            }
        }
        for _ in 0..ITERATIONS {
            for j in 0..self.joints.len() {
                if self.joints[j].broken || rests(j) {
                    continue;
                }
                let spent = self.solve_hard(j, dt);
                let jt = &mut self.joints[j];
                jt.force = jt.force.max(spent / (dt * dt));
            }
        }
        for j in 0..self.joints.len() {
            if self.joints[j].broken || rests(j) {
                continue;
            }
            let spent = self.solve_soft(j, dt);
            let (position, speed) = self.joint_position(j);
            let jt = &mut self.joints[j];
            jt.force = jt.force.max(spent / dt);
            jt.position = position;
            jt.speed = speed;
            if jt.break_force > 0.0 && jt.force > jt.break_force {
                jt.broken = true;
            }
        }
    }

    /// Whether end `l` of a joint is asleep.
    fn joint_end_asleep(&self, l: Option<Link>) -> bool {
        match l {
            Some(Link::Compound(i)) => self.compounds.get(i).is_some_and(|c| c.asleep),
            Some(Link::Body(i)) => self.bodies.get(i).is_some_and(|b| b.asleep),
            None => false,
        }
    }

    /// Whether end `l` of a joint cannot move this step: asleep, pinned,
    /// moved by something else, or the world.
    fn joint_end_rests(&self, l: Option<Link>) -> bool {
        match l {
            Some(Link::Compound(i)) => self.compounds.get(i).is_none_or(|c| c.asleep || !c.live() || c.anchored),
            Some(Link::Body(i)) => self.bodies.get(i).is_none_or(|b| b.asleep || !b.live() || b.kinematic || b.driven || b.pushbox_only),
            None => true,
        }
    }

    /// Whether joint `j` rests this step (see `solve_joints`), waking its
    /// sleeping ends instead when it drives them.
    fn joint_rests(&mut self, j: usize) -> bool {
        let jt = &self.joints[j];
        let (a, b) = (Some(jt.a), jt.b);
        if jt.broken || !(self.joint_end_asleep(a) || self.joint_end_asleep(b)) {
            return false;
        }
        if !(self.joint_end_rests(a) && self.joint_end_rests(b)) {
            return false;
        }
        let drives = jt.drives();
        if !drives {
            return true;
        }
        for l in [a, b].into_iter().flatten() {
            match l {
                Link::Compound(i) => self.compounds[i].wake(crate::body::WakeReason::Joint),
                Link::Body(i) => {
                    let b = &mut self.bodies[i];
                    if b.asleep {
                        b.wake_for(crate::body::WakeReason::Joint);
                    }
                }
            }
        }
        false
    }

    /// Where joint `j` is and how fast that is changing: a hinge's or a
    /// wheel's turn (radians), a slider's travel, a distance joint's length.
    fn joint_position(&self, j: usize) -> (f32, f32) {
        let jt = &self.joints[j];
        let (ea, eb) = self.ends(j);
        let axis_w = self.orient_of(jt.b) * jt.axis.normalize_or(Vec3::Z);
        let rel_v = self.point_vel(Some(jt.a), ea.r) - self.point_vel(jt.b, eb.r);
        match jt.kind {
            JointKind::Hinge => {
                let w = (self.ang_vel_of(Some(jt.a)) - self.ang_vel_of(jt.b)).dot(axis_w);
                (self.hinge_angle(j), w)
            }
            // A wheel's travel on its suspension, and its spin about the axle.
            JointKind::Wheel => {
                let axle = self.wheel_axle(jt);
                let w = (self.ang_vel_of(Some(jt.a)) - self.ang_vel_of(jt.b)).dot(axle);
                (axis_w.dot(ea.p - eb.p), w)
            }
            JointKind::Slider => (-axis_w.dot(eb.p - ea.p), rel_v.dot(axis_w)),
            // How far this node's axis leans from the other's, and how fast
            // the two are turning against each other, every way at once.
            JointKind::Ball => {
                let w = (self.ang_vel_of(Some(jt.a)) - self.ang_vel_of(jt.b)).length();
                (self.ball_lean(j).0, w)
            }
            JointKind::Distance | JointKind::Rope | JointKind::Spring => {
                let c = eb.p - ea.p;
                (c.length(), -c.normalize_or_zero().dot(rel_v))
            }
            _ => (0.0, 0.0),
        }
    }

    /// The speed a motor drives at: its own, or for a servo the speed that
    /// closes on its goal, no faster than its own.
    fn motor_command(&mut self, j: usize, dt: f32) -> f32 {
        if !self.joints[j].servo {
            self.joints[j].servo_sum = 0.0;
            return self.joints[j].motor_speed;
        }
        // How quickly the remaining error is closed (a tenth of a second's
        // worth each second, so it arrives without overshooting), and how
        // quickly a standing error is worked off.
        const GAIN: f32 = 10.0;
        const SUM_GAIN: f32 = 25.0;
        let pos = self.joint_position(j).0;
        let jt = &mut self.joints[j];
        let cap = jt.motor_speed.abs();
        let err = jt.goal - pos;
        // The sum never asks for more than the motor's speed on its own, so
        // a servo held off its goal does not wind up and fling past it. And
        // it only gathers once the goal is near enough that the error alone
        // asks for less than full speed: summed over the whole swing it
        // arrived full, kept driving past the goal, and an arm sent to −45°
        // swung on to −51° before it came back.
        let limit = if SUM_GAIN > 0.0 { cap / SUM_GAIN } else { 0.0 };
        // Past the goal, what was gathered on the way pushes the wrong way.
        if (err * GAIN).abs() >= cap || jt.servo_sum * err < 0.0 {
            jt.servo_sum = 0.0;
        } else {
            jt.servo_sum = (jt.servo_sum + err * dt).clamp(-limit, limit);
        }
        (err * GAIN + jt.servo_sum * SUM_GAIN).clamp(-cap, cap)
    }

    fn ends(&self, j: usize) -> (End, End) {
        let jt = &self.joints[j];
        (
            self.joint_end(Some(jt.a), jt.anchor_a, Vec3::ZERO),
            self.joint_end(jt.b, jt.anchor_b, self.world_end(j)),
        )
    }

    /// The positional part of a joint, one pass. Returns the impulse spent.
    fn solve_hard(&mut self, j: usize, dt: f32) -> f32 {
        let kind = self.joints[j].kind;
        let (ea, eb) = self.ends(j);
        let c = eb.p - ea.p;
        let axis_w = self.orient_of(self.joints[j].b) * self.joints[j].axis.normalize_or(Vec3::Z);
        let mut spent = 0.0;
        match kind {
            JointKind::Hinge | JointKind::Fixed => {
                if let Some(n) = c.try_normalize() {
                    spent += self.close_gap(j, n, c.length(), dt);
                }
                if kind == JointKind::Fixed {
                    let e = self.weld_error(j);
                    spent += self.close_turn(j, e, dt);
                } else {
                    // A 3D hinge keeps the two axles in line; a 2D body only
                    // turns about Z, so its axle is in line already.
                    let qa = self.orient_of(Some(self.joints[j].a));
                    let axis_a = qa * (self.joints[j].rel0 * self.joints[j].axis.normalize_or(Vec3::Z));
                    let e = axis_a.cross(axis_w);
                    spent += self.close_turn(j, -e, dt);
                    if self.joints[j].limit {
                        let th = self.hinge_angle(j);
                        let (lo, hi) = (self.joints[j].min, self.joints[j].max);
                        let over = if th < lo { th - lo } else if th > hi { th - hi } else { 0.0 };
                        if over != 0.0 {
                            spent += self.close_turn(j, axis_w * over, dt);
                        }
                    }
                }
            }
            JointKind::Ball => {
                if let Some(n) = c.try_normalize() {
                    spent += self.close_gap(j, n, c.length(), dt);
                }
                if self.joints[j].limit {
                    let (lean, about) = self.ball_lean(j);
                    let over = lean - self.joints[j].max.max(0.0);
                    if over > 0.0 && about != Vec3::ZERO {
                        spent += self.close_turn(j, -about * over, dt);
                    }
                }
            }
            JointKind::Distance | JointKind::Rope => {
                let d = c.length();
                if let Some(n) = c.try_normalize() {
                    let gap = d - self.joints[j].length;
                    if kind == JointKind::Distance || gap > 0.0 {
                        spent += self.close_gap(j, n, gap, dt);
                    }
                }
            }
            JointKind::Slider | JointKind::Wheel => {
                // Off the rail: the part of the gap across the axis.
                let across = c - axis_w * axis_w.dot(c);
                if let Some(n) = across.try_normalize() {
                    spent += self.close_gap(j, n, across.length(), dt);
                }
                if kind == JointKind::Slider {
                    let e = self.weld_error(j);
                    spent += self.close_turn(j, e, dt);
                    if self.joints[j].limit {
                        // Travel of this node along the rail from the other's
                        // anchor; past a limit it is moved back along it.
                        let s = -axis_w.dot(c);
                        let (lo, hi) = (self.joints[j].min, self.joints[j].max);
                        let over = if s < lo { s - lo } else if s > hi { s - hi } else { 0.0 };
                        if over != 0.0 {
                            spent += self.close_gap(j, -axis_w, over, dt);
                        }
                    }
                }
            }
            JointKind::Spring | JointKind::Target => {}
        }
        spent
    }

    /// A soft constraint's impulse along `n` for a gap `c` and the speed
    /// apart `vn`: a spring of `freq` cycles a second, critically damped at
    /// `zeta` = 1. The form from Box2D's soft step, stable at any frequency.
    fn soft_impulse(w: f32, c: f32, vn: f32, freq: f32, zeta: f32, dt: f32) -> f32 {
        if !w.is_finite() || w <= 1e-9 || freq <= 0.0 {
            return 0.0;
        }
        let omega = std::f32::consts::TAU * freq;
        let a1 = 2.0 * zeta + dt * omega;
        let a2 = dt * omega * a1;
        let bias_rate = omega / a1;
        let mass_scale = a2 / (1.0 + a2);
        -(1.0 / w) * mass_scale * (vn + bias_rate * c)
    }

    /// The speed part of a joint, once a step. Returns the impulse spent.
    fn solve_soft(&mut self, j: usize, dt: f32) -> f32 {
        let speed = self.motor_command(j, dt);
        let mut jt = self.joints[j].clone();
        jt.motor_speed = speed;
        let (ea, eb) = self.ends(j);
        let axis_w = self.orient_of(jt.b) * jt.axis.normalize_or(Vec3::Z);
        let mut spent = 0.0;
        match jt.kind {
            JointKind::Spring => {
                let c = eb.p - ea.p;
                if let Some(n) = c.try_normalize() {
                    let gap = c.length() - jt.length;
                    // Apart speed of B from A along the line between them.
                    let vn = (self.point_vel(jt.b, eb.r) - self.point_vel(Some(jt.a), ea.r)).dot(n);
                    let w = ea.w(n) + eb.w(n);
                    let l = Self::soft_impulse(w, gap, vn, jt.stiffness, jt.damping, dt);
                    self.joint_impulse(Some(jt.a), ea.r, -n * l);
                    self.joint_impulse(jt.b, eb.r, n * l);
                    spent += l.abs();
                }
            }
            JointKind::Wheel => {
                // Suspension: this node (the wheel) held at the other's mount
                // along the axis by a spring.
                let c = ea.p - eb.p;
                let s = axis_w.dot(c);
                let vn = (self.point_vel(Some(jt.a), ea.r) - self.point_vel(jt.b, eb.r)).dot(axis_w);
                let w = ea.w(axis_w) + eb.w(axis_w);
                let l = Self::soft_impulse(w, s, vn, jt.stiffness, jt.damping, dt);
                self.joint_impulse(Some(jt.a), ea.r, axis_w * l);
                self.joint_impulse(jt.b, eb.r, -axis_w * l);
                spent += l.abs();
                // A wheel turns about its axle, Z of what it hangs from, and
                // about nothing else relative to it: a wheel left free to spin
                // about the upright creeps sideways down any slope it is
                // parked on.
                let axle = self.wheel_axle(&jt);
                let rel = self.ang_vel_of(Some(jt.a)) - self.ang_vel_of(jt.b);
                let off_axle = rel - axle * rel.dot(axle);
                let n = off_axle.normalize_or_zero();
                let w = ea.w_ang(n) + eb.w_ang(n);
                if n != Vec3::ZERO && w.is_finite() && w > 1e-9 {
                    let l = -rel.dot(n) / w;
                    self.spin_impulse(Some(jt.a), n * l);
                    self.spin_impulse(jt.b, -n * l);
                    spent += l.abs();
                }
                if jt.motor {
                    // At the motor's speed relative to what it hangs from.
                    let rel = (self.ang_vel_of(Some(jt.a)) - self.ang_vel_of(jt.b)).dot(axle);
                    let w = ea.w_ang(axle) + eb.w_ang(axle);
                    spent += self.motor_turn(&jt, axle, rel, w, dt, true);
                }
                // After the drive, which turns the wheel and so moves the
                // patch: the grip has the last word on whether it slides.
                spent += self.tyre_grip(j, &jt, l, dt);
            }
            JointKind::Hinge if jt.motor => {
                // This node turns relative to the other at the motor's speed.
                let rel = (self.ang_vel_of(Some(jt.a)) - self.ang_vel_of(jt.b)).dot(axis_w);
                let w = ea.w_ang(axis_w) + eb.w_ang(axis_w);
                spent += self.motor_turn(&jt, axis_w, rel, w, dt, true);
            }
            JointKind::Slider if jt.motor => {
                // This node slides along the rail at the motor's speed.
                let rel = (self.point_vel(Some(jt.a), ea.r) - self.point_vel(jt.b, eb.r)).dot(axis_w);
                let w = ea.w(axis_w) + eb.w(axis_w);
                if w > 1e-9 {
                    let mut l = (jt.motor_speed - rel) / w;
                    if jt.max_force > 0.0 {
                        let cap = jt.max_force * dt;
                        l = l.clamp(-cap, cap);
                    }
                    self.joint_impulse(Some(jt.a), ea.r, axis_w * l);
                    self.joint_impulse(jt.b, eb.r, -axis_w * l);
                    spent += l.abs();
                }
            }
            JointKind::Target => {
                if let Some(t) = jt.target {
                    let c = ea.p - t;
                    let v = self.point_vel(Some(jt.a), ea.r);
                    let mut total = Vec3::ZERO;
                    for n in [Vec3::X, Vec3::Y, Vec3::Z] {
                        let w = ea.w(n);
                        total += n * Self::soft_impulse(w, c.dot(n), v.dot(n), jt.stiffness, jt.damping, dt);
                    }
                    if jt.max_force > 0.0 {
                        let cap = jt.max_force * dt;
                        if total.length() > cap {
                            total = total.normalize() * cap;
                        }
                    }
                    self.joint_impulse(Some(jt.a), ea.r, total);
                    spent += total.length();
                }
            }
            _ => {}
        }
        spent
    }

    /// Drive the relative spin about `axle` toward the motor's speed, within
    /// its force. `on_a` drives this node; otherwise the other end.
    fn motor_turn(&mut self, jt: &JointRt, axle: Vec3, rel: f32, w: f32, dt: f32, on_a: bool) -> f32 {
        if !w.is_finite() || w <= 1e-9 {
            return 0.0;
        }
        let mut l = (jt.motor_speed - rel) / w;
        if jt.max_force > 0.0 {
            let cap = jt.max_force * dt;
            l = l.clamp(-cap, cap);
        }
        let (fwd, back) = if on_a { (Some(jt.a), jt.b) } else { (jt.b, Some(jt.a)) };
        self.spin_impulse(fwd, axle * l);
        self.spin_impulse(back, -axle * l);
        l.abs()
    }

    /// Tyre friction for a wheel joint's wheel (a body on the ground): the
    /// contact patch's sliding, along and across the tread, is stopped by an
    /// impulse no larger than friction times what the wheel carries this
    /// step, its own weight and the suspension's push `load`. Solved here,
    /// after the joints, because the load arrives through the joint: the
    /// contact pass alone sees only the wheel's own weight, and a rover parked
    /// on a slope slid on wheels that held a third of it up.
    fn tyre_grip(&mut self, j: usize, jt: &JointRt, load: f32, dt: f32) -> f32 {
        let Link::Body(i) = jt.a else { return 0.0 };
        // Held only while braked and on the ground: a parked wheel.
        let braked = jt.motor && jt.motor_speed == 0.0;
        let grounded = self.bodies[i].grounded;
        if !(braked && grounded) {
            self.joints[j].park = None;
        }
        let (n, r, mu, mass) = {
            let b = &self.bodies[i];
            let Some(n) = b.ground_normal.filter(|_| b.grounded && b.live()) else { return 0.0 };
            (n, -n * b.radius, b.friction.max(0.0), b.mass.max(1e-4))
        };
        let g = self.gravity.accel_at(self.bodies[i].pos, &self.colliders);
        // What presses the patch into the ground: the wheel's weight and the
        // suspension's share of the craft, both as impulses this step.
        let press = (mass * (-g).dot(n)).max(0.0) * dt + load.abs();
        let budget = mu * press;
        if budget <= 0.0 {
            return 0.0;
        }
        let b = &self.bodies[i];
        let v = b.vel + b.ang_vel.cross(r);
        // The aim is a patch at rest once the next step's gravity has been
        // added, before it moves: the slope's pull is taken off now, or a
        // parked rover creeps one step's worth of it every step.
        let g_t = g - n * g.dot(n);
        let slide = v - n * v.dot(n) + g_t * dt;
        let len = slide.length();
        if len < 1e-6 {
            return 0.0;
        }
        let t = slide / len;
        // The wheel cannot slide along the ground without dragging what it
        // hangs from, which the joint holds to it across the suspension: the
        // grip stops the two together, each taking its share.
        let wheel = End { r, ..self.joint_end(Some(jt.a), Vec3::ZERO, Vec3::ZERO) };
        let patch = self.bodies[i].pos + r;
        let mount = self.joint_end(jt.b, jt.anchor_b, Vec3::ZERO);
        let mount = End { r: patch - (mount.p - mount.r), ..mount };
        if !braked {
            return self.rolling_grip(jt, &wheel, &mount, r, n, slide, budget);
        }
        let (wa, wb) = (wheel.w(t), mount.w(t));
        if !wa.is_finite() || wa <= 1e-9 {
            return 0.0;
        }
        let (ma, mb) = (1.0 / wa, if wb.is_finite() && wb > 1e-9 { 1.0 / wb } else { 0.0 });
        let need = len * (ma + mb);
        let imp = need.min(budget);
        let share_a = ma / (ma + mb);
        self.joint_impulse(Some(jt.a), r, -t * (imp * share_a));
        self.joint_impulse(jt.b, mount.r, -t * (imp * (1.0 - share_a)));
        // Braked and not slipping: the wheel stays where it stopped on the
        // ground it stopped on. Cancelling its speed each step still lets a
        // parked rover creep by what the joints' position fixes leave over;
        // holding the spot does not, and the spot rides the collider.
        // Once parked, a wheel stays parked while its ground could hold it
        // at all (the slope's pull within what friction allows), not just on
        // steps where its own grip alone can stop the whole craft: each
        // wheel's share of stopping the chassis counts the chassis whole,
        // so on a slope close to the limit some wheel came up short every
        // few steps, let go and parked afresh a hair downhill. That was a
        // parked rover creeping centimetres a second on a hill it could hold.
        let could_hold = g_t.length() <= mu * (-g).dot(n).max(0.0);
        let keep = could_hold && self.joints[j].park.is_some();
        if braked && (need <= budget || keep) {
            self.park_wheel(j, i, n);
        } else {
            self.joints[j].park = None;
        }
        imp
    }

    /// The grip of a wheel that is free to turn (not braked): along its
    /// rolling direction the ground turns the wheel, and the wheel alone
    /// takes that, as a tyre does; only across it does the grip hold the
    /// wheel and what it hangs from together. Stopping the craft along the
    /// roll as well treated a free wheel as a locked one: a plane's ground
    /// roll dragged at a skid's friction, and a driven wheel hauled its craft
    /// to the rim's speed past what its motor could give. The two share the
    /// one friction budget. Returns the impulse spent.
    #[allow(clippy::too_many_arguments)]
    fn rolling_grip(&mut self, jt: &JointRt, wheel: &End, mount: &End, r: Vec3, n: Vec3, slide: Vec3, budget: f32) -> f32 {
        let fwd = self.wheel_axle(jt).cross(n).normalize_or_zero();
        let roll = fwd * slide.dot(fwd);
        let side = slide - roll;
        let mut on_wheel = Vec3::ZERO;
        if let Some(d) = roll.try_normalize() {
            let w = wheel.w(d);
            if w.is_finite() && w > 1e-9 {
                on_wheel = -roll / w;
            }
        }
        let (mut across, mut share) = (Vec3::ZERO, 1.0);
        if let Some(d) = side.try_normalize() {
            let (wa, wb) = (wheel.w(d), mount.w(d));
            if wa.is_finite() && wa > 1e-9 {
                let (ma, mb) = (1.0 / wa, if wb.is_finite() && wb > 1e-9 { 1.0 / wb } else { 0.0 });
                across = -side * (ma + mb);
                share = ma / (ma + mb);
            }
        }
        let total = (on_wheel + across).length();
        let k = if total > budget { budget / total } else { 1.0 };
        self.joint_impulse(Some(jt.a), r, (on_wheel + across * share) * k);
        self.joint_impulse(jt.b, mount.r, across * ((1.0 - share) * k));
        total * k
    }

    /// Hold joint `j`'s wheel (body `i`) on the spot it parked on, along the
    /// ground (normal `n`): taken the first step it is parked, against the
    /// collider it is resting on, then each step it is moved back there.
    fn park_wheel(&mut self, j: usize, i: usize, n: Vec3) {
        let pos = self.bodies[i].pos;
        match self.joints[j].park {
            Some((ci, rel)) if ci < self.colliders.len() => {
                let err = self.colliders[ci].offset() + rel - pos;
                let along = err - n * err.dot(n);
                self.bodies[i].pos += along;
            }
            _ => {
                let Some(ci) = self.contacts.iter().find(|c| c.body == i).map(|c| c.collider) else { return };
                self.joints[j].park = Some((ci, pos - self.colliders[ci].offset()));
            }
        }
    }

    /// Change an end's spin by the angular impulse `ang`.
    fn spin_impulse(&mut self, bi: Option<Link>, ang: Vec3) {
        match bi {
            Some(Link::Compound(i)) => {
                if self.compound_moves(i) {
                    let c = &mut self.compounds[i];
                    let dw = c.world_inv_inertia() * ang;
                    c.ang_vel += dw;
                    if c.asleep && dw.length_squared() > 1e-10 {
                        c.wake(crate::body::WakeReason::Joint);
                    }
                }
            }
            Some(Link::Body(i)) => {
                let b = &mut self.bodies[i];
                if b.live() && !b.kinematic && !b.driven && b.rotates {
                    let dw = b.free_spin(b.world_inv_inertia() * ang);
                    b.ang_vel += dw;
                    if b.asleep && dw.length_squared() > 1e-10 {
                        b.wake_for(crate::body::WakeReason::Joint);
                    }
                }
            }
            None => {}
        }
    }
}
