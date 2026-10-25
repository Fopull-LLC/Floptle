//! Dynamic rigid bodies: shape, integration state, material response
//! (restitution/friction), and contact resolution against collision shapes.

use floptle_core::math::{Mat3, Quat, Vec3};


/// The collision shape of a dynamic body.
///
/// A plain value, hull included, so a body stays one the solver and a
/// rollback snapshot copy freely; boxing the hull would make every body an
/// allocation to clone.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BodyShape {
    /// A sphere of the body's `radius`.
    Sphere,
    /// A capsule: `radius` thick, with `half_height` from the center to each end-sphere
    /// center, aligned to the body's `up` (kept along −gravity, so it stands upright).
    Capsule { half_height: f32 },
    /// A box with the given half-extents, in the body's own axes: turned by
    /// `orient`, which starts at the node's rotation, so a box placed tilted
    /// collides tilted. A body that `rotates` tumbles it; one that does not
    /// holds it at the angle it was placed. Depenetrated by sampling its 8
    /// corners + center as points against the world (so a crate rests flat
    /// on a floor).
    Box { half: Vec3 },
    /// A convex outline in the XY plane, `half_depth` thick either side of it.
    /// Like a box it keeps its orientation: a 2D body that stands on its art
    /// rather than tumbling. Depenetrated by its corners, its edge midpoints
    /// and its centre, all in its own plane, the way a 2D box is.
    Polygon { outline: floptle_core::outline::Outline, half_depth: f32 },
    /// A convex solid from a mesh: the hull of the node's own geometry,
    /// scaled, in the body's axes and about the node's origin (so a mesh
    /// modelled off-centre collides where it is drawn). It turns with the
    /// body like a box does and is depenetrated by its corners.
    Hull(crate::hull::Hull),
}

/// The most sample points a body tests: a polygon's eight corners, its eight
/// edge midpoints and its centre, or a hull's corners and its centroid.
pub(crate) const MAX_SAMPLES: usize = {
    let poly = 2 * floptle_core::outline::OUTLINE_MAX + 1;
    let hull = crate::hull::HULL_VERTS + 1;
    if poly > hull { poly } else { hull }
};

/// Why a body or an assembly is awake: what last woke it or started its
/// count toward sleep again, or `Settling` while it counts. Scripts read it
/// as `node.wake` and `assembly.info(node).wake`. Only reported: nothing in
/// the solver reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WakeReason {
    /// Still, and counting toward sleep (or never judged yet).
    #[default]
    Settling,
    /// It moved or turned more in a step than a sleeper may.
    Moving,
    /// Nothing holds it up: in the air, or on ground too steep to rest on.
    Unsupported,
    /// Something moving touched it: a body, an assembly, a platform.
    Contact,
    /// What it rests on changed or went away: a dig, a chunk, a collider.
    Ground,
    /// A joint pulled it into place, or a motor or servo drives it.
    Joint,
    /// A held force or torque.
    Force,
    /// An impulse.
    Impulse,
    /// A velocity, position or `asleep` written by a script or the engine.
    Script,
    /// Its parts moved or changed mass.
    Reshaped,
}

impl WakeReason {
    /// What it is awake for once its own motion has stopped its count: the
    /// cause that set it going, kept until it settles again, or its own
    /// motion (`Moving`, or `Unsupported` when nothing holds it up) when it
    /// was settling.
    pub fn or_own(self, supported: bool) -> Self {
        match self {
            Self::Settling if supported => Self::Moving,
            Self::Settling | Self::Moving | Self::Unsupported if !supported => Self::Unsupported,
            Self::Unsupported => Self::Moving,
            other => other,
        }
    }

    /// What it is awake for while it counts toward sleep: `Settling` when
    /// its own motion was what stopped its count, or the cause that set it
    /// counting again (a joint, a contact, a script), kept until it sleeps.
    /// Overwritten with `Settling`, a craft woken again and again by the
    /// same thing read `settling` every frame and never named it.
    pub fn counting(self) -> Self {
        match self {
            Self::Moving | Self::Unsupported => Self::Settling,
            other => other,
        }
    }

