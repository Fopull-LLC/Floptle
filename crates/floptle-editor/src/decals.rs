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

/// A batch's key: the picture, the node its marks ride (`None` for marks
/// that stay where they were laid), and the page: the decal's id over
/// [`PAGE`]. A mark that changes rebuilds its own page, not every mark
/// sharing its picture.
type BatchKey = (String, Option<u32>, u32);

/// Marks per batch page. Small enough that rebuilding the page a dug mark is
/// in costs a handful of marks; big enough that a meadow of marks is a few
/// dozen draws, not hundreds.
const PAGE: u32 = 16;

#[derive(Default)]
pub(crate) struct DecalBatches {
    epoch: u64,
    batches: BTreeMap<BatchKey, DecalBatch>,
    /// Pictures that would not load, said once.
    warned: HashSet<String>,
    /// The level edit the marks were last laid against (`level_edits`).
    seen_edits: u64,
    /// Marks whose ground may have moved, waiting their turn to be checked
    /// and laid again, oldest first; and the same as a set.
    pending: std::collections::VecDeque<u32>,
    pending_set: HashSet<u32>,
    /// Marks laid again since Play started, for the guard that a dig lays
    /// only the marks it reached.
    relaid: u64,
    /// The page each mark was last packed into, so a mark that moves to
    /// another frame (or goes) rebuilds the page it left.
    keyed: std::collections::HashMap<u32, BatchKey>,
    /// Marks packed into batches since Play started, for the guard that a
    /// change repacks its page and not the whole picture.
    repacked: u64,
}

/// How long a frame may spend checking and laying marks again after digs.
const RELAY_BUDGET_MS: f64 = 1.5;

