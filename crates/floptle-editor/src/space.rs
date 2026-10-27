//! On-rails celestial driver (solar demo S2, the Solar demo build order).
//!
//! Each gameplay tick: advance space time by `warp × dt`, assemble the scene's
//! `CelestialBody` nodes into a [`floptle_core::frames::System`], write every
//! non-root body node's translation from its Kepler elements (exact analytic
//! orbits — stable at any warp), re-anchor their terrain colliders in the sim,
//! rebuild gravity (the µ/r² centers moved), and feed the `space.*` snapshot to
//! scripts. The root body (empty `parent`) stays where the scene put it. Bodies
//! may live under a scene group (a generator's "<Star> System" folder): rails
//! positions are computed in world space and converted into the scene parent's
//! frame before the local-translation write.

use floptle_core::frames::{Body, Kepler, System};
use floptle_core::{CelestialBody, Entity, Transform};
use floptle_core::math::DVec3;

use crate::Editor;

/// The statics the rails carry, each with the celestial it rides, as of a
/// point in the change log and a count of baked statics. See
/// `Editor::carried_statics`.
pub(crate) struct CarriedStatics {
    cursor: floptle_core::ChangeCursor,
    baked: usize,
    list: Vec<(Entity, Entity)>,
    /// The statics in `list`, and every celestial, by index: what a logged
    /// change is held against.
    members: std::collections::HashSet<u32>,
    celestials: std::collections::HashSet<u32>,
}

/// Physics LOD for a distant compound craft (a deployed satellite, a spent
/// stage, a parked rover half a planet away). Far from the camera the full
/// contact sim is wasted heat: landed/slow craft freeze (anchored — pinned,
/// carried with their planet's frame), in-flight craft coast on an analytic
/// Kepler conic (drift-free orbits at any warp, for any number of craft).
/// Both wake to live physics when approached.
#[derive(Clone, Copy)]
pub(crate) enum CompoundLod {
    /// Anchored in place; `was_anchored` remembers a gameplay anchor (launch
    /// clamps) so waking doesn't silently release it, and `was_grounded`
    /// whether it was standing on something when it froze — what
    /// `assembly.info` keeps reporting while it is frozen.
    Frozen { was_anchored: bool, was_grounded: bool },
    /// On rails around dominant celestial `dom` with cached elements.
    Coast { dom: u32, k: Kepler },
}

/// Beyond this distance from the camera a compound drops to its LOD…
const LOD_FAR: f64 = 700.0;
/// …and inside this one it wakes back to live physics (hysteresis).
const LOD_NEAR: f64 = 500.0;
/// Seconds a far craft a script pushed stays out of LOD after the push: long
/// enough to span the gap between ticks of thrust, short enough that a craft
/// left alone goes back to its rails.
const PUSH_GRACE: f64 = 1.0;

/// Bring a compound out of its LOD into full physics: off the LOD's anchor
/// unless a gameplay anchor held it before, and a coasting one at the
/// velocity its rails give it now (the anchor zeroed the real one).
fn wake_lod(sim: &mut floptle_physics::Sim, eid: u32, lod: CompoundLod, cb: &[(Entity, CelestialBody)], t: f64) {
    match lod {
        CompoundLod::Coast { dom, k } => {
            sim.set_compound_anchored(eid, false);
            if let Some(i) = cb.iter().position(|(e, _)| e.index() == dom) {
                let (_r, v) = k.pos_vel(cb[i].1.mu, t);
                sim.set_compound_velocity(eid, v.as_vec3());
            }
        }
        CompoundLod::Frozen { was_anchored, .. } => {
            if !was_anchored {
                sim.set_compound_anchored(eid, false);
            }
        }
    }
}

impl Editor {
    /// Wake a compound out of distant-craft LOD before a script acts on it:
    /// an impulse or a held force on the LOD's anchored stand-in would be
    /// zeroed by the anchor. The next rails tick puts it back on its LOD if it
    /// is still far — a coasting craft on the conic its new velocity gives.
    pub(crate) fn wake_compound_from_lod(&mut self, eid: u32) {
        // However it is filed now, it stays live a moment after the push.
        self.lod_woken.insert(eid, PUSH_GRACE);
        let Some(lod) = self.compound_lod.remove(&eid) else { return };
        let cb: Vec<(Entity, CelestialBody)> =
            self.world.query::<CelestialBody>().map(|(e, b)| (e, b.clone())).collect();
        let t = self.space_time;
        if let Some(sim) = self.sim.as_mut() {
            wake_lod(sim, eid, lod, &cb, t);
        }
    }

    /// What `assembly.info` reports for a compound in LOD, rather than the
    /// anchored stand-in the sim holds: `(anchored, grounded, velocity)`. A
    /// coasting craft is free and moving at its rails velocity; a frozen one
    /// is as it was when it froze, at rest. `None` when it is not in LOD.
    pub(crate) fn lod_state(&self, eid: u32) -> Option<(bool, bool, [f32; 3])> {
        match *self.compound_lod.get(&eid)? {
            CompoundLod::Coast { dom, k } => {
                let mu = self
                    .world
                    .entity_with::<CelestialBody>(dom)
                    .and_then(|e| self.world.get::<CelestialBody>(e))
                    .map(|b| b.mu)?;
                let v = k.pos_vel(mu, self.space_time).1.as_vec3();
                Some((false, false, [v.x, v.y, v.z]))
            }
            CompoundLod::Frozen { was_anchored, was_grounded } => {
                Some((was_anchored, was_grounded, [0.0; 3]))
            }
        }
    }

    /// Republish `space.*` without advancing the clock when the set of
    /// celestial bodies has changed since the last rails tick. A no-op before
    /// the first tick (Play start snaps every body on that tick) and when
    /// nothing was added or removed.
    pub(crate) fn refresh_space_listing(&mut self) {
        if self.space_rails_prev.is_empty() {
            return;
        }
        let mut n = 0usize;
        let mut same = true;
        for (e, _) in self.world.query::<CelestialBody>() {
            n += 1;
            same &= self.space_rails_prev.contains_key(&e);
        }
        if same && n == self.space_rails_prev.len() {
            return;
        }
        // Zero time: every body already on the rails moves nowhere and carries
        // nothing; a new one takes the rails position its first tick would.
        self.update_space_rails(0.0);
    }

    /// Every static the rails carry, with the celestial it rides: a Static
    /// body, or any other baked static, under a celestial at any depth.
    ///
    /// Walking each one's parents every tick cost half a millisecond a tick
    /// on a base of 5,000 pieces, so the answer is kept until the change log
    /// says something changed that could change it: a parent, a collider, a
    /// body turned static or no longer static, a celestial made or unmade, a
    /// listed static spawned or despawned, or the set of baked statics. Bodies
    /// a script writes every frame, props coming and going and a celestial's
    /// fields changing are not that, and on a busy world they are most of the
    /// log.
    fn carried_statics(&mut self) -> Vec<(Entity, Entity)> {
        use floptle_core::{ChangeKind, Collidable, MeshCollider, Parent, RigidBody};
        use std::any::TypeId;
        let baked = self.static_baked.borrow().len();
        let is_static = |w: &floptle_core::World, i: u32| {
            w.entity_at(i).and_then(|e| w.get::<RigidBody>(e)).is_some_and(|rb| rb.mode == floptle_core::BodyMode::Static)
        };
        let is_celestial = |w: &floptle_core::World, i: u32| w.entity_at(i).is_some_and(|e| w.get::<CelestialBody>(e).is_some());
        let still = self.carried_statics.as_ref().is_some_and(|kept| {
            kept.baked == baked
                && self.world.changes_since(kept.cursor).is_some_and(|log| {
                    log.iter().all(|c| match c.kind {
                        ChangeKind::Lifetime => !kept.members.contains(&c.index) && !kept.celestials.contains(&c.index),
                        ChangeKind::Component(t) if t == TypeId::of::<RigidBody>() => {
                            is_static(&self.world, c.index) == kept.members.contains(&c.index)
                                || self.static_baked.borrow().contains_key(&c.index)
                        }
                        ChangeKind::Component(t) if t == TypeId::of::<CelestialBody>() => {
                            is_celestial(&self.world, c.index) == kept.celestials.contains(&c.index)
                        }
                        ChangeKind::Component(t) => {
                            t != TypeId::of::<Parent>() && t != TypeId::of::<Collidable>() && t != TypeId::of::<MeshCollider>()
                        }
                    })
                })
        });
        let cursor = self.world.change_cursor();
        if let Some(kept) = self.carried_statics.as_mut().filter(|_| still) {
            kept.cursor = cursor;
            return kept.list.clone();
        }
        let mut statics: Vec<Entity> = self
            .world
            .query::<RigidBody>()
            .filter(|(_, rb)| rb.mode == floptle_core::BodyMode::Static)
            .map(|(e, _)| e)
            .collect();
        statics.extend(self.static_baked.borrow().keys().filter_map(|&eid| self.world.entity_at(eid)));
        statics.sort_unstable_by_key(|e| e.index());
        statics.dedup();
        let carried: Vec<(Entity, Entity)> = statics
            .into_iter()
            .filter_map(|e| {
                let mut cur = e;
                for _ in 0..64 {
                    let Parent(p) = self.world.get::<Parent>(cur).copied()?;
                    if self.world.get::<CelestialBody>(p).is_some() {
                        return Some((e, p));
                    }
                    cur = p;
                }
                None
            })
            .collect();
        let members = carried.iter().map(|(e, _)| e.index()).collect();
        let celestials = self.world.query::<CelestialBody>().map(|(e, _)| e.index()).collect();
        self.carried_statics = Some(CarriedStatics { cursor, baked, list: carried.clone(), members, celestials });
        carried
    }