    /// The name scripts read: `settling`, `moving`, `unsupported`,
    /// `contact`, `ground`, `joint`, `force`, `impulse`, `script` or
    /// `reshaped`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Settling => "settling",
            Self::Moving => "moving",
            Self::Unsupported => "unsupported",
            Self::Contact => "contact",
            Self::Ground => "ground",
            Self::Joint => "joint",
            Self::Force => "force",
            Self::Impulse => "impulse",
            Self::Script => "script",
            Self::Reshaped => "reshaped",
        }
    }
}

/// A dynamic body — a sphere or capsule integrated + depenetrated each step.
#[derive(Clone, Copy, Debug)]
pub struct Body {
    pub pos: Vec3,
    /// `pos` as of the start of the most recent fixed step. Rendering interpolates
    /// between the two by the accumulator's leftover fraction, so on-screen motion
    /// is smooth even though the sim advances in whole 1/120 s steps (without this,
    /// frames alternate between covering 1 and 2 steps — visible micro-jitter).
    pub prev_pos: Vec3,
    pub vel: Vec3,
    pub radius: f32,
    pub shape: BodyShape,
    /// Capsule axis, kept aligned to −gravity each step.
    pub up: Vec3,
    /// Bounciness: 0 = no bounce, 1 = perfectly elastic.
    pub restitution: f32,
    /// Surface friction, as a **Coulomb coefficient**: the fraction of the load
    /// the surface carries that it can also resist sideways. 0 is ice; a ramp
    /// holds while `tan(angle) ≤ friction`, so 1 holds exactly 45° and grippier
    /// surfaces go above 1 (rubber on rubber is about 1.5).
    ///
    /// It is a coefficient rather than a damping factor because that is the only
    /// form in which "does this crate stay on this ramp" has an answer at all —
    /// one that holds whatever the frame rate, and however many colliders the
    /// crate happens to be touching. Spent once per step against the surface
    /// being stood on; see `PhysicsWorld::rub`.
    pub friction: f32,
    /// The steepest surface, in radians from "up", that counts as **ground**.
    ///
    /// Steeper than this and the body is not `grounded`, the surface becomes a
    /// `wall_normal` rather than a `ground_normal`, and — the part that makes it
    /// a gameplay knob rather than a label — the surface stops carrying the
    /// body's weight, so friction can no longer hold it and it slides off
    /// however grippy it is. 60° is the historical default.
    pub slope_limit: f32,
    /// Whether the gravity field pulls on this body (false = floats, still collides).
    pub use_gravity: bool,
    /// Freeze world-axis translation (x, y, z) — e.g. lock Z for a 2.5D game.
    pub lock_pos: [bool; 3],
    /// Set each step when the body is resting on a surface that opposes gravity.
    pub grounded: bool,
    /// The contact normal from the most recent resolved collision this step (telegraph).
    pub contact: Option<Vec3>,
    /// The floor this body is standing on: the most gravity-opposing contact
    /// normal of the step, or `None` when nothing this step counted as ground
    /// (so it is exactly `grounded`, with the surface attached).
    pub ground_normal: Option<Vec3>,
    /// The steepest surface the body is pressed against, when that surface is
    /// too steep to be a floor — a wall, a cliff face, the side of a crate.
    ///
    /// Separate from [`Self::ground_normal`] because a controller asks two
    /// different questions: *what am I standing on* and *what am I pushing
    /// into*. Standing at the foot of a cliff answers both, and a controller
    /// that only had the first one would keep driving into the cliff — whose
    /// push-out has an upward component, which is what launches a walking
    /// character into the sky. Both are extremes (max / min of `n · up`), so
    /// neither depends on the order colliders happen to be resolved in.
    pub wall_normal: Option<Vec3>,
    /// The position restored on locked axes: captured at spawn, re-captured per
    /// axis at the moment its lock engages (so locking mid-play freezes in place).
    pub(crate) home: Vec3,
    /// Inactive bodies are skipped by the step and the transform writeback —
    /// a networked client deactivates server-authoritative bodies so local
    /// physics never fights the interpolated snapshots driving their
    /// transforms (`docs/multiplayer.md` §6). Default true.
    pub active: bool,
    /// Its node is switched off (`node.enabled = false`): out of the step, the
    /// pair pass, touch events and rays until it is switched on, when it
    /// carries on from where it stopped. Its pose is still written back, so a
    /// node switched off on a moving planet rides the planet with its body.
    /// Separate from `active`, which networking owns.
    pub off: bool,
    /// Collision-layer bit index (resolved from the node's named layer by
    /// `floptle_core::Layers`). The solver only resolves this body against
    /// colliders whose layer bit is set in `PhysicsWorld::matrix[layer]`.
    pub layer: u8,
    /// Kinematic: transform-driven — the step skips it entirely (no gravity,
    /// no depenetration; the node's transform is authoritative and the sim
    /// follows it). Dynamic bodies collide with it via
    /// `PhysicsWorld::kin_hulls` — moving platforms that push the player.
    pub kinematic: bool,
    /// Sensor (a `Trigger` on a rigidbody node): the body never blocks or gets
    /// blocked — it passes through everything and nothing pushes back — but
    /// overlap still fires the trigger hooks. A dynamic sensor still falls
    /// (pair with gravity-off or Kinematic for pickups/zones that stay put).
    /// Sensors are also invisible to raycasts, like static trigger colliders.
    pub sensor: bool,
    /// Driver-stepped: the whole-world step skips this body because something
    /// else advances it one tick at a time — the rollback driver
    /// (`docs/multiplayer.md` §7 P3), which must run the identical
    /// integration live and during re-simulation. Stepping it here as well
    /// would advance a rollback fighter twice per live tick and once per
    /// replayed one, so a correction would visibly shorten every jump.
    ///
    /// Unlike `active = false`, a driven body is fully live: it collides, it
    /// raycasts, its transform writes back. Only *who calls the step* differs.
    pub driven: bool,
    /// pushbox only (`RigidBody::pushbox_only`): integrate velocity, and stop
    /// there — no gravity, no depenetration, no ground detection, no locks. The
    /// body remains fully visible to raycasts and hulls; the script owns where
    /// it is allowed to be. The supported rollback profile
    /// (`docs/multiplayer.md` §3).
    pub pushbox_only: bool,
    /// Mass, kg — from the node's `RigidBody::mass`.
    ///
    /// The depenetration solver is mass-independent (a simple body is pushed
    /// out of geometry, not negotiated with), so this exists for the forces
    /// that genuinely need it: buoyancy compares the body's density against the
    /// water's, which is the difference between a cork and a cannonball.
    pub mass: f32,
    /// The node this body belongs to, by entity index; `u32::MAX` when it
    /// belongs to none. [`crate::PhysicsWorld::ignore_pair`] names bodies by it.
    pub owner: u32,
    /// Sleeping: grounded and below rest speed for long enough that the step
    /// skips it entirely (no gravity, no depenetration, no ground detection),
    /// the same near-zero cost `kinematic` gets. Woken by a kinematic hull
    /// touching it, a Lua velocity or impulse write, a teleport, a
    /// Kinematic→Dynamic switch, or its supporting collider disappearing (a
    /// chunk unload).
    ///
    /// Part of the body's own state, not a cache: a rollback resimulation
    /// replaying this body alone through `PhysicsWorld::step_body` must reach
    /// this exact value on the exact same tick the live run did, or a peer
    /// could integrate ticks the other skipped. See
    /// [`crate::sim::BodySnapshot`].
    pub asleep: bool,
    /// Seconds this body has been grounded and below the sleep speed,
    /// consecutively — reset the instant either condition breaks. Reaching
    /// [`crate::world::SLEEP_SETTLE_TIME`] is what sets `asleep`.
    pub sleep_time: f32,
    /// What last woke it or restarted its count toward sleep.
    pub wake: WakeReason,
    /// Feet (`RigidBody::feet`): a capsule stands on the ground straight
    /// beneath its centre line, not wherever its rounded bottom first touches.
    ///
    /// A sphere resting on a slope touches it off to one side, so the point
    /// directly under the centre, where a character's feet are drawn, hangs
    /// `r·(1 − cos θ)` above the ground; in a crease it bridges the two sides,
    /// on a bump it perches. On coarse terrain that reads as hovering over
    /// ground the model is standing on: up to 0.19 units from a 0.35 capsule
    /// at rest on 10–20° ground.
    ///
    /// With feet, walkable ground under the bottom sphere is resolved by a
    /// probe straight down from the sphere's centre: the body is pushed up
    /// until that point clears the surface, and pulled down onto it when it
    /// was standing and is not moving upward, so a jump is never snapped back
    /// and a crest is followed rather than hopped. Anything the probe does not
    /// find (a wall, a slope past `slope_limit`, a ledge the centre has stepped
    /// past) is left to the rounded bottom, so walls still push and a
    /// character still hangs by its rim at an edge. Only a capsule has feet; a
    /// ball rolls on its curve.
    pub feet: bool,
    /// Seconds left during which one-way platforms do not hold this body, so
    /// it drops through the ledge it stands on. Counts down each step; a
    /// script starts it (`node.dropThrough`) when the player presses down.
    pub drop_through: f32,
    /// The solver turns this body: contacts off its centre spin it, so a
    /// crate tips off a ledge, a ball rolls and a plank see-saws. Off, the
    /// body keeps its orientation however it is hit, which is what a
    /// character wants. See [`crate::PhysicsWorld::step_body`].
    pub rotates: bool,
    /// How far the body has turned since it was built, applied to its shape
    /// about its centre of mass. Identity for a body that does not rotate.
    pub orient: Quat,
    /// `orient` as of the start of the last step, for interpolation.
    pub prev_orient: Quat,
    /// Angular velocity, radians a second about each world axis.
    pub ang_vel: Vec3,
    /// World axes the body may not turn about: X and Y for a 2D body, so it
    /// spins only in its plane.
    pub lock_rot: [bool; 3],
}

