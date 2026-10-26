//! What each terrain holds in memory, part by part, for `terrain.stats` (see
//! `floptle_script::terrain_stats_api`). Counted once a second while a game
//! runs: a pass over every chunk table, too much for every frame and nothing
//! once a second.

use floptle_core::Matter;

/// Bytes a CPU chunk mesh holds.
fn mesh_bytes(m: &floptle_field::ChunkMesh) -> u64 {
    (m.positions.capacity() * 12 + m.normals.capacity() * 12 + m.colors.capacity() * 4 + m.indices.capacity() * 4) as u64
}

impl crate::Editor {
    /// Count every terrain's memory now. Chunks shared between the editor's
    /// field, the physics' copy and Play's snapshot are counted once.
    pub(crate) fn terrain_stats(&self) -> Vec<floptle_script::TerrainStats> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (&e, t) in &self.terrains {
            let Some(Matter::Terrain { id, .. }) = self.world.get::<Matter>(e) else { continue };
            let mut s = floptle_script::TerrainStats { id: *id, ..Default::default() };
            let mut field = |f: &floptle_field::ChunkField, s: &mut floptle_script::TerrainStats, chunks: bool| {
                let m = f.memory(&mut seen);
                s.field += m.data as u64;
                s.stand_ins += m.stand_ins as u64;
                s.tables += m.tables as u64;
                if chunks {
                    s.data_chunks = m.data_chunks as u64;
                    s.stand_in_chunks = m.stand_in_chunks as u64;
                }
            };
            field(&t.field, &mut s, true);
            if let Some(col) = self.sim.as_ref().and_then(|sim| {
                sim.world.colliders.iter().find(|c| c.eid == Some(e.index())).and_then(|c| c.shape.chunk_terrain())
            }) {
                field(&col.field, &mut s, false);
                s.collision += col.surface_bytes() as u64;
            }
            if let Some((fields, _)) = self.play_terrains.as_ref() {
                for (_, f) in fields.iter().filter(|(fid, _)| fid == id) {
                    field(f, &mut s, false);
                }
            }
            if let Some(r) = self.terrain_render.get(&e) {
                if let Some(raster) = self.raster.as_ref() {
                    s.meshes_gpu += r.slots.values().map(|(mid, _)| raster.mesh_gpu_bytes(*mid)).sum::<u64>();
                    s.meshes_gpu += r.groups.values().map(|mid| raster.mesh_gpu_bytes(*mid)).sum::<u64>();
                }
                s.meshes_cpu += r.far.values().map(mesh_bytes).sum::<u64>();
            }
            s.meshes_cpu += t.premeshed.values().map(mesh_bytes).sum::<u64>();
            s.collision += t.collision.iter().map(|(_, c)| c.heap_bytes() as u64).sum::<u64>();
            s.shadow += (t.shadow.distance.capacity() * 4 + t.shadow.color.capacity() * 4) as u64;
            s.undo += self.history.terrain_undo_bytes(*id);
            if let Some((_, u)) = self.stroke_snapshot.as_ref().filter(|(sid, _)| sid == id) {
                s.undo += u.memory_bytes() as u64;
            }
            out.push(s);
        }
        out.sort_by_key(|s| s.id);
        out
    }

    /// How much of its detail the terrain keeps (see `rings_for_body`).
    pub(crate) fn terrain_detail(&self) -> f32 {
        1.0 - self.terrain_shed
    }

    /// Once a second of Play: count the terrains for `terrain.stats`, and
    /// hold them to the budget a game set.
    pub(crate) fn publish_terrain_stats(&mut self, dt: f32) {
        self.terrain_stats_in -= dt;
        if self.terrain_stats_in > 0.0 {
            return;
        }
        self.terrain_stats_in = 1.0;
        let stats = self.terrain_stats();
        let total: u64 = stats.iter().map(|s| s.total()).sum();
        self.terrain_shed = shed_for(self.script_host.terrain_budget(), total, self.terrain_shed);
        if self.terrain_shed > 0.0 && !self.terrain_shed_said {
            self.terrain_shed_said = true;
            let budget = self.script_host.terrain_budget().unwrap_or(0);
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "terrain.setBudget: the terrain holds {:.0} MB of a {:.0} MB budget, so it is keeping less \
                     of its far detail. terrain.stats().detail says how much",
                    total as f64 / 1e6,
                    budget as f64 / 1e6
                ),
                None,
            );
        }
        if self.terrain_shed >= MOST_SHED
            && self.script_host.terrain_budget().is_some_and(|b| total > b)
            && !std::mem::replace(&mut self.terrain_budget_floor_said, true)
        {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "terrain.setBudget: even at its least far detail the terrain holds {:.0} MB, over the budget: \
                     what is round the camera is kept whatever the budget",
                    total as f64 / 1e6
                ),
                None,
            );
        }
        self.script_host.set_terrain_stats(stats, self.terrain_detail());
    }
}