    /// Advance rails one gameplay tick. No-op unless Playing with celestial bodies.
    pub(crate) fn update_space_rails(&mut self, tick_dt: f64) {
        // Filled below for whatever this tick carries; nothing moved, nothing
        // carried.
        self.tick_carry.clear();
        // Where the floating origin's focus is before anything moves: the
        // sim's frame rides the frame it is in (see the carry below).
        let focus = floptle_core::active_camera(&self.world)
            .map(|e| floptle_core::world_transform(&self.world, e).translation);
        let cb: Vec<(Entity, CelestialBody)> = self
            .world
            .query::<CelestialBody>()
            .map(|(e, b)| (e, b.clone()))
            .collect();
        if cb.is_empty() {
            return;
        }
        if let Some(m) = self.script_host.take_warp_request() {
            self.space_warp = m.clamp(1.0, 100_000.0);
        }
        if self.space_warp <= 0.0 {
            self.space_warp = 1.0;
        }
        self.space_time += tick_dt * self.space_warp;
        let t = self.space_time;

        // Assemble the system: parent linkage by node name; SOI auto-derives
        // Laplace when left 0. A dangling parent name degrades to root (loud
        // would be better; the Inspector shows the field, keep Play running).
        let names: Vec<String> = cb
            .iter()
            .map(|(e, _)| {
                self.world
                    .get::<floptle_core::Name>(*e)
                    .map(|n| n.0.clone())
                    .unwrap_or_default()
            })
            .collect();
        let mut sys = System::default();
        for (_, b) in &cb {
            let parent = (!b.parent.is_empty())
                .then(|| names.iter().position(|n| *n == b.parent))
                .flatten();
            let soi = if b.soi > 0.0 {
                b.soi
            } else if let Some(p) = parent {
                System::auto_soi(b.a.abs(), b.mu, cb[p].1.mu, b.body_radius, b.reach)
            } else {
                f64::INFINITY
            };
            sys.bodies.push(Body {
                name: String::new(), // resolved via `names` (kept outside the math)
                parent,
                mu: b.mu,
                radius: b.body_radius,
                soi,
                elements: Kepler {
                    a: b.a,
                    e: b.e,
                    i: b.i,
                    lan: b.lan,
                    arg_pe: b.arg_pe,
                    m0: b.m0,
                    epoch: 0.0,
                },
                atmosphere: None,
            });
        }
        let root = sys.root();
        let root_pos = floptle_core::world_transform(&self.world, cb[root].0).translation;

        let mut bodies = Vec::with_capacity(cb.len());
        // Rails deltas for the dominant-frame carry below: how far each
        // celestial moved this tick (old node pos → new rails pos).
        let mut deltas = Vec::with_capacity(cb.len());
        // Where each celestial was on the rails last tick. When none of them
        // has a last tick (Play start, a scene swap) the authored position
        // stands in, so the authored-to-rails snap carries the crew with its
        // planet. A celestial created beside ones already running moved
        // nowhere, and carries nothing, however far its authored position is
        // from its rails.
        let first_tick = !cb.iter().any(|(e, _)| self.space_rails_prev.contains_key(e));
        let mut rails_now = std::collections::HashMap::with_capacity(cb.len());
        // Terrain colliders to put where their celestial is now: (celestial
        // index, entity, world anchor). Placed with the carry below, so the
        // one the sim's frame rides keeps its exact place in it.
        let mut terrain_moves = Vec::new();
        for (i, (e, b)) in cb.iter().enumerate() {
            let (sp, sv) = sys.body_pos_vel(i, t);
            let wp = root_pos + sp;
            let old = match self.space_rails_prev.get(e) {
                Some(p) => *p,
                None if first_tick => floptle_core::world_transform(&self.world, *e).translation,
                None => wp,
            };
            rails_now.insert(*e, wp);
            deltas.push(wp - old);
            if i != root {
                // `wp` is world-space but `Transform.translation` is parent-local:
                // a body under a scene group (generators build a "<Star> System"
                // folder) must convert through the parent's frame, or the world
                // position would be re-offset by the group. Top-level bodies
                // write straight through (identity parent).
                let local = world_to_parent_local(&self.world, *e, wp);
                if let Some(tr) = self.world.get_mut::<Transform>(*e) {
                    tr.translation = local;
                }
            }
            // An orbiting planet's terrain collider must ride along.
            if self.terrains.contains_key(e) {
                terrain_moves.push((i, e.index(), wp));
            }
            bodies.push(floptle_script::SpaceBodyInfo {
                name: names[i].clone(),
                pos: [wp.x, wp.y, wp.z],
                vel: [sv.x, sv.y, sv.z],
                mu: b.mu,
                radius: b.body_radius,
                soi: sys.bodies[i].soi,
            });
        }
        self.space_rails_prev = rails_now;
        // Dominant-frame carry (the patched-conic frame, made physical): every
        // dynamic body inside a moving celestial's sphere of influence shifts
        // by that body's rails delta this tick — stand on an orbiting moon and
        // you ride it instead of it sliding out from under you; orbit inside
        // its SOI and you orbit it, not a point it left behind. Velocity is
        // untouched (positions are the frame); crossing an SOI boundary swaps
        // frames with a small world-velocity step — the v1 patched-conic seam.
        //
        // Warp coasting (S4): while warp > 1 every in-flight body (off the
        // ground, clear of the surface, actually moving relative to its
        // dominant celestial) snaps to its own Kepler rails — its conic is
        // captured once on warp engage and evaluated analytically each tick,
        // so a 1000× warp is exactly as drift-free as the planets' rails. The
        // realtime sim still steps underneath, but its one-tick integration is
        // overwritten by the conic every tick and discarded; dropping back to
        // warp 1 resumes physics from the exact on-conic state. Landed bodies
        // stay on normal realtime physics + carry (contacts pin them), which is
        // also what makes warping while parked on a surface safe.
        let warping = self.space_warp > 1.0 + 1e-9;
        if !warping && !self.space_coast.is_empty() {
            self.space_coast.clear();
        }
        let lod_anchor = self.lod_anchor_world();
        // Surface structures ride their planet: a Static-bodied node parented
        // (at any depth) under a celestial follows it visually through the
        // transform hierarchy for free, but its baked collider blob would stay
        // behind in space. Each moves by its celestial's rails delta, so a
        // launchpad on an orbiting world stays exactly as solid as the
        // terrain it stands on. That goes for every baked static, not only a
        // Static body: a Collidable mesh or primitive left to the per-frame
        // follow stood where its planet was before this frame's ticks, 4 m a
        // tick behind at 250 m/s, and further while the follow took turns.
        let carried: Vec<(Entity, usize)> = self
            .carried_statics()
            .into_iter()
            .filter_map(|(e, c)| Some((e, cb.iter().position(|(ce, _)| *ce == c)?)))
            .collect();
        // The smallest sphere of influence holding `p` before this tick, by
        // each celestial's old centre.
        let dom_before = |p: DVec3| -> Option<usize> {
            let mut dom: Option<(usize, f64)> = None;
            for (i, sb) in sys.bodies.iter().enumerate() {
                let old_center = DVec3::from(bodies[i].pos) - deltas[i];
                if (p - old_center).length() <= sb.soi && dom.is_none_or(|(_, s)| sb.soi < s) {
                    dom = Some((i, sb.soi));
                }
            }
            dom.map(|(i, _)| i)
        };
        if let Some(sim) = self.sim.as_mut() {
            let states: std::collections::HashMap<u32, (floptle_core::math::Vec3, bool)> = sim
                .body_states()
                .map(|r| (r.entity.index(), (r.vel, r.grounded)))
                .collect();
            // Where everything was before this tick moved it.
            let body_pos = sim.body_positions();
            let compound_pos = sim.compound_positions();
            // The sim's frame rides the frame the floating origin's focus is
            // in, exactly: the origin moves with that celestial and nothing
            // in it moves in the sim at all. Shifting every body by the rails
            // delta and re-anchoring its ground rounded both, a different way
            // each tick, a few tenths of a millimetre kilometres out. The
            // contact pass answered that with a few cm/s of push, and nothing
            // on a moving world ever slept. It also carried the craft away
            // from the origin at the planet's speed, kilometres out within
            // seconds, where f32 steps are a quarter of a millimetre.
            // Bodies in another frame move by the difference, those in none
            // by `-ride`.
            // With no camera, the frame the first craft or body is in.
            let anchor = focus.or_else(|| compound_pos.first().map(|c| c.1)).or_else(|| body_pos.first().map(|b| b.1));
            let ride_i = anchor.and_then(dom_before);
            let ride = ride_i.map_or(DVec3::ZERO, |i| deltas[i]);
            let mut riders: Vec<u32> = Vec::new();
            if let Some(f) = ride_i {
                riders.push(cb[f].0.index());
                riders.extend(carried.iter().filter(|(_, j)| *j == f).map(|(e, _)| e.index()));
            }
            sim.ride_frame(ride, &riders);
            for &(i, eid, wp) in &terrain_moves {
                if Some(i) != ride_i {
                    sim.set_collider_anchor(eid, wp);
                }
            }
            let t_old = t - tick_dt * self.space_warp;
            for (eid, pos) in body_pos {
                let mut dom: Option<(usize, f64)> = None; // (index, soi)
                for (i, sb) in sys.bodies.iter().enumerate() {
                    // Containment against the old center: `pos` is the body's
                    // Pre-tick position while `bodies[i].pos` already moved by
                    // this tick's rails delta. Testing the new center strands
                    // a body when its planet jumps (worst on the first tick,
                    // where authored scene positions can differ from the rails
                    // by the whole inclination offset — the planet teleports
                    // out from under the spawn and leaves the crew in space).
                    let old_center = DVec3::from(bodies[i].pos) - deltas[i];
                    if (pos - old_center).length() <= sb.soi
                        && dom.is_none_or(|(_, s)| sb.soi < s)
                    {
                        dom = Some((i, sb.soi));
                    }
                }
                let Some((i, _)) = dom else {
                    // In no frame: it stays where it is in the world.
                    if ride != DVec3::ZERO {
                        sim.shift_body(eid, -ride);
                    }
                    continue;
                };
                // `node.carry`: how far this tick moved the frame it rides in.
                self.tick_carry.insert(eid, deltas[i].to_array());
                let (mut vel, grounded) = states.get(&eid).copied().unwrap_or_default();
                // frame convention: a dynamic body's sim velocity is measured
                // in its dominant celestial's carried frame — the carry moves
                // positions only, so a landed ship reads v ≈ 0 while its
                // planet orbits the star at full speed. Everything below (and
                // `space.elements`) treats velocities as frame-relative;
                // subtracting the center's world velocity here again was the
                // bug that bent trajectories the moment warp engaged.
                //
                // SOI seam: crossing into a different dominant frame must keep
                // the world velocity continuous, so the sim velocity jumps by
                // (old frame vel − new frame vel) — leave a planet's SOI and
                // you carry its orbital velocity into the star's frame.
                let dom_key = cb[i].0;
                // The tick-sampled SOI seam only runs for bodies not coasting
                // on rails: a coast handles its own frame handoffs at the
                // exact crossing time (bisected on the conic below). Applying
                // this sampled seam to a coasting ship at high warp put the
                // velocity step at the wrong time/place — every moon-SOI
                // transit bent the orbit a little until clean ellipses
                // "escaped".
                let coast_active = warping && self.space_coast.contains_key(&eid);
                if !coast_active {
                    let prev = self.space_frame.insert(eid, dom_key);
                    if let Some(p) = prev
                        && p != dom_key
                        && let Some(j) = cb.iter().position(|(e, _)| *e == p)
                    {
                        let dv = DVec3::from(bodies[j].vel) - DVec3::from(bodies[i].vel);
                        vel = (vel.as_dvec3() + dv).as_vec3();
                        sim.set_body_velocity(eid, vel);
                        self.console.push(
                            floptle_script::LogLevel::Debug,
                            format!("entered {}'s sphere of influence", names[i]),
                            None,
                        );
                    }
                }
                let center = DVec3::from(bodies[i].pos);
                // Relative to the old center: `center` already moved by delta
                // this tick, the body's sim position has not.
                let rel = (pos - center) + deltas[i];
                let flying = warping
                    && !grounded
                    && rel.length() > cb[i].1.body_radius + 8.0
                    && vel.as_dvec3().length_squared() > 0.01;
                if flying && cb[i].1.mu > 0.0 {
                    // Capture on engage only. An existing coast keeps its conic
                    // even when this tick's sampled dominant differs — the
                    // crossing walk below hands frames off at the exact
                    // boundary time instead of the tick boundary.
                    self.space_coast.entry(eid).or_insert_with(|| {
                        // Capture the conic from the pre-tick state (old center,
                        // old time) — from here on the cached elements are truth.
                        // The sim velocity is the frame-relative velocity.
                        (dom_key, Kepler::from_state(rel, vel.as_dvec3(), cb[i].1.mu, t_old))
                    });
                    // World state of body index `j` at absolute time τ.
                    let body_w = |j: usize, tau: f64| {
                        let (p, v) = sys.body_pos_vel(j, tau);
                        (root_pos + p, v)
                    };
                    // Smallest containing SOI at τ — the patched-conic rule,
                    // evaluated on the analytic rails (exact at any warp).
                    let dom_at = |wp: DVec3, tau: f64| -> usize {
                        let mut best = root;
                        let mut best_soi = f64::INFINITY;
                        for (j, sb) in sys.bodies.iter().enumerate() {
                            if sb.soi < best_soi && (wp - body_w(j, tau).0).length() <= sb.soi
                            {
                                best = j;
                                best_soi = sb.soi;
                            }
                        }
                        best
                    };
                    let (mut fdk, mut k) = *self.space_coast.get(&eid).unwrap();
                    let mut fd =
                        cb.iter().position(|(e, _)| *e == fdk).unwrap_or(i);
                    // Evaluate t_old → t, bisecting each SOI crossing to its
                    // exact time and re-capturing the conic there (world
                    // velocity continuous by construction). A warped tick can
                    // hop hundreds of seconds; the handoff must not.
                    let mut t_lo = t_old;
                    let (mut r2, mut v2);
                    let mut hops = 0;
                    loop {
                        let (rr, vv) = k.pos_vel(cb[fd].1.mu, t);
                        r2 = rr;
                        v2 = vv;
                        let d_now = dom_at(body_w(fd, t).0 + r2, t);
                        if d_now == fd || hops >= 4 {
                            break;
                        }
                        hops += 1;
                        let (mut lo, mut hi) = (t_lo, t);
                        for _ in 0..32 {
                            let mid = 0.5 * (lo + hi);
                            let (rm, _) = k.pos_vel(cb[fd].1.mu, mid);
                            if dom_at(body_w(fd, mid).0 + rm, mid) == fd {
                                lo = mid;
                            } else {
                                hi = mid;
                            }
                        }
                        let tx = hi;
                        let (rx, vx) = k.pos_vel(cb[fd].1.mu, tx);
                        let (op, ov) = body_w(fd, tx);
                        let (wpx, wvx) = (op + rx, ov + vx);
                        let nd = dom_at(wpx, tx);
                        if nd == fd || cb[nd].1.mu <= 0.0 {
                            break; // numeric edge, or a massless marker: keep frame
                        }
                        self.console.push(
                            floptle_script::LogLevel::Debug,
                            if sys.bodies[nd].parent == Some(fd) {
                                format!("entered {}'s sphere of influence", names[nd])
                            } else {
                                format!("left {}'s sphere of influence", names[fd])
                            },
                            None,
                        );
                        let (np, nv) = body_w(nd, tx);
                        k = Kepler::from_state(wpx - np, wvx - nv, cb[nd].1.mu, tx);
                        fd = nd;
                        fdk = cb[nd].0;
                        t_lo = tx;
                    }
                    self.space_coast.insert(eid, (fdk, k));
                    self.space_frame.insert(eid, fdk);
                    // G1 residency: warp crosses the 80-radii terrain-load
                    // lead in milliseconds — closing on a body whose field is
                    // still cold drops to realtime so the background stream
                    // gets its seconds (the residency driver kicks the load
                    // as the camera arrives). Re-warp once it's resident.
                    if self.terrain_cold.contains_key(&cb[fd].0)
                        && r2.length() < cb[fd].1.body_radius * 60.0
                    {
                        self.space_warp = 1.0;
                        self.space_coast.remove(&eid);
                        self.console.push(
                            floptle_script::LogLevel::Debug,
                            format!(
                                "time-warp dropped to 1× — streaming {}'s terrain in \
                                 (re-warp in a moment)",
                                names[fd]
                            ),
                            None,
                        );
                        // Not carried this tick, as before the frame rode.
                        if ride != DVec3::ZERO {
                            sim.shift_body(eid, -ride);
                        }
                        continue;
                    }
                    // Surface proximity kills warp (the KSP rule): a conic
                    // whose next sample dips near the ground would teleport
                    // the ship into rock at 1000×. Drop to realtime and let
                    // physics take it from the last on-conic state.
                    if r2.length() < cb[fd].1.body_radius + 25.0 {
                        self.space_warp = 1.0;
                        self.space_coast.remove(&eid);
                        self.console.push(
                            floptle_script::LogLevel::Debug,
                            "time-warp dropped to 1× — surface proximity".into(),
                            None,
                        );
                        if ride != DVec3::ZERO {
                            sim.shift_body(eid, -ride);
                        }
                        continue;
                    }
                    sim.set_body_position(eid, DVec3::from(bodies[fd].pos) + r2);
                    sim.set_body_velocity(eid, v2.as_vec3());
                } else {
                    self.space_coast.remove(&eid);
                    // The frame it rides in, less what the sim's frame rode:
                    // nothing at all in the frame the sim rides.
                    let d = deltas[i] - ride;
                    if d.length_squared() > 1e-18 {
                        sim.shift_body(eid, d);
                    }
                }
            }
            // Compounds ride their dominant frame too. Skipping them was the
            // launch-day fling: the spawn planet orbits its star at ~90 u/s,
            // every carried body (the astronaut) rode along, and the freshly
            // assembled vessel — never shifted — watched its planet sail away,
            // which read as "my ship got sucked into space the moment it
            // spawned". Same containment-vs-old-center rule, same soi-seam
            // velocity step; no warp coasting yet (v0 vessels fly realtime —
            // grounded compounds are contact-pinned, which is what makes
            // warping while parked safe, same as single bodies).
            for (eid, pos) in compound_pos {
                let Some(i) = dom_before(pos) else {
                    if ride != DVec3::ZERO {
                        sim.shift_compound(eid, -ride);
                    }
                    continue;
                };
                // `node.carry`: how far this tick moved the frame it rides in.
                self.tick_carry.insert(eid, deltas[i].to_array());
                let dom_key = cb[i].0;
                let prev = self.space_frame.insert(eid, dom_key);
                if let Some(p) = prev
                    && p != dom_key
                    && let Some(j) = cb.iter().position(|(e, _)| *e == p)
                {
                    let dv = DVec3::from(bodies[j].vel) - DVec3::from(bodies[i].vel);
                    let vel = sim
                        .compound_of(eid)
                        .map(|c| (c.vel.as_dvec3() + dv).as_vec3())
                        .unwrap_or_default();
                    sim.set_compound_velocity(eid, vel);
                    self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!("entered {}'s sphere of influence", names[i]),
                        None,
                    );
                }
                let d = deltas[i] - ride;
                if d.length_squared() > 1e-18 {
                    sim.shift_compound(eid, d);
                }
            }
            let mut shifts = std::collections::HashMap::new();
            for &(e, j) in &carried {
                if deltas[j].length_squared() > 1e-18 {
                    // The frame the sim rides carried its own already.
                    if Some(j) != ride_i {
                        shifts.insert(e.index(), deltas[j]);
                    }
                    // Followed by script moves too: what the rails carried is
                    // not a move of the script's to carry again.
                    if let Some(b) = self.static_baked.borrow_mut().get_mut(&e.index()) {
                        b.pos += deltas[j];
                    }
                }
            }
            sim.shift_statics_by(&shifts);
            let dom_of =|p: DVec3| -> Option<usize> {
                let mut best: Option<(usize, f64)> = None;
                for (i, sb) in sys.bodies.iter().enumerate() {
                    if (p - DVec3::from(bodies[i].pos)).length() <= sb.soi
                        && best.is_none_or(|(_, s)| sb.soi < s)
                    {
                        best = Some((i, sb.soi));
                    }
                }
                best.map(|(i, _)| i)
            };
            // Compound warp coasting: while warp > 1, every live in-flight
            // compound (the piloted vessel above all) snaps to its own Kepler
            // conic, exactly like single bodies — captured on engage,
            // evaluated analytically at rails time, velocity kept current so
            // dropping to 1× resumes physics on-conic. Grounded/anchored
            // craft stay realtime (contacts pin them — warping while parked
            // is safe). SOI handoffs are tick-sampled (no bisect yet): fine
            // at gameplay warps, noted for the exact-crossing upgrade.
            if warping {
                for (eid, com) in sim.compound_positions() {
                    if self.compound_lod.contains_key(&eid) {
                        continue; // far craft: the LOD's own conic drives it
                    }
                    let Some(c) = sim.compound_of(eid) else { continue };
                    // On the ground through its wheels counts: a parked rover
                    // is not in flight, however its suspension jiggles.
                    if c.anchored || sim.compound_grounded(eid) || c.vel.length_squared() < 0.01 {
                        self.compound_coast.remove(&eid);
                        continue;
                    }
                    let vel = c.vel.as_dvec3();
                    let Some(i0) = dom_of(com) else { continue };
                    if cb[i0].1.mu <= 0.0 {
                        continue;
                    }
                    let (dk, k) = *self.compound_coast.entry(eid).or_insert_with(|| {
                        (
                            cb[i0].0.index(),
                            Kepler::from_state(
                                com - DVec3::from(bodies[i0].pos),
                                vel,
                                cb[i0].1.mu,
                                t,
                            ),
                        )
                    });
                    let Some(j) = cb.iter().position(|(e, _)| e.index() == dk) else {
                        self.compound_coast.remove(&eid);
                        continue;
                    };
                    let (r, v) = k.pos_vel(cb[j].1.mu, t);
                    // Surface proximity kills warp (the KSP rule) — resume
                    // realtime physics from the last on-conic state.
                    if r.length() < cb[j].1.body_radius + 25.0 {
                        self.space_warp = 1.0;
                        self.compound_coast.remove(&eid);
                        self.console.push(
                            floptle_script::LogLevel::Debug,
                            "time-warp dropped to 1× — surface proximity".into(),
                            None,
                        );
                        continue;
                    }
                    let target = DVec3::from(bodies[j].pos) + r;
                    sim.shift_compound(eid, target - com);
                    sim.set_compound_velocity(eid, v.as_vec3());
                    // Tick-sampled SOI seam: recapture in the new dominant
                    // frame with the world velocity kept continuous.
                    if let Some(nj) = dom_of(target)
                        && nj != j
                        && cb[nj].1.mu > 0.0
                    {
                        let wv = v + DVec3::from(bodies[j].vel);
                        let nk = Kepler::from_state(
                            target - DVec3::from(bodies[nj].pos),
                            wv - DVec3::from(bodies[nj].vel),
                            cb[nj].1.mu,
                            t,
                        );
                        self.compound_coast.insert(eid, (cb[nj].0.index(), nk));
                        self.console.push(
                            floptle_script::LogLevel::Debug,
                            format!("entered {}'s sphere of influence", names[nj]),
                            None,
                        );
                    }
                }
            } else if !self.compound_coast.is_empty() {
                // Warp ended: velocities were kept on-conic every tick, so
                // realtime physics resumes exactly where the rails left off.
                self.compound_coast.clear();
            }
            // Distant-craft LOD (hundreds of deployed craft, cheaply): far
            // compounds leave live physics — landed/slow ones freeze in the
            // carried frame, in-flight ones snap to their own Kepler rails —
            // and wake on approach. The active camera is "near", unless a
            // game named a `terrain.lodAnchor`: a map camera thousands of units
            // out must not freeze the craft beside the player.
            let cam = self
                .world
                .query::<floptle_core::Matter>()
                .find_map(|(e, m)| {
                    matches!(m, floptle_core::Matter::Camera { active: true, .. })
                        .then(|| floptle_core::world_transform(&self.world, e).translation)
                })
                .map(|c| lod_anchor.unwrap_or(c));
            if let Some(cam) = cam {
                for (eid, com) in sim.compound_positions() {
                    if self.compound_coast.contains_key(&eid) {
                        continue; // warp rails own it this tick
                    }
                    // Kept live (assembly.keepLive — the piloted vessel in the map
                    // view): stay in full physics however far the camera roams. If
                    // it was already LOD'd when the exemption came in, wake it now —
                    // an in-flight craft on its conic (its real vel was zeroed by the
                    // anchor, so resuming from rest would drop it out of orbit), a
                    // landed one at rest (respecting a prior clamp).
                    if self.lod_keep_live.contains(&eid) {
                        if let Some(lod) = self.compound_lod.remove(&eid) {
                            wake_lod(sim, eid, lod, &cb, t);
                        }
                        continue;
                    }
                    let d = (com - cam).length();
                    match self.compound_lod.get(&eid).copied() {
                        None => {
                            let Some(c) = sim.compound_of(eid) else { continue };
                            if d <= LOD_FAR || self.lod_woken.contains_key(&eid) {
                                continue;
                            }
                            let grounded = sim.compound_grounded(eid);
                            // Only what rests on something, or is pinned, is
                            // frozen where it stands. Slow alone is not resting:
                            // debris at the top of its arc froze there in mid-air.
                            let resting = grounded || c.anchored;
                            let vel = c.vel.as_dvec3();
                            let was_anchored = c.anchored;
                            let was_grounded = grounded;
                            let falling = (!resting)
                                .then(|| dom_of(com))
                                .flatten()
                                .filter(|&i| cb[i].1.mu > 0.0);
                            if let Some(i) = falling {
                                let k = Kepler::from_state(
                                    com - DVec3::from(bodies[i].pos),
                                    vel,
                                    cb[i].1.mu,
                                    t,
                                );
                                // A conic that comes down into its world would
                                // carry the craft through the ground on rails:
                                // that one stays in physics and lands.
                                if (k.a * (1.0 - k.e)).abs() < cb[i].1.body_radius + 25.0 {
                                    continue;
                                }
                                sim.set_compound_anchored(eid, true);
                                self.compound_lod.insert(
                                    eid,
                                    CompoundLod::Coast { dom: cb[i].0.index(), k },
                                );
                            } else {
                                sim.set_compound_anchored(eid, true);
                                self.compound_lod
                                    .insert(eid, CompoundLod::Frozen { was_anchored, was_grounded });
                            }
                        }
                        Some(CompoundLod::Coast { dom, k }) => {
                            let Some(i) = cb.iter().position(|(e, _)| e.index() == dom) else {
                                self.compound_lod.remove(&eid);
                                sim.set_compound_anchored(eid, false);
                                continue;
                            };
                            // Evaluate the conic at rails time — exact at any
                            // warp — and place the craft absolutely (the carry
                            // above already moved it; this overwrites).
                            let (r, v) = k.pos_vel(cb[i].1.mu, t);
                            let target = DVec3::from(bodies[i].pos) + r;
                            sim.shift_compound(eid, target - com);
                            // Keep the reported velocity on the conic every tick
                            // (not only on wake): a LOD-anchored craft has its
                            // real linvel zeroed, but `assembly.info().vel` — read
                            // by the map/HUD to draw the vessel's orbit — must stay
                            // the true orbital velocity, or the trajectory vanishes
                            // the instant the map camera pulls back past LOD_FAR
                            // ("trajectory only shows while I'm moving"). This
                            // runs at tick start, before `feed_assembly_info`
                            // publishes it and before the physics step re-zeroes
                            // the anchored body — so the value survives to scripts.
                            sim.set_compound_velocity(eid, v.as_vec3());
                            if d < LOD_NEAR {
                                self.compound_lod.remove(&eid);
                                sim.set_compound_anchored(eid, false);
                            }
                        }
                        Some(CompoundLod::Frozen { was_anchored, .. }) => {
                            if d < LOD_NEAR {
                                self.compound_lod.remove(&eid);
                                if !was_anchored {
                                    sim.set_compound_anchored(eid, false);
                                }
                            }
                        }
                    }
                }
                for left in self.lod_woken.values_mut() {
                    *left -= tick_dt;
                }
                self.lod_woken.retain(|_, left| *left > 0.0);
                // Despawned compounds leave stale LOD entries — sweep them.
                self.compound_lod
                    .retain(|eid, _| sim.compound_of(*eid).is_some());
            }
        }
        self.script_host.set_space(floptle_script::SpaceInfo {
            time: t,
            warp: self.space_warp,
            bodies,
        });
        // The µ/r² centers just moved — refresh gravity for this tick's step.
        if let Some(sim) = self.sim.as_mut() {
            sim.world.gravity = Self::build_gravity_field(&self.world, sim.world.origin);
        }
    }
}

