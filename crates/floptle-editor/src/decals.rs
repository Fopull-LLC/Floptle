//! The editor half of `decals.*`: one mesh per picture, rebuilt when the
//! script host says that picture's decals changed, and drawn through the
//! raster like any other surface, so a mark takes the sun, the shadows and
//! the fog of the ground it lies on.
//!
//! The laying happens in the script host (`floptle_script::decal_api`), which
//! has the colliders; this side only packs what it laid into GPU buffers.

use std::collections::{BTreeMap, HashSet};

use floptle_core::math::{DVec3, Mat4, Quat, Vec3};
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
    /// Marks asked whether their ground moved since Play started, for the
    /// guard that a dig asks only the marks over its own box.
    checked: u64,
    /// The page each mark was last packed into, so a mark that moves to
    /// another frame (or goes) rebuilds the page it left.
    keyed: std::collections::HashMap<u32, BatchKey>,
    /// Marks packed into batches since Play started, for the guard that a
    /// change repacks its page and not the whole picture.
    repacked: u64,
    /// Counts `sync_decals` calls: when each mark was last laid, and when each
    /// terrain chunk was last put away, are told apart by it.
    tick: u64,
    /// The tick each mark was last laid (or laid again) at.
    laid_at: std::collections::HashMap<u32, u64>,
    /// The tick after which a mark laid over each put-away chunk lay on its
    /// coarse stand-in, by (terrain node, chunk). A chunk with no entry has
    /// been a stand-in since before anything was laid.
    evicted_at: std::collections::HashMap<(floptle_core::Entity, [i32; 3]), u64>,
    /// Terrain chunks made again since the last frame: (terrain node, chunks).
    landed: Vec<(floptle_core::Entity, Vec<[i32; 3]>)>,
    /// Terrain writes since the last frame: (terrain node, the box written,
    /// in the terrain's own frame).
    dug: Vec<(floptle_core::Entity, Vec3, Vec3)>,
    /// Level edits (`level_edits` numbers) the terrain reported here itself,
    /// as `landed` or `dug`, so the edit log's copy of them is passed over.
    pub(crate) handled: std::collections::BTreeSet<u64>,
    /// Waiting marks known to have been laid on other ground than is there
    /// now: laid again without first asking whether their ground moved.
    forced: HashSet<u32>,
    /// The mark being laid again, a piece a frame within the budget; the old
    /// mark stays drawn until it is done.
    relaying: Option<Relay>,
    /// This frame's marks asked whether their ground moved, and marks laid
    /// again: `perf.counts().decalsChecked` / `decalsRelaid`.
    checked_frame: u32,
    relaid_frame: u32,
}

/// One mark being laid again: its job, and where it was and what it rode
/// when the job started, which is the frame the job lays it in.
struct Relay {
    id: u32,
    job: floptle_script::decal_api::LayJob,
    pos: DVec3,
    frame: DecalFrame,
    spec: floptle_script::decal_api::DecalSpec,
    /// How far the mark had turned since it was first laid, when the job started.
    turn: Quat,
}

/// Each terrain's world anchor, turn, scale and voxel size.
struct TerrainFrames(std::collections::HashMap<floptle_core::Entity, (DVec3, Quat, f32, f32)>);

impl TerrainFrames {
    /// A mark at `pos` (world) reaching `reach` metres, in terrain `e`'s own
    /// frame: its centre and reach there.
    fn local(&self, e: floptle_core::Entity, pos: DVec3, reach: f64) -> Option<(Vec3, f32)> {
        let &(anchor, rot, scale, _) = self.0.get(&e)?;
        Some(((rot.inverse() * (pos - anchor).as_vec3()) / scale, reach as f32 / scale))
    }

    /// Does the ball `local`, `r` across (terrain frame), reach the box `lo..hi`?
    fn ball_meets(local: Vec3, r: f32, lo: Vec3, hi: Vec3) -> bool {
        (lo - local).max(local - hi).max(Vec3::ZERO).length() <= r
    }

    /// Terrain `e`'s chunk `c`, in its own frame, grown by two voxels: a
    /// neighbour's surface along its border is read from it.
    fn chunk_box(&self, e: floptle_core::Entity, c: [i32; 3]) -> Option<(Vec3, Vec3)> {
        let v = self.0.get(&e)?.3;
        let side = floptle_field::CHUNK as f32 * v;
        let lo = Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32) * side;
        Some((lo - Vec3::splat(2.0 * v), lo + Vec3::splat(side + 2.0 * v)))
    }

    /// Does a mark at `pos` reaching `reach` lie over terrain `e`'s chunk `c`?
    fn meets_chunk(&self, e: floptle_core::Entity, c: [i32; 3], pos: DVec3, reach: f64) -> bool {
        match (self.local(e, pos, reach), self.chunk_box(e, c)) {
            (Some((p, r)), Some((lo, hi))) => Self::ball_meets(p, r, lo, hi),
            _ => false,
        }
    }
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