/// The most of its detail a terrain gives up to its budget: past this the
/// rings are as near as the floors that keep what the player can touch.
pub(crate) const MOST_SHED: f32 = 0.75;

/// How much detail to give up next, from what is held against the budget:
/// a tenth more a second while over it, a twentieth back while comfortably
/// under it (below 85 %, so it does not see-saw on the line), none without
/// a budget.
fn shed_for(budget: Option<u64>, total: u64, shed: f32) -> f32 {
    match budget {
        None => 0.0,
        Some(b) if total > b => (shed + 0.1).min(MOST_SHED),
        Some(b) if (total as f64) < b as f64 * 0.85 => (shed - 0.05).max(0.0),
        Some(_) => shed,
    }
}


#[cfg(test)]
mod tests {
    use floptle_core::{Matter, Transform};

    /// Over its budget the terrain gives up a tenth more of its detail each
    /// count, to a floor; comfortably under it, a twentieth comes back; on
    /// the line it holds; with no budget it keeps everything.
    #[test]
    fn the_terrain_gives_up_detail_while_over_its_budget_and_takes_it_back() {
        let mut shed = 0.0f32;
        for _ in 0..20 {
            shed = super::shed_for(Some(100), 150, shed);
        }
        assert_eq!(shed, super::MOST_SHED, "over the budget for long, it should shed to the floor");
        assert_eq!(super::shed_for(Some(100), 95, 0.4), 0.4, "just under the budget it should hold, not see-saw");
        let back = super::shed_for(Some(100), 50, 0.4);
        assert!((back - 0.35).abs() < 1e-6, "well under the budget it should take detail back: {back}");
        assert_eq!(super::shed_for(None, 10_000, 0.6), 0.0, "with no budget nothing is given up");
    }

    /// A terrain's count is its field's bytes, and Play's snapshot of the
    /// field, sharing its chunks, adds nothing to it; a terrain that takes
    /// up nothing more is not counted twice either.
    #[test]
    fn play_s_snapshot_of_a_terrain_is_not_counted_again() {
        let mut ed = crate::Editor::default();
        let mut field = floptle_field::ChunkField::new(1.0);
        field.fill_slab(
            floptle_core::math::Vec3::new(-40.0, -8.0, -40.0),
            floptle_core::math::Vec3::new(40.0, 0.0, 40.0),
            0.0,
            [0.5; 3],
        );
        let own = field.memory(&mut std::collections::HashSet::new());
        let e = ed.world.spawn();
        ed.world.insert(e, Transform::IDENTITY);
        ed.world.insert(e, Matter::Terrain { id: 4, collision: Default::default() });
        ed.play_terrains = Some((vec![(4, field.clone())], Vec::new()));
        ed.terrains.insert(e, crate::terrain_edit::EditorTerrain::new(field));
        let stats = ed.terrain_stats();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].id, 4);
        assert_eq!(stats[0].field, own.data as u64, "Play's snapshot shares the field's chunks and was counted again");
        assert_eq!(stats[0].data_chunks, own.data_chunks as u64);
        assert!(stats[0].shadow > 0, "the shadow volume was not counted");
    }
}