/// Convert a world position into `e`'s scene-parent-local frame — what a
/// `Transform.translation` write on `e` must contain to land the node at `wp`.
/// Top-level nodes pass through unchanged. This is what lets celestial bodies
/// live under a scene group (a generator's "<Star> System" folder): rails
/// computes world positions, the write converts through the parent.
pub(crate) fn world_to_parent_local(
    world: &floptle_core::World,
    e: Entity,
    wp: DVec3,
) -> DVec3 {
    match world.get::<floptle_core::Parent>(e).copied() {
        Some(floptle_core::Parent(pe)) => {
            let pw = floptle_core::world_transform(world, pe);
            let s = pw.scale.as_dvec3().max(DVec3::splat(1e-9));
            (pw.rotation.as_dquat().inverse() * (wp - pw.translation)) / s
        }
        None => wp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::{Quat, Vec3};
    use floptle_core::{Parent, World};

    /// The rails write must round-trip: writing the converted local translation
    /// puts the node's world transform exactly at the rails position, whatever
    /// frame the scene parent (a generator's system group) sits in.
    #[test]
    fn rails_world_position_survives_a_scene_parent() {
        let mut w = World::default();
        let group = w.spawn();
        // A group not at identity — offset, rotated, scaled.
        w.insert(
            group,
            Transform {
                translation: DVec3::new(100.0, -25.0, 7.0),
                rotation: Quat::from_rotation_y(0.7),
                scale: Vec3::splat(2.0),
            },
        );
        let body = w.spawn();
        w.insert(body, Transform::IDENTITY);
        w.insert(body, Parent(group));

        let rails_wp = DVec3::new(4839.0, 0.0, -1200.0);
        let local = world_to_parent_local(&w, body, rails_wp);
        w.get_mut::<Transform>(body).unwrap().translation = local;
        let got = floptle_core::world_transform(&w, body).translation;
        // Tolerance: rotation quats are f32, so a rotated parent leaves f32-epsilon × lever-arm residue
        // (~1e-7 × 5000 units). Millimeter-scale at system scale is exact
        // for our purposes — and generator groups are identity anyway.
        assert!((got - rails_wp).length() < 1e-3, "world pos drifted: {got:?}");

        // Top-level nodes pass through untouched.
        let top = w.spawn();
        w.insert(top, Transform::IDENTITY);
        assert_eq!(world_to_parent_local(&w, top, rails_wp), rails_wp);
    }

    /// **A celestial created mid-play does not carry anyone on its first
    /// tick.** Its first rails delta used to be measured from its authored
    /// position, so a planet authored on top of the crew and railed 4000 units
    /// away took them with it. A streamer adding a neighbouring star system is
    /// the ordinary case: a crew standing still was thrown 3.5 Mm.
    #[test]
    fn a_celestial_created_mid_play_does_not_carry_a_body_on_its_first_tick() {
        use floptle_core::{Name, RigidBody};
        let mut ed = crate::Editor::default();
        let celestial = |ed: &mut crate::Editor, name: &str, at: DVec3, body: CelestialBody| {
            let e = ed.world.spawn();
            ed.world.insert(e, Name(name.into()));
            ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
            ed.world.insert(e, body);
            e
        };
        celestial(&mut ed, "Sun", DVec3::ZERO, CelestialBody { mu: 1.0e6, ..Default::default() });
        let home = CelestialBody { parent: "Sun".into(), a: 1000.0, mu: 1.0e4, soi: 200.0, ..Default::default() };
        celestial(&mut ed, "Home", DVec3::new(1000.0, 0.0, 0.0), home);
        let crew = ed.world.spawn();
        ed.world.insert(crew, Name("Crew".into()));
        ed.world.insert(crew, Transform { translation: DVec3::new(1000.0, 60.0, 0.0), ..Transform::IDENTITY });
        ed.world.insert(crew, RigidBody::default());

        ed.toggle_play();
        assert!(ed.playing);
        let crew_at = |ed: &crate::Editor| ed.sim.as_ref().unwrap().body_states().find(|r| r.entity == crew).unwrap().pos;
        for _ in 0..5 {
            ed.play_step(1.0 / 60.0, true);
        }
        let before = crew_at(&ed);

        // Authored right where the crew is, with a smaller sphere of influence
        // than Home, and railed at the far side of its own orbit.
        let far = CelestialBody { parent: "Sun".into(), a: 5000.0, mu: 1.0e4, soi: 50.0, ..Default::default() };
        celestial(&mut ed, "Neighbour", before, far);
        ed.play_step(1.0 / 60.0, true);
        let moved = (crew_at(&ed) - before).length();
        assert!(moved < 5.0, "the crew moved {moved} units the tick a planet appeared elsewhere");
    }

    /// **A celestial a script makes is in `space.*` before the next frame's
    /// scripts run, tick or no tick.** The listing used to be written only by a
    /// rails tick, so on a frame that ran none a planet whose terrain had
    /// already landed answered a raycast while `space.bodies()` did not have
    /// it. A launch pad sited off that raycast spawned unparented, with world
    /// up, and its craft fell through.
    #[test]
    fn a_celestial_made_on_a_frame_is_listed_by_the_next_without_a_tick() {
        use floptle_core::Name;
        let mut ed = crate::Editor::default();
        let celestial = |ed: &mut crate::Editor, name: &str, body: CelestialBody| {
            let e = ed.world.spawn();
            ed.world.insert(e, Name(name.into()));
            ed.world.insert(e, Transform::IDENTITY);
            ed.world.insert(e, body);
            e
        };
        celestial(&mut ed, "Sun", CelestialBody { mu: 1.0e6, ..Default::default() });
        ed.toggle_play();
        assert!(ed.playing);
        for _ in 0..3 {
            ed.play_step(1.0 / 60.0, true);
        }
        assert_eq!(ed.script_host.space_body_names(), vec!["Sun".to_string()]);
        let clock = ed.space_time;

        celestial(&mut ed, "Home", CelestialBody { parent: "Sun".into(), a: 1000.0, mu: 1.0e4, ..Default::default() });
        // A frame too short to run a tick.
        ed.play_step(0.0, true);
        assert_eq!(
            ed.script_host.space_body_names(),
            vec!["Sun".to_string(), "Home".to_string()],
            "a planet made this frame must be listed before the next frame's scripts"
        );
        assert_eq!(ed.space_time, clock, "listing it must not move the clock");
    }

    /// **A body resting on a planet that moves on rails stays put on its
    /// ground at any frame rate.** Under a frame clock that runs 0, 1, 2 and 6
    /// ticks a frame, the planet jumps 0 to 8.7 m between frames; the body on it
    /// must jump with it, never lag or lead, or everything standing on a
    /// moving world jitters against it.
    #[test]
    fn a_body_on_a_planet_on_rails_never_jitters_against_its_ground() {
        use floptle_core::{BodyKind, BodyMode, Name, Parent, RigidBody};
        let mut ed = crate::Editor::default();
        let celestial = |ed: &mut crate::Editor, name: &str, at: DVec3, body: CelestialBody| {
            let e = ed.world.spawn();
            ed.world.insert(e, Name(name.into()));
            ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
            ed.world.insert(e, body);
            e
        };
        // 87 m/s: sqrt(mu / a).
        celestial(&mut ed, "Sun", DVec3::ZERO, CelestialBody { mu: 3.78e7, ..Default::default() });
        let home = CelestialBody { parent: "Sun".into(), a: 5000.0, mu: 1.0e4, soi: 500.0, ..Default::default() };
        let home = celestial(&mut ed, "Home", DVec3::new(5000.0, 0.0, 0.0), home);
        let floor = ed.world.spawn();
        ed.world.insert(floor, Transform { translation: DVec3::new(0.0, 58.0, 0.0), ..Transform::IDENTITY });
        ed.world.insert(floor, Parent(home));
        let slab = RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [20.0, 1.0, 20.0], ..Default::default() };
        ed.world.insert(floor, slab);
        let crew = ed.world.spawn();
        ed.world.insert(crew, Name("Crew".into()));
        ed.world.insert(crew, Transform { translation: DVec3::new(5000.0, 60.0, 0.0), ..Transform::IDENTITY });
        ed.world.insert(crew, RigidBody { gravity: true, ..Default::default() });
        ed.toggle_play();
        // Settle onto the floor.
        for _ in 0..240 {
            ed.play_step(1.0 / 60.0, true);
        }
        let world = |ed: &crate::Editor, e| floptle_core::world_transform(&ed.world, e).translation;
        let rel0 = world(&ed, crew) - world(&ed, home);
        let mut moved = 0.0f64;
        for dt in [0.005f32, 0.016, 0.03, 0.008, 0.1, 0.0167, 0.004, 0.02, 0.033, 0.011].repeat(6) {
            let before = world(&ed, home);
            ed.play_step(dt, true);
            moved = moved.max((world(&ed, home) - before).length());
            let dev = (world(&ed, crew) - world(&ed, home) - rel0).length();
            assert!(dev < 0.01, "the body moved {dev} m against its planet on a {dt} s frame");
        }
        assert!(moved > 5.0, "the fixture needs frames that run several ticks, the planet moved at most {moved} m");
    }

    /// **A static collider on a planet on rails is where the planet draws
    /// it, between frames, which is what the next `update` raycasts.**
    #[test]
    fn a_static_on_a_planet_on_rails_collides_where_it_is_drawn() {
        use floptle_core::{BodyKind, BodyMode, Collidable, Matter, Name, Parent, RigidBody, Shape};
        let mut ed = crate::Editor::default();
        let celestial = |ed: &mut crate::Editor, name: &str, at: DVec3, body: CelestialBody| {
            let e = ed.world.spawn();
            ed.world.insert(e, Name(name.into()));
            ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
            ed.world.insert(e, body);
            e
        };
        // 250 m/s: sqrt(mu / a).
        celestial(&mut ed, "Sun", DVec3::ZERO, CelestialBody { mu: 3.125e8, ..Default::default() });
        let home = CelestialBody { parent: "Sun".into(), a: 5000.0, mu: 1.0e4, soi: 500.0, ..Default::default() };
        let home = celestial(&mut ed, "Home", DVec3::new(5000.0, 0.0, 0.0), home);
        let cam = ed.world.spawn();
        ed.world.insert(cam, Transform { translation: DVec3::new(0.0, 0.0, 25.0), ..Transform::IDENTITY });
        ed.world.insert(cam, Parent(home));
        ed.world.insert(
            cam,
            Matter::Camera {
                fov_y: 1.0,
                active: true,
                target: String::new(),
                cull_mask: u32::MAX,
                target_w: 0,
                target_h: 0,
                target_hz: 0.0,
                ortho: false,
                ortho_height: 10.0,
                stack: Vec::new(),
                draws: floptle_core::CameraDraws::default(),
                post_after: false,
            },
        );
        let body_box = ed.world.spawn();
        ed.world.insert(body_box, Transform { translation: DVec3::new(100.0, 0.0, 0.0), ..Transform::IDENTITY });
        ed.world.insert(body_box, Parent(home));
        let slab = RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [2.0, 2.0, 2.0], ..Default::default() };
        ed.world.insert(body_box, slab);
        let plain_box = ed.world.spawn();
        ed.world.insert(plain_box, Transform { translation: DVec3::new(-100.0, 0.0, 0.0), ..Transform::IDENTITY });
        ed.world.insert(plain_box, Parent(home));
        ed.world.insert(plain_box, Matter::Primitive { shape: Shape::Cube, color: [1.0; 3] });
        ed.world.insert(plain_box, Collidable);
        ed.toggle_play();
        // How far in front of its drawn centre, along +z, each collider's near
        // face is: constant while the collider stays with the node.
        let face = |ed: &crate::Editor, e| {
            let at = floptle_core::world_transform(&ed.world, e).translation;
            let from = at - DVec3::new(0.0, 0.0, 30.0);
            let hit = ed.sim.as_ref().unwrap().raycast(from, Vec3::Z, 60.0)?;
            Some(from.z + hit.distance as f64 - at.z)
        };
        ed.play_step(1.0 / 60.0, true);
        let rest = [face(&ed, body_box).expect("the body box"), face(&ed, plain_box).expect("the plain box")];
        let mut worst = [0.0f64; 2];
        for dt in [0.005f32, 0.016, 0.03, 0.008, 0.1, 0.0167, 0.004, 0.02, 0.033, 0.011].repeat(3) {
            ed.play_step(dt, true);
            for (k, e) in [body_box, plain_box].into_iter().enumerate() {
                let off = face(&ed, e).map_or(f64::INFINITY, |f| (f - rest[k]).abs());
                worst[k] = worst[k].max(off);
            }
        }
        assert!(worst[0] < 0.05 && worst[1] < 0.05, "a static on a moving planet collided away from where it is drawn: {worst:?}");

        // One that arrives while the planet is already moving rides it too.
        let late = ed.world.spawn();
        ed.world.insert(late, Transform { translation: DVec3::new(0.0, 100.0, 0.0), ..Transform::IDENTITY });
        ed.world.insert(late, Parent(home));
        ed.world.insert(late, Matter::Primitive { shape: Shape::Cube, color: [1.0; 3] });
        ed.world.insert(late, Collidable);
        let mut sim = ed.sim.take().unwrap();
        ed.add_static_collider_for(&mut sim, late);
        ed.sim = Some(sim);
        ed.play_step(0.0, true);
        let rest = face(&ed, late).expect("the late box");
        let mut drift = 0.0f64;
        for dt in [0.016f32, 0.03, 0.1, 0.0167, 0.033] {
            ed.play_step(dt, true);
            drift = drift.max(face(&ed, late).map_or(f64::INFINITY, |f| (f - rest).abs()));
        }
        assert!(drift < 0.05, "a static added mid-play collided {drift} m from where it is drawn");
    }

    /// **A sim rebuilt while the scripts hold its colliders keeps every one
    /// where it is drawn.** `water.setFrozen` used to rebuild the sim in the
    /// middle of the frame pass. Taking the lent colliders back then
    /// handed the new sim the old one's, measured from the old origin: on a
    /// planet on rails, where the origin rides, every static on the level
    /// moved by the difference for good, and the ground under a launch pad
    /// was no longer there to be hit.
    #[test]
    fn a_sim_rebuilt_while_scripts_hold_the_colliders_keeps_them_in_place() {
        use floptle_core::{Collidable, Matter, Name, Parent, Shape};
        let mut ed = crate::Editor::default();
        let celestial = |ed: &mut crate::Editor, name: &str, at: DVec3, body: CelestialBody| {
            let e = ed.world.spawn();
            ed.world.insert(e, Name(name.into()));
            ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
            ed.world.insert(e, body);
            e
        };
        // 250 m/s: sqrt(mu / a).
        celestial(&mut ed, "Sun", DVec3::ZERO, CelestialBody { mu: 3.125e8, ..Default::default() });
        let home = CelestialBody { parent: "Sun".into(), a: 5000.0, mu: 1.0e4, soi: 500.0, ..Default::default() };
        let home = celestial(&mut ed, "Home", DVec3::new(5000.0, 0.0, 0.0), home);
        let cam = ed.world.spawn();
        ed.world.insert(cam, Transform { translation: DVec3::new(0.0, 0.0, 25.0), ..Transform::IDENTITY });
        ed.world.insert(cam, Parent(home));
        ed.world.insert(
            cam,
            Matter::Camera {
                fov_y: 1.0,
                active: true,
                target: String::new(),
                cull_mask: u32::MAX,
                target_w: 0,
                target_h: 0,
                target_hz: 0.0,
                ortho: false,
                ortho_height: 10.0,
                stack: Vec::new(),
                draws: floptle_core::CameraDraws::default(),
                post_after: false,
            },
        );
        let wall = ed.world.spawn();
        ed.world.insert(wall, Transform { translation: DVec3::new(-100.0, 0.0, 0.0), ..Transform::IDENTITY });
        ed.world.insert(wall, Parent(home));
        ed.world.insert(wall, Matter::Primitive { shape: Shape::Cube, color: [1.0; 3] });
        ed.world.insert(wall, Collidable);
        ed.toggle_play();
        let face = |ed: &crate::Editor| {
            let at = floptle_core::world_transform(&ed.world, wall).translation;
            let from = at - DVec3::new(0.0, 0.0, 30.0);
            let hit = ed.sim.as_ref().unwrap().raycast(from, Vec3::Z, 60.0)?;
            Some(from.z + hit.distance as f64 - at.z)
        };
        // Long enough for the origin to ride somewhere a rebuild would not put it.
        for _ in 0..37 {
            ed.play_step(1.0 / 60.0, true);
        }
        let rest = face(&ed).expect("the wall");
        // The frame pass's order: lend, a script asks for a rebuild, take back.
        let sim = ed.sim.as_mut().unwrap();
        ed.script_host.set_colliders(std::mem::take(&mut sim.world.colliders), sim.world.origin);
        ed.rebuild_sim();
        let back = ed.script_host.take_colliders();
        ed.sim.as_mut().unwrap().world.set_colliders(back);
        let moved = face(&ed).map_or(f64::INFINITY, |f| (f - rest).abs());
        assert!(moved < 0.05, "the wall collided {moved} m from where it is drawn after the rebuild");
        ed.play_step(1.0 / 60.0, true);
        let moved = face(&ed).map_or(f64::INFINITY, |f| (f - rest).abs());
        assert!(moved < 0.05, "and {moved} m a frame later");
    }

    /// **A script's ray from a drawn point finds the static drawn there, in
    /// `update` and in `lateUpdate`, and a body teleported onto it lands on
    /// it,** on a planet moving at 250 m/s. A launch pad check rayed down
    /// onto a deck a tick away from where it was drawn, and a crew put on a
    /// pad fell past it.
    #[test]
    fn a_script_finds_a_static_on_a_planet_on_rails_where_it_is_drawn() {
        use floptle_core::{Collidable, Matter, Name, Parent, RigidBody, Shape};
        let dir = std::env::temp_dir().join(format!("floptle-rails-static-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        // How far the near face of the pad (a cube of half 0.7) is from where
        // it is drawn, along the way the planet moves. Worst seen so far in
        // `update` goes in node.x, in `lateUpdate` in node.y.
        std::fs::write(
            dir.join("scripts/probe.lua"),
            "local frames, worstU, worstL = 0, 0, 0\n\
             local function off()\n  local pad = find('Pad')\n  \
             local hit = raycast(pad:toWorld(vec3(0, 0, -30)), vec3(0, 0, 1), 60)\n  \
             if not hit then return 1000 end\n  return math.abs(hit.distance - 29.3)\nend\n\
             function update(node, dt)\n  frames = frames + 1\n  worstU = math.max(worstU, off())\n  node.x = worstU\n  \
             if frames == 30 then find('Crew').pos = find('Pad'):toWorld(vec3(0, 3, 0)) end\nend\n\
             function lateUpdate(node, dt)\n  worstL = math.max(worstL, off())\n  node.y = worstL\nend\n",
        )
        .unwrap();
        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        let celestial = |ed: &mut crate::Editor, name: &str, at: DVec3, body: CelestialBody| {
            let e = ed.world.spawn();
            ed.world.insert(e, Name(name.into()));
            ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
            ed.world.insert(e, body);
            e
        };
        // 250 m/s: sqrt(mu / a).
        celestial(&mut ed, "Sun", DVec3::ZERO, CelestialBody { mu: 3.125e8, ..Default::default() });
        let home = CelestialBody { parent: "Sun".into(), a: 5000.0, mu: 1.0e4, soi: 500.0, ..Default::default() };
        let home = celestial(&mut ed, "Home", DVec3::new(5000.0, 0.0, 0.0), home);
        let cam = ed.world.spawn();
        ed.world.insert(cam, Transform { translation: DVec3::new(0.0, 58.0, 25.0), ..Transform::IDENTITY });
        ed.world.insert(cam, Parent(home));
        ed.world.insert(
            cam,
            Matter::Camera {
                fov_y: 1.0,
                active: true,
                target: String::new(),
                cull_mask: u32::MAX,
                target_w: 0,
                target_h: 0,
                target_hz: 0.0,
                ortho: false,
                ortho_height: 10.0,
                stack: Vec::new(),
                draws: floptle_core::CameraDraws::default(),
                post_after: false,
            },
        );
        let pad = ed.world.spawn();
        ed.world.insert(pad, Name("Pad".into()));
        ed.world.insert(pad, Transform { translation: DVec3::new(0.0, 58.0, 0.0), ..Transform::IDENTITY });
        ed.world.insert(pad, Parent(home));
        ed.world.insert(pad, Matter::Primitive { shape: Shape::Cube, color: [1.0; 3] });
        ed.world.insert(pad, Collidable);
        // Falling toward the planet well away from the pad until it is put on it.
        let crew = ed.world.spawn();
        ed.world.insert(crew, Name("Crew".into()));
        ed.world.insert(crew, Transform { translation: DVec3::new(5040.0, 58.0, 0.0), ..Transform::IDENTITY });
        ed.world.insert(crew, RigidBody { gravity: true, ..Default::default() });
        let probe = ed.world.spawn();
        ed.world.insert(probe, Name("Probe".into()));
        ed.world.insert(probe, Transform::IDENTITY);
        ed.world.insert(
            probe,
            floptle_core::Scripts(vec![floptle_core::ScriptInst {
                kind: "probe".into(),
                enabled: true,
                params: Vec::new(),
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
        ed.toggle_play();
        for dt in [0.005f32, 0.016, 0.03, 0.008, 0.1, 0.0167, 0.004, 0.02, 0.033, 0.011].repeat(4) {
            ed.play_step(dt, true);
            assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        }
        for _ in 0..60 {
            ed.play_step(1.0 / 60.0, true);
        }
        let world = |ed: &crate::Editor, e| floptle_core::world_transform(&ed.world, e).translation;
        let worst = world(&ed, probe);
        let on_pad = world(&ed, crew) - world(&ed, pad);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(worst.x < 0.05, "a ray in update found the pad {} m from where it is drawn", worst.x);
        assert!(worst.y < 0.05, "a ray in lateUpdate found the pad {} m from where it is drawn", worst.y);
        assert!(
            on_pad.y > 0.5 && on_pad.y < 3.5 && on_pad.x.hypot(on_pad.z) < 1.0,
            "the crew put on the pad did not land on it: {on_pad}"
        );
    }

    /// **A label projected in `lateUpdate` sits on a planet-fixed point as
    /// this frame draws it.** The view `camera.worldToScreen` reads is fed by
    /// the renderer, so it used to be last frame's camera: with the planet on
    /// rails at 87 m/s and the camera riding it, a point fixed to the planet
    /// was projected off by the planet's motion since the last draw — 0, 1 or
    /// 2 ticks' worth — and shook by tens of pixels. The renderer's feed is
    /// emulated after every step, the way the window does it.
    // Drives `shot::feed_view`, which only the editor build has.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_planet_fixed_point_projected_in_late_update_holds_still_on_screen() {
        use floptle_core::{Matter, Name, Parent};
        let dir = std::env::temp_dir().join(format!("floptle-label-jitter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(
            dir.join("scripts/label.lua"),
            "function lateUpdate(node, dt)\n  local p = find('Home'):toWorld(vec3(3, 2, 0))\n  \
             local sx, sy, _, on = camera.worldToScreen(p.x, p.y, p.z)\n  \
             if on then node.x = sx; node.y = sy end\nend\n",
        )
        .unwrap();
        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        let celestial = |ed: &mut crate::Editor, name: &str, at: DVec3, body: CelestialBody| {
            let e = ed.world.spawn();
            ed.world.insert(e, Name(name.into()));
            ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
            ed.world.insert(e, body);
            e
        };
        // 87 m/s: sqrt(mu / a).
        celestial(&mut ed, "Sun", DVec3::ZERO, CelestialBody { mu: 3.78e7, ..Default::default() });
        let home = CelestialBody { parent: "Sun".into(), a: 5000.0, mu: 1.0e4, soi: 500.0, ..Default::default() };
        let home = celestial(&mut ed, "Home", DVec3::new(5000.0, 0.0, 0.0), home);
        // The camera rides the planet, 25 m back from the point it looks at.
        let cam = ed.world.spawn();
        ed.world.insert(cam, Name("Cam".into()));
        ed.world.insert(cam, Transform { translation: DVec3::new(0.0, 0.0, 25.0), ..Transform::IDENTITY });
        ed.world.insert(cam, Parent(home));
        ed.world.insert(
            cam,
            Matter::Camera {
                fov_y: 1.0,
                active: true,
                target: String::new(),
                cull_mask: u32::MAX,
                target_w: 0,
                target_h: 0,
                target_hz: 0.0,
                ortho: false,
                ortho_height: 10.0,
                stack: Vec::new(),
                draws: floptle_core::CameraDraws::default(),
                post_after: false,
            },
        );
        let label = ed.world.spawn();
        ed.world.insert(label, Name("Label".into()));
        ed.world.insert(label, Transform::IDENTITY);
        ed.world.insert(
            label,
            floptle_core::Scripts(vec![floptle_core::ScriptInst {
                kind: "label".into(),
                enabled: true,
                params: Vec::new(),
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
        ed.toggle_play();
        let world = |ed: &crate::Editor, e| floptle_core::world_transform(&ed.world, e).translation;
        for _ in 0..5 {
            ed.play_step(1.0 / 60.0, true);
            crate::shot::feed_view(&mut ed, None, 800, 600);
        }
        let screen = |ed: &crate::Editor| world(ed, label);
        let mut last = screen(&ed);
        let (mut moved, mut worst) = (0.0f64, 0.0f64);
        for dt in [0.005f32, 0.016, 0.03, 0.008, 0.02, 0.0167, 0.004, 0.033, 0.011].repeat(6) {
            let before = world(&ed, home);
            ed.play_step(dt, true);
            crate::shot::feed_view(&mut ed, None, 800, 600);
            assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
            moved = moved.max((world(&ed, home) - before).length());
            let now = screen(&ed);
            worst = worst.max((now - last).length());
            last = now;
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(last.x > 400.0 && last.y < 300.0, "the label never landed on the point: {last}");
        assert!(moved > 2.0, "the fixture needs frames that run several ticks, the planet moved at most {moved} m");
        assert!(worst < 0.5, "the label moved {worst} px between frames on a still point");
    }

    /// **At Play start the crew rides its planet onto the rails.** A planet is
    /// authored wherever the author put it, and its first tick snaps it to its
    /// orbit; the crew standing on it goes with it rather than being left in
    /// space where the planet was.
    #[test]
    fn at_play_start_the_crew_rides_its_planet_onto_the_rails() {
        use floptle_core::{Name, RigidBody};
        let mut ed = crate::Editor::default();
        let celestial = |ed: &mut crate::Editor, name: &str, at: DVec3, body: CelestialBody| {
            let e = ed.world.spawn();
            ed.world.insert(e, Name(name.into()));
            ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
            ed.world.insert(e, body);
            e
        };
        celestial(&mut ed, "Sun", DVec3::ZERO, CelestialBody { mu: 1.0e6, ..Default::default() });
        // A quarter of an orbit from where it is authored.
        let home = CelestialBody {
            parent: "Sun".into(),
            a: 1000.0,
            mu: 1.0e4,
            soi: 200.0,
            m0: std::f64::consts::FRAC_PI_2,
            ..Default::default()
        };
        let planet = celestial(&mut ed, "Home", DVec3::new(1000.0, 0.0, 0.0), home);
        let crew = ed.world.spawn();
        ed.world.insert(crew, Transform { translation: DVec3::new(1000.0, 60.0, 0.0), ..Transform::IDENTITY });
        ed.world.insert(crew, RigidBody::default());

        ed.toggle_play();
        ed.play_step(1.0 / 60.0, true);
        let railed = floptle_core::world_transform(&ed.world, planet).translation;
        assert!((railed - DVec3::new(1000.0, 0.0, 0.0)).length() > 500.0, "the fixture needs the planet to jump");
        let pos = ed.sim.as_ref().unwrap().body_states().find(|r| r.entity == crew).unwrap().pos;
        assert!((pos - railed).length() < 80.0, "the crew was left at {pos:?}, its planet is at {railed:?}");
    }
}
