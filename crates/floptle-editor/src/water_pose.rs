//! Water that moves with what it lies on, tick by tick.
//!
//! The solver's water is built once a frame, before the frame's ticks, and a
//! frame can run several. Water on anything that moves between ticks (a
//! planet on its rails, a platform a script moves in `fixedUpdate`, a pool on
//! a ship's deck) stayed where the frame began while its world moved on: at
//! 280 m/s that is metres a tick, and a crate over a lake rose out of it or
//! sank to the bed. So every body of water remembers the node it takes its
//! pose from, and each tick reads that node again and moves the water there,
//! in place: no rebuild, no scan of the scene. Water whose node has not moved
//! is left exactly as built.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::Entity;

/// Where one body of water in the solver takes its pose from.
pub(crate) struct WaterPoser {
    /// Its place in the solver's volume list.
    volume: usize,
    /// The node it lies on: a Water Volume node itself, or the node declared
    /// water names as its parent.
    node: Entity,
    /// The nearest node at or above `node` carrying a body the solver moves,
    /// if any: its tick pose is the sim's, not the node's.
    carrier: Option<Entity>,
    /// The node's world pose, and the sim origin, when the water was last put
    /// there.
    seen: (DVec3, Quat, Vec3, DVec3),
    kind: PoseKind,
}

enum PoseKind {
    /// A Water Volume sea of this radius before its node's scale.
    Sea { radius: f32 },
    /// A Water Volume pool of these half-extents before its node's scale.
    Pool { half: Vec3 },
    /// Declared water, by id.
    Declared { id: u32 },
}

impl crate::Editor {
    /// Build the solver's water from the scene and from what scripts
    /// declared, once a frame before its ticks, and note where each body of
    /// water takes its pose from.
    pub(crate) fn build_frame_water(&mut self) {
        let Some(origin) = self.sim.as_ref().map(|s| s.world.origin) else {
            self.water_sims.posers.clear();
            return;
        };
        let declared = self.declared_water_volumes(origin);
        let mut water = Self::build_water_field(&self.world, origin);
        water.volumes.extend(declared);
        let mut posers = Vec::new();
        let mut carriers: Vec<(Entity, Option<Entity>)> = Vec::new();
        for (i, v) in water.volumes.iter().enumerate() {
            let (node, kind) = match &v.shape {
                floptle_physics::WaterShape::Declared { water, .. } => {
                    let Some(&(_, e)) = self.water_sims.anchors.iter().find(|(id, _)| *id == water.id) else { continue };
                    (e, PoseKind::Declared { id: water.id })
                }
                _ => {
                    let Some(e) = self.world.entity_at(v.entity) else { continue };
                    let kind = match self.world.get::<floptle_core::Matter>(e) {
                        Some(floptle_core::Matter::WaterVolume { kind: floptle_core::WaterKind::Sea, radius, .. }) => {
                            PoseKind::Sea { radius: *radius }
                        }
                        Some(floptle_core::Matter::WaterVolume { half_extents, .. }) => {
                            PoseKind::Pool { half: Vec3::from(*half_extents) }
                        }
                        _ => continue,
                    };
                    (e, kind)
                }
            };
            // A planet's lakes all lie on one node: its carrier is found once.
            let carrier = match carriers.iter().find(|(n, _)| *n == node) {
                Some((_, c)) => *c,
                None => {
                    let c = self.water_carrier(node);
                    carriers.push((node, c));
                    c
                }
            };
            let wt = floptle_core::world_transform(&self.world, node);
            posers.push(WaterPoser {
                volume: i,
                node,
                carrier,
                seen: (wt.translation, wt.rotation, wt.scale, origin),
                kind,
            });
        }
        if let Some(sim) = self.sim.as_mut() {
            sim.world.water = water;
            let slam = self.script_host.water_slam();
            if !slam.is_empty() || sim.has_water_slam() {
                sim.set_water_slam(slam);
            }
        }
        self.water_sims.posers = posers;
    }

    /// Tell the scripts which nodes are round worlds, before they run: a
    /// river declared on a celestial body lies round it, and one declared
    /// anywhere else lies flat.
    pub(crate) fn publish_water_worlds(&self) {
        let names: Vec<&str> = self
            .world
            .query::<floptle_core::CelestialBody>()
            .filter_map(|(e, _)| self.world.get::<floptle_core::Name>(e).map(|n| n.0.as_str()))
            .collect();
        self.script_host.set_water_worlds(names.iter().copied());
    }

    /// Hand the scripts what this tick's physics saw of water: what entered
    /// or left it (`water.splashes`) and what is in it (`water.wet`).
    pub(crate) fn publish_water_crossings(&mut self) {
        let Some(sim) = self.sim.as_mut() else { return };
        let events = sim.take_splashes();
        if !events.is_empty() {
            self.script_host.push_water_splashes(events);
        }
        let (clock, wet) = sim.wet_now();
        self.script_host.set_water_wet(clock, &wet);
    }