/// Does the ground under mark `d` still lie where the mark does? Rays down
/// the mark's facing through a handful of its own vertices, each of which was
/// laid on the surface: if every one still meets the surface there, the dig
/// did not reach this mark and laying it again would change nothing.
fn ground_holds(
    cols: &[floptle_physics::AnchoredCollider],
    origin: DVec3,
    d: &floptle_script::decal_api::Decal,
    pos: DVec3,
    turn: Quat,
) -> bool {
    const SAMPLES: usize = 9;
    const TOLERANCE: f32 = 0.01;
    let n = (turn * d.spec.normal).normalize_or_zero();
    let depth = d.spec.depth.max(0.05);
    let step = (d.verts.len() / SAMPLES).max(1);
    let lift = floptle_script::decal_api::LIFT;
    d.verts.iter().step_by(step).all(|v| {
        let at = (pos - origin).as_vec3() + turn * v.pos - n * lift;
        let from = at + n * depth;
        match floptle_physics::raycast_colliders(cols, from, -n, depth * 2.0, d.spec.mask) {
            Some(hit) => (hit.distance - depth).abs() < TOLERANCE,
            None => false,
        }
    })
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
#[cfg(test)]
pub(crate) fn pack(store: &DecalStore, texture: &str, frame: Option<u32>) -> Option<(DVec3, MeshData)> {
    pack_decals(store.with_texture(texture).filter(|d| ridden(d).map(|(f, _)| f) == frame))
}

/// Pack these decals (one picture, one frame) into a mesh, relative to the
/// first one's centre, in the frame's own space when it has one. `None` when
/// there are none.
pub(crate) fn pack_decals<'a>(decals: impl Iterator<Item = &'a Decal>) -> Option<(DVec3, MeshData)> {
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
    let mut decals = decals.peekable();
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

    /// Rebuild the batch pages whose decals changed since the last frame:
    /// the page each changed mark is in now, and the one it was in before.
    /// Once a frame, after the scripts; nothing to do without a GPU. Timed as
    /// the `decalBatches` part of the terrain bucket.
    pub(crate) fn sync_decals(&mut self) {
        self.lay_waiting_decals();
        self.relay_decals_on_edits();
        let t0 = floptle_core::time::Instant::now();
        let store = self.script_host.decals().clone();
        let mut store = store.borrow_mut();
        let mut dirty = store.take_dirty();
        if store.epoch() != self.decal_batches.epoch {
            // The store was emptied wholesale: every batch goes, and every
            // mark it holds now is packed afresh.
            self.decal_batches.epoch = store.epoch();
            if let Some(raster) = self.raster.as_mut() {
                for b in std::mem::take(&mut self.decal_batches.batches).into_values() {
                    raster.free_dynamic(b.mesh);
                }
            }
            self.decal_batches.batches.clear();
            self.decal_batches.keyed.clear();
            dirty = store.ids();
        }
        if dirty.is_empty() || self.gpu.is_none() || self.raster.is_none() {
            return;
        }
        let mut pages: std::collections::BTreeSet<BatchKey> = std::collections::BTreeSet::new();
        for id in dirty {
            if let Some(old) = self.decal_batches.keyed.remove(&id) {
                pages.insert(old);
            }
            if let Some(d) = store.get(id) {
                pages.insert((d.texture.clone(), ridden(d).map(|(f, _)| f), id / PAGE));
            }
        }
        for key in pages {
            self.rebuild_decal_page(&store, key);
        }
        drop(store);
        let ms = t0.elapsed().as_secs_f64() as f32 * 1000.0;
        self.profile_record(floptle_core::profile::Bucket::Terrain, ms);
        self.script_host.profile().borrow_mut().record_part("decalBatches", ms);
    }

    /// Carry on laying the marks `decals.add` could not finish in its frame's
    /// budget, oldest first, a piece at a time in what is left of this
    /// frame's budget; then the next frame's budget starts whole. Each piece
    /// is gathered where the mark is now, on whatever it rides as that stands
    /// now. Timed as the `decalLay` part.
    fn lay_waiting_decals(&mut self) {
        let store_rc = self.script_host.decals().clone();
        let Some(sim) = self.sim.as_ref() else {
            store_rc.borrow_mut().new_frame();
            return;
        };
        let waiting = store_rc.borrow().waiting();
        let t0 = floptle_core::time::Instant::now();
        let mut worked = false;
        'marks: for id in waiting {
            // Not placed yet (asked for in the pass just run, and placed
            // after it): its turn comes next frame.
            let Some((pos, turn, _)) = store_rc.borrow().get(id).and_then(|d| self.decal_place_now(d)) else { continue };
            let center = (pos - sim.world.origin).as_vec3();
            loop {
                let mut store = store_rc.borrow_mut();
                if store.budget_left_ms() <= 0.0 {
                    break 'marks;
                }
                worked = true;
                match store.step_job(id, &sim.world.colliders, center, turn) {
                    Some(false) => continue,
                    _ => break,
                }
            }
        }
        store_rc.borrow_mut().new_frame();
        if worked {
            let ms = t0.elapsed().as_secs_f64() as f32 * 1000.0;
            self.profile_record(floptle_core::profile::Bucket::Terrain, ms);
            self.script_host.profile().borrow_mut().record_part("decalLay", ms);
        }
    }

    /// Lay again every mark whose ground was dug or re-meshed, on the surface
    /// there now; a mark with nothing left under it goes.
    ///
    /// The level's edit log names whole collision chunks, tens of metres
    /// across, so it only says which marks *might* have moved. Each of those
    /// is asked first whether the ground under its own corners still lies
    /// where the mark does, a few rays, and only one whose ground moved is laid
    /// again. The asking and laying take at most [`RELAY_BUDGET_MS`] a frame;
    /// what is left waits for the next frame. Laying every mark in a chunk on
    /// every bite of a mining laser cost up to a second a frame.
    fn relay_decals_on_edits(&mut self) {
        let Some(sim) = self.sim.as_ref() else { return };
        let t0 = floptle_core::time::Instant::now();
        let edits = sim.world.level_edits;
        let store_rc = self.script_host.decals().clone();
        if edits != self.decal_batches.seen_edits {
            let seen = std::mem::replace(&mut self.decal_batches.seen_edits, edits);
            // Where the ground changed: each edit's bounding sphere, or anywhere
            // when one could be anywhere or the log no longer reaches back.
            let log = &sim.world.level_edit_log;
            let mut anywhere = log.front().is_none_or(|(first, _)| *first > seen + 1);
            let mut regions = Vec::new();
            for (_, r) in log.iter().filter(|(seq, _)| *seq > seen) {
                match r {
                    Some(r) => regions.push(*r),
                    None => anywhere = true,
                }
            }
            let store = store_rc.borrow();
            for id in store.ids() {
                let Some(d) = store.get(id) else { continue };
                if self.decal_batches.pending_set.contains(&id) {
                    continue;
                }
                let reach = (floptle_core::math::Vec3::new(d.spec.width, d.spec.height, d.spec.depth).length() * 0.5
                    + d.spec.depth) as f64;
                let near = anywhere
                    || match self.decal_place_now(d) {
                        Some((pos, _, _)) => regions.iter().any(|(c, r)| (pos - *c).length() < r + reach),
                        None => false,
                    };
                if near {
                    self.decal_batches.pending_set.insert(id);
                    self.decal_batches.pending.push_back(id);
                }
            }
        }
        if self.decal_batches.pending.is_empty() {
            return;
        }
        let mut store = store_rc.borrow_mut();
        let undrawn = store.undrawn().clone();
        loop {
            if t0.elapsed().as_secs_f64() * 1000.0 > RELAY_BUDGET_MS {
                break;
            }
            let Some(id) = self.decal_batches.pending.pop_front() else { break };
            self.decal_batches.pending_set.remove(&id);
            let Some(d) = store.get(id) else { continue };
            let Some((pos, turn, frame)) = self.decal_place_now(d) else { continue };
            if ground_holds(&sim.world.colliders, sim.world.origin, d, pos, turn) {
                continue;
            }
            let mut spec = d.spec.clone();
            spec.center = (pos - sim.world.origin).as_vec3();
            spec.normal = turn * spec.normal;
            spec.up = turn * spec.up;
            let laid = floptle_script::decal_api::lay_on(&sim.world.colliders, &spec, &undrawn);
            store.relaid(id, pos, frame, spec, laid);
            self.decal_batches.relaid += 1;
        }
        drop(store);
        let ms = t0.elapsed().as_secs_f64() as f32 * 1000.0;
        self.profile_record(floptle_core::profile::Bucket::Terrain, ms);
        self.script_host.profile().borrow_mut().record_part("decalRelay", ms);
    }

    /// Where a placed mark is now, which way it has turned since it was laid,
    /// and what it rides: a mark on a moving world has been carried with it.
    /// `None` for one not placed yet (it was laid this frame, on today's
    /// ground) or riding a node that has gone.
    fn decal_place_now(&self, d: &floptle_script::decal_api::Decal) -> Option<(DVec3, Quat, DecalFrame)> {
        match d.frame {
            DecalFrame::On(f, at) => {
                let e = self.world.entity_with::<Transform>(f)?;
                let now = floptle_core::world_transform(&self.world, e);
                let local = at.inv_mul(&Transform::from_translation(d.pos)).translation;
                let pos = now.mul_transform(&Transform::from_translation(local)).translation;
                Some((pos, now.rotation * at.rotation.inverse(), DecalFrame::On(f, now)))
            }
            DecalFrame::World => Some((d.pos, Quat::IDENTITY, DecalFrame::World)),
            DecalFrame::Asked(_) => None,
        }
    }

    /// Pack one page again: the marks with ids in it that show its picture on
    /// its frame.
    fn rebuild_decal_page(&mut self, store: &DecalStore, key: BatchKey) {
        let (texture, frame, page) = (key.0.as_str(), key.1, key.2);
        let members: Vec<(u32, &Decal)> = store
            .in_range(page * PAGE, page.saturating_add(1).saturating_mul(PAGE))
            .filter(|(_, d)| d.texture == texture && ridden(d).map(|(f, _)| f) == frame)
            .collect();
        for (id, _) in &members {
            self.decal_batches.keyed.insert(*id, key.clone());
        }
        self.decal_batches.repacked += members.len() as u64;
        let packed = pack_decals(members.iter().map(|(_, d)| *d));
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
    for ((_, frame, _), b) in &batches.batches {
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

    /// A project with `script` on one node and a terrain slab 24 m across,
    /// its top at 0, in Play.
    fn on_a_slab(tag: &str, script: &str, half: f32) -> (crate::Editor, floptle_core::Entity, std::path::PathBuf) {
        on_a_slab_in(crate::Editor::default(), tag, script, half)
    }

    /// [`on_a_slab`], starting from `ed` (one with a GPU, say).
    fn on_a_slab_in(
        mut ed: crate::Editor,
        tag: &str,
        script: &str,
        half: f32,
    ) -> (crate::Editor, floptle_core::Entity, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("floptle-decal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
        floptle_vfs::write(dir.join("scripts/marks.lua"), script).unwrap();
        ed.project_root = dir.clone();
        let mut field = floptle_field::ChunkField::new(0.5);
        field.fill_slab(Vec3::new(-half, -6.0, -half), Vec3::new(half, 0.0, half), 0.0, [0.5, 0.5, 0.5]);
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.terrains.insert(e, crate::terrain_edit::EditorTerrain::new(field));
        let host = ed.world.spawn();
        ed.world.insert(host, floptle_core::Transform::IDENTITY);
        ed.world.insert(host, floptle_core::Matter::Sprite { ppu: 32.0, size: 1.0, cell: 0, flip_x: false, flip_y: false, pivot: [0.5, 0.5] });
        ed.world.insert(
            host,
            floptle_core::Scripts(vec![floptle_core::ScriptInst {
                kind: "marks".into(),
                enabled: true,
                params: Vec::new(),
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
        (ed, e, dir)
    }

    /// **A dig lays again only the marks it reached.** 150 turf marks around
    /// a point and one bite 2.4 m across in the middle: the edit log names the
    /// whole collision chunk, tens of metres, and every mark in it was laid
    /// again, up to a second a frame on a mining laser. Now each is first asked
    /// whether its own ground moved; only the few under the bite are laid
    /// again, and the asking is spread over frames by a budget rather than
    /// done all at once.
    #[test]
    fn a_dig_lays_again_only_the_marks_it_reached() {
        use floptle_field::{Brush, BrushProfile};
        let script = "local done = false\nfunction update(node, dt)\n  if done then return end\n  done = true\n  \
                      for i = 0, 149 do\n    local a = i * 2.39996\n    local r = 2 + 27 * (i / 149)\n    \
                      decals.add{ texture = 'turf.png', pos = vec3(r * math.cos(a), 0, r * math.sin(a)), \
                      normal = vec3(0, 1, 0), size = 1.5, depth = 4 }\n  end\nend\n";
        let (mut ed, e, dir) = on_a_slab("budget", script, 36.0);
        ed.toggle_play();
        ed.play_step(1.0 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        assert_eq!(ed.script_host.decals().borrow().len(), 150, "the marks were not all laid");
        // A bite at the first mark's spot, through the path `terrain.dig` takes.
        let touched = {
            let t = ed.terrains.get_mut(&e).unwrap();
            t.field.sculpt(Brush::Lower, Vec3::new(2.0, 0.0, 0.0), 2.4, 1.0, BrushProfile { hardness: 1.0, ..Default::default() })
        };
        ed.mirror_terrain_chunks_to_sim(e, &touched);
        for _ in 0..4 {
            ed.pump_collision_meshes();
        }
        let mut frames = 0;
        loop {
            ed.sync_decals();
            frames += 1;
            if ed.decal_batches.pending.is_empty() || frames > 600 {
                break;
            }
        }
        assert!(ed.decal_batches.pending.is_empty(), "the marks were still waiting after {frames} frames");
        // Marks whose box reaches the bite: within its radius plus a mark's
        // half-diagonal of it.
        let reached = (0..150)
            .filter(|i| {
                let a = *i as f64 * 2.39996;
                let r = 2.0 + 27.0 * (*i as f64 / 149.0);
                let (x, z) = (r * a.cos(), r * a.sin());
                ((x - 2.0).powi(2) + z * z).sqrt() < 2.4 + 1.6
            })
            .count() as u64;
        let relaid = ed.decal_batches.relaid;
        assert!(relaid >= 1, "the mark under the bite was not laid again");
        assert!(relaid <= reached, "{relaid} marks laid again for a bite that reaches {reached} of 150");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A dig repacks the pages of the marks it reached, not the meadow.**
    /// Every mark sharing a picture was one mesh, rebuilt whole when any one
    /// of them changed: a dig among 150 marks repacked all 150 and uploaded
    /// them again, the 250 ms Solar's meadow digs spent outside every timer.
    /// Now a picture's marks are pages of 16, and only the pages holding a
    /// mark laid again are packed.
    #[test]
    fn a_dig_repacks_only_the_pages_of_the_marks_it_reached() {
        use floptle_field::{Brush, BrushProfile};
        let Some(gpu_ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let script = "local done = false\nfunction update(node, dt)\n  if done then return end\n  done = true\n  \
                      for i = 0, 149 do\n    local a = i * 2.39996\n    local r = 2 + 27 * (i / 149)\n    \
                      decals.add{ texture = 'turf.png', pos = vec3(r * math.cos(a), 0, r * math.sin(a)), \
                      normal = vec3(0, 1, 0), size = 1.5, depth = 4 }\n  end\nend\n";
        let (mut ed, e, dir) = on_a_slab_in(gpu_ed, "pages", script, 36.0);
        image::RgbaImage::from_pixel(4, 4, image::Rgba([90, 140, 60, 255])).save(dir.join("turf.png")).unwrap();
        ed.toggle_play();
        // Marks past a frame's laying budget land over the next frames; a
        // loaded machine lays fewer a frame, so wait for the last of them
        // rather than for a count of frames.
        for _ in 0..5000 {
            ed.play_step(1.0 / 60.0, true);
            if ed.script_host.decals().borrow().waiting().is_empty() {
                break;
            }
        }
        assert!(ed.script_host.decals().borrow().waiting().is_empty(), "the marks never all landed");
        ed.play_step(1.0 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        assert_eq!(ed.script_host.decals().borrow().len(), 150, "the marks were not all laid");
        let pages = ed.decal_batches.batches.len();
        assert!((10..=12).contains(&pages), "150 marks make {pages} pages, not about 150 / 16");
        let before = ed.decal_batches.repacked;
        assert!(before >= 150, "the first frames packed only {before} marks");
        let touched = {
            let t = ed.terrains.get_mut(&e).unwrap();
            t.field.sculpt(Brush::Lower, Vec3::new(2.0, 0.0, 0.0), 2.4, 1.0, BrushProfile { hardness: 1.0, ..Default::default() })
        };
        ed.mirror_terrain_chunks_to_sim(e, &touched);
        for _ in 0..4 {
            ed.pump_collision_meshes();
        }
        for _ in 0..600 {
            ed.sync_decals();
            if ed.decal_batches.pending.is_empty() {
                break;
            }
        }
        ed.sync_decals();
        let relaid = ed.decal_batches.relaid;
        let repacked = ed.decal_batches.repacked - before;
        assert!(relaid >= 1, "the mark under the bite was not laid again");
        assert!(
            repacked <= relaid * u64::from(super::PAGE),
            "{relaid} marks laid again repacked {repacked} marks: more than their pages hold"
        );
        assert!(repacked < 75, "a dig among 150 marks repacked {repacked} of them");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Big marks asked for at once land over frames, a piece at a time,
    /// and the script can tell when.** Laying a 29 m mark over terrain took
    /// 30 to 100 ms, all of it inside the `decals.add` call: ten asked for at
    /// once were a second-long hitch. Now a mark is laid a piece of its box at
    /// a time while the frame's budget lasts, and the rest is carried over;
    /// `decals.landed(id)` is false until it is done. No frame does more than
    /// a small share of the whole job.
    #[test]
    fn big_marks_land_over_frames_within_the_budget() {
        let script = "ids = ids or {}\nlocal asked = false\nfunction update(node, dt)\n  if not asked then\n    asked = true\n    \
                      for i = 0, 9 do\n      ids[#ids + 1] = decals.add{ texture = 'scorch.png', pos = vec3(-27 + i * 6, 0, 0), \
                      normal = vec3(0, 1, 0), size = 29, depth = 4 }\n    end\n  end\n  local landed = 0\n  \
                      for _, id in ipairs(ids) do\n    if decals.landed(id) then landed = landed + 1 end\n  end\n  \
                      print('landed ' .. landed)\nend\n";
        let (mut ed, _e, dir) = on_a_slab("budgeted", script, 60.0);
        ed.toggle_play();
        let landed = |ed: &crate::Editor| {
            let store = ed.script_host.decals().clone();
            let store = store.borrow();
            store.ids().into_iter().filter(|id| store.get(*id).is_some_and(|d| !d.waiting)).count()
        };
        let lay_ms = |ed: &crate::Editor| ed.script_host.decals().borrow().last_frame_ms();
        ed.play_step(1.0 / 60.0, true);
        ed.drain_script_logs();
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        assert_eq!(ed.script_host.decals().borrow().len(), 10, "a mark was refused");
        let first = landed(&ed);
        assert!(first < 10, "all ten 29 m marks were laid in the frame that asked for them");
        let mut frames_ms = vec![lay_ms(&ed)];
        while landed(&ed) < 10 && frames_ms.len() < 2000 {
            ed.play_step(1.0 / 60.0, true);
            ed.drain_script_logs();
            frames_ms.push(lay_ms(&ed));
        }
        assert_eq!(landed(&ed), 10, "the marks were still waiting after {} frames", frames_ms.len());
        let total: f64 = frames_ms.iter().sum();
        let worst = frames_ms.iter().copied().fold(0.0, f64::max);
        assert!(
            worst < total / 5.0,
            "one frame spent {worst:.1} ms of the {total:.1} ms the ten marks took to lay, over {} frames",
            frames_ms.len()
        );
        assert!(ed.script_host.decals().borrow().get(1).is_some_and(|d| d.indices.len() > 300), "a landed mark is empty");
        ed.play_step(1.0 / 60.0, true);
        ed.drain_script_logs();
        let said: Vec<String> = ed.console.entries.iter().map(|e| e.msg.clone()).collect();
        assert!(said.iter().any(|m| m.contains("landed 10")), "decals.landed never said all ten had landed");
        assert!(said.iter().any(|m| m.contains(&format!("landed {first}"))), "the first frame's count");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A mark does not lie on a collider nothing draws.** An Empty node with
    /// a box collider is a wall nobody sees; a mark laid over it hung in the
    /// air as a pane. It lies on the drawn ground below instead.
    #[test]
    fn a_decal_lies_on_the_ground_under_an_undrawn_collider() {
        let script = "local done = false\nfunction update(node, dt)\n  if done then return end\n  done = true\n  \
                      decals.add{ texture = 'turf.png', pos = vec3(0, 2.5, 0), normal = vec3(0, 1, 0), size = 1, depth = 6 }\nend\n";
        let (mut ed, _, dir) = on_a_slab("undrawn", script, 12.0);
        let wall = ed.world.spawn();
        ed.world.insert(wall, floptle_core::Transform::from_translation(DVec3::new(0.0, 1.0, 0.0)));
        ed.world.insert(wall, floptle_core::Matter::Empty);
        ed.world.insert(
            wall,
            floptle_core::RigidBody {
                kind: floptle_core::BodyKind::Box,
                mode: floptle_core::BodyMode::Static,
                half_extents: [2.0, 0.5, 2.0],
                ..Default::default()
            },
        );
        ed.toggle_play();
        ed.play_step(1.0 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        let store = ed.script_host.decals().clone();
        let store = store.borrow();
        let id = *store.ids().first().expect("the mark was laid");
        let d = store.get(id).unwrap();
        let highest = d.verts.iter().map(|v| d.pos.y + v.pos.y as f64).fold(f64::MIN, f64::max);
        assert!(highest < 0.1, "the mark lies at {highest}, on the undrawn box's top at 1.5");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

