//! **How a 2D camera follows.** Attached to an orthographic Camera node.
//!
//! A 2D game's camera is not a transform somebody animates — it is a rule about
//! a target, and every 2D project writes the same rule out again in Lua: chase
//! the player, but not exactly, and not off the edge of the level, and shake
//! when something hits. Writing it once is not just convenience: three of those
//! four parts have a version that looks right and is subtly wrong, and a project
//! only finds out at the boundary.
//!
//! The order matters and is the whole design:
//!
//! 1. **Dead zone.** The camera does not move at all until the target leaves a
//!    box around it. Without one, every footstep moves the camera, which reads
//!    as the world wobbling.
//! 2. **Smoothing.** What is left is approached *exponentially*, not by a
//!    fraction per frame — `lerp(a, b, k * dt)` is the version that looks right
//!    and is frame-rate dependent, so the camera lags differently at 30 and 144.
//! 3. **Limits.** The result is clamped to the level's bounds, so the camera
//!    never shows outside the world.
//! 4. **Shake**, added *after* all of that and **not fed back**.
//!
//! Step 4 is why the camera keeps [`Camera2D::pos`] of its own rather than
//! reading its node's transform each frame. A shake written into the transform
//! and read back next frame is a shake the follow then chases and the limits
//! then clamp: the shake damps itself near a boundary and drags the camera off
//! its target everywhere else. Keeping the follow state separate is what lets
//! the two compose instead of fight.

use crate::math::DVec2;

/// Follow behaviour for an orthographic camera. Additive — a camera without one
/// is exactly the camera that existed before.
#[derive(Clone, Debug, PartialEq)]
pub struct Camera2D {
    /// Name of the node to follow. Empty means the camera stays where it is put
    /// — which is still useful, because the limits and the shake work without a
    /// target.
    pub follow: String,
    /// Seconds to close the gap. `0` snaps.
    ///
    /// Read as a time constant, not a speed: after `smoothing` seconds the
    /// camera has closed about 63% of the distance, whatever the frame rate.
    pub smoothing: f32,
    /// Half-size of the box the target may move inside before the camera moves
    /// at all, in world units. `[0, 0]` follows exactly.
    pub dead_zone: [f32; 2],
    /// Clamp the camera to a rectangle of the world.
    pub limits_on: bool,
    pub limit_min: [f32; 2],
    pub limit_max: [f32; 2],

    // ---- live state, never saved ----
    /// Where the follow has got to. Seeded from the node's transform the first
    /// time it steps ([`started`]), then owned by this component.
    ///
    /// [`started`]: Camera2D::started
    pub pos: DVec2,
    /// Has `pos` been seeded?
    pub started: bool,
    /// Current shake strength in world units, and how much of it is left to
    /// spend. Set by `node:shake(...)`; decays on its own.
    pub shake_amp: f32,
    pub shake_left: f32,
    pub shake_total: f32,
    /// The offset the last frame added for the shake.
    ///
    /// Kept because the shake is written into the node's transform and the
    /// transform is what a re-seed reads back. Without it, adopting the camera's
    /// current position mid-shake bakes that frame's shake into the follow —
    /// permanently, and asymmetrically at a world limit, which is exactly the
    /// drift this module is arranged to avoid.
    pub last_shake: DVec2,
}

impl Default for Camera2D {
    fn default() -> Self {
        Self {
            follow: String::new(),
            smoothing: 0.15,
            dead_zone: [0.0, 0.0],
            limits_on: false,
            limit_min: [0.0, 0.0],
            limit_max: [0.0, 0.0],
            pos: DVec2::ZERO,
            started: false,
            shake_amp: 0.0,
            shake_left: 0.0,
            shake_total: 0.0,
            last_shake: DVec2::ZERO,
        }
    }
}

/// Is this node an **orthographic camera** — the only thing a 2D camera rule
/// may drive?
pub fn is_ortho_camera(world: &crate::ecs::World, e: crate::ecs::Entity) -> bool {
    matches!(world.get::<crate::matter::Matter>(e), Some(crate::matter::Matter::Camera { ortho: true, .. }))
}

/// How fast a shake oscillates, in cycles per second.
///
/// Fixed rather than exposed: a shake is a *feel*, and the two numbers people
/// actually want to say are how hard and how long. A frequency knob mostly
/// produces shakes that read as a vibration or as a slow drift.
const SHAKE_HZ: f64 = 27.0;