impl Body {
    /// Wake it for `why`: the next step simulates it in full, and its
    /// count toward sleep starts again.
    pub fn wake_for(&mut self, why: WakeReason) {
        self.asleep = false;
        self.sleep_time = 0.0;
        self.wake = why;
    }

    pub fn sphere(pos: Vec3, radius: f32) -> Self {
        Self {
            pos,
            prev_pos: pos,
            vel: Vec3::ZERO,
            radius,
            shape: BodyShape::Sphere,
            up: Vec3::Y,
            restitution: 0.0,
            friction: 0.3,
            slope_limit: std::f32::consts::FRAC_PI_3, // 60°
            use_gravity: true,
            lock_pos: [false; 3],
            grounded: false,
            contact: None,
            ground_normal: None,
            wall_normal: None,
            home: pos,
            active: true,
            layer: 0,
            kinematic: false,
            sensor: false,
            driven: false,
            pushbox_only: false,
            mass: 1.0,
            asleep: false,
            owner: u32::MAX,
            off: false,
            sleep_time: 0.0,
            wake: WakeReason::Settling,
            feet: false,
            drop_through: 0.0,
            rotates: false,
            orient: Quat::IDENTITY,
            prev_orient: Quat::IDENTITY,
            ang_vel: Vec3::ZERO,
            lock_rot: [false; 3],
        }
    }

