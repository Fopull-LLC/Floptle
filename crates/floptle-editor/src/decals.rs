//! The editor half of `decals.*`: one mesh per picture, rebuilt when the
//! script host says that picture's decals changed, and drawn through the
//! raster like any other surface, so a mark takes the sun, the shadows and
//! the fog of the ground it lies on.
//!
//! The laying happens in the script host (`floptle_script::decal_api`), which
//! has the colliders; this side only packs what it laid into GPU buffers.

use std::collections::{BTreeMap, HashSet};

use floptle_core::math::{DVec3, Mat4, Quat};
use floptle_core::transform::Transform;
use floptle_script::decal_api::{Decal, DecalFrame, DecalStore};
use floptle_render::{MaterialParams, MeshData, MeshId, TexId, Vertex};

/// Instance alpha for a decal batch: just under opaque, so the batch is drawn
/// blended, after the surfaces it lies on, and each mark's own alpha (its
/// vertex colour's) shows.
const BATCH_ALPHA: f32 = 0.99;

/// One picture's decals on one frame, as one mesh.
pub(crate) struct DecalBatch {
    mesh: MeshId,
    tex: TexId,
    /// The mesh's vertices are relative to this, so a mark far from the world
    /// origin keeps its precision: in the world, or in the frame's own space
    /// when the batch rides one.
    anchor: DVec3,
}

/// A batch's key: the picture, and the node its marks ride (`None` for marks
/// that stay where they were laid).
type BatchKey = (String, Option<u32>);

#[derive(Default)]
pub(crate) struct DecalBatches {
    epoch: u64,
    batches: BTreeMap<BatchKey, DecalBatch>,
    /// Pictures that would not load, said once.
    warned: HashSet<String>,
    /// The level edit the marks were last laid against (`level_edits`).
    seen_edits: u64,
}

/// The frame a placed decal rides, and the frame's pose when it was laid.
fn ridden(d: &Decal) -> Option<(u32, Transform)> {
    match d.frame {
        DecalFrame::On(f, at) => Some((f, at)),
        _ => None,
    }
}

/// Pack every decal showing one picture on one frame into a mesh, relative to
/// the first one's centre, in the frame's own space when it has one. `None`
/// when there are none.
pub(crate) fn pack(store: &DecalStore, texture: &str, frame: Option<u32>) -> Option<(DVec3, MeshData)> {
    // Each mark's centre and turn in the space the batch is kept in.
    let placed = |d: &Decal| -> (DVec3, Quat) {
        match ridden(d) {
            Some((_, at)) => {
                let local = at.inv_mul(&Transform::from_translation(d.pos));
                (local.translation, at.rotation.inverse())
            }
            None => (d.pos, Quat::IDENTITY),
        }
    };
    let mut decals = store.with_texture(texture).filter(|d| ridden(d).map(|(f, _)| f) == frame).peekable();
    let anchor = placed(decals.peek()?).0;
    let mut data = MeshData { vertices: Vec::new(), indices: Vec::new(), colors: Some(Vec::new()) };
    let colors = data.colors.as_mut().expect("colors");
    for d in decals {
        let (at, turn) = placed(d);
        let off = (at - anchor).as_vec3();
        let base = data.vertices.len() as u32;
        let byte = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
        for v in &d.verts {
            data.vertices.push(Vertex {
                pos: (off + turn * v.pos).to_array(),
                normal: (turn * v.normal).to_array(),
                uv: v.uv,
            });
            colors.push([byte(d.color[0]), byte(d.color[1]), byte(d.color[2]), byte(d.alpha * v.fade)]);
        }
        data.indices.extend(d.indices.iter().map(|i| base + i));
    }
    Some((anchor, data))
}

impl crate::Editor {
    /// Settle what each decal laid in the script pass just run rides: the node
    /// `decals.add` named, or the moving world its centre is in, as that node
    /// stands now, which is how it stood when the mark was laid.
    pub(crate) fn place_script_decals(&mut self) {
        let store = self.script_host.decals().clone();
        let unplaced = store.borrow().unplaced();
        if unplaced.is_empty() {
            return;
        }
        let spheres = self.moving_world_spheres();
        let mut store = store.borrow_mut();
        for (id, asked, pos) in unplaced {
            let entity = asked
                .and_then(|f| self.world.entity_with::<Transform>(f))
                .or_else(|| crate::play_step::moving_world_at(&spheres, pos));
            let frame = match entity {
                Some(e) => DecalFrame::On(e.index(), floptle_core::world_transform(&self.world, e)),
                None => DecalFrame::World,
            };
            store.place(id, frame);
        }
    }