    /// Hand the solver the surfaces scripts moved since the last tick
    /// (`water.setLevel`, `water.setLevels`): the next step floats things
    /// on the new level, without the water being built again.
    fn take_new_water_levels(&mut self) {
        let changed = self.script_host.take_water_changed();
        if changed.is_empty() {
            return;
        }
        let Some(sim) = self.sim.as_mut() else { return };
        let declared = self.script_host.declared_water();
        for v in &mut sim.world.water.volumes {
            let floptle_physics::WaterShape::Declared { water, .. } = &mut v.shape else { continue };
            if !changed.contains(&water.id) {
                continue;
            }
            if let Some(w) = declared.iter().find(|w| w.id == water.id) {
                let water = std::sync::Arc::make_mut(water);
                water.surface = w.surface.clone();
                water.simulated.clone_from(&w.simulated);
            }
        }
    }

    /// The node at or above `node` whose body the solver moves: what a pool
    /// on a ship's deck rides. Only a node with a Rigid Body is asked about.
    fn water_carrier(&self, node: Entity) -> Option<Entity> {
        let sim = self.sim.as_ref()?;
        let mut at = Some(node);
        for _ in 0..64 {
            let e = at?;
            if self.world.get::<floptle_core::RigidBody>(e).is_some() && sim.carrier_motion(e.index()).is_some() {
                return Some(e);
            }
            at = self.world.get::<floptle_core::Parent>(e).map(|p| p.0);
        }
        None
    }

    /// Put the solver's water where this tick has its nodes: after the rails
    /// move the planets, and again after the tick's scripts, before its
    /// physics step. `tick_start` also turns the winds with their worlds.
    pub(crate) fn pose_water_for_tick(&mut self, tick_start: bool) {
        if tick_start && !self.space_rails_prev.is_empty() {
            // The winds ride the same worlds.
            self.refresh_winds();
            self.vfx.wind = (self.script_host.wind_field().clone(), self.script_host.weather_time());
        }
        self.take_new_water_levels();
        if self.water_sims.posers.is_empty() {
            return;
        }
        let Some(origin) = self.sim.as_ref().map(|s| s.world.origin) else { return };
        // A node riding a body the solver moves reads where the last frame
        // drew it, part-way between two ticks, until the frame's writeback:
        // its carrier is put at its tick pose while the water is posed, and
        // put back.
        let mut drawn: Vec<(Entity, floptle_core::transform::Transform)> = Vec::new();
        for p in &self.water_sims.posers {
            let Some(c) = p.carrier else { continue };
            if drawn.iter().any(|(e, _)| *e == c) {
                continue;
            }
            let Some(t) = self.world.get::<floptle_core::transform::Transform>(c).copied() else { continue };
            drawn.push((c, t));
            if let Some(sim) = self.sim.as_ref() {
                sim.write_body_node(c.index(), &mut self.world);
                sim.write_compound_node(c.index(), &mut self.world);
            }
        }
        let posers = std::mem::take(&mut self.water_sims.posers);
        let mut posed = posers;
        for p in &mut posed {
            let wt = floptle_core::world_transform(&self.world, p.node);
            let now = (wt.translation, wt.rotation, wt.scale, origin);
            let carry = p.carrier.and_then(|c| self.sim.as_ref()?.carrier_motion(c.index()));
            if now == p.seen && carry.is_none() {
                continue;
            }
            p.seen = now;
            let Some(sim) = self.sim.as_mut() else { break };
            let Some(v) = sim.world.water.volumes.get_mut(p.volume) else { continue };
            let center = (wt.translation - origin).as_vec3();
            match (&p.kind, &mut v.shape) {
                (PoseKind::Sea { radius }, floptle_physics::WaterShape::Sphere { center: c, radius: r }) => {
                    *c = center;
                    *r = radius * wt.scale.max_element().max(1e-4);
                    let r = *r as f64;
                    self.script_host.pose_water_volume(v.entity, wt.translation, Quat::IDENTITY, r, DVec3::ZERO);
                }
                (PoseKind::Pool { half }, floptle_physics::WaterShape::Box { center: c, half: h, rot }) => {
                    *c = center;
                    *h = (*half * wt.scale).abs().max(Vec3::splat(1e-4));
                    *rot = wt.rotation;
                    let h = h.as_dvec3();
                    self.script_host.pose_water_volume(v.entity, wt.translation, wt.rotation, 0.0, h);
                }
                (PoseKind::Declared { id }, floptle_physics::WaterShape::Declared { water, origin: o }) => {
                    let frame = floptle_core::scatter::Frame { origin: wt.translation, rot: wt.rotation.normalize() };
                    // Built fresh this frame and held nowhere else, so this
                    // writes in place rather than copying the water.
                    std::sync::Arc::make_mut(water).frame = frame;
                    *o = origin;
                    self.script_host.set_water_frame(*id, frame);
                }
                _ => {}
            }
            // The carrier's own motion is the water's: what floats in a deck
            // pool is dragged along with the ship, not held back by it.
            v.carry = carry.map_or_else(floptle_physics::WaterCarry::default, |(about, vel, ang_vel)| {
                floptle_physics::WaterCarry { vel, ang_vel, about }
            });
        }
        self.water_sims.posers = posed;
        for (c, t) in drawn {
            if let Some(tr) = self.world.get_mut::<floptle_core::transform::Transform>(c) {
                *tr = t;
            }
        }
    }
}
