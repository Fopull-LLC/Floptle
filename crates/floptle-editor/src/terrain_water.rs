//! The water each terrain's far ground is kept under.
//!
//! A terrain drawn from afar is simplified, and over a sea floor a few
//! metres down the simplified ground can stand out of the water: dark
//! triangles in a sea seen from orbit. The mesher keeps submerged ground
//! under a declared level ([`floptle_field::WaterLevel`]); this is where the
//! level comes from, and what is drawn again when it changes.
//!
//! A terrain's water is, in order:
//!
//! 1. what a script set for it (`terrain.setWaterLevel`): a level, or none;
//! 2. otherwise the water declared on it: a Water Volume sea centred on the
//!    terrain or a child of it, a Water Volume pool that is a child of it,
//!    and every `water.addVolume` lake about its centre.
//!
//! With none of those the terrain's field holds no level and its meshes are
//! exactly what they always were.

use std::collections::HashMap;

use floptle_core::ecs::Entity;
use floptle_core::math::{DVec3, Vec3};
use floptle_core::Matter;
use floptle_field::{WaterLevel, WaterSurface};

/// The most levels one terrain is given: a planet's lakes, the largest first.
const MAX_LEVELS: usize = 32;

/// Water somewhere in the scene, in world space.
enum Found {
    /// Part of a sphere: a sea (`cap` is `None`) or a lake, within `cap.1`
    /// of the point `cap.0`.
    Round { node: Option<Entity>, centre: DVec3, radius: f64, cap: Option<(DVec3, f64)> },
    /// A pool's flat top: its middle, its up, and how far its corners are.
    Flat { node: Entity, top: DVec3, up: Vec3, reach: f64 },
}

/// Are two levels the same water, to within a hundredth of `voxel`? A sea
/// riding its planet round an orbit reads a hair differently each frame, and
/// that is not a change.
fn same(a: &WaterLevel, b: &WaterLevel, voxel: f32) -> bool {
    let tol = voxel * 0.01;
    let near = |x: f32, y: f32| (x - y).abs() <= tol;
    let near3 = |x: [f32; 3], y: [f32; 3]| (0..3).all(|k| near(x[k], y[k]));
    let surface = match (a.surface, b.surface) {
        (WaterSurface::Sphere { centre: c1, radius: r1 }, WaterSurface::Sphere { centre: c2, radius: r2 }) => near3(c1, c2) && near(r1, r2),
        (WaterSurface::Plane { up: u1, height: h1 }, WaterSurface::Plane { up: u2, height: h2 }) => {
            (0..3).all(|k| (u1[k] - u2[k]).abs() <= 1e-4) && near(h1, h2)
        }
        _ => false,
    };
    let within = match (a.within, b.within) {
        (None, None) => true,
        (Some((p1, r1)), Some((p2, r2))) => near3(p1, p2) && near(r1, r2),
        _ => false,
    };
    surface && within && near(a.margin, b.margin) && a.flatten == b.flatten
}

/// Does a change to `levels` reach chunk `c` of a field whose chunks are
/// `chunk_units` across? Water with no bounds reaches every chunk.
fn reaches(levels: &[WaterLevel], c: [i32; 3], chunk_units: f32) -> bool {
    let mid = (Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32) + Vec3::splat(0.5)) * chunk_units;
    levels.iter().any(|w| match w.within {
        None => true,
        // A chunk's own corners, and what its mesh reads past them.
        Some((at, reach)) => (mid - Vec3::from(at)).length() <= reach + chunk_units * 1.5,
    })
}

impl crate::Editor {
    /// Is `node` at or under `root` in the scene tree?
    fn is_under(&self, mut node: Entity, root: Entity) -> bool {
        for _ in 0..64 {
            if node == root {
                return true;
            }
            match self.world.get::<floptle_core::Parent>(node) {
                Some(p) => node = p.0,
                None => return false,
            }
        }
        false
    }

