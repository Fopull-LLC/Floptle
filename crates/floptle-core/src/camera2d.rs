//! How a 2D camera follows. Attached to an orthographic Camera node.
//!
//! The rule every 2D game writes: chase the target, but not exactly, not off the
//! edge of the level, and shake when something hits. In this order:
//!
//! 1. **Dead zone.** The camera does not move until the target leaves a box
//!    around it; otherwise every footstep reads as the world wobbling.
//! 2. **Smoothing.** The remainder is approached exponentially, not by a
//!    fraction per frame — `lerp(a, b, k * dt)` lags differently at 30 and 144.
//! 3. **Limits.** The result is clamped to the level's bounds.
//! 4. **Shake**, added after all of that and not fed back.
//!
//! Step 4 is why the camera keeps [`Camera2D::pos`] of its own rather than
//! reading its node's transform each frame: a shake written into the transform
//! and read back is a shake the follow then chases and the limits then clamp.

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
    /// Land the drawn camera position on a whole pixel of this many per world
    /// unit. `0` is off.
    ///
    /// The number is `camera.pixelsPerUnit()` — a sprite drawn at 32 pixels per
    /// unit wants `32`. Without it a camera that stops between two pixels
    /// resamples every sprite in the scene by a fraction of one, and pixel art
    /// shimmers along its edges while nothing in the scene is moving. Every 2D
    /// project ends up writing this by hand, against a constant the engine
    /// already knows.
    ///
    /// Applied to what is drawn and not to [`pos`](Self::pos), for the same
    /// reason the shake is: the follow has to keep its sub-pixel place or the
    /// rounding is re-applied to an already-rounded number every frame and the
    /// camera cannot creep at less than a pixel per frame at all.
    pub pixel_snap: f32,
    /// How far ahead of the target to look, in world units per axis: the
    /// camera leads the way the target last moved, so a player running right
    /// sees more of what is coming than of what is behind. `[0, 0]` is off.
    pub look_ahead: [f32; 2],
    /// Nodes carrying this tag are rooms. The camera keeps its view inside the
    /// room the target is in, and glides to the next one as the target crosses
    /// into it. A room is the area of the tile layers it is, or holds. Empty is
    /// off.
    pub room_tag: String,
    /// Keep pixel art pixel-perfect: the art's pixels per world unit (`0` is
    /// off). The camera's height is held at the nearest value where each art
    /// pixel covers a whole number of screen pixels, so no pixel draws two
    /// wide beside one drawn three, and the pixel snap is set to match.
    pub art_pixels: f32,
    /// The height the camera was asked for, by its author or a script, which
    /// the pixel-perfect one is rounded from. Live state.
    pub zoom_asked: Option<f32>,
    /// The height last written, to tell a script's own write from ours.
    pub zoom_written: Option<f32>,

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
    /// Everything the last frame added on top of [`pos`](Self::pos) before
    /// writing the transform — the shake, and the pixel snap.
    ///
    /// Kept because that sum is written into the node's transform and the
    /// transform is what a re-seed reads back. Without it, adopting the camera's
    /// current position mid-shake bakes that frame's shake into the follow —
    /// permanently, and asymmetrically at a world limit, which is exactly the
    /// drift this module is arranged to avoid. The snap is in it for the same
    /// reason: a rounding read back as a position is a rounding applied to an
    /// already-rounded number, every frame, forever.
    pub last_shake: DVec2,
    /// Where the camera was last drawn, which is what its transform holds
    /// until something else writes it. A transform that no longer says this
    /// was moved by a script, and is read as it is.
    pub last_out: Option<DVec2>,
    /// Half the size of what the camera shows, in world units, this frame.
    /// The limits keep the VIEW inside them, so they need to know how far it
    /// reaches either side of the centre. Set by [`step_all_view`].
    pub view_half: DVec2,
    /// The rooms, as world rectangles `[min x, min y, max x, max y]`, this
    /// frame. Set by [`step_all_view`].
    pub rooms: Vec<[f64; 4]>,
    /// The room the camera is keeping to, once the target has entered one.
    pub room: Option<[f64; 4]>,
    /// Where the target was last frame, to tell which way it is going.
    pub last_target: Option<DVec2>,
    /// The look-ahead this frame, eased towards the way the target moves.
    pub lead: DVec2,
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
            pixel_snap: 0.0,
            pos: DVec2::ZERO,
            started: false,
            shake_amp: 0.0,
            shake_left: 0.0,
            shake_total: 0.0,
            view_half: DVec2::ZERO,
            last_shake: DVec2::ZERO,
            last_out: None,
            look_ahead: [0.0, 0.0],
            room_tag: String::new(),
            art_pixels: 0.0,
            zoom_asked: None,
            zoom_written: None,
            rooms: Vec::new(),
            room: None,
            last_target: None,
            lead: DVec2::ZERO,
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
        // meant a bang during a three-second rumble ended the rumble after a
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
        // `here` carries last frame's shake and snap, when it is still what
        // was written to the transform, and that has to come back off before
        // it is read as a position. A transform a script has written since is
        // a fresh position with nothing of ours in it: taking last frame's
        // rounding off that moved a script-driven camera by a different
        // fraction of a pixel every frame.
        let unshaken = if self.last_out.is_some_and(|o| (o - here).length_squared() < 1e-18) {
            here - self.last_shake
        } else {
            here
        };
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
        if let Some(raw) = target {
            // 0. Look-ahead: lead the way the target is moving, and keep
            // leading that way while it stands still, so stopping does not
            // swing the view back. Eased over half a second, or the view
            // lurches every time the player turns round.
            let mut target = raw;
            let dt64 = dt.max(0.0) as f64;
            if let Some(prev) = self.last_target
                && dt64 > 0.0
            {
                let v = (raw - prev) / dt64;
                for i in 0..2 {
                    let reach = self.look_ahead[i].max(0.0) as f64;
                    if reach > 0.0 && v[i].abs() > 0.5 {
                        let goal = reach * v[i].signum();
                        let k = 1.0 - (-dt64 / 0.5).exp();
                        self.lead[i] += (goal - self.lead[i]) * k;
                    } else if reach <= 0.0 {
                        self.lead[i] = 0.0;
                    }
                }
            }
            self.last_target = Some(raw);
            target += self.lead;
            // The room the target stands in. Leaving every room keeps the last
            // one, so a gap between two rooms does not let the view drift free.
            if let Some(r) = self.rooms.iter().find(|r| raw.x >= r[0] && raw.x <= r[2] && raw.y >= r[1] && raw.y <= r[3]) {
                self.room = Some(*r);
            }
            if let Some(r) = self.room {
                // Clamped before the smoothing, not after, so a change of room
                // is a glide rather than a cut.
                for i in 0..2 {
                    let half = self.view_half[i].max(0.0);
                    let (lo, hi) = (r[i] + half, r[i + 2] - half);
                    target[i] = if lo <= hi { target[i].clamp(lo, hi) } else { (lo + hi) * 0.5 };
                }
            }
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
        // Whether a limit is holding the camera this frame, rather than the
        // target leading it: the pixel snap below treats the two differently.
        let before_limits = self.pos;
        // 3. Limits, on the followed position — so the state itself never leaves
        // the level and there is nothing to un-clamp when the target comes back.
        // They bound what is SHOWN: the centre stays half a view inside each
        // edge. Clamping the centre to them instead showed half a screen of
        // nothing past every end of the level, which is the one thing limits
        // are for.
        if self.limits_on {
            for i in 0..2 {
                let half = self.view_half[i].max(0.0);
                let (lo, hi) = (self.limit_min[i] as f64 + half, self.limit_max[i] as f64 - half);
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
        // 4. Shake, added to what is drawn and never written back into `pos`.
        let mut out = self.pos;
        if self.shaking() {
            let left = (self.shake_left / self.shake_total.max(1e-6)).clamp(0.0, 1.0) as f64;
            let amp = self.shake_amp as f64 * left;
            let phase = t * SHAKE_HZ * std::f64::consts::TAU;
            out += DVec2::new(
                amp * phase.sin(),
                // A different rate on Y, so the motion is a jitter rather than a
                // line through the corners.
                amp * (phase * 1.37 + 1.7).sin(),
            );
            self.shake_left = (self.shake_left - dt.max(0.0)).max(0.0);
            if self.shake_left <= 0.0 {
                self.shake_amp = 0.0;
            }
        }
        // 5. Pixel snap, last — so it is the drawn position that lands on the
        // grid, shake included. Snapping before the shake would put the camera
        // on a whole pixel and then move it off one again, which is the entire
        // problem.
        //
        // Following, the camera is snapped to a whole number of pixels from
        // its target rather than to the world's grid: the target is drawn
        // where it really is, and rounding the camera on its own moved it a
        // pixel against the target on some frames and not others, so a player
        // walking steadily wobbled on screen. Held by a limit, there is
        // nothing to wobble against, and the world grid keeps the level still.
        if self.pixel_snap.is_finite() && self.pixel_snap > 0.0 {
            let ppu = self.pixel_snap as f64;
            let snap = |v: DVec2| DVec2::new((v.x * ppu).round() / ppu, (v.y * ppu).round() / ppu);
            let held = (self.pos - before_limits).length_squared() > 1e-18;
            out = match (target, held) {
                (Some(t), false) => t + snap(out - t),
                _ => snap(out),
            };
        }
        // Whatever those two added is what the transform will carry, and what
        // the next frame has to take back off before it reads a position.
        self.last_shake = out - self.pos;
        self.last_out = Some(out);
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
    step_all_view(world, dt, t, None);
}

/// [`step_all`], knowing the viewport's width over height, so the limits can
/// keep the whole view inside them. `None` clamps the centre alone.
pub fn step_all_view(world: &mut crate::ecs::World, dt: f32, t: f64, aspect: Option<f32>) {
    step_all_screen(world, dt, t, aspect, None);
}

/// The camera height in `art_pixels` world units per art pixel that puts a
/// whole number of the picture's `screen_px` rows on each art pixel, nearest
/// to `asked`; and that whole number.
pub fn pixel_perfect_height(asked: f32, screen_px: f32, art_pixels: f32) -> (f32, f32) {
    let per = screen_px / (asked.max(1e-3) * art_pixels.max(1e-3));
    let k = per.round().max(1.0);
    (screen_px / (k * art_pixels), k)
}

/// [`step_all_view`], also knowing how many pixels tall the picture is
/// (`screen_px`: the retro height, or the window's), which pixel-perfect
/// zoom (`Camera2D::art_pixels`) needs.
pub fn step_all_screen(world: &mut crate::ecs::World, dt: f32, t: f64, aspect: Option<f32>, screen_px: Option<f32>) {
    if let Some(px) = screen_px.filter(|p| *p > 0.0) {
        let cams: Vec<crate::ecs::Entity> = world.query::<Camera2D>().map(|(e, _)| e).collect();
        for e in cams {
            let Some(Some(cur)) = world.get::<crate::matter::Matter>(e).map(|m| match m {
                crate::matter::Matter::Camera { ortho: true, ortho_height, .. } => Some(*ortho_height),
                _ => None,
            }) else {
                continue;
            };
            let Some(c) = world.get_mut::<Camera2D>(e) else { continue };
            // A height that is not the one written last frame was written by
            // somebody else (a script zooming, the Inspector): it is the new ask.
            if c.zoom_written != Some(cur) {
                c.zoom_asked = Some(cur);
            }
            let write = if c.art_pixels > 0.0 {
                let (h, k) = pixel_perfect_height(c.zoom_asked.unwrap_or(cur), px, c.art_pixels);
                c.pixel_snap = k * c.art_pixels;
                Some(h)
            } else {
                // Switched off: hand back the height that was asked for.
                c.zoom_asked.take().filter(|a| *a != cur)
            };
            c.zoom_written = write.or(Some(cur));
            if let Some(h) = write
                && let Some(crate::matter::Matter::Camera { ortho_height, .. }) = world.get_mut::<crate::matter::Matter>(e)
            {
                *ortho_height = h;
            }
        }
    }
    // Collected first: resolving a target's world transform reads the world,
    // and the write below needs it mutably.
    //
    // **Only an orthographic camera.** This function owns the transform of
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
    // Rooms, by tag, only when some camera keeps to them.
    let tags: Vec<String> = world
        .query::<Camera2D>()
        .filter(|(_, c)| !c.room_tag.is_empty())
        .map(|(_, c)| c.room_tag.clone())
        .collect();
    let rooms: std::collections::HashMap<String, Vec<[f64; 4]>> = tags.into_iter().map(|t| {
        let r = rooms_tagged(world, &t);
        (t, r)
    }).collect();
    for (e, follow) in jobs {
        // The camera's own place, and the frame its `translation` is written in.
        // A camera under a parent is unusual and legal; doing the arithmetic in
        // world space and converting back is what makes it work rather than
        // drifting by the parent's transform.
        let cam_world = crate::matter::world_transform(world, e);
        // An empty follow is "follow nothing", and must not be able to match a
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
        let half = match (aspect, world.get::<crate::matter::Matter>(e)) {
            (Some(a), Some(crate::matter::Matter::Camera { ortho_height, .. })) if a.is_finite() && a > 0.0 => {
                let hh = (*ortho_height as f64 * 0.5).max(0.0);
                // Scaled with the camera's own scale, as the projection is.
                DVec2::new(hh * a as f64, hh)
            }
            _ => DVec2::ZERO,
        };
        let Some(c) = world.get_mut::<Camera2D>(e) else { continue };
        c.view_half = half;
        c.rooms = rooms.get(&c.room_tag).cloned().unwrap_or_default();
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

/// The world rectangle of every node tagged `tag`: the tile layers it is, or
/// holds anywhere beneath it, together.
pub fn rooms_tagged(world: &crate::ecs::World, tag: &str) -> Vec<[f64; 4]> {
    let mut out: std::collections::BTreeMap<u32, [f64; 4]> = std::collections::BTreeMap::new();
    for (e, m) in world.query::<crate::matter::Matter>() {
        let crate::matter::Matter::Tilemap { cols, rows, tile, .. } = m else { continue };
        // The tagged node this layer belongs to: itself, or its nearest
        // tagged ancestor.
        let mut at = Some(e);
        let mut room = None;
        let mut guard = 0;
        while let Some(n) = at {
            if world.get::<crate::matter::Tags>(n).is_some_and(|t| t.0.iter().any(|x| x == tag)) {
                room = Some(n);
                break;
            }
            at = world.get::<crate::matter::Parent>(n).map(|p| p.0);
            guard += 1;
            if guard > 256 {
                break;
            }
        }
        let Some(room) = room else { continue };
        let xf = crate::matter::world_transform(world, e);
        let (hw, hh) = (*cols as f64 * *tile as f64 * 0.5, *rows as f64 * *tile as f64 * 0.5);
        let mut r = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            let local = crate::math::Vec3::new((sx * hw) as f32, (sy * hh) as f32, 0.0);
            let p = xf.translation + (xf.rotation * (xf.scale * local)).as_dvec3();
            r = [r[0].min(p.x), r[1].min(p.y), r[2].max(p.x), r[3].max(p.y)];
        }
        let e = out.entry(room.index()).or_insert(r);
        *e = [e[0].min(r[0]), e[1].min(r[1]), e[2].max(r[2]), e[3].max(r[3])];
    }
    out.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Running right, the camera settles ahead of the player; stopping keeps
    /// the lead, and running left swings it to the other side.
    #[test]
    fn the_camera_looks_ahead_of_where_the_target_is_going() {
        let mut c = Camera2D { smoothing: 0.0, look_ahead: [3.0, 0.0], ..Default::default() };
        c.step(at(0.0, 0.0), None, 0.016, 0.0);
        let mut x = 0.0;
        for _ in 0..240 {
            x += 0.1;
            c.step(at(0.0, 0.0), Some(at(x, 0.0)), 1.0 / 60.0, 0.0);
        }
        assert!((c.pos.x - (x + 3.0)).abs() < 0.05, "ahead by 3: {} vs {}", c.pos.x, x);
        for _ in 0..60 {
            c.step(at(0.0, 0.0), Some(at(x, 0.0)), 1.0 / 60.0, 0.0);
        }
        assert!((c.pos.x - (x + 3.0)).abs() < 0.05, "standing still keeps the lead");
        for _ in 0..240 {
            x -= 0.1;
            c.step(at(0.0, 0.0), Some(at(x, 0.0)), 1.0 / 60.0, 0.0);
        }
        assert!((c.pos.x - (x - 3.0)).abs() < 0.05, "running left leads left");
    }

    /// A room is the area of the tile layers under the tagged node, wherever
    /// they sit beneath it; untagged layers are not rooms.
    #[test]
    fn a_tagged_node_is_a_room_the_size_of_its_tile_layers() {
        use crate::matter::{Matter, Parent, Tags};
        use crate::transform::Transform;
        let mut w = crate::ecs::World::default();
        let level = w.spawn();
        w.insert(level, Transform { translation: crate::math::DVec3::new(10.0, 0.0, 0.0), ..Transform::IDENTITY });
        w.insert(level, Tags(vec!["room".into()]));
        let layer = w.spawn();
        w.insert(layer, Transform { translation: crate::math::DVec3::new(4.0, -2.0, 0.0), ..Transform::IDENTITY });
        w.insert(layer, Matter::Tilemap { cols: 8, rows: 4, tile: 1.0, data: vec![], tileset: String::new() });
        w.insert(layer, Parent(level));
        let stray = w.spawn();
        w.insert(stray, Transform::IDENTITY);
        w.insert(stray, Matter::Tilemap { cols: 2, rows: 2, tile: 1.0, data: vec![], tileset: String::new() });
        assert_eq!(rooms_tagged(&w, "room"), vec![[10.0, -4.0, 18.0, 0.0]]);
        assert!(rooms_tagged(&w, "other").is_empty());
    }

    /// In a room the view stays inside it; crossing into the next room glides
    /// the camera over rather than cutting.
    #[test]
    fn the_camera_keeps_to_the_room_the_target_is_in() {
        let mut c = Camera2D { smoothing: 0.2, ..Default::default() };
        c.view_half = DVec2::new(4.0, 2.0);
        c.rooms = vec![[0.0, 0.0, 20.0, 10.0], [20.0, 0.0, 40.0, 10.0]];
        c.step(at(10.0, 5.0), None, 0.016, 0.0);
        for _ in 0..120 {
            c.step(at(10.0, 5.0), Some(at(19.0, 5.0)), 1.0 / 60.0, 0.0);
        }
        assert!((c.pos.x - 16.0).abs() < 0.01, "the view's right edge stops at the room's: {}", c.pos.x);
        c.step(at(10.0, 5.0), Some(at(21.0, 5.0)), 1.0 / 60.0, 0.0);
        assert!(c.pos.x > 16.0 && c.pos.x < 24.0, "a glide, not a cut: {}", c.pos.x);
        for _ in 0..240 {
            c.step(at(10.0, 5.0), Some(at(21.0, 5.0)), 1.0 / 60.0, 0.0);
        }
        assert!((c.pos.x - 24.0).abs() < 0.01, "the view's left edge stops at the new room's: {}", c.pos.x);
    }

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
        // Three units out on X: the camera moves the one unit that left the box.
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

    /// The limits clamp the follow state, so a target that wanders far outside
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

    /// **A pixel-art camera must land on whole pixels.** A camera that stops
    /// between two of them resamples every sprite in the scene by a fraction of
    /// one, and the art shimmers along its edges while nothing is moving. Every
    /// 2D project writes this by hand against a constant the engine already
    /// knows.
    ///
    /// Snapping the drawn position and not the position it was given is the
    /// part that matters: round what the script wrote and a camera eased a
    /// third of a pixel a frame never moves at all. (A camera following a
    /// target snaps to whole pixels from the target instead; see
    /// `a_followed_target_does_not_wobble_on_screen`.)
    #[test]
    fn a_snapped_camera_draws_on_whole_pixels_and_still_creeps_between_them() {
        let mut c = Camera2D { smoothing: 0.0, pixel_snap: 32.0, ..Default::default() };
        let step = 1.0 / 32.0;
        let out = c.step(at(0.0, 0.0), None, 0.016, 0.0);
        assert_eq!(out.x, 0.0);
        // A script easing it a third of a pixel at a time.
        let out = c.step(at(step / 3.0, 0.0), None, 0.016, 0.0);
        assert_eq!(out.x, 0.0, "a third of a pixel should draw at the same pixel");
        c.step(at(step * 0.7, 0.0), None, 0.016, 0.0);
        let out = c.step(at(step * 1.1, 0.0), None, 0.016, 0.0);
        assert!((out.x - step).abs() < 1e-9, "the camera never crossed a pixel: {}", out.x);

        // Off is off.
        let mut plain = Camera2D { smoothing: 0.0, ..Default::default() };
        plain.step(at(0.0, 0.0), Some(at(0.0, 0.0)), 0.016, 0.0);
        let out = plain.step(at(0.0, 0.0), Some(at(step / 3.0, 0.0)), 0.016, 0.0);
        assert!((out.x - step / 3.0).abs() < 1e-9, "an unsnapped camera was rounded");
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
                stack: Vec::new(),
                post_after: false,
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
        // A second camera that does follow something, so the name map is built
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
            stack: Vec::new(),
            post_after: false,
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
                stack: Vec::new(),
                post_after: false,
            },
        );
        world.insert(cam, Camera2D::default());
        step_all(&mut world, 0.016, 0.0);
        assert_eq!(world.get::<crate::transform::Transform>(cam).unwrap().translation.x, 7.0);
    }

    /// Limits keep what the camera SHOWS inside them. An 11-unit-tall view at
    /// 16:9 is ±9.78 wide, so a camera limited to a 60×16 level and chasing a
    /// target at its left end stops 9.78 in, not at the edge with half a
    /// screen of nothing beyond it.
    #[test]
    fn limits_keep_the_view_inside_the_level() {
        let mut c = Camera2D {
            limits_on: true,
            limit_min: [0.0, 0.0],
            limit_max: [60.0, 16.0],
            smoothing: 0.0,
            view_half: DVec2::new(5.5 * 16.0 / 9.0, 5.5),
            ..Default::default()
        };
        let out = c.step(DVec2::ZERO, Some(DVec2::new(1.0, 1.0)), 1.0 / 60.0, 0.0);
        assert!((out.x - 5.5 * 16.0 / 9.0).abs() < 1e-6 && (out.y - 5.5).abs() < 1e-6, "{out:?}");
        // A level narrower than the view: the camera sits in its middle.
        c.limit_max = [8.0, 16.0];
        let out = c.step(out, Some(DVec2::new(1.0, 1.0)), 1.0 / 60.0, 0.0);
        assert!((out.x - 4.0).abs() < 1e-6, "{out:?}");
    }

    /// A script moving a snapped camera itself, a third of a pixel a frame:
    /// what is drawn never steps backwards. Last frame's rounding used to be
    /// taken off each fresh position, a different fraction of a pixel every
    /// frame.
    #[test]
    fn a_script_driven_snapped_camera_moves_steadily() {
        let ppu = 8.0f64;
        let mut c = Camera2D { pixel_snap: ppu as f32, ..Default::default() };
        let mut script = DVec2::ZERO;
        let mut last = f64::MIN;
        for f in 0..200 {
            // The script keeps its own unrounded position and writes it.
            script.x += 0.3 / ppu;
            let out = c.step(script, None, 1.0 / 60.0, f as f64 / 60.0);
            assert!(out.x >= last, "frame {f}: drawn at {} after {last}", out.x * ppu);
            assert!((out.x - script.x).abs() <= 0.5 / ppu + 1e-9, "within half a pixel of where the script put it");
            last = out.x;
        }
    }

    /// Following a target that moves a steady 2.8 pixels a frame, the target's
    /// place on screen never reverses: the camera keeps a whole-pixel offset
    /// from it. Rounded on its own, the camera crossed pixel boundaries on
    /// different frames from the target and the target wobbled a pixel.
    #[test]
    fn a_followed_target_does_not_wobble_on_screen() {
        for fps in [60.0f64, 75.0, 120.0, 144.0, 165.0] {
            let ppu = 24.0f64;
            let mut c = Camera2D { pixel_snap: ppu as f32, smoothing: 0.15, ..Default::default() };
            let speed = 2.8 * 60.0 / ppu; // 2.8 px a frame at 60, the same speed at every rate
            let dt = 1.0 / fps;
            let mut here = DVec2::ZERO;
            let mut screen: Vec<f64> = Vec::new();
            for f in 0..(fps as usize * 4) {
                let target = DVec2::new(speed * f as f64 * dt, 0.0);
                here = c.step(here, Some(target), dt as f32, f as f64 * dt);
                // Where the target lands on screen, in pixels from the centre.
                screen.push(((target.x - here.x) * ppu).round());
            }
            let moves: Vec<f64> = screen.windows(2).map(|w| w[1] - w[0]).filter(|d| *d != 0.0).collect();
            let reversals = moves.windows(2).filter(|w| w[0].signum() != w[1].signum()).count();
            assert_eq!(reversals, 0, "at {fps} fps the target turned back on screen {reversals} times");
        }
    }

    /// Held still by a limit, a snapped camera sits on the world's pixel grid,
    /// so the level does not shimmer while the player walks into a wall.
    #[test]
    fn a_camera_held_by_a_limit_sits_on_the_grid() {
        let ppu = 8.0f64;
        let mut c = Camera2D {
            pixel_snap: ppu as f32,
            smoothing: 0.0,
            limits_on: true,
            limit_min: [-10.0, -10.0],
            limit_max: [5.03, 10.0],
            ..Default::default()
        };
        let mut here = DVec2::ZERO;
        for f in 0..30 {
            here = c.step(here, Some(DVec2::new(20.0 + f as f64 * 0.01, 0.0)), 1.0 / 60.0, f as f64 / 60.0);
        }
        assert!(((here.x * ppu) - (here.x * ppu).round()).abs() < 1e-9, "on the grid: {}", here.x * ppu);
    }

    /// `artPixels = 8` on a 240-row picture: a camera 11 units tall (2.73
    /// screen pixels an art pixel) is held at 10 (exactly 3), the snap is set
    /// to match, a script's zoom to 15 lands on 15 (exactly 2), and switching
    /// it off hands back the height that was asked for.
    #[test]
    fn art_pixels_hold_the_zoom_at_a_whole_scale() {
        use crate::ecs::World;
        use crate::matter::Matter;
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, crate::transform::Transform::IDENTITY);
        world.insert(e, Matter::Camera {
            fov_y: 1.0,
            active: true,
            target: String::new(),
            cull_mask: !0,
            target_w: 0,
            target_h: 0,
            target_hz: 0.0,
            ortho: true,
            ortho_height: 11.0,
            stack: Vec::new(),
            post_after: false,
        });
        world.insert(e, Camera2D { art_pixels: 8.0, ..Default::default() });
        let height = |w: &World| match w.get::<Matter>(e) {
            Some(Matter::Camera { ortho_height, .. }) => *ortho_height,
            _ => unreachable!(),
        };
        step_all_screen(&mut world, 1.0 / 60.0, 0.0, Some(16.0 / 9.0), Some(240.0));
        assert_eq!(height(&world), 10.0);
        assert_eq!(world.get::<Camera2D>(e).unwrap().pixel_snap, 24.0);
        // A script zooms out.
        if let Some(Matter::Camera { ortho_height, .. }) = world.get_mut::<Matter>(e) {
            *ortho_height = 15.0;
        }
        step_all_screen(&mut world, 1.0 / 60.0, 0.0, Some(16.0 / 9.0), Some(240.0));
        assert_eq!(height(&world), 15.0);
        step_all_screen(&mut world, 1.0 / 60.0, 0.0, Some(16.0 / 9.0), Some(240.0));
        assert_eq!(height(&world), 15.0, "and stays put");
        // Off: back to what was asked.
        if let Some(Matter::Camera { ortho_height, .. }) = world.get_mut::<Matter>(e) {
            *ortho_height = 11.0;
        }
        step_all_screen(&mut world, 1.0 / 60.0, 0.0, Some(16.0 / 9.0), Some(240.0));
        world.get_mut::<Camera2D>(e).unwrap().art_pixels = 0.0;
        step_all_screen(&mut world, 1.0 / 60.0, 0.0, Some(16.0 / 9.0), Some(240.0));
        assert_eq!(height(&world), 11.0);
    }
}
