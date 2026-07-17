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
        if let Some(sim) = self.sim.as_mut() {
            for (eid, pos) in sim.body_positions() {
                let mut dom: Option<(usize, f64)> = None; // (index, soi)
                for (i, sb) in sys.bodies.iter().enumerate() {
                    let center = DVec3::from(bodies[i].pos);
                    if (pos - center).length() <= sb.soi
                        && dom.is_none_or(|(_, s)| sb.soi < s)
                    {
                        dom = Some((i, sb.soi));
                    }
                }
                if let Some((i, _)) = dom
                    && deltas[i].length_squared() > 1e-18
                {
                    sim.shift_body(eid, deltas[i]);
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