    /// Rebuild the batch of every picture whose decals changed since the last
    /// frame. Once a frame, after the scripts; nothing to do without a GPU.
    pub(crate) fn sync_decals(&mut self) {
        self.relay_decals_on_edits();
        let store = self.script_host.decals().clone();
        let mut store = store.borrow_mut();
        let mut dirty = store.take_dirty();
        if store.epoch() != self.decal_batches.epoch {
            // The store was emptied wholesale: every batch goes.
            self.decal_batches.epoch = store.epoch();
            dirty.extend(self.decal_batches.batches.keys().map(|(t, _)| t.clone()));
            dirty.sort();
            dirty.dedup();
        }
        if dirty.is_empty() || self.gpu.is_none() || self.raster.is_none() {
            return;
        }
        for texture in dirty {
            // Every frame this picture is on now, and every one it had a batch on.
            let mut frames: Vec<Option<u32>> =
                store.with_texture(&texture).map(|d| ridden(d).map(|(f, _)| f)).collect();
            frames.extend(self.decal_batches.batches.keys().filter(|(t, _)| *t == texture).map(|(_, f)| *f));
            frames.sort();
            frames.dedup();
            for frame in frames {
                self.rebuild_decal_batch(&store, &texture, frame);
            }
        }
    }

    /// Lay again every mark whose ground was dug or re-meshed since the last
    /// frame, on the surface there now; a mark with nothing left under it
    /// goes. Read off the level's own edit log, so a dig across the planet
    /// touches none of the marks here, and a mark costs nothing while its
    /// ground holds still.
    fn relay_decals_on_edits(&mut self) {
        let Some(sim) = self.sim.as_ref() else { return };
        let edits = sim.world.level_edits;
        if edits == self.decal_batches.seen_edits {
            return;
        }
        let seen = std::mem::replace(&mut self.decal_batches.seen_edits, edits);
        // Where the ground changed: each edit's bounding sphere, or anywhere
        // when one could be anywhere or the log no longer reaches back.
        let log = &sim.world.level_edit_log;
        let fell_behind = log.front().is_none_or(|(first, _)| *first > seen + 1);
        let mut regions = Vec::new();
        let mut anywhere = fell_behind;
        for (_, r) in log.iter().filter(|(seq, _)| *seq > seen) {
            match r {
                Some(r) => regions.push(*r),
                None => anywhere = true,
            }
        }
        let store = self.script_host.decals().clone();
        let mut store = store.borrow_mut();
        for id in store.ids() {
            let Some(d) = store.get(id) else { continue };
            // Where the mark is now, and which way it faces: a mark on a
            // moving world has been carried with it since it was laid.
            let (pos, turn, frame) = match d.frame {
                DecalFrame::On(f, at) => match self.world.entity_with::<Transform>(f) {
                    Some(e) => {
                        let now = floptle_core::world_transform(&self.world, e);
                        let local = at.inv_mul(&Transform::from_translation(d.pos)).translation;
                        let pos = now.mul_transform(&Transform::from_translation(local)).translation;
                        (pos, now.rotation * at.rotation.inverse(), DecalFrame::On(f, now))
                    }
                    None => continue,
                },
                DecalFrame::World => (d.pos, Quat::IDENTITY, DecalFrame::World),
                // Not placed yet: it was laid this very frame, on today's ground.
                DecalFrame::Asked(_) => continue,
            };
            let reach = (floptle_core::math::Vec3::new(d.spec.width, d.spec.height, d.spec.depth).length() * 0.5
                + d.spec.depth) as f64;
            if !anywhere && !regions.iter().any(|(c, r)| (pos - *c).length() < r + reach) {
                continue;
            }
            let mut spec = d.spec.clone();
            spec.center = (pos - sim.world.origin).as_vec3();
            spec.normal = turn * spec.normal;
            spec.up = turn * spec.up;
            let laid = floptle_script::decal_api::lay(&sim.world.colliders, &spec);
            store.relaid(id, pos, frame, spec, laid);
        }
    }

    fn rebuild_decal_batch(&mut self, store: &DecalStore, texture: &str, frame: Option<u32>) {
        let key = (texture.to_string(), frame);
        let packed = pack(store, texture, frame);
        let tex = packed.as_ref().and_then(|_| self.ensure_texture(texture));
        if packed.is_some() && tex.is_none() && self.decal_batches.warned.insert(texture.to_string()) {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!("decals: the picture \"{texture}\" will not load, so its marks are not drawn"),
                None,
            );
        }
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else { return };
        let (Some((anchor, data)), Some(tex)) = (packed, tex) else {
            if let Some(old) = self.decal_batches.batches.remove(&key) {
                raster.free_dynamic(old.mesh);
            }
            return;
        };
        let mesh = match self.decal_batches.batches.get(&key) {
            Some(b) if raster.replace_dynamic(gpu, b.mesh, &data) => b.mesh,
            _ => {
                if let Some(old) = self.decal_batches.batches.remove(&key) {
                    raster.free_dynamic(old.mesh);
                }
                let id = raster.register_dynamic(gpu, data.vertices.len() as u32, data.indices.len() as u32, true);
                raster.replace_dynamic(gpu, id, &data);
                id
            }
        };
        self.decal_batches.batches.insert(key, DecalBatch { mesh, tex, anchor });
    }
}