    /// The centre of mass, relative to `pos`, before turning: the middle of
    /// a polygon's area (its outline need not be centred on the node), the
    /// centre of every other shape.
    pub(crate) fn com_local(&self) -> Vec3 {
        match self.shape {
            BodyShape::Polygon { outline, .. } => {
                let [x, y] = outline.centroid();
                Vec3::new(x, y, 0.0)
            }
            BodyShape::Hull(h) => h.centroid(),
            _ => Vec3::ZERO,
        }
    }

    pub(crate) fn shape_is_capsule(&self) -> bool {
        matches!(self.shape, BodyShape::Capsule { .. })
    }

    /// The radius a 3D box's or a hull's corner samples carry, each pulled
    /// in by as much (see `sample_centers`): two centimetres, or a quarter of
    /// the shape's thinnest half for a small one.
    pub(crate) fn corner_skin(&self) -> f32 {
        let thin = match self.shape {
            BodyShape::Box { half } => half.min_element(),
            BodyShape::Hull(h) => h.inner_radius(),
            _ => return 0.0,
        };
        (thin * 0.25).clamp(0.0, 0.02)
    }

    /// The centre of mass, in the sim frame.
    pub(crate) fn com(&self) -> Vec3 {
        self.pos + self.orient * self.com_local()
    }