impl Camera2D {
    /// Start (or restart) a shake. Restarting takes the **stronger** of the two
    /// rather than adding, so a script that shakes every frame while something
    /// is exploding does not build an unbounded shake.
    pub fn shake(&mut self, amount: f32, seconds: f32) {
        // Infinity and NaN are refused rather than clamped. `shake_left / shake_total`
        // with both infinite is NaN, `NaN.clamp(0, 1)` is NaN (clamp guards the
        // bounds, not the value), and the camera is then at NaN for the rest of
        // the session with nothing to put it back.
        if !amount.is_finite() || !seconds.is_finite() {
            return;
        }
        let amount = amount.max(0.0);
        let seconds = seconds.max(0.0);
        if amount <= 0.0 || seconds <= 0.0 {
            return;
        }
        // Strength takes the louder of the two and time takes the longer, always
        // and independently. Letting a strong short one replace the time as well
        // meant a bang during a three-second rumble ENDED the rumble after a
        // tenth of a second, which reads as the bang having cancelled it.
        self.shake_amp = self.shake_amp.max(amount);
        self.shake_left = self.shake_left.max(seconds);
        self.shake_total = self.shake_total.max(self.shake_left);
    }

    /// True while the shake still has something to spend.
    pub fn shaking(&self) -> bool {
        self.shake_left > 0.0 && self.shake_amp > 0.0
    }

    /// Advance the follow one frame and return **where to draw the camera**:
    /// the followed position plus this frame's shake.
    ///
    /// `target` is `None` when nothing is being followed (or the named node is
    /// gone), in which case the camera holds its place — and is still clamped
    /// and still shakes.
    ///
    /// `t` is the play clock. The shake is a function of it rather than of a
    /// random number so that two machines simulating the same frame see the
    /// same camera, and so a replay looks like what happened.
    pub fn step(&mut self, here: DVec2, target: Option<DVec2>, dt: f32, t: f64) -> DVec2 {
        // `here` carries last frame's shake, because that is what was written
        // to the transform. Every read of it has to take that back off first.
        let unshaken = here - self.last_shake;
        if !self.started {
            self.pos = unshaken;
            self.started = true;
        }
        // **Nothing to follow? Then the position is somebody else's.** A script
        // driving the camera by hand, a cutscene, the author's own placement —
        // all of them set the transform, and a component that owned the position
        // regardless would silently stomp them after the scripts had run. That
        // is what made asking for a screen shake take the camera away from
        // whatever was moving it. With no target we adopt the position and only
        // clamp and shake it.
        if target.is_none() {
            self.pos = unshaken;
        }
        if let Some(target) = target {
            // 1. Dead zone: only the part of the gap that leaves the box counts.
            let mut want = self.pos;
            for i in 0..2 {
                let dz = self.dead_zone[i].max(0.0) as f64;
                let d = target[i] - self.pos[i];
                if d > dz {
                    want[i] = target[i] - dz;
                } else if d < -dz {
                    want[i] = target[i] + dz;
                }
            }
            // 2. Smoothing: exponential, so the lag is the same at any frame
            // rate. `1 - e^(-dt/tau)` rather than `k * dt`, which is the version
            // that looks right in the editor and drifts on someone else's machine.
            let tau = self.smoothing.max(0.0) as f64;
            let k = if tau <= 1e-6 { 1.0 } else { 1.0 - (-(dt.max(0.0) as f64) / tau).exp() };
            self.pos += (want - self.pos) * k;
        }
        // 3. Limits, on the followed position — so the state itself never leaves
        // the level and there is nothing to un-clamp when the target comes back.
        if self.limits_on {
            for i in 0..2 {
                let (lo, hi) = (self.limit_min[i] as f64, self.limit_max[i] as f64);
                // A limit that is not a number is ignored rather than applied.
                // `NaN <= hi` is false, so without this the "backwards
                // rectangle" branch below computes `(NaN + hi) * 0.5` and the
                // camera is NaN for the rest of the session.
                if !lo.is_finite() || !hi.is_finite() {
                    continue;
                }
                // A backwards rectangle is a typo, not a reason to snap the
                // camera to a corner: take the middle and stay put.
                self.pos[i] = if lo <= hi { self.pos[i].clamp(lo, hi) } else { (lo + hi) * 0.5 };
            }
        }
        // 4. Shake, added to what is drawn and NEVER written back into `pos`.
        let mut out = self.pos;
        self.last_shake = DVec2::ZERO;
        if self.shaking() {
            let left = (self.shake_left / self.shake_total.max(1e-6)).clamp(0.0, 1.0) as f64;
            let amp = self.shake_amp as f64 * left;
            let phase = t * SHAKE_HZ * std::f64::consts::TAU;
            self.last_shake = DVec2::new(
                amp * phase.sin(),
                // A different rate on Y, so the motion is a jitter rather than a
                // line through the corners.
                amp * (phase * 1.37 + 1.7).sin(),
            );
            out += self.last_shake;
            self.shake_left = (self.shake_left - dt.max(0.0)).max(0.0);
            if self.shake_left <= 0.0 {
                self.shake_amp = 0.0;
            }
        }
        out
    }
}