/// How far from its centre a mark's box reaches, folded faces included.
fn mark_reach(d: &Decal) -> f64 {
    (Vec3::new(d.spec.width, d.spec.height, d.spec.depth).length() * 0.5 + d.spec.depth) as f64
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
        self.decal_batches.tick += 1;
        self.lay_waiting_decals();
        self.relay_decals_on_edits();
        let (checked, relaid) = (self.decal_batches.checked_frame, self.decal_batches.relaid_frame);
        self.script_host.profile().borrow_mut().set_decal_relay_counts(checked, relaid);
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
        // When each mark changed was laid: a chunk made again lays again only
        // the marks laid while it was a stand-in.
        for &id in &dirty {
            match store.get(id) {
                Some(d) if !d.waiting => {
                    self.decal_batches.laid_at.insert(id, self.decal_batches.tick);
                }
                Some(_) => {}
                None => {
                    self.decal_batches.laid_at.remove(&id);
                }
            }
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

    /// Tell the marks that terrain `e`'s chunks `coords` were put away: each
    /// becomes a coarse stand-in, and a mark laid over one from now on lies on
    /// that. Nothing is laid again: the ground a mark already lies on comes
    /// back exactly as it was. Call after the sim's copy is told, as
    /// [`Self::decals_note_terrain_edit`] for the edit the sim recorded.
    pub(crate) fn decals_note_evicted(&mut self, e: floptle_core::Entity, coords: &[[i32; 3]]) {
        let after = self.decal_batches.tick + 1;
        for &c in coords {
            self.decal_batches.evicted_at.insert((e, c), after);
        }
        self.decals_note_terrain_edit();
    }

    /// Tell the marks that terrain `e`'s chunks `coords` were made again,
    /// exactly as generated. Only a mark laid while one of them was a
    /// stand-in is laid again.
    pub(crate) fn decals_note_landed(&mut self, e: floptle_core::Entity, coords: Vec<[i32; 3]>) {
        if !coords.is_empty() {
            self.decal_batches.landed.push((e, coords));
        }
        self.decals_note_terrain_edit();
    }

    /// Tell the marks that terrain `e` was written over `lo..hi`, in its own
    /// frame: the marks over that box are asked whether their ground moved,
    /// once the collision meshes there have landed.
    pub(crate) fn decals_note_dug(&mut self, e: floptle_core::Entity, lo: Vec3, hi: Vec3) {
        self.decal_batches.dug.push((e, lo, hi));
        self.decals_note_terrain_edit();
    }

    /// The level edit the sim recorded last was a terrain change these marks
    /// are told of directly, by chunk or box: their reading of the edit log
    /// passes over it.
    pub(crate) fn decals_note_terrain_edit(&mut self) {
        if let Some(sim) = self.sim.as_ref() {
            self.decal_batches.handled.insert(sim.world.level_edits);
        }
    }

    /// Where each terrain stands now, for asking which marks lie over its
    /// chunks: read once a frame, not once a mark.
    fn terrain_frames(&self) -> TerrainFrames {
        TerrainFrames(
            self.terrains
                .iter()
                .map(|(&e, t)| {
                    let (anchor, rot, scale) = self.terrain_world_frame_of(e);
                    (e, (anchor, rot, scale, t.field.voxel()))
                })
                .collect(),
        )
    }

    /// Is a collision mesh under this mark still being rebuilt? Its ground
    /// is asked about once the new triangles stand.
    fn mark_awaits_collision(&self, frames: &TerrainFrames, pos: DVec3, reach: f64) -> bool {
        self.collision_pending.keys().any(|&(e, c)| frames.meets_chunk(e, c, pos, reach))
    }

    /// Is ground under this mark being made again on a worker? A 29 m mark
    /// lies over several chunks, which arrive a few at a time: laid again
    /// once they are all in, it is laid again once, not once a chunk.
    fn mark_awaits_ground(&self, frames: &TerrainFrames, pos: DVec3, reach: f64) -> bool {
        self.chunks_being_made().any(|(e, c)| frames.meets_chunk(e, c, pos, reach))
    }

    /// Queue mark `id` to be laid again; `forced` when its ground is known
    /// to have changed, so it is laid again without asking.
    fn queue_relay(&mut self, id: u32, forced: bool) {
        if forced {
            self.decal_batches.forced.insert(id);
        }
        if self.decal_batches.relaying.as_ref().is_some_and(|r| r.id == id) {
            // Laid partly on the ground before: start again.
            self.decal_batches.relaying = None;
            self.decal_batches.forced.insert(id);
        }
        if self.decal_batches.pending_set.insert(id) {
            self.decal_batches.pending.push_back(id);
        }
    }

    /// Lay again every mark whose ground was dug, re-meshed or made exactly,
    /// on the surface there now; a mark with nothing left under it goes.
    ///
    /// The terrain says what it changed: a dig names the box it wrote, and a
    /// chunk made again names itself. A mark over a dig is asked first whether
    /// the ground under its own corners still lies where the mark does, a few
    /// rays, once the dig's collision meshes have landed, and only one whose
    /// ground moved is laid again. A mark over a chunk made again is laid
    /// again only when it was laid while that chunk was a coarse stand-in. A
    /// chunk put away lays nothing again. Other level edits come from the
    /// level's edit log, whose regions name whole colliders or chunks.
    ///
    /// The laying goes a piece of the mark at a time (a `LayJob`, like a
    /// first lay), carried over frames, the old mark drawn until the new one
    /// is whole. The asking and laying take at most [`RELAY_BUDGET_MS`] a
    /// frame, checked before every piece; what is left waits for the next
    /// frame. A streamed chunk landing re-laid every mark in the world, one
    /// whole mark a frame, 12 to 34 ms each.
    fn relay_decals_on_edits(&mut self) {
        self.decal_batches.checked_frame = 0;
        self.decal_batches.relaid_frame = 0;
        let store_rc = self.script_host.decals().clone();
        if store_rc.borrow().epoch() != self.decal_batches.epoch {
            // The store was emptied wholesale, and its ids start again: what
            // was queued or being laid was another mark.
            let b = &mut self.decal_batches;
            b.relaying = None;
            b.pending.clear();
            b.pending_set.clear();
            b.forced.clear();
            b.laid_at.clear();
        }
        // Out of Play there are no marks to lay again, and nothing is kept.
        let landed = std::mem::take(&mut self.decal_batches.landed);
        let dug = std::mem::take(&mut self.decal_batches.dug);
        let Some(sim) = self.sim.as_ref() else { return };
        let t0 = floptle_core::time::Instant::now();
        let edits = sim.world.level_edits;
        let mut queue: Vec<(u32, bool)> = Vec::new();
        let logged = edits != self.decal_batches.seen_edits;
        if logged || !landed.is_empty() || !dug.is_empty() {
            let seen = std::mem::replace(&mut self.decal_batches.seen_edits, edits);
            // Where else the level changed: each edit's bounding sphere, or
            // anywhere when one could be anywhere or the log no longer reaches
            // back past edits the terrain did not report itself.
            let handled = &self.decal_batches.handled;
            let log = &sim.world.level_edit_log;
            let mut anywhere = logged
                && log.front().is_none_or(|(first, _)| (seen + 1..*first).any(|s| !handled.contains(&s)));
            let mut regions = Vec::new();
            for (_, r) in log.iter().filter(|(seq, _)| *seq > seen && !handled.contains(seq)) {
                match r {
                    Some(r) => regions.push(*r),
                    None => anywhere = true,
                }
            }
            self.decal_batches.handled.clear();
            let store = store_rc.borrow();
            let frames = self.terrain_frames();
            let b = &self.decal_batches;
            let relaying = b.relaying.as_ref().map(|r| r.id);
            // Nothing to ask when every edit was one the terrain reported and
            // none of those reached a mark (a collision mesh landing).
            let asked = anywhere || !regions.is_empty() || !landed.is_empty() || !dug.is_empty();
            for id in if asked { store.ids() } else { Vec::new() } {
                let Some(d) = store.get(id) else { continue };
                let Some((pos, _, _)) = self.decal_place_now(d) else { continue };
                let reach = mark_reach(d);
                // Over a chunk made again that was a stand-in when the mark
                // was laid, or is being laid: what it lies on changed.
                let laid = if d.waiting || relaying == Some(id) { None } else { b.laid_at.get(&id).copied() };
                let on_standin = landed.iter().any(|(e, coords)| {
                    let Some((p, r)) = frames.local(*e, pos, reach) else { return false };
                    coords.iter().any(|&c| {
                        let laid_on_it = laid.is_none_or(|t| t >= b.evicted_at.get(&(*e, c)).copied().unwrap_or(0));
                        laid_on_it && frames.chunk_box(*e, c).is_some_and(|(lo, hi)| TerrainFrames::ball_meets(p, r, lo, hi))
                    })
                });
                if on_standin {
                    queue.push((id, true));
                    continue;
                }
                let near = anywhere
                    || regions.iter().any(|(c, r)| (pos - *c).length() < r + reach)
                    || dug.iter().any(|(e, lo, hi)| {
                        frames.local(*e, pos, reach).is_some_and(|(p, r)| TerrainFrames::ball_meets(p, r, *lo, *hi))
                    });
                if near {
                    queue.push((id, false));
                }
            }
            for (e, coords) in &landed {
                for c in coords {
                    self.decal_batches.evicted_at.remove(&(*e, *c));
                }
            }
        }
        for (id, forced) in queue {
            self.queue_relay(id, forced);
        }
        if self.decal_batches.pending.is_empty() && self.decal_batches.relaying.is_none() {
            return;
        }
        let mut store = store_rc.borrow_mut();
        let undrawn = store.undrawn().clone();
        let mut relay = self.decal_batches.relaying.take();
        let frames = self.terrain_frames();
        // Marks whose turn is not yet: being laid for the first time, or over
        // collision meshes not built yet. They wait in the queue.
        let mut later = Vec::new();
        loop {
            if t0.elapsed().as_secs_f64() * 1000.0 > RELAY_BUDGET_MS {
                break;
            }
            if let Some(r) = relay.as_mut() {
                // The next piece, where the mark is now.
                let Some(((pos, turn, _), reach)) = store.get(r.id).and_then(|d| self.decal_place_now(d).map(|p| (p, mark_reach(d)))) else {
                    relay = None;
                    continue;
                };
                // Ground beside a chunk made again loses its collision mesh
                // too: the piece waits for it rather than meshing it here.
                if !self.collision_meshed_under(pos, reach) {
                    break;
                }
                let Some(sim) = self.sim.as_ref() else { break };
                let center = (pos - sim.world.origin).as_vec3();
                if !r.job.step(&sim.world.colliders, &undrawn, center, turn * r.turn.inverse()) {
                    continue;
                }
                let r = relay.take().expect("a relay");
                store.relaid(r.id, r.pos, r.frame, r.spec, r.job.finish());
                self.decal_batches.relaid += 1;
                self.decal_batches.relaid_frame += 1;
                continue;
            }
            let Some(id) = self.decal_batches.pending.pop_front() else { break };
            let placed = store.get(id).and_then(|d| self.decal_place_now(d).map(|p| (p, mark_reach(d), d.waiting)));
            let Some(((pos, turn, frame), reach, waiting)) = placed else {
                self.decal_batches.pending_set.remove(&id);
                self.decal_batches.forced.remove(&id);
                continue;
            };
            // Its ground is asked about on finished ground: none still being
            // made, a dig's triangles once they land, and none meshed on this
            // thread.
            if waiting
                || self.mark_awaits_ground(&frames, pos, reach)
                || self.mark_awaits_collision(&frames, pos, reach)
                || !self.collision_meshed_under(pos, reach) {
                later.push(id);
                continue;
            }
            let Some(sim) = self.sim.as_ref() else { break };
            let Some(d) = store.get(id) else { continue };
            self.decal_batches.pending_set.remove(&id);
            let forced = self.decal_batches.forced.remove(&id);
            self.decal_batches.checked_frame += 1;
            self.decal_batches.checked += 1;
            if !forced && ground_holds(&sim.world.colliders, sim.world.origin, d, pos, turn) {
                continue;
            }
            let mut spec = d.spec.clone();
            spec.center = (pos - sim.world.origin).as_vec3();
            spec.normal = turn * spec.normal;
            spec.up = turn * spec.up;
            relay = Some(Relay { id, job: floptle_script::decal_api::LayJob::new(&spec), pos, frame, spec, turn });
        }
        self.decal_batches.pending.extend(later);
        self.decal_batches.relaying = relay;
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
        settle(&mut ed);
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

    /// Frames of the decal passes (and the collision meshes they wait for)
    /// until no mark is waiting to be laid again, bounded by the clock: a
    /// loaded machine takes more frames.
    fn settle(ed: &mut crate::Editor) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            ed.pump_collision_meshes();
            ed.sync_decals();
            let b = &ed.decal_batches;
            if b.pending.is_empty() && b.relaying.is_none() && ed.collision_pending.is_empty() {
                break;
            }
            assert!(std::time::Instant::now() < until, "the marks were still waiting to be laid again");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
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
        // Play starting is a change anywhere: every mark is asked once.
        settle(&mut ed);
        let checked_before = ed.decal_batches.checked;
        // A bite at the first mark's spot, through the path `terrain.dig` takes.
        let touched = {
            let t = ed.terrains.get_mut(&e).unwrap();
            // The field's record of what changed still holds the slab being
            // made; a game's first edit has had earlier ones take it.
            let _ = t.field.take_changed();
            t.field.sculpt(Brush::Lower, Vec3::new(2.0, 0.0, 0.0), 2.4, 1.0, BrushProfile { hardness: 1.0, ..Default::default() })
        };
        ed.mirror_terrain_chunks_to_sim(e, &touched);
        settle(&mut ed);
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
        // Asked about: the marks whose box reaches the box the bite wrote
        // (its radius, the field's 2 m band, a voxel more); not every mark
        // in the bite's 16 m chunks.
        let asked = (0..150)
            .filter(|i| {
                let a = *i as f64 * 2.39996;
                let r = 2.0 + 27.0 * (*i as f64 / 149.0);
                let (x, z) = (r * a.cos(), r * a.sin());
                let reach = (1.5f64 * 1.5 * 2.0 + 16.0).sqrt() * 0.5 + 4.0;
                let half = 2.4 + 2.0 + 1.0;
                let (gx, gz) = (((x - 2.0).abs() - half).max(0.0), (z.abs() - half).max(0.0));
                (gx * gx + gz * gz).sqrt() <= reach
            })
            .count() as u64;
        let checked = ed.decal_batches.checked - checked_before;
        assert!(checked <= asked, "{checked} marks asked about their ground for a bite whose box reaches {asked} of 150");
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
        // The bite's collision meshes land on a worker: wait for them all, so
        // the re-lay below is the whole of the dig's.
        for _ in 0..5000 {
            ed.pump_collision_meshes();
            if ed.collision_pending.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(ed.collision_pending.is_empty(), "the bite's collision meshes never landed");
        settle(&mut ed);
        ed.sync_decals();
        let relaid = ed.decal_batches.relaid;
        let repacked = ed.decal_batches.repacked - before;
        assert!(relaid >= 1, "the mark under the bite was not laid again");
        assert!(
            repacked <= relaid * u64::from(super::PAGE),
            "{relaid} marks laid again repacked {repacked} marks: more than their pages hold"
        );
        // A loaded machine spreads the re-lay over more frames, and a page
        // holding two of the marks is packed once per frame it changes in
        // (61 idle, up to 91 with every core busy), but never the whole
        // picture: that was 150 a re-laid mark.
        assert!(repacked < 150, "a dig among 150 marks repacked {repacked} of them");
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

    /// A 200 m planet in Play, its far side put away to stand-ins by the
    /// camera standing at the top, and a script laying 29 m marks at `marks`
    /// (surface points, facing up from the centre). The field as generated,
    /// whole, comes back too.
    fn planet_with_marks(
        tag: &str,
        marks: &dyn Fn(&floptle_field::ChunkField) -> Vec<Vec3>,
    ) -> (crate::Editor, floptle_core::Entity, floptle_field::ChunkField, std::path::PathBuf) {
        let fill = floptle_field::procgen::PlanetFill { seed: 12, radius: 200.0, voxel: 1.5, relief: 8.0, cave_depth: 0.0, ..Default::default() };
        let mut field = floptle_field::procgen::generate_planet(&fill);
        field.set_spec("marks".into());
        let exact = field.clone();
        let points = marks(&field);
        let list: Vec<String> = points
            .iter()
            .map(|p| {
                let n = p.normalize();
                format!("{{{},{},{},{},{},{}}}", p.x, p.y, p.z, n.x, n.y, n.z)
            })
            .collect();
        let script = format!(
            "local P = {{ {} }}\nlocal done = false\nfunction update(node, dt)\n  if done then return end\n  done = true\n  \
             for _, p in ipairs(P) do\n    decals.add{{ texture = 'turf.png', pos = vec3(p[1], p[2], p[3]), \
             normal = vec3(p[4], p[5], p[6]), size = 29, depth = 4 }}\n  end\nend\n",
            list.join(",")
        );
        let dir = std::env::temp_dir().join(format!("floptle-decal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
        floptle_vfs::write(dir.join("scripts/marks.lua"), script).unwrap();
        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.world.insert(e, floptle_core::CelestialBody { body_radius: 200.0, ..Default::default() });
        ed.world.insert(e, floptle_core::Matter::Terrain { id: 1, collision: Default::default() });
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
        let top = DVec3::new(0.0, 205.0, 0.0);
        settle_streaming(&mut ed, top, &mut RelayWatch::default());
        assert!(ed.terrains[&e].field.evicted_count() > 0, "nothing was put away: the far side is not far enough");
        ed.toggle_play();
        assert!(ed.playing && ed.sim.is_some());
        let until = std::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            ed.play_step(1.0 / 60.0, true);
            assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
            let store = ed.script_host.decals().clone();
            if store.borrow().len() == points.len() && store.borrow().waiting().is_empty() {
                break;
            }
            assert!(std::time::Instant::now() < until, "the marks never all landed");
        }
        (ed, e, exact, dir)
    }

    /// What the relay did over the frames watched.
    #[derive(Default)]
    struct RelayWatch {
        checked: u64,
        relaid: u64,
        /// The mark being laid again, and its triangles when it started.
        laying: Option<(u32, Vec<floptle_script::decal_api::DecalVert>)>,
        /// Frames a mark being laid again stayed drawn as it was.
        held: u64,
        /// Terrain collision chunks the decal passes meshed on the frame,
        /// which is what made one mark's re-lay cost 12 to 34 ms.
        meshed_on_frame: u64,
    }

    /// One frame of the passes a mark's ground moves under, with the camera
    /// at `cam`: streaming, collision meshes, the decal passes.
    fn stream_frame(ed: &mut crate::Editor, cam: DVec3, w: &mut RelayWatch) {
        ed.camera.position = cam;
        let at = ed.lod_center(cam);
        ed.stream_far_chunks(at);
        ed.pump_collision_meshes();
        let meshed = |ed: &crate::Editor| {
            ed.sim.as_ref().map_or(0, |s| s.world.colliders.iter().filter_map(|c| c.shape.chunk_terrain()).map(|t| t.meshed_by_queries()).sum())
        };
        let before = meshed(ed);
        ed.sync_decals();
        w.meshed_on_frame += meshed(ed) - before;
        let b = &ed.decal_batches;
        w.checked += u64::from(b.checked_frame);
        w.relaid += u64::from(b.relaid_frame);
        // A mark being laid again is drawn as it was until the new one is
        // whole: never gone, never half laid.
        let store = ed.script_host.decals().clone();
        let store = store.borrow();
        match (b.relaying.as_ref().map(|r| r.id), w.laying.take()) {
            (Some(id), Some((was, verts))) if id == was => {
                let d = store.get(id).expect("a mark being laid again went");
                assert!(!d.verts.is_empty() && d.verts == verts, "mark {id} changed before its new lay was whole");
                w.held += 1;
                w.laying = Some((id, verts));
            }
            (Some(id), _) => {
                let d = store.get(id).expect("a mark being laid again went");
                assert!(!d.verts.is_empty(), "mark {id} is drawn as nothing while it is laid again");
                w.laying = Some((id, d.verts.clone()));
            }
            (None, _) => {}
        }
    }

    /// Frames at `cam` until the ground there has settled (nothing being made,
    /// nothing more put away) and no mark waits to be laid again, bounded by
    /// the clock.
    fn settle_streaming(ed: &mut crate::Editor, cam: DVec3, w: &mut RelayWatch) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(300);
        let away = |ed: &crate::Editor| ed.terrains.values().map(|t| t.field.evicted_count()).sum::<usize>();
        loop {
            let before = away(ed);
            for _ in 0..crate::terrain_edit::FAR_SCAN_FRAMES * 2 {
                stream_frame(ed, cam, w);
            }
            let b = &ed.decal_batches;
            let idle = b.pending.is_empty() && b.relaying.is_none() && ed.collision_pending.is_empty();
            if ed.terrain_promotions.is_empty() && away(ed) == before && idle {
                return;
            }
            assert!(std::time::Instant::now() < until, "the ground or the marks never settled at {cam:?}");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// The marks' vertices, by id, where they are in the world.
    fn mark_verts(ed: &crate::Editor) -> std::collections::BTreeMap<u32, (DVec3, Vec<floptle_script::decal_api::DecalVert>)> {
        let store = ed.script_host.decals().clone();
        let store = store.borrow();
        store.ids().into_iter().filter_map(|id| store.get(id).map(|d| (id, (d.pos, d.verts.clone())))).collect()
    }

    /// How far a mark's vertices lie off the ground `field` holds, on average.
    fn off_ground(field: &floptle_field::ChunkField, pos: DVec3, verts: &[floptle_script::decal_api::DecalVert]) -> f32 {
        let sum: f32 = verts.iter().map(|v| field.d((pos.as_vec3()) + v.pos).abs()).sum();
        sum / verts.len().max(1) as f32
    }

    /// **Streaming ground lays again only the marks laid on its stand-ins.**
    /// Every chunk made again or put away as the player walked was noted as
    /// a change that could be anywhere, and every mark in the world was laid
    /// again, a whole mark a frame: Solar's meadow of 70 marks cost 12 ms on
    /// the average frame and 21 to 27 ms on each landing. Now a chunk put
    /// away lays nothing again, and a chunk made again lays again only the
    /// marks laid while it was a stand-in, a piece at a time within the
    /// frame's budget, the old mark drawn until the new one is whole.
    ///
    /// 150 marks of 29 m, half on the ground the camera stands on and half on
    /// the far side's stand-ins; the camera walks to the far side and back.
    /// The near marks' ground is put away and made again, exactly as it was:
    /// not one is laid again. Each far mark is laid again on the exact ground
    /// when it arrives. No frame of the walk spends more than half of what
    /// laying one mark again takes.
    #[test]
    fn streaming_ground_lays_again_only_the_marks_on_its_stand_ins() {
        let ring = |field: &floptle_field::ChunkField, up: Vec3| -> Vec<Vec3> {
            let (u, v) = (up.any_orthonormal_vector(), up.cross(up.any_orthonormal_vector()));
            (0..75)
                .map(|i| {
                    let (r, a) = (0.18 * (i as f32 / 75.0).sqrt(), i as f32 * 2.39996);
                    let dir = (up + u * (r * a.cos()) + v * (r * a.sin())).normalize();
                    field.raycast(dir * 300.0, -dir, 300.0).expect("ground under a mark")
                })
                .collect()
        };
        let marks = |f: &floptle_field::ChunkField| [ring(f, Vec3::Y), ring(f, Vec3::NEG_Y)].concat();
        let (mut ed, e, exact, dir) = planet_with_marks("streaming", &marks);
        let laid = mark_verts(&ed);
        assert_eq!(laid.len(), 150, "the marks were not all laid");
        let near = |pos: &DVec3| pos.y > 0.0;
        // The far marks were laid on stand-ins: further off the ground than
        // the near ones.
        let mean = |pick: &dyn Fn(&DVec3) -> bool, marks: &std::collections::BTreeMap<u32, (DVec3, Vec<floptle_script::decal_api::DecalVert>)>| {
            let picked: Vec<f32> = marks.values().filter(|(p, _)| pick(p)).map(|(p, v)| off_ground(&exact, *p, v)).collect();
            picked.iter().sum::<f32>() / picked.len().max(1) as f32
        };
        let (near_off, far_off) = (mean(&near, &laid), mean(&|p: &DVec3| !near(p), &laid));
        assert!(far_off > near_off * 3.0, "the far marks lie {far_off:.3} m off the ground, the near {near_off:.3}: not on stand-ins");

        // When each near mark was laid: one laid again, even identically,
        // is laid at a later frame.
        let near_laid_at: Vec<(u32, Option<u64>)> =
            laid.iter().filter(|(_, (p, _))| near(p)).map(|(id, _)| (*id, ed.decal_batches.laid_at.get(id).copied())).collect();
        assert!(near_laid_at.iter().all(|(_, t)| t.is_some()), "a near mark has no record of when it was laid");
        let mut w = RelayWatch::default();
        // Standing still, nothing is asked about.
        let top = DVec3::new(0.0, 205.0, 0.0);
        for _ in 0..120 {
            stream_frame(&mut ed, top, &mut w);
        }
        assert_eq!((w.checked, w.relaid), (0, 0), "marks were asked about with nothing changing");
        // Round to the far side and back.
        let mut near_put_away = false;
        for step in (0..=16).chain((0..16).rev()) {
            let a = std::f64::consts::PI * step as f64 / 16.0;
            settle_streaming(&mut ed, DVec3::new(a.sin(), a.cos(), 0.0) * 205.0, &mut w);
            let f = &ed.terrains[&e].field;
            let side = floptle_field::CHUNK as f32 * f.voxel();
            near_put_away |= laid.values().filter(|(p, _)| near(p)).any(|(p, _)| {
                f.is_evicted((p.as_vec3() / side).floor().to_array().map(|v| v as i32))
            });
        }
        assert!(near_put_away, "the near marks' ground was never put away: the walk tests nothing");
        let now = mark_verts(&ed);
        let moved: Vec<u32> = laid.iter().filter(|(id, (p, _))| near(p) && now.get(id) != laid.get(id)).map(|(id, _)| *id).collect();
        assert!(moved.is_empty(), "{} near marks were laid again on ground that came back as it was", moved.len());
        let again = near_laid_at.iter().filter(|(id, t)| ed.decal_batches.laid_at.get(id).copied() != *t).count();
        assert_eq!(again, 0, "{again} near marks were laid again, identically, on ground that came back as it was");
        let far_after = mean(&|p: &DVec3| !near(p), &now);
        assert!(
            far_after < near_off * 1.5 + 0.02,
            "the far marks lie {far_after:.3} m off the ground after it arrived, the near ones {near_off:.3}"
        );
        assert!(w.relaid >= 75, "only {} marks were laid again for 75 on stand-ins", w.relaid);
        // A 29 m mark lies over up to eight 48 m chunks, which arrive with
        // the walk a few at a time: it is laid again as each batch under it
        // lands, a handful of times. Every mark laid again on every landing
        // was 150 a landing, hundreds of landings.
        assert!(w.relaid <= 75 * 4 && w.checked <= 75 * 4, "{} marks asked about and {} laid again for 75 on stand-ins", w.checked, w.relaid);
        // A mark's ground is meshed on the collision worker before it is laid
        // again, so a piece of it is a piece's work, never a chunk meshed on
        // the frame.
        assert_eq!(w.meshed_on_frame, 0, "laying marks again meshed {} collision chunks on the frame", w.meshed_on_frame);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A big mark is laid again a piece at a time, drawn as it was until
    /// it is whole.** Laying a mark again ran every piece of it in one call,
    /// unlike its first lay: one 28.8 m mark was 12 to 34 ms of one frame.
    /// One 29 m mark over finely meshed ground, laid again as though its
    /// ground had just arrived: the work is carried over frames, no frame
    /// does more than a share of it, and every frame between, the mark is
    /// the one laid before.
    #[test]
    fn a_big_mark_is_laid_again_over_frames() {
        let script = "local done = false\nfunction update(node, dt)\n  if done then return end\n  done = true\n  \
                      decals.add{ texture = 'turf.png', pos = vec3(0, 0, 0), normal = vec3(0, 1, 0), size = 29, depth = 4 }\nend\n";
        let (mut ed, e, dir) = on_a_slab("relay-pieces", script, 40.0);
        // Finer ground than the slab's: a quarter-metre voxel, so the mark is
        // tens of thousands of faces to sort, many budgets' work on any machine.
        let mut field = floptle_field::ChunkField::new(0.25);
        field.fill_slab(Vec3::new(-24.0, -3.0, -24.0), Vec3::new(24.0, 0.0, 24.0), 0.0, [0.5, 0.5, 0.5]);
        ed.terrains.insert(e, crate::terrain_edit::EditorTerrain::new(field));
        ed.toggle_play();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(300);
        while !(ed.script_host.decals().borrow().len() == 1 && ed.script_host.decals().borrow().waiting().is_empty()) {
            ed.play_step(1.0 / 60.0, true);
            assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
            assert!(std::time::Instant::now() < until, "the mark never landed");
        }
        settle(&mut ed);
        let laid = mark_verts(&ed);
        let chunks = ed.terrains[&e].field.chunk_coords();
        ed.decals_note_landed(e, chunks);
        let relaid = ed.decal_batches.relaid;
        let mut w = RelayWatch::default();
        let mut frames_ms = Vec::new();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(300);
        while ed.decal_batches.relaid == relaid {
            ed.pump_collision_meshes();
            let t = std::time::Instant::now();
            ed.sync_decals();
            frames_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            if let Some(r) = ed.decal_batches.relaying.as_ref() {
                let store = ed.script_host.decals().clone();
                let store = store.borrow();
                let same = store.get(r.id).map(|d| (d.pos, d.verts.clone())) == laid.get(&r.id).cloned();
                assert!(same, "the mark changed before its new lay was whole");
                w.held += 1;
            }
            assert!(std::time::Instant::now() < until, "the mark was never laid again");
        }
        let total: f64 = frames_ms.iter().sum();
        let worst = frames_ms.iter().copied().fold(0.0, f64::max);
        assert!(w.held > 0, "the mark was laid again in one frame, {total:.1} ms of work");
        assert!(worst < total / 3.0, "one frame spent {worst:.2} ms of the {total:.2} ms the re-lay took");
        assert_eq!(mark_verts(&ed), laid, "laid again on the same ground, the mark came out otherwise");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A mark laid again is drawn the same, every frame of it.** Ten 29 m
    /// marks on a slab, every one laid again as though its ground had just
    /// arrived (a chunk made again under a mark laid on its stand-in), and a
    /// picture taken every frame of the re-lay: each is the picture from
    /// before, to the pixel. The old mark stays drawn until the new one is
    /// whole, and on the same ground the new one is the old one.
    /// `FLOPTLE_DECAL_PNG=<dir>` writes the first, a middle and the last.
    #[test]
    #[cfg(feature = "editor-ui")]
    fn a_mark_laid_again_draws_the_same_every_frame() {
        let Some(gpu_ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let script = "local done = false\nfunction update(node, dt)\n  if done then return end\n  done = true\n  \
                      for i = 0, 9 do\n    decals.add{ texture = 'turf.png', pos = vec3(-27 + i * 6, 0, (i % 3) * 8 - 8), \
                      normal = vec3(0, 1, 0), size = 29, depth = 4 }\n  end\nend\n";
        let (mut ed, e, dir) = on_a_slab_in(gpu_ed, "relay-png", script, 60.0);
        // A picture with a pattern, so a mark laid even slightly otherwise shows.
        let turf = image::RgbaImage::from_fn(16, 16, |x, y| {
            if (x / 2 + y / 2) % 2 == 0 { image::Rgba([40, 160, 60, 200]) } else { image::Rgba([200, 90, 30, 120]) }
        });
        turf.save(dir.join("turf.png")).unwrap();
        let sun = ed.world.spawn();
        ed.world.insert(
            sun,
            floptle_core::Transform {
                rotation: floptle_core::math::Quat::from_rotation_x(-0.9) * floptle_core::math::Quat::from_rotation_y(0.4),
                ..floptle_core::Transform::IDENTITY
            },
        );
        ed.world.insert(sun, floptle_core::Light { intensity: 1.2, ..Default::default() });
        ed.toggle_play();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(300);
        while !(ed.script_host.decals().borrow().len() == 10 && ed.script_host.decals().borrow().waiting().is_empty()) {
            ed.play_step(1.0 / 60.0, true);
            assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
            assert!(std::time::Instant::now() < until, "the marks never all landed");
        }
        settle(&mut ed);
        ed.sync_decals();
        ed.camera.position = DVec3::new(0.0, 45.0, 40.0);
        ed.camera.pitch = -0.85;
        ed.camera.yaw = 0.0;
        let cam_world = ed.camera.position;
        ed.settle_terrain_meshes(cam_world, std::time::Duration::from_secs(60));
        let (w, h) = (240u32, 150u32);
        let cam = ed.camera.render_camera();
        let shoot = |ed: &mut crate::Editor| crate::shot::render_frame_pixels(ed, &cam, None, w, h, !0, false).expect("a frame");
        let before = shoot(&mut ed);
        let lum: Vec<i32> = before.chunks(4).map(|p| p[0] as i32 + p[1] as i32 + p[2] as i32).collect();
        assert!(lum.iter().max().unwrap() - lum.iter().min().unwrap() > 60, "the picture is one flat colour: nothing to compare");
        let laid = mark_verts(&ed);
        // Every chunk of the slab made again, none of them recorded as put
        // away: every mark is laid again.
        let chunks = ed.terrains[&e].field.chunk_coords();
        ed.decals_note_landed(e, chunks);
        let relaid = ed.decal_batches.relaid;
        let (mut frames, mut mid_relay, mut middle) = (0, 0, None);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            ed.pump_collision_meshes();
            ed.sync_decals();
            let px = shoot(&mut ed);
            frames += 1;
            let differ = before.chunks(4).zip(px.chunks(4)).filter(|(a, b)| a != b).count();
            assert_eq!(differ, 0, "{differ} pixels changed on frame {frames} of the re-lay");
            if ed.decal_batches.relaying.is_some() {
                mid_relay += 1;
                if middle.is_none() && ed.decal_batches.relaid > relaid + 3 {
                    middle = Some(px);
                }
            }
            let b = &ed.decal_batches;
            if b.pending.is_empty() && b.relaying.is_none() && ed.collision_pending.is_empty() {
                break;
            }
            assert!(std::time::Instant::now() < until, "the marks were never all laid again");
        }
        assert_eq!(ed.decal_batches.relaid - relaid, 10, "every mark is laid again");
        assert!(mid_relay > 0, "no picture was taken with a mark half laid again: the fixture shows nothing");
        let after = shoot(&mut ed);
        assert_eq!(mark_verts(&ed), laid, "a mark laid again on the same ground came out otherwise");
        if let Ok(out) = std::env::var("FLOPTLE_DECAL_PNG") {
            for (name, px) in [("before", Some(&before)), ("middle", middle.as_ref()), ("after", Some(&after))] {
                let Some(px) = px else { continue };
                let tex = floptle_render::TextureData { pixels: px.clone(), width: w, height: h };
                std::fs::write(std::path::Path::new(&out).join(format!("relay-{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
            }
        }
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

