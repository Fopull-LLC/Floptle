//! The swell a drawn surface reads: which water's waves a shader on a node
//! takes, and the block of numbers it takes them as.
//!
//! A water shader moves its vertices with `waterSwell(objectPos)`. The waves
//! are not typed into the shader: they are the ones on the water the surface
//! lies on, the same list the solver floats things on, so the picture and
//! the physics cannot drift apart. Each frame the block is written again,
//! with the clock folded into each wave's phase and with where the node is
//! in the water's frame, for every material whose shader reads it.

use floptle_core::math::{DMat3, DQuat, DVec3};
use floptle_core::water::{Lay, Surface, Swell};
use floptle_core::{Entity, Matter, WaterKind};
use floptle_script::water_api::SwellSource;

/// A swell block of no waves: the surface draws still.
pub(crate) const STILL: [[f32; 4]; floptle_core::swell::SWELL_BLOCK] = [[0.0; 4]; floptle_core::swell::SWELL_BLOCK];

/// A body of water's swell, and where it is laid: the frame's origin and
/// turn in the world, and the still surface.
struct Laid {
    swell: std::sync::Arc<Swell>,
    origin: DVec3,
    rot: DQuat,
    lay: Lay,
}

impl crate::Editor {
    /// The simulation time the frame is drawn at, seconds. Bodies are drawn
    /// part-way between the last two ticks, so this is one step behind
    /// `simTime`: the surface a boat is drawn on is the one it floated on.
    pub(crate) fn swell_draw_time(&self) -> f64 {
        if !self.playing || self.sim.is_none() {
            return 0.0;
        }
        (self.sim_now() + self.game_tick.banked() as f64 - self.game_tick.step as f64).max(0.0)
    }

    /// The swell of a Water Volume node, laid where the node is.
    fn node_swell(&self, e: Entity) -> Option<Laid> {
        let Some(Matter::WaterVolume { kind, radius, half_extents, waves, .. }) = self.world.get::<Matter>(e) else {
            return None;
        };
        let swell = std::sync::Arc::new(Swell::new(waves, DVec3::Y)?);
        let wt = floptle_core::world_transform(&self.world, e);
        let lay = match kind {
            WaterKind::Sea => Lay::Round { radius: (*radius * wt.scale.max_element().max(1e-4)) as f64 },
            WaterKind::Pool => Lay::Flat { level: (half_extents[1] * wt.scale.y).abs().max(1e-4) as f64 },
        };
        Some(Laid { swell, origin: wt.translation, rot: wt.rotation.as_dquat(), lay })
    }

    /// The swell of declared water `id`, laid where it is under world point
    /// `near`.
    fn declared_swell(&self, id: u32, near: DVec3) -> Option<Laid> {
        let declared = self.script_host.declared_water();
        let w = declared.iter().find(|w| w.id == id)?;
        let swell = w.swell.clone()?;
        let lay = w.lay_at(near).unwrap_or(match &w.surface {
            Surface::Lake { radius, .. } => Lay::Round { radius: *radius },
            Surface::Grid(_) => Lay::Flat { level: 0.0 },
            Surface::Tiles(t) => t.patch.radius.map_or(Lay::Flat { level: 0.0 }, |radius| Lay::Round { radius }),
            // A ribbon carries no swell, so `w.swell` was `None` above.
            Surface::Ribbon { .. } => return None,
        });
        let r = w.frame.rot;
        Some(Laid { swell, origin: w.frame.origin, rot: DQuat::from_xyzw(r.x as f64, r.y as f64, r.z as f64, r.w as f64), lay })
    }

    /// The water whose swell a shader on node `e` reads: the one a script
    /// bound it to (`water.bindSwell`); else the Water Volume the node is or
    /// is under; else, of the declared water lying on the node, on a node
    /// above it or on the world itself, the one whose surface the node is
    /// nearest.
    fn swell_for(&self, e: Entity) -> Option<Laid> {
        let at = floptle_core::world_transform(&self.world, e).translation;
        match self.script_host.water_swell_bind(e.index()) {
            Some(SwellSource::Node(n)) => return self.node_swell(self.world.entity_at(n)?),
            Some(SwellSource::Declared(id)) => return self.declared_swell(id, at),
            None => {}
        }
        let mut chain = Vec::new();
        let mut up = Some(e);
        while let Some(n) = up.filter(|_| chain.len() < 64) {
            if matches!(self.world.get::<Matter>(n), Some(Matter::WaterVolume { .. })) {
                return self.node_swell(n);
            }
            chain.push(n);
            up = self.world.get::<floptle_core::Parent>(n).map(|p| p.0);
        }
        // Water pinned to the world lies under every node.
        let id = {
            let declared = self.script_host.declared_water();
            let anchors = &self.water_sims.anchors;
            declared
                .iter()
                .filter(|w| w.swell.is_some())
                .filter(|w| w.anchor.is_none() || anchors.iter().any(|(id, node)| *id == w.id && chain.contains(node)))
                .map(|w| (w.id, w.at(at).above.abs()))
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(id, _)| id)
        }?;
        self.declared_swell(id, at)
    }

    /// The swell block for a shader on node `e`, as it stands at the time
    /// the frame is drawn: [`STILL`] where the node is on no water with
    /// waves.
    pub(crate) fn swell_lanes_for(&self, e: Entity) -> [[f32; 4]; floptle_core::swell::SWELL_BLOCK] {
        let Some(laid) = self.swell_for(e) else { return STILL };
        let wt = floptle_core::world_transform(&self.world, e);
        let back = laid.rot.inverse();
        let node_to_water = (DMat3::from_quat(back * wt.rotation.as_dquat()), back * (wt.translation - laid.origin));
        laid.swell.pack(laid.lay, self.swell_draw_time(), node_to_water, DMat3::from_quat(laid.rot))
    }
}