/// Step every 2D camera in the world one frame.
///
/// Runs **after** scripts, animation and physics — a follow camera has to read
/// where its target ended up this frame, not where it started. Play only: an
/// authoring session's camera is a thing you placed, and moving it because a
/// node moved would be the editor editing your scene.
pub fn step_all(world: &mut crate::ecs::World, dt: f32, t: f64) {
    // Collected first: resolving a target's world transform reads the world,
    // and the write below needs it mutably.
    //
    // **Only an orthographic camera.** This function OWNS the transform of
    // everything it steps — it writes a position every frame — so stepping a
    // node that is not a 2D camera pins that node in place for the rest of the
    // session, and a script setting its position is silently stomped after the
    // scripts have run. Anything else carrying the component is inert, which is
    // also what makes the Inspector's "this does nothing until you switch the
    // projection back" true rather than aspirational.
    let jobs: Vec<(crate::ecs::Entity, String)> = world
        .query::<Camera2D>()
        .filter(|(e, _)| is_ortho_camera(world, *e) && !crate::matter::is_disabled(world, *e))
        .map(|(e, c)| (e, c.follow.clone()))
        .collect();
    // **Every followed name resolved once, not once per camera.** The lookup was
    // a scan of every `Name` in the world per camera per frame, which is fine
    // for the one camera a 2D game usually has and is quadratic in the case
    // somebody eventually builds — split screen, a minimap, a set of render
    // targets. Built only when something is actually being followed, so a scene
    // whose cameras are all placed by hand pays nothing.
    //
    // **A duplicate name resolves to the lowest entity index**, which is stable
    // for a scene loaded from a file (entities are allocated in document order)
    // and is at least always the same answer within a session. Two nodes sharing
    // a name is still ambiguous by construction — this makes the ambiguity
    // deterministic rather than dependent on how the world happened to be walked.
    let mut by_name: std::collections::HashMap<String, crate::ecs::Entity> =
        std::collections::HashMap::new();
    if jobs.iter().any(|(_, f)| !f.is_empty()) {
        for (te, n) in world.query::<crate::matter::Name>() {
            // A switched-off node is not somewhere to take the camera: it draws
            // nothing and its scripts do not run, so chasing it looks like the
            // camera wandering off on its own.
            if crate::matter::is_disabled(world, te) {
                continue;
            }
            match by_name.get_mut(&n.0) {
                Some(prev) if te.index() < prev.index() => *prev = te,
                Some(_) => {}
                None => {
                    by_name.insert(n.0.clone(), te);
                }
            }
        }
    }
    for (e, follow) in jobs {
        // The camera's own place, and the frame its `translation` is written in.
        // A camera under a parent is unusual and legal; doing the arithmetic in
        // world space and converting back is what makes it work rather than
        // drifting by the parent's transform.
        let cam_world = crate::matter::world_transform(world, e);
        // An EMPTY follow is "follow nothing", and must not be able to match a
        // node that happens to have an empty name — which the map would
        // otherwise hand back quite happily.
        let target = (!follow.is_empty())
            .then(|| by_name.get(follow.as_str()))
            .flatten()
            .map(|te| crate::matter::world_transform(world, *te).translation);
        let parent_world = world
            .get::<crate::matter::Parent>(e)
            .map(|p| p.0)
            .map(|p| crate::matter::world_transform(world, p))
            .unwrap_or(crate::transform::Transform::IDENTITY);
        let Some(c) = world.get_mut::<Camera2D>(e) else { continue };
        let here = DVec2::new(cam_world.translation.x, cam_world.translation.y);
        let want = c.step(here, target.map(|p| DVec2::new(p.x, p.y)), dt, t);
        // Z is left exactly where it was: a 2D camera's depth is a decision
        // about the ortho range, not something a follow gets to touch.
        let world_pos =
            crate::math::DVec3::new(want.x, want.y, cam_world.translation.z);
        let mut w = cam_world;
        w.translation = world_pos;
        let local = parent_world.inv_mul(&w);
        if let Some(tr) = world.get_mut::<crate::transform::Transform>(e) {
            tr.translation = local.translation;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f64, y: f64) -> DVec2 {
        DVec2::new(x, y)
    }

    /// The first step adopts where the camera already is, rather than snapping
    /// it to the origin — a camera placed in the editor must not jump on Play.
    #[test]
    fn the_first_frame_adopts_the_camera_where_it_stands() {
        let mut c = Camera2D { smoothing: 0.0, ..Default::default() };
        let out = c.step(at(10.0, -4.0), None, 1.0 / 60.0, 0.0);
        assert_eq!(out, at(10.0, -4.0));
        assert_eq!(c.pos, at(10.0, -4.0));
    }

    /// Inside the dead zone nothing moves; outside it, only the part that left.
    #[test]
    fn a_dead_zone_holds_the_camera_still_until_the_target_leaves_it() {
        let mut c = Camera2D { smoothing: 0.0, dead_zone: [2.0, 1.0], ..Default::default() };
        c.step(at(0.0, 0.0), None, 0.016, 0.0);
        // Well inside: not a pixel.
        c.step(at(0.0, 0.0), Some(at(1.9, 0.9)), 0.016, 0.0);
        assert_eq!(c.pos, at(0.0, 0.0), "a target inside the box must not move the camera");
        // Three units out on X: the camera moves the ONE unit that left the box.
        c.step(at(0.0, 0.0), Some(at(3.0, 0.0)), 0.016, 0.0);
        assert!((c.pos.x - 1.0).abs() < 1e-9, "moved {} instead of 1", c.pos.x);
        assert_eq!(c.pos.y, 0.0);
    }

    /// The lag must be the same at 30 fps and at 144 — the reason smoothing is
    /// exponential and not a fraction per frame.
    #[test]
    fn the_lag_does_not_depend_on_the_frame_rate() {
        let run = |dt: f32, steps: usize| {
            let mut c = Camera2D { smoothing: 0.2, ..Default::default() };
            c.step(at(0.0, 0.0), None, dt, 0.0);
            for _ in 0..steps {
                c.step(at(0.0, 0.0), Some(at(10.0, 0.0)), dt, 0.0);
            }
            c.pos.x
        };
        let slow = run(1.0 / 30.0, 30); // one second
        let fast = run(1.0 / 144.0, 144); // the same second
        assert!(
            (slow - fast).abs() < 0.02,
            "a second of catching up gave {slow} at 30fps and {fast} at 144fps"
        );
        // …and a second at tau = 0.2 really has nearly arrived.
        assert!(slow > 9.9, "{slow}");
    }

    /// Zero smoothing snaps, which is what a game with no camera lag wants and
    /// what a `0` in the Inspector has to mean.
    #[test]
    fn no_smoothing_means_no_smoothing() {
        let mut c = Camera2D { smoothing: 0.0, ..Default::default() };
        c.step(at(0.0, 0.0), None, 0.016, 0.0);
        c.step(at(0.0, 0.0), Some(at(7.0, -3.0)), 0.016, 0.0);
        assert_eq!(c.pos, at(7.0, -3.0));
    }

    /// The limits clamp the FOLLOW state, so a target that wanders far outside
    /// and comes back does not drag the camera through a stored position it
    /// never actually had.
    #[test]
    fn limits_clamp_the_state_and_not_just_the_picture() {
        let mut c = Camera2D {
            smoothing: 0.0,
            limits_on: true,
            limit_min: [-5.0, -5.0],
            limit_max: [5.0, 5.0],
            ..Default::default()
        };
        c.step(at(0.0, 0.0), None, 0.016, 0.0);
        c.step(at(0.0, 0.0), Some(at(100.0, 0.0)), 0.016, 0.0);
        assert_eq!(c.pos.x, 5.0, "the camera must not show outside the level");
        // Back inside, immediately — not after crawling home from x = 100.
        let out = c.step(at(0.0, 0.0), Some(at(1.0, 0.0)), 0.016, 0.0);
        assert_eq!(out.x, 1.0);
    }

    /// A backwards limit rectangle is a typo. It must not pin the camera into a
    /// corner, which reads as the camera being broken rather than the numbers.
    #[test]
    fn a_backwards_limit_rectangle_parks_in_the_middle() {
        let mut c = Camera2D {
            smoothing: 0.0,
            limits_on: true,
            limit_min: [10.0, 0.0],
            limit_max: [-10.0, 0.0],
            ..Default::default()
        };
        c.step(at(3.0, 0.0), None, 0.016, 0.0);
        assert_eq!(c.pos.x, 0.0);
    }

    /// Shake moves the picture and not the follow — the point of keeping `pos`.
    #[test]
    fn a_shake_never_feeds_back_into_the_follow() {
        let mut c = Camera2D { smoothing: 0.0, ..Default::default() };
        c.step(at(0.0, 0.0), None, 0.016, 0.0);
        c.shake(2.0, 1.0);
        let mut moved = false;
        let mut t = 0.0;
        for _ in 0..30 {
            let out = c.step(at(0.0, 0.0), Some(at(0.0, 0.0)), 1.0 / 60.0, t);
            t += 1.0 / 60.0;
            assert_eq!(c.pos, at(0.0, 0.0), "the shake leaked into the follow state");
            moved |= (out - c.pos).length() > 1e-6;
        }
        assert!(moved, "a shake that never moved the picture is not a shake");
    }

    /// It ends, and it ends by fading rather than by stopping mid-swing.
    #[test]
    fn a_shake_runs_down_and_stops() {
        let mut c = Camera2D { smoothing: 0.0, ..Default::default() };
        c.step(at(0.0, 0.0), None, 0.016, 0.0);
        c.shake(3.0, 0.5);
        let mut t = 0.0;
        let mut peak_early: f64 = 0.0;
        let mut peak_late: f64 = 0.0;
        for i in 0..60 {
            let out = c.step(at(0.0, 0.0), None, 1.0 / 60.0, t);
            t += 1.0 / 60.0;
            let d = (out - c.pos).length();
            if i < 10 {
                peak_early = peak_early.max(d);
            } else if (20..30).contains(&i) {
                peak_late = peak_late.max(d);
            }
        }
        assert!(peak_late < peak_early, "{peak_late} was not weaker than {peak_early}");
        assert!(!c.shaking(), "the shake outlived its seconds");
        let out = c.step(at(0.0, 0.0), None, 1.0 / 60.0, t);
        assert_eq!(out, c.pos, "a finished shake must leave the camera exactly on target");
    }

    /// Shaking every frame while something explodes must not compound.
    #[test]
    fn shaking_repeatedly_does_not_build_an_unbounded_shake() {
        let mut c = Camera2D::default();
        for _ in 0..100 {
            c.shake(1.0, 0.3);
        }
        assert_eq!(c.shake_amp, 1.0);
        assert!(c.shake_left <= 0.3 + 1e-6);
    }

    /// The same frame on two machines has to look the same — the shake is a
    /// function of the clock, never of a random number.
    #[test]
    fn the_shake_is_the_same_on_every_machine() {
        let run = || {
            let mut c = Camera2D { smoothing: 0.0, ..Default::default() };
            c.step(at(0.0, 0.0), None, 0.016, 0.0);
            c.shake(2.0, 1.0);
            c.step(at(0.0, 0.0), None, 1.0 / 60.0, 12.345)
        };
        assert_eq!(run(), run());
    }

    /// **Two nodes with the same name resolve to the same one every frame.**
    /// Following was a linear scan that took whichever `Name` the world yielded
    /// first, so a scene with a duplicate name — a prefab dropped in twice, a
    /// spawned copy — could hand the camera a different target from one frame to
    /// the next and the picture jumped between two places with nothing in the
    /// scene changing.
    ///
    /// It is still ambiguous by construction. The guarantee is that it is
    /// ambiguous the same way each time: the lowest entity index wins, which for
    /// a scene loaded from a file is the one written first.
    #[test]
    fn a_duplicated_follow_target_resolves_to_the_same_node_every_frame() {
        let mut world = crate::ecs::World::default();
        let named = |world: &mut crate::ecs::World, name: &str, x: f64| {
            let e = world.spawn();
            let mut t = crate::transform::Transform::default();
            t.translation.x = x;
            world.insert(e, t);
            world.insert(e, crate::matter::Name(name.to_string()));
            e
        };
        // Two "Player"s. The first one spawned is the one to follow.
        let _first = named(&mut world, "Player", 100.0);
        let _second = named(&mut world, "Player", -100.0);
        let cam = world.spawn();
        world.insert(cam, crate::transform::Transform::default());
        world.insert(
            cam,
            crate::matter::Matter::Camera {
                fov_y: 1.0,
                active: true,
                target: String::new(),
                cull_mask: u32::MAX,
                target_w: 0,
                target_h: 0,
                target_hz: 0.0,
                ortho: true,
                ortho_height: 10.0,
            },
        );
        world.insert(cam, Camera2D { follow: "Player".into(), smoothing: 0.0, ..Default::default() });
        for _ in 0..4 {
            step_all(&mut world, 0.016, 0.0);
            let x = world.get::<crate::transform::Transform>(cam).unwrap().translation.x;
            assert_eq!(x, 100.0, "the camera followed the other Player");
        }
    }

    /// **An empty `follow` is "follow nothing", not "follow the node with no
    /// name".** Resolving the whole map up front made that a live possibility —
    /// an unnamed node is `Name("")` and the map would have handed it back.
    #[test]
    fn following_nothing_does_not_match_a_node_with_no_name() {
        let mut world = crate::ecs::World::default();
        let unnamed = world.spawn();
        let mut t = crate::transform::Transform::default();
        t.translation.x = -500.0;
        world.insert(unnamed, t);
        world.insert(unnamed, crate::matter::Name(String::new()));
        // A second camera that DOES follow something, so the name map is built
        // at all — the empty-name entry only exists once it is.
        let target = world.spawn();
        world.insert(target, crate::transform::Transform::default());
        world.insert(target, crate::matter::Name("Player".into()));

        let ortho = || crate::matter::Matter::Camera {
            fov_y: 1.0,
            active: true,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: 0,
            target_h: 0,
            target_hz: 0.0,
            ortho: true,
            ortho_height: 10.0,
        };
        let follower = world.spawn();
        world.insert(follower, crate::transform::Transform::default());
        world.insert(follower, ortho());
        world.insert(follower, Camera2D { follow: "Player".into(), ..Default::default() });

        let idle = world.spawn();
        let mut t = crate::transform::Transform::default();
        t.translation.x = 9.0;
        world.insert(idle, t);
        world.insert(idle, ortho());
        world.insert(idle, Camera2D::default());

        step_all(&mut world, 0.016, 0.0);
        assert_eq!(
            world.get::<crate::transform::Transform>(idle).unwrap().translation.x,
            9.0,
            "a camera following nothing was taken to the node with no name"
        );
    }

    /// A camera that follows nothing is not moved by this at all — the position
    /// is somebody else's, and stepping it would be the engine editing a node
    /// a script or the editor owns.
    #[test]
    fn a_camera_following_nothing_is_left_where_it_is() {
        let mut world = crate::ecs::World::default();
        let cam = world.spawn();
        let mut t = crate::transform::Transform::default();
        t.translation.x = 7.0;
        world.insert(cam, t);
        world.insert(
            cam,
            crate::matter::Matter::Camera {
                fov_y: 1.0,
                active: true,
                target: String::new(),
                cull_mask: u32::MAX,
                target_w: 0,
                target_h: 0,
                target_hz: 0.0,
                ortho: true,
                ortho_height: 10.0,
            },
        );
        world.insert(cam, Camera2D::default());
        step_all(&mut world, 0.016, 0.0);
        assert_eq!(world.get::<crate::transform::Transform>(cam).unwrap().translation.x, 7.0);
    }
}