    /// Every body of water in the scene a terrain's far ground could lie
    /// under.
    fn scene_water(&self) -> Vec<Found> {
        use floptle_core::WaterKind;
        let mut out = Vec::new();
        for (e, m) in self.world.query::<Matter>() {
            let Matter::WaterVolume { kind, radius, half_extents, .. } = m else { continue };
            if floptle_core::is_disabled(&self.world, e) {
                continue;
            }
            let wt = floptle_core::world_transform(&self.world, e);
            match kind {
                WaterKind::Sea => out.push(Found::Round {
                    node: Some(e),
                    centre: wt.translation,
                    // As the solver reads a scaled sea: by its largest axis.
                    radius: f64::from(radius * wt.scale.max_element().max(1e-4)),
                    cap: None,
                }),
                WaterKind::Pool => {
                    let half = Vec3::new(half_extents[0] * wt.scale.x, half_extents[1] * wt.scale.y, half_extents[2] * wt.scale.z).abs();
                    let up = wt.rotation * Vec3::Y;
                    out.push(Found::Flat {
                        node: e,
                        top: wt.translation + (up * half.y).as_dvec3(),
                        up,
                        reach: f64::from(half.x.hypot(half.z)),
                    });
                }
            }
        }
        for w in self.script_host.declared_water().iter() {
            let floptle_core::water::Surface::Lake { radius, axis, reach } = &w.surface else { continue };
            let middle = w.frame.to_world(axis.try_normalize().unwrap_or(DVec3::Y) * *radius);
            // A lake that reaches right round its world is a sea.
            let cap = (*reach < std::f64::consts::PI * radius * 0.999).then_some((middle, *reach));
            out.push(Found::Round { node: None, centre: w.frame.origin, radius: *radius, cap });
        }
        out
    }

    /// The water every terrain's far ground is to be kept under, in each
    /// terrain's own field space: what [`floptle_field::ChunkField::set_water_levels`]
    /// takes. A terrain with none is not in the map.
    pub(crate) fn terrain_water_levels(&self) -> HashMap<Entity, Vec<WaterLevel>> {
        use floptle_script::TerrainWater;
        let mut out = HashMap::new();
        if self.terrains.is_empty() {
            return out;
        }
        let found = self.scene_water();
        for (&e, t) in &self.terrains {
            let rule = match self.world.get::<Matter>(e) {
                Some(Matter::Terrain { id, .. }) => self.terrain_water_rules.get(id).copied().unwrap_or_default(),
                _ => Default::default(),
            };
            if rule.water == TerrainWater::Off || (found.is_empty() && rule.water == TerrainWater::Declared) {
                continue;
            }
            let wt = floptle_core::world_transform(&self.world, e);
            let (anchor, rot, ts) = (wt.translation, wt.rotation.normalize(), wt.scale.x.max(1e-6));
            // World space into the field's: the node's rotation and its
            // uniform scale, as its meshes are placed.
            let to_field = |p: DVec3| (rot.inverse() * (p - anchor).as_vec3()) / ts;
            let voxel = t.field.voxel();
            let margin = rule.margin.map_or(WaterLevel::MARGIN / ts, |m| m as f32 / ts);
            let level = |surface, within| WaterLevel { surface, within, margin, flatten: rule.flatten };
            let mut levels = Vec::new();
            match rule.water {
                TerrainWater::Off => {}
                // The level is in the terrain's own frame, which is the
                // field's: a radius about its centre, or a height on its +Y.
                TerrainWater::At(at) => {
                    let flat = rule.flat.unwrap_or_else(|| self.world.get::<floptle_core::CelestialBody>(e).is_none());
                    levels.push(if flat {
                        level(WaterSurface::Plane { up: [0.0, 1.0, 0.0], height: at as f32 }, None)
                    } else {
                        level(WaterSurface::Sphere { centre: [0.0; 3], radius: at as f32 }, None)
                    });
                }
                TerrainWater::Declared => {
                    for w in &found {
                        match *w {
                            Found::Round { node, centre, radius, cap } => {
                                let (c, r) = (to_field(centre), radius as f32 / ts);
                                // This terrain's own water: a child of it, or
                                // centred where it is. Another world's sea is
                                // not, however near it passes.
                                let own = node.is_some_and(|n| self.is_under(n, e)) || c.length() <= (2.0 * voxel).max(0.02 * r);
                                if !own {
                                    continue;
                                }
                                let within = cap.map(|(p, reach)| (to_field(p).to_array(), reach as f32 / ts));
                                levels.push(level(WaterSurface::Sphere { centre: c.to_array(), radius: r }, within));
                            }
                            // A pool rides whatever it is a child of: only
                            // one that is the terrain's own holds still on it.
                            Found::Flat { node, top, up, reach } => {
                                if !self.is_under(node, e) {
                                    continue;
                                }
                                let (p, up) = (to_field(top), (rot.inverse() * up).normalize_or(Vec3::Y));
                                let surface = WaterSurface::Plane { up: up.to_array(), height: p.dot(up) };
                                levels.push(level(surface, Some((p.to_array(), reach as f32 / ts))));
                            }
                        }
                    }
                }
            }
            if levels.is_empty() {
                continue;
            }
            // The widest water first: the mesher takes the first level over a
            // cell that the cell is wholly under.
            levels.sort_by(|a, b| {
                let wide = |w: &WaterLevel| w.within.map_or(f32::INFINITY, |(_, r)| r);
                wide(b).total_cmp(&wide(a))
            });
            levels.truncate(MAX_LEVELS);
            out.insert(e, levels);
        }
        out
    }

