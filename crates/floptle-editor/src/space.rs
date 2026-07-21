//! On-rails celestial driver (solar demo S2, `docs/solar-demo-plan.md`).
//!
//! Each gameplay tick: advance space time by `warp × dt`, assemble the scene's
//! `CelestialBody` nodes into a [`floptle_core::frames::System`], WRITE every
//! non-root body node's translation from its Kepler elements (exact analytic
//! orbits — stable at any warp), re-anchor their terrain colliders in the sim,
//! rebuild gravity (the µ/r² centers moved), and feed the `space.*` snapshot to
//! scripts. The ROOT body (empty `parent`) stays where the scene put it. Bodies
//! may live under a scene group (a generator's "<Star> System" folder): rails
//! positions are computed in WORLD space and converted into the scene parent's
//! frame before the local-translation write.

use floptle_core::frames::{Body, Kepler, System};
use floptle_core::{CelestialBody, Entity, Transform};
use floptle_core::math::DVec3;

use crate::Editor;

impl Editor {
    /// Advance rails one gameplay tick. No-op unless Playing with celestial bodies.
    pub(crate) fn update_space_rails(&mut self, tick_dt: f64) {
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

        // Assemble the system: parent linkage by node NAME; SOI auto-derives
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
                System::soi_radius(b.a.abs(), b.mu, cb[p].1.mu)
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
        for (i, (e, b)) in cb.iter().enumerate() {
            let (sp, sv) = sys.body_pos_vel(i, t);
            let wp = root_pos + sp;
            let old = floptle_core::world_transform(&self.world, *e).translation;
            deltas.push(wp - old);
            if i != root {
                // `wp` is WORLD-space but `Transform.translation` is parent-local:
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
            if self.terrains.contains_key(e)
                && let Some(sim) = self.sim.as_mut()
            {
                sim.set_collider_anchor(e.index(), wp);
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
        // Dominant-frame CARRY (the patched-conic frame, made physical): every
        // dynamic body inside a moving celestial's sphere of influence shifts
        // by that body's rails delta this tick — stand on an orbiting moon and
        // you ride it instead of it sliding out from under you; orbit inside
        // its SOI and you orbit IT, not a point it left behind. Velocity is
        // untouched (positions ARE the frame); crossing an SOI boundary swaps
        // frames with a small world-velocity step — the v1 patched-conic seam.
        //
        // WARP COASTING (S4): while warp > 1 every IN-FLIGHT body (off the
        // ground, clear of the surface, actually moving relative to its
        // dominant celestial) snaps to its OWN Kepler rails — its conic is
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
        if let Some(sim) = self.sim.as_mut() {
            let states: std::collections::HashMap<u32, (floptle_core::math::Vec3, bool)> = sim
                .body_states()
                .map(|(e, vel, _, grounded, _)| (e.index(), (vel, grounded)))
                .collect();
            let t_old = t - tick_dt * self.space_warp;
            for (eid, pos) in sim.body_positions() {
                let mut dom: Option<(usize, f64)> = None; // (index, soi)
                for (i, sb) in sys.bodies.iter().enumerate() {
                    // Containment against the OLD center: `pos` is the body's
                    // PRE-tick position while `bodies[i].pos` already moved by
                    // this tick's rails delta. Testing the new center strands
                    // a body when its planet jumps (worst on the FIRST tick,
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
                let Some((i, _)) = dom else { continue };
                let (mut vel, grounded) = states.get(&eid).copied().unwrap_or_default();
                // FRAME CONVENTION: a dynamic body's sim velocity is measured
                // in its DOMINANT celestial's carried frame — the carry moves
                // positions only, so a landed ship reads v ≈ 0 while its
                // planet orbits the star at full speed. Everything below (and
                // `space.elements`) treats velocities as frame-relative;
                // subtracting the center's world velocity here again was the
                // bug that bent trajectories the moment warp engaged.
                //
                // SOI SEAM: crossing into a different dominant frame must keep
                // the WORLD velocity continuous, so the sim velocity jumps by
                // (old frame vel − new frame vel) — leave a planet's SOI and
                // you carry its orbital velocity into the star's frame.
                let dom_key = cb[i].0.index();
                // The tick-sampled SOI seam ONLY runs for bodies NOT coasting
                // on rails: a coast handles its own frame handoffs at the
                // EXACT crossing time (bisected on the conic below). Applying
                // this sampled seam to a coasting ship at high warp put the
                // velocity step at the wrong time/place — every moon-SOI
                // transit bent the orbit a little until clean ellipses
                // "escaped".
                let coast_active = warping && self.space_coast.contains_key(&eid);
                if !coast_active {
                    let prev = self.space_frame.insert(eid, dom_key);
                    if let Some(p) = prev
                        && p != dom_key
                        && let Some(j) = cb.iter().position(|(e, _)| e.index() == p)
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
                // Relative to the OLD center: `center` already moved by delta
                // this tick, the body's sim position has not.
                let rel = (pos - center) + deltas[i];
                let flying = warping
                    && !grounded
                    && rel.length() > cb[i].1.body_radius + 8.0
                    && vel.as_dvec3().length_squared() > 0.01;
                if flying && cb[i].1.mu > 0.0 {
                    // Capture on engage only. An EXISTING coast keeps its conic
                    // even when this tick's sampled dominant differs — the
                    // crossing walk below hands frames off at the exact
                    // boundary time instead of the tick boundary.
                    self.space_coast.entry(eid).or_insert_with(|| {
                        // Capture the conic from the PRE-TICK state (old center,
                        // old time) — from here on the cached elements are truth.
                        // The sim velocity IS the frame-relative velocity.
                        (dom_key, Kepler::from_state(rel, vel.as_dvec3(), cb[i].1.mu, t_old))
                    });
                    // World state of body index `j` at absolute time τ.
                    let body_w = |j: usize, tau: f64| {
                        let (p, v) = sys.body_pos_vel(j, tau);
                        (root_pos + p, v)
                    };
                    // Smallest containing SOI at τ — the patched-conic rule,
                    // evaluated on the ANALYTIC rails (exact at any warp).
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
                        cb.iter().position(|(e, _)| e.index() == fdk).unwrap_or(i);
                    // Evaluate t_old → t, bisecting each SOI crossing to its
                    // exact time and re-capturing the conic THERE (world
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
                        fdk = cb[nd].0.index();
                        t_lo = tx;
                    }
                    self.space_coast.insert(eid, (fdk, k));
                    self.space_frame.insert(eid, fdk);
                    // G1 residency: warp crosses the 80-radii terrain-load
                    // lead in milliseconds — closing on a body whose field is
                    // still COLD drops to realtime so the background stream
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
                        continue;
                    }
                    // Surface proximity KILLS warp (the KSP rule): a conic
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
                        continue;
                    }
                    sim.set_body_position(eid, DVec3::from(bodies[fd].pos) + r2);
                    sim.set_body_velocity(eid, v2.as_vec3());
                } else {
                    self.space_coast.remove(&eid);
                    if deltas[i].length_squared() > 1e-18 {
                        sim.shift_body(eid, deltas[i]);
                    }
                }
            }
            // COMPOUNDS ride their dominant frame too. Skipping them was the
            // launch-day fling: the spawn planet orbits its star at ~90 u/s,
            // every carried body (the astronaut) rode along, and the freshly
            // assembled vessel — never shifted — watched its planet sail away,
            // which read as "my ship got sucked into space the moment it
            // spawned". Same containment-vs-old-center rule, same SOI-seam
            // velocity step; no warp coasting yet (v0 vessels fly realtime —
            // grounded compounds are contact-pinned, which is what makes
            // warping while parked safe, same as single bodies).
            for (eid, pos) in sim.compound_positions() {
                let mut dom: Option<(usize, f64)> = None;
                for (i, sb) in sys.bodies.iter().enumerate() {
                    let old_center = DVec3::from(bodies[i].pos) - deltas[i];
                    if (pos - old_center).length() <= sb.soi
                        && dom.is_none_or(|(_, s)| sb.soi < s)
                    {
                        dom = Some((i, sb.soi));
                    }
                }
                let Some((i, _)) = dom else { continue };
                let dom_key = cb[i].0.index();
                let prev = self.space_frame.insert(eid, dom_key);
                if let Some(p) = prev
                    && p != dom_key
                    && let Some(j) = cb.iter().position(|(e, _)| e.index() == p)
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
                if deltas[i].length_squared() > 1e-18 {
                    sim.shift_compound(eid, deltas[i]);
                }
            }
            // SURFACE STRUCTURES ride their planet: a Static-bodied node
            // parented (at any depth) under a celestial follows it visually
            // through the transform hierarchy for free — but its baked
            // collider blob would stay behind in space. Shift the colliders
            // by the same rails delta so a launchpad on an orbiting world
            // stays exactly as solid as the terrain it stands on.
            let carried: Vec<(Entity, usize)> = self
                .world
                .query::<floptle_core::RigidBody>()
                .filter(|(_, rb)| rb.mode == floptle_core::BodyMode::Static)
                .filter_map(|(e, _)| {
                    let mut cur = e;
                    for _ in 0..64 {
                        let floptle_core::Parent(p) =
                            self.world.get::<floptle_core::Parent>(cur).copied()?;
                        if let Some(j) = cb.iter().position(|(ce, _)| *ce == p) {
                            return Some((e, j));
                        }
                        cur = p;
                    }
                    None
                })
                .collect();
            for (e, j) in carried {
                if deltas[j].length_squared() > 1e-18 {
                    sim.shift_statics_of(e.index(), deltas[j]);
                }
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

/// Convert a WORLD position into `e`'s scene-parent-local frame — what a
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
    /// puts the node's WORLD transform exactly at the rails position, whatever
    /// frame the scene parent (a generator's system group) sits in.
    #[test]
    fn rails_world_position_survives_a_scene_parent() {
        let mut w = World::default();
        let group = w.spawn();
        // A group deliberately NOT at identity — offset, rotated, scaled.
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
        // Tolerance: rotation quats are f32 (ADR-0015 keeps only translation
        // f64), so a rotated parent leaves f32-epsilon × lever-arm residue
        // (~1e-7 × 5000 units). Millimeter-scale at system scale is exact
        // for our purposes — and generator groups are identity anyway.
        assert!((got - rails_wp).length() < 1e-3, "world pos drifted: {got:?}");

        // Top-level nodes pass through untouched.
        let top = w.spawn();
        w.insert(top, Transform::IDENTITY);
        assert_eq!(world_to_parent_local(&w, top, rails_wp), rails_wp);
    }
}