    /// The moments of inertia about the centre of mass along the body's own
    /// axes: solid shapes of uniform density and `mass`.
    pub(crate) fn inertia_local(&self) -> Vec3 {
        let m = self.mass.max(1e-4);
        let box_i = |h: Vec3| Vec3::new(h.y * h.y + h.z * h.z, h.x * h.x + h.z * h.z, h.x * h.x + h.y * h.y) * (m / 3.0);
        match self.shape {
            BodyShape::Sphere => Vec3::splat(0.4 * m * self.radius * self.radius),
            BodyShape::Capsule { half_height } => {
                box_i(Vec3::new(self.radius, half_height + self.radius, self.radius))
            }
            BodyShape::Box { half } => box_i(half),
            BodyShape::Polygon { outline, half_depth } => {
                // Z from the outline's own area, X and Y as the box round it.
                let [hx, hy] = outline.half_extents();
                let b = box_i(Vec3::new(hx, hy, half_depth));
                Vec3::new(b.x, b.y, m * outline.polar_moment_per_area())
            }
            BodyShape::Hull(h) => h.inertia(m),
        }
    }

    /// The inverse inertia in world axes, with the locked axes taken out: a
    /// push that would turn the body about one of them turns it about the
    /// free ones only. Zero for a body that does not rotate.
    pub(crate) fn world_inv_inertia(&self) -> Mat3 {
        if !self.rotates {
            return Mat3::ZERO;
        }
        let i = self.inertia_local().max(Vec3::splat(1e-6));
        let r = Mat3::from_quat(self.orient);
        let w = r * Mat3::from_diagonal(Vec3::ONE / i) * r.transpose();
        let mask = Mat3::from_diagonal(Vec3::new(
            if self.lock_rot[0] { 0.0 } else { 1.0 },
            if self.lock_rot[1] { 0.0 } else { 1.0 },
            if self.lock_rot[2] { 0.0 } else { 1.0 },
        ));
        mask * w * mask
    }

    /// `v` with the locked rotation axes zeroed.
    pub(crate) fn free_spin(&self, v: Vec3) -> Vec3 {
        Vec3::new(
            if self.lock_rot[0] { 0.0 } else { v.x },
            if self.lock_rot[1] { 0.0 } else { v.y },
            if self.lock_rot[2] { 0.0 } else { v.z },
        )
    }

    /// Turn the body by the rotation vector `rot` about its centre of mass,
    /// which stays where it is.
    pub(crate) fn turn(&mut self, rot: Vec3) {
        let rot = self.free_spin(rot);
        if rot.length_squared() < 1e-14 {
            return;
        }
        let com = self.com();
        self.orient = (Quat::from_scaled_axis(rot) * self.orient).normalize();
        self.pos = com - self.orient * self.com_local();
    }

    /// The radius of a sphere enclosing this body — what displaces water when
    /// the shape has no radius of its own (a box).
    /// In the simulation: neither frozen by networking nor switched off.
    pub fn live(&self) -> bool {
        self.active && !self.off
    }

    pub(crate) fn bounding_radius(&self) -> f32 {
        match self.shape {
            BodyShape::Sphere => self.radius,
            BodyShape::Capsule { half_height } => half_height + self.radius,
            BodyShape::Box { half } => half.length(),
            BodyShape::Polygon { outline, half_depth } => outline.reach().hypot(half_depth),
            BodyShape::Hull(h) => h.reach_from(Vec3::ZERO),
        }
    }

