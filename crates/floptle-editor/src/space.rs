//! On-rails celestial driver (solar demo S2, `docs/solar-demo-plan.md`).
//!
//! Each gameplay tick: advance space time by `warp × dt`, assemble the scene's
//! `CelestialBody` nodes into a [`floptle_core::frames::System`], WRITE every
//! non-root body node's translation from its Kepler elements (exact analytic
//! orbits — stable at any warp), re-anchor their terrain colliders in the sim,
//! rebuild gravity (the µ/r² centers moved), and feed the `space.*` snapshot to
//! scripts. The ROOT body (empty `parent`) stays where the scene put it; every
//! other body should be a TOP-LEVEL node (rails write world positions).

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
            if i != root
                && let Some(tr) = self.world.get_mut::<Transform>(*e)
            {
                tr.translation = wp;
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
                let center = DVec3::from(bodies[i].pos);
                // Relative to the OLD center: `center` already moved by delta
                // this tick, the body's sim position has not.
                let rel = (pos - center) + deltas[i];
                let flying = warping
                    && !grounded
                    && rel.length() > cb[i].1.body_radius + 8.0
                    && vel.as_dvec3().length_squared() > 0.01;
                if flying && cb[i].1.mu > 0.0 {
                    let recapture = self
                        .space_coast
                        .get(&eid)
                        .is_none_or(|(d, _)| *d != dom_key);
                    if recapture {
                        // Capture the conic from the PRE-TICK state (old center,
                        // old time) — from here on the cached elements are truth.
                        // The sim velocity IS the frame-relative velocity.
                        let k = Kepler::from_state(rel, vel.as_dvec3(), cb[i].1.mu, t_old);
                        self.space_coast.insert(eid, (dom_key, k));
                    }
                    if let Some((_, k)) = self.space_coast.get(&eid) {
                        let (r2, v2) = k.pos_vel(cb[i].1.mu, t);
                        // Surface proximity KILLS warp (the KSP rule): a conic
                        // whose next sample dips near the ground would teleport
                        // the ship into rock at 1000×. Drop to realtime and let
                        // physics take it from the last on-conic state.
                        if r2.length() < cb[i].1.body_radius + 25.0 {
                            self.space_warp = 1.0;
                            self.space_coast.remove(&eid);
                            self.console.push(
                                floptle_script::LogLevel::Debug,
                                "time-warp dropped to 1× — surface proximity".into(),
                                None,
                            );
                            continue;
                        }
                        sim.set_body_position(eid, center + r2);
                        sim.set_body_velocity(eid, v2.as_vec3());
                    }
                } else {
                    self.space_coast.remove(&eid);
                    if deltas[i].length_squared() > 1e-18 {
                        sim.shift_body(eid, deltas[i]);
                    }
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