    /// Give every terrain's field the water its far ground is kept under,
    /// and where that changed, ask for the coarse meshes it reaches again.
    ///
    /// Each is meshed again on the workers at the level it is drawn at, a
    /// few a frame like any other streaming, and draws its old mesh until the
    /// new one lands: a sea that rises does not stall the frame it rises in.
    /// A coarse mesh already on its way was made under the old water, and is
    /// not taken. Full-detail meshes never read the level and are left.
    pub(crate) fn sync_terrain_water(&mut self) {
        for (id, rule) in self.script_host.take_terrain_water_levels() {
            if rule == Default::default() {
                self.terrain_water_rules.remove(&id);
            } else {
                self.terrain_water_rules.insert(id, rule);
            }
        }
        let mut want = self.terrain_water_levels();
        for (e, t) in &mut self.terrains {
            let want = want.remove(e).unwrap_or_default();
            let (have, voxel) = (t.field.water_levels(), t.field.voxel());
            if have.len() == want.len() && have.iter().zip(&want).all(|(a, b)| same(a, b, voxel)) {
                continue;
            }
            // What came, went or moved.
            let mut changed: Vec<WaterLevel> = have.iter().filter(|a| !want.iter().any(|b| same(a, b, voxel))).copied().collect();
            changed.extend(want.iter().filter(|b| !have.iter().any(|a| same(a, b, voxel))).copied());
            t.field.set_water_levels(want);
            let Some(r) = self.terrain_render.get_mut(e) else { continue };
            // A world all under a sea draws its ball under it too.
            r.impostor_cap = None;
            let chunk_units = floptle_field::CHUNK as f32 * voxel;
            let again: Vec<[i32; 3]> =
                r.slots.iter().filter(|(c, (_, lod))| *lod > 0 && reaches(&changed, **c, chunk_units)).map(|(c, _)| *c).collect();
            r.stale.extend(again);
            r.pending.retain(|c, (lod, _)| *lod == 0 || !reaches(&changed, *c, chunk_units));
        }
    }
}

/// The radius a body's impostor ball must stay within, in its field's units,
/// for the ball not to stand out of a sea its ground is all under: infinite
/// when it has no such sea.
///
/// The same rule its far meshes follow, asked of the same 26 rays the ball's
/// colour is read along: where every one of them finds the ground under the
/// water, the ball is drawn the level's margin under it.
pub(crate) fn impostor_water_cap(field: &floptle_field::ChunkField, radius: f32) -> f32 {
    let mut cap = f32::INFINITY;
    for w in field.water_levels() {
        let WaterSurface::Sphere { radius: sea, .. } = w.surface else { continue };
        if w.within.is_some() {
            continue;
        }
        let (mut hits, mut dry) = (0, 0);
        for x in -1..=1i32 {
            for y in -1..=1i32 {
                for z in -1..=1i32 {
                    if x == 0 && y == 0 && z == 0 {
                        continue;
                    }
                    let d = Vec3::new(x as f32, y as f32, z as f32).normalize();
                    let reach = radius.max(sea) * 1.6 + 8.0;
                    if let Some(hit) = field.raycast(d * reach, -d, reach * 2.0) {
                        hits += 1;
                        dry += i32::from(w.above(hit) > 0.0);
                    }
                }
            }
        }
        if hits > 0 && dry == 0 {
            cap = cap.min(sea - w.margin);
        }
    }
    cap
}

#[cfg(test)]
mod tests;