    /// A capsule body of total standing `height` (clamped to ≥ 2·radius). It
    /// stands on its [feet](Self::feet).
    pub fn capsule(pos: Vec3, radius: f32, height: f32) -> Self {
        let half = (height.max(2.0 * radius) * 0.5 - radius).max(0.0);
        Self {
            shape: BodyShape::Capsule { half_height: half },
            feet: true,
            ..Self::sphere(pos, radius)
        }
    }

    /// A polygon body from a convex outline (see [`BodyShape::Polygon`]).
    /// `radius` is the outline's smallest half-size, for the few things that
    /// size a body by one number (water displacement, the sweep's step).
    pub fn polygon(pos: Vec3, outline: floptle_core::outline::Outline, half_depth: f32) -> Self {
        let [hx, hy] = outline.half_extents();
        let r = hx.min(hy).max(1e-3);
        Self {
            shape: BodyShape::Polygon { outline, half_depth: half_depth.abs().max(1e-3) },
            ..Self::sphere(pos, r)
        }
    }

    /// A body shaped like `hull`, which is in the body's own axes about `pos`
    /// (see [`BodyShape::Hull`]). `radius` is the hull's inner radius, for
    /// the few things that size a body by one number.
    pub fn hull(pos: Vec3, hull: crate::hull::Hull) -> Self {
        let r = hull.inner_radius().max(1e-3);
        Self { shape: BodyShape::Hull(hull), ..Self::sphere(pos, r) }
    }

    /// A box body with the given world-axis half-extents (a crate / falling platform).
    pub fn boxx(pos: Vec3, half: Vec3) -> Self {
        let h = half.abs().max(Vec3::splat(1e-3));
        Self { shape: BodyShape::Box { half: h }, radius: h.min_element(), ..Self::sphere(pos, h.min_element()) }
    }

    /// Total standing height (tip to tip): `2·radius` for a sphere, the full capsule
    /// length for a capsule, `2·half.y` for a box.
    pub(crate) fn height(&self) -> f32 {
        match self.shape {
            BodyShape::Sphere => 2.0 * self.radius,
            BodyShape::Capsule { half_height } => 2.0 * (half_height + self.radius),
            BodyShape::Box { half } => 2.0 * half.y,
            BodyShape::Polygon { outline, .. } => {
                let ys = outline.points().iter().map(|q| q[1]);
                ys.clone().fold(f32::MIN, f32::max) - ys.fold(f32::MAX, f32::min)
            }
            BodyShape::Hull(h) => {
                let (lo, hi) = h.bounds();
                hi.y - lo.y
            }
        }
    }

    /// A sphere around `pos` that contains every sample centre this body
    /// probes, plus the tolerance those probes are compared against.
    ///
    /// The reject a sensor pass needs: `sample_centers` is tested as
    /// `shape.distance(c) < radius`, and a shape's distance from outside its
    /// own bounding sphere is at least `|c − bc| − br`, so a collider whose
    /// bound is further than this from `pos` cannot overlap a single centre
    /// and the whole body is skipped without a `distance` call.
    ///
    /// Over-covers rather than clipping, so the reject never loses a touch
    /// event the exact test would find.
    pub(crate) fn bound_sphere(&self) -> (Vec3, f32) {
        let (centers, n, radius) = self.sample_centers();
        let reach = centers[..n]
            .iter()
            .map(|c| (*c - self.pos).length())
            .fold(0.0f32, f32::max);
        (self.pos, reach + radius)
    }