/// One instance per batch, camera-relative: a batch that rides a frame is
/// drawn where the frame is this frame. A free function for the render
/// loop's borrow split, like `push_terrain_instances`.
pub(crate) fn push_decal_instances(
    batches: &DecalBatches,
    world: &floptle_core::World,
    raster: &floptle_render::Raster,
    cam_world: DVec3,
    instances: &mut Vec<(MeshId, Option<TexId>, floptle_render::InstanceRaw)>,
) {
    for ((_, frame), b) in &batches.batches {
        let model = match frame.and_then(|f| world.entity_with::<Transform>(f)) {
            Some(e) => floptle_core::world_transform(world, e)
                .mul_transform(&Transform::from_translation(b.anchor))
                .render_matrix(cam_world),
            None => Mat4::from_translation((b.anchor - cam_world).as_vec3()),
        };
        let mut mp = MaterialParams::flat([1.0; 3]);
        mp.alpha = BATCH_ALPHA;
        mp.terrain_paint_base = raster.dyn_paint_base(b.mesh);
        instances.push((b.mesh, Some(b.tex), floptle_render::instance_of_mat(model, &mp)));
    }
}

#[cfg(test)]
mod tests {
    use floptle_core::math::{DVec3, Vec3};

    /// **A mark stays on the ground when the ground is dug.** A decal was laid
    /// once, on the surface under it then, and a dig left it floating where
    /// the ground had been (a laser burn 1.84 m up). Now an edit of the
    /// ground under it lays it again on the surface there, once the dig's
    /// collision lands.
    #[test]
    fn a_decal_follows_the_ground_dug_from_under_it() {
        use floptle_field::{Brush, BrushProfile};
        let dir = std::env::temp_dir().join(format!("floptle-decal-dig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
        floptle_vfs::write(
            dir.join("scripts/mark.lua"),
            "local done = false\nfunction update(node, dt)\n  if not done then\n    done = true\n    \
             MARK = decals.add{ texture = 'scorch.png', pos = vec3(1, 0, 1), normal = vec3(0, 1, 0), size = 1, depth = 4 }\n  \
             end\nend\n",
        )
        .unwrap();
        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        let mut field = floptle_field::ChunkField::new(0.5);
        field.fill_slab(Vec3::new(-12.0, -6.0, -12.0), Vec3::new(12.0, 0.0, 12.0), 0.0, [0.5, 0.5, 0.5]);
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.terrains.insert(e, crate::terrain_edit::EditorTerrain::new(field));
        let host = ed.world.spawn();
        ed.world.insert(host, floptle_core::Transform::IDENTITY);
        ed.world.insert(
            host,
            floptle_core::Scripts(vec![floptle_core::ScriptInst {
                kind: "mark".into(),
                enabled: true,
                params: Vec::new(),
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
        ed.toggle_play();
        assert!(ed.playing && ed.sim.is_some());
        ed.play_step(1.0 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        let height = |ed: &crate::Editor| {
            let store = ed.script_host.decals().clone();
            let store = store.borrow();
            let id = *store.ids().first().expect("the mark was laid");
            let d = store.get(id).unwrap();
            // The middle of the mark: its vertices nearest its centre.
            let near: Vec<f64> =
                d.verts.iter().filter(|v| v.pos.x.abs() < 0.3 && v.pos.z.abs() < 0.3).map(|v| d.pos.y + v.pos.y as f64).collect();
            near.iter().sum::<f64>() / near.len().max(1) as f64
        };
        let before = height(&ed);
        assert!(before.abs() < 0.1, "the mark is not on the slab: {before}");

        // Dig 2 m out from under it, through the path `terrain.dig` takes.
        let touched = {
            let t = ed.terrains.get_mut(&e).unwrap();
            t.field.sculpt(Brush::Lower, Vec3::new(1.0, 0.0, 1.0), 2.0, 1.0, BrushProfile { hardness: 1.0, ..Default::default() })
        };
        ed.mirror_terrain_chunks_to_sim(e, &touched);
        for _ in 0..4 {
            ed.pump_collision_meshes();
        }
        ed.sync_decals();
        let ground = {
            let sim = ed.sim.as_ref().unwrap();
            let o = (DVec3::new(1.0, 5.0, 1.0) - sim.world.origin).as_vec3();
            let hit = sim.world.raycast(o, Vec3::NEG_Y, 20.0).expect("ground under the mark");
            5.0 - hit.distance as f64
        };
        let after = height(&ed);
        assert!(ground < before - 1.0, "the dig did not reach the ground under the mark: {ground}");
        let lift = f64::from(floptle_script::decal_api::LIFT);
        assert!((after - ground - lift).abs() < 0.02, "the mark is at {after}, the ground at {ground}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