    /// The body's collision sample points, how many, and the sphere radius
    /// inflating each: 1 point (sphere), the 2 end-sphere centres (capsule), or
    /// the 8 corners plus centre (box, as zero-radius points), or a polygon's
    /// corners, edge midpoints and centre. The depenetration loop pushes each
    /// point that has sunk inside a collider back out along the collider
    /// normal.
    pub(crate) fn sample_centers(&self) -> ([Vec3; MAX_SAMPLES], usize, f32) {
        let mut a = [self.pos; MAX_SAMPLES];
        match self.shape {
            BodyShape::Sphere => (a, 1, self.radius),
            BodyShape::Capsule { half_height } => {
                a[0] = self.pos - self.up * half_height;
                a[1] = self.pos + self.up * half_height;
                (a, 2, self.radius)
            }
            // A body held in the XY plane (2D) is tested in that plane: its
            // four corners, its four edge midpoints and its centre, all at its
            // own Z. Its 3D corners sit half its depth in front of and behind
            // the plane, and a 2D level's colliders are only as deep as one
            // tile, so those corners landed on the colliders' faces and never
            // counted. The box then stood on its centre point alone, half sunk
            // into the floor, and anything thinner or faster than that went
            // straight through it.
            BodyShape::Box { half } if self.lock_pos[2] => {
                for (n, &(sx, sy)) in [
                    (-1.0f32, -1.0f32),
                    (1.0, -1.0),
                    (-1.0, 1.0),
                    (1.0, 1.0),
                    (0.0, -1.0),
                    (0.0, 1.0),
                    (-1.0, 0.0),
                    (1.0, 0.0),
                ]
                .iter()
                .enumerate()
                {
                    a[n] = self.pos + self.orient * Vec3::new(sx * half.x, sy * half.y, 0.0);
                }
                a[8] = self.pos;
                (a, 9, 0.0)
            }
            // Corners pulled in by a skin and given it as their radius: the
            // faces are where they were, the corners round by the skin. A
            // corner with no radius had no contact with anything whose
            // distance has no inside (a mesh floor, a blockout wall): only a
            // point *inside* a surface pushes out, and a mesh cannot say
            // which side is inside, so a box fell through every mesh it met.
            BodyShape::Box { half } => {
                let skin = self.corner_skin();
                let half = half - Vec3::splat(skin);
                let mut n = 0;
                for &sx in &[-1.0f32, 1.0] {
                    for &sy in &[-1.0f32, 1.0] {
                        for &sz in &[-1.0f32, 1.0] {
                            a[n] = self.pos + self.orient * Vec3::new(sx * half.x, sy * half.y, sz * half.z);
                            n += 1;
                        }
                    }
                }
                a[8] = self.pos; // center, so a deeply-buried box still resolves
                (a, 9, skin)
            }
            // In its own plane whether or not the body is locked to it: an
            // outline has no corners in depth to sample.
            BodyShape::Polygon { outline, .. } => {
                let pts = outline.points();
                let n = pts.len();
                for (i, q) in pts.iter().enumerate() {
                    let next = pts[(i + 1) % n];
                    a[2 * i] = self.pos + self.orient * Vec3::new(q[0], q[1], 0.0);
                    a[2 * i + 1] = self.pos
                        + self.orient * Vec3::new((q[0] + next[0]) * 0.5, (q[1] + next[1]) * 0.5, 0.0);
                }
                a[2 * n] = self.com();
                (a, 2 * n + 1, 0.0)
            }
            // Every corner, and the centroid so a body pushed deep into
            // something still finds its way out.
            // Pulled in by a skin like a box's corners, for the same reason.
            BodyShape::Hull(h) => {
                let skin = self.corner_skin();
                let n = h.verts().len();
                for (i, s) in a.iter_mut().enumerate().take(n) {
                    *s = self.pos + self.orient * h.inset_vert(i, skin);
                }
                a[n] = self.com();
                (a, n + 1, skin)
            }
        }
    }
}

/// A resolved contact this step — for collision telegraphing / events.
#[derive(Clone, Copy, Debug)]
pub struct Contact {
    pub body: usize,
    /// Index of the static collider the body resolved against (into
    /// `PhysicsWorld::colliders`) — how touch events name the other side.
    pub collider: usize,
    pub point: Vec3,
    pub normal: Vec3,
}

pub(crate) fn axis(v: Vec3, i: usize) -> f32 {
    match i {
        0 => v.x,
        1 => v.y,
        _ => v.z,
    }
}
pub(crate) fn set_axis(v: &mut Vec3, i: usize, val: f32) {
    match i {
        0 => v.x = val,
        1 => v.y = val,
        _ => v.z = val,
    }
}
