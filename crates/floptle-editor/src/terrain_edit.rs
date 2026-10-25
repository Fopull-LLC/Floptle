//! Terrain editing + the shared SDF volume atlas: sculpt strokes, terrain
//! creation/adoption, per-frame terrain state, and the shadow-only mesh
//! occluder bakes that ride the same atlas.

use floptle_core::Entity;
use floptle_core::Material;
use floptle_core::Matter;
use floptle_core::Name;
use floptle_core::math::DVec3;
use floptle_core::math::Mat4;
use floptle_core::math::Quat;
use floptle_core::math::Vec2;
use floptle_core::math::Vec3;
use floptle_core::math::Vec4;
use floptle_core::transform::Transform;
use floptle_render::MaterialParams;
use floptle_render::MeshId;
use floptle_render::RaymarchGlobals;
use std::collections::HashMap;
use std::path::PathBuf;
use floptle_core::time::Instant;
#[cfg(feature = "editor-ui")]
use crate::dock::{focus_terrain_tab};
use crate::gizmo::{Tool};
use crate::shading::{OccKey, material_params};
#[cfg(feature = "editor-ui")]
use crate::terrain_ui::{NewTerrainCfg};
use crate::viz::{TerrainViz, project};
use crate::{Editor};

/// One editable terrain (Terrain 2.0 / P3): the sparse unbounded [`ChunkField`] is the
/// authority — brushes write it, physics collides it, saves serialize it, the mesher
/// extracts the drawn surface from it. The dense `shadow` proxy is derived from it at a
/// capped resolution purely to feed the GPU shadow/AO atlas (until the P5 clipmap).
pub(crate) struct EditorTerrain {
    pub field: floptle_field::ChunkField,
    pub shadow: floptle_field::BakedSdf,
    /// Names this `shadow` in the GPU atlas: a new one for every proxy made
    /// whole, kept through the boxes an edit refreshes (those are uploaded
    /// as boxes). An atlas re-laid around a key it holds copies the volume
    /// on the GPU instead of uploading it again.
    pub shadow_key: u64,
    /// Drawn meshes of the full-detail chunks round where the player was when
    /// the world was asked for, made with the field off the frame: the frame
    /// it lands draws them instead of meshing them (see [`Self::premesh`]).
    pub premeshed: HashMap<[i32; 3], floptle_field::ChunkMesh>,
    /// Collision meshes of the chunks nearest that place, made the same way,
    /// for the sim's collider when the world lands while the game runs.
    pub collision: Vec<([i32; 3], floptle_physics::CollisionChunk)>,
}

/// Where the player was when a world was asked for, in the world's own frame:
/// what the meshes made with its field are made round.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LandingView {
    /// The place, in field units (the node's rotation and scale undone).
    pub near: Vec3,
    /// The body's radius times the node's scale, for a celestial body.
    pub body_radius: Option<f64>,
    /// The node's uniform scale.
    pub scale: f32,
}

/// Chunks within this many of the landing view get collision meshes made with
/// the world: the ground a cast from the player can reach on the frame the
/// world lands, which otherwise meshed it there.
const LANDING_COLLISION_CHUNKS: i32 = 2;

/// Chunks with no mesh at all that a frame of the game meshes at full detail
/// itself; the rest of them go to the workers ahead of everything else, and
/// draw a few frames later. A world landing, or the camera put down on ground
/// never drawn, meshed every chunk round it on one frame: 280 ms on Solar's
/// home world (debug). The editor, which is not playing, meshes them all.
pub(crate) const NEAR_MESHES_A_FRAME: usize = 4;

/// How far chunk `c` is from `cl` (in chunk units, in the field's frame), in
/// quarter chunks, rounded up: zero inside it. What the LOD rings are measured in.
fn chunk_ring_dist(c: [i32; 3], cl: Vec3) -> i32 {
    let lo = Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32);
    let gap = (lo - cl).max(cl - (lo + Vec3::ONE)).max(Vec3::ZERO);
    (gap.length() * RING_STEPS as f32).ceil() as i32
}

/// A fresh [`EditorTerrain::shadow_key`]: unique in the process, never 0, and
/// clear of the high bit that occluder keys carry.
fn next_shadow_key() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Longest-axis cells of shadow-proxy upload a frame may spend (6 bytes a
/// cell): about two milliseconds. A new terrain's whole proxy, up to 192³ =
/// 7 M cells, is written over frames into an atlas beside the drawn one, and
/// edits' boxes queue behind the budget, so no frame pays for a volume at once.
pub(crate) const TERRAIN_SHADOW_UPLOAD_CELLS: u64 = 384 * 1024;

/// The proxy boxes of one terrain waiting to be uploaded, as voxel boxes
/// `[min, max)` of the proxy named `key`. Boxes that overlap or nearly touch
/// are merged as they arrive, so a frame's cluster of edits is one upload and
/// edits far apart stay separate instead of becoming one box spanning both.
#[derive(Default)]
pub(crate) struct ShadowDirty {
    pub key: u64,
    pub boxes: Vec<([u32; 3], [u32; 3])>,
    /// A sculpt (not a paint) moved the surface: the wireframe is rebuilt.
    pub geom: bool,
}

/// Boxes a terrain keeps apart before the new one is merged into the box it
/// grows least.
const SHADOW_DIRTY_BOXES: usize = 64;

fn box_cells(b: &([u32; 3], [u32; 3])) -> u64 {
    (0..3).map(|a| b.1[a].saturating_sub(b.0[a]) as u64).product()
}

fn box_union(a: &([u32; 3], [u32; 3]), b: &([u32; 3], [u32; 3])) -> ([u32; 3], [u32; 3]) {
    (
        [a.0[0].min(b.0[0]), a.0[1].min(b.0[1]), a.0[2].min(b.0[2])],
        [a.1[0].max(b.1[0]), a.1[1].max(b.1[1]), a.1[2].max(b.1[2])],
    )
}

impl ShadowDirty {
    /// Add a box, merging it with any box their union barely exceeds (a
    /// quarter more than the two apart), until none is left to merge.
    pub fn add(&mut self, mn: [u32; 3], mx: [u32; 3]) {
        let mut b = (mn, mx);
        if box_cells(&b) == 0 {
            return;
        }
        while let Some(i) = self.boxes.iter().position(|o| box_cells(&box_union(o, &b)) * 4 <= (box_cells(o) + box_cells(&b)) * 5) {
            let o = self.boxes.swap_remove(i);
            b = box_union(&o, &b);
        }
        if self.boxes.len() >= SHADOW_DIRTY_BOXES {
            let grow = |o: &([u32; 3], [u32; 3])| box_cells(&box_union(o, &b)) - box_cells(o);
            if let Some(i) = (0..self.boxes.len()).min_by_key(|&i| grow(&self.boxes[i])) {
                let o = self.boxes.swap_remove(i);
                return self.add(box_union(&o, &b).0, box_union(&o, &b).1);
            }
        }
        self.boxes.push(b);
    }
}

/// Where a terrain's shadow volume sits in the drawn atlas: its slot holds
/// the proxy the box was taken from, which stays drawn there until a new
/// atlas holding the next one is whole.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TerrainSlot {
    pub e: Entity,
    pub center: [f32; 3],
    pub half: [f32; 3],
}

/// Longest-axis cell cap for the shadow proxy. Soft sun shadows are forgiving of a
/// coarse field; primary visibility (the unforgiving part) is the chunk meshes.
pub(crate) const TERRAIN_SHADOW_MAX_DIM: u32 = 192;

/// Frames a turned or resized static mesh must hold its new shape before its
/// shadow occluder is baked again: about a third of a second at 60 fps.
pub(crate) const OCCLUDER_SETTLE_FRAMES: u32 = 20;

/// What one frame's script terrain ops wrote to one terrain, applied together:
/// one collider copy, and one shadow/remesh refresh per cluster of boxes.
#[derive(Default)]
struct TerrainBatch {
    boxes: Vec<(Vec3, Vec3)>,
    geom: bool,
    touched: Vec<[i32; 3]>,
}

impl TerrainBatch {
    /// Add an op's box, merging it with every box it overlaps, so ops that
    /// cluster share one refresh and ops far apart each keep their own.
    fn add_box(&mut self, mut lo: Vec3, mut hi: Vec3) {
        while let Some(i) = self.boxes.iter().position(|(l, h)| l.cmple(hi).all() && lo.cmple(*h).all()) {
            let (l, h) = self.boxes.swap_remove(i);
            lo = lo.min(l);
            hi = hi.max(h);
        }
        self.boxes.push((lo, hi));
    }
}

impl EditorTerrain {
    /// Wrap a field, deriving its shadow proxy.
    pub(crate) fn new(field: floptle_field::ChunkField) -> Self {
        let shadow = shadow_proxy_of(&field);
        Self { field, shadow, shadow_key: next_shadow_key(), premeshed: HashMap::new(), collision: Vec::new() }
    }

    /// Wrap a field whose shadow proxy is made off the frame: a stand-in
    /// casts nothing until [`Editor::kick_terrain_proxy`]'s lands. Deriving it
    /// here sampled the whole field into a 192³ grid on the frame, half a
    /// second of the frame a world loaded from its file arrived in.
    pub(crate) fn proxy_later(field: floptle_field::ChunkField) -> Self {
        Self { field, shadow: empty_proxy(), shadow_key: next_shadow_key(), premeshed: HashMap::new(), collision: Vec::new() }
    }

    /// Make the meshes the frame this world lands on would make round `v`,
    /// here, where the field was made: the full-detail chunks' drawn meshes,
    /// and the nearest chunks' collision meshes. Each is what that frame makes
    /// from the same field, so what is drawn and what collides are unchanged.
    pub(crate) fn premesh(&mut self, v: &LandingView) {
        let chunk_units = floptle_field::CHUNK as f32 * self.field.voxel();
        let rings = rings_for_body(v.body_radius, (chunk_units * v.scale) as f64);
        let cl = v.near / chunk_units;
        for c in self.field.chunk_coords() {
            let d = chunk_ring_dist(c, cl);
            if raw_lod(d, rings) != 0 || self.field.is_evicted(c) {
                continue;
            }
            let m = floptle_field::mesh_chunk(&self.field, c, 1, true);
            if d <= LANDING_COLLISION_CHUNKS * RING_STEPS {
                // The collider's mesh is the drawn one without its skirt,
                // which `from_mesh` leaves out.
                self.collision.push((c, floptle_physics::CollisionChunk::from_mesh(&m, self.field.voxel().max(1e-3))));
            }
            self.premeshed.insert(c, m);
        }
    }

    /// Re-derive the shadow proxy from the current field (structural change / undo /
    /// bounds outgrown). The empty-field proxy is a tiny inert box.
    pub(crate) fn rebuild_shadow(&mut self) {
        self.shadow = shadow_proxy_of(&self.field);
        self.shadow_key = next_shadow_key();
    }
}

/// Which thread did each piece of landing work, for the guards that it is
/// done off the frame: `(what, thread)`.
#[cfg(test)]
pub(crate) static MADE_ON: std::sync::Mutex<Vec<(&'static str, std::thread::ThreadId)>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn made_on(what: &'static str) {
    if let Ok(mut v) = MADE_ON.lock() {
        v.push((what, std::thread::current().id()));
    }
}

/// Did this thread do any `what`?
#[cfg(test)]
pub(crate) fn made_here(what: &str) -> usize {
    let me = std::thread::current().id();
    MADE_ON.lock().map_or(0, |v| v.iter().filter(|(w, t)| *w == what && *t == me).count())
}

#[cfg(test)]
thread_local! {
    /// Full-detail chunks with no mesh drawn on this thread's frames:
    /// `[meshed on the frame, drawn from what landed with the field, sent to
    /// the workers past the frame's share]`.
    pub(crate) static NEAR_MESHES: std::cell::Cell<[usize; 3]> = const { std::cell::Cell::new([0; 3]) };
}

fn shadow_proxy_of(field: &floptle_field::ChunkField) -> floptle_field::BakedSdf {
    #[cfg(test)]
    made_on("proxy");
    field.to_dense(TERRAIN_SHADOW_MAX_DIM).unwrap_or_else(empty_proxy)
}

/// The proxy of an empty field: a tiny box of open air, casting nothing.
fn empty_proxy() -> floptle_field::BakedSdf {
    floptle_field::BakedSdf {
        dims: [2, 2, 2],
        center: [0.0; 3],
        half_extent: [0.5; 3],
        distance: vec![1.0; 8],
        color: vec![[128, 128, 128, 255]; 8],
    }
}

/// A terrain palette read off the frame: each slot's 256² layer, and its
/// mean colour (what an impostor is painted with).
pub(crate) type PaletteRead = (Vec<floptle_render::TextureData>, Vec<[f32; 3]>);

/// An occluder bake made off the frame: the bake, its model's triangles, and
/// the milliseconds it took.
pub(crate) type OccluderBake = (std::sync::Arc<floptle_field::BakedSdf>, u64, f64);

/// A terrain's shadow proxy being made off the frame, from a copy of its
/// field (see [`EditorTerrain::proxy_later`]).
pub(crate) struct ProxyJob {
    pub e: Entity,
    /// The stand-in's key: a proxy made whole since (an undo, a field replaced)
    /// supersedes this one.
    pub key: u64,
    /// The terrain's edit stamp when its field was copied: an edit since and
    /// the proxy is of ground that is not there any more, so it is made again.
    pub stamp: u64,
    pub rx: std::sync::mpsc::Receiver<floptle_field::BakedSdf>,
}

/// The GPU residency of one terrain's chunk meshes. Chunk vertices are field-space, so
/// every chunk shares one camera-relative instance matrix and the triplanar material
/// stays continuous.
#[derive(Default)]
pub(crate) struct TerrainRender {
    /// One dynamic raster slot per non-empty chunk (+ the LOD it was meshed at),
    /// keyed by chunk coord so a sculpt can re-mesh just the chunks it touched and
    /// free the ones that emptied.
    pub slots: HashMap<[i32; 3], (MeshId, u8)>,
    /// When each chunk first became resident, in seconds on the editor's clock —
    /// what [`chunk_fade`] measures the dissolve-in against.
    ///
    /// A separate map rather than a third tuple field so that re-meshing an
    /// existing chunk does not touch it: a dig re-uploads the chunk it bit, and
    /// dissolving the ground back in every time you mine it would be worse than
    /// the pop this exists to remove. Only a coord arriving from nothing is
    /// stamped. Entries are dropped with their slot.
    pub born: HashMap<[i32; 3], f32>,
    /// Chunks a worker job is in flight for: coord → (target lod, job epoch). A
    /// result is applied only if its (lod, epoch) still matches — anything else is
    /// stale (the chunk was re-dirtied, re-ringed, or the scene changed) and drops.
    pub pending: HashMap<[i32; 3], (u8, u64)>,
    /// Data chunks whose last mesh came out empty (a band remnant with no zero
    /// crossing). Without this the coverage scan would re-queue them every frame
    /// forever; a brush dirtying the chunk clears its entry.
    pub empty: std::collections::HashSet<[i32; 3]>,
    /// Far-body impostor mode: the camera is so far from this (celestial) terrain
    /// that chunk meshes would be sub-pixel noise — streaming stops, resident
    /// chunks are evicted, and the body draws as one shaded sphere instead
    /// (radial star light gives it the correct terminator for free).
    pub impostor: bool,
    /// The impostor sphere's albedo — the body's average surface color, sampled
    /// from the field once on the first switch to impostor mode.
    pub impostor_color: Option<[f32; 3]>,
    /// The meshes of the chunks drawn at the coarsest level, kept so they can
    /// be drawn [`FAR_GROUP`]³ to a draw ([`Self::groups`]).
    pub far: HashMap<[i32; 3], floptle_field::ChunkMesh>,
    /// One merged mesh per group of far chunks, keyed by group.
    pub groups: HashMap<[i32; 3], MeshId>,
    /// Groups whose far chunks changed since their mesh was merged.
    pub groups_dirty: std::collections::HashSet<[i32; 3]>,
}

/// Chunks along each edge of a far group: the coarsest chunks are drawn this
/// many cubed to a draw. One draw a chunk was 6,100 draws for a 1 km planet
/// seen from orbit, and most of a 24 ms frame.
pub(crate) const FAR_GROUP: i32 = 4;

/// The level whose chunks are drawn in groups: stride 8, beyond the last ring.
const FAR_LOD: u8 = 3;

fn far_group_of(c: [i32; 3]) -> [i32; 3] {
    c.map(|v| v.div_euclid(FAR_GROUP))
}

impl TerrainRender {
    /// Forget chunk `c`'s far mesh, if it had one: it is no longer drawn at the
    /// coarsest level, and its group is merged again without it.
    fn forget_far(&mut self, c: [i32; 3]) {
        if self.far.remove(&c).is_some() {
            self.groups_dirty.insert(far_group_of(c));
        }
    }

    /// Free every mesh this terrain draws: its chunks' and its groups'.
    pub(crate) fn free_all(&mut self, raster: &mut floptle_render::Raster) {
        for (_, (mid, _)) in self.slots.drain() {
            raster.free_dynamic(mid);
        }
        for (_, mid) in self.groups.drain() {
            raster.free_dynamic(mid);
        }
        self.far.clear();
        self.groups_dirty.clear();
    }

    /// Merge each changed group's far chunks into its one mesh.
    fn merge_far_groups(&mut self, gpu: &floptle_render::Gpu, raster: &mut floptle_render::Raster, chunk_units: f32) {
        for g in std::mem::take(&mut self.groups_dirty) {
            let origin = Vec3::new(g[0] as f32, g[1] as f32, g[2] as f32) * (FAR_GROUP as f32 * chunk_units);
            let mut merged = floptle_field::ChunkMesh { origin: origin.to_array(), ..Default::default() };
            for dz in 0..FAR_GROUP {
                for dy in 0..FAR_GROUP {
                    for dx in 0..FAR_GROUP {
                        let c = [g[0] * FAR_GROUP + dx, g[1] * FAR_GROUP + dy, g[2] * FAR_GROUP + dz];
                        let Some(m) = self.far.get(&c) else { continue };
                        let shift = Vec3::from(m.origin) - origin;
                        let base = merged.positions.len() as u32;
                        merged.positions.extend(m.positions.iter().map(|p| (Vec3::from(*p) + shift).to_array()));
                        merged.normals.extend_from_slice(&m.normals);
                        merged.colors.extend_from_slice(&m.colors);
                        merged.indices.extend(m.indices.iter().map(|i| i + base));
                    }
                }
            }
            if merged.is_empty() {
                if let Some(mid) = self.groups.remove(&g) {
                    raster.free_dynamic(mid);
                }
                continue;
            }
            let data = floptle_render::chunk_mesh_data(&merged);
            match self.groups.get(&g).copied() {
                Some(mid) if raster.replace_dynamic(gpu, mid, &data) => {}
                old => {
                    if let Some(mid) = old {
                        raster.free_dynamic(mid);
                    }
                    let id = raster.register_dynamic(gpu, data.vertices.len() as u32, data.indices.len() as u32, true);
                    raster.replace_dynamic(gpu, id, &data);
                    self.groups.insert(g, id);
                }
            }
        }
    }
}

/// P4 LOD ring radii, in chunks of Chebyshev distance from the camera's chunk:
/// within `RINGS[l]` → stride `2^l`; beyond the last ring → stride 8. One chunk
/// ≈ 48 units at the default 1.5-unit voxel.
///
/// These are the radii for a world big enough that "24 chunks away" is over the
/// horizon. On a small body they are not — see [`rings_for_body`].
const LOD_RINGS: [i32; 3] = [4, 10, 24];

/// What fraction of a body's radius each ring should reach, on a body small
/// enough for the absolute rings to swallow it whole.
///
/// A walkable planet is 100–250 units across the radius and one chunk is ~48,
/// so `LOD_RINGS[0]` alone — 192 units — contains the entire body. Standing on
/// a 180-unit world meant every one of its ~177 surface chunks was queued for
/// surface-net meshing at full detail, through a 16-deep worker queue: that is
/// the arrival hitch, and the pop-in is the queue draining.
///
/// The rings were always trying to say "detail near you, coarse over the
/// horizon". On a small body that sentence has to be written in units of the
/// body, because its horizon is a few dozen metres away rather than a few
/// hundred.
const BODY_RING_FRACTION: [f64; 3] = [0.15, 0.40, 1.0];

/// The ground within this many metres of the camera is always full detail:
/// what a player walks on, digs and shoots at is drawn as the collider has it.
const NEAR_METRES: f64 = 64.0;
/// …and nothing within this many metres is coarser than stride 2.
const MID_METRES: f64 = 120.0;

/// Ring distances are measured in quarter chunks: in whole chunks, rounding up
/// made a 64 m ring reach 96 m on a 48 m chunk, and doubled what was meshed at
/// full detail.
const RING_STEPS: i32 = 4;

/// The LOD rings to use for a terrain, given the body radius it belongs to
/// (`None` for ordinary, non-celestial terrain).
///
/// Never larger than the absolute rings, so a big world is untouched: this can
/// only tighten. Never smaller than one chunk per ring either — a body you can
/// stand on always gets a ring of full detail under your feet and two coarser
/// ones around it, however small it is.
fn rings_for_body(body_radius: Option<f64>, chunk_units: f64) -> [i32; 3] {
    let chunk = chunk_units.max(1e-3);
    let steps = |metres: f64| (metres / chunk * RING_STEPS as f64).ceil() as i32;
    // What a player can touch is drawn as it collides, however small the body:
    // full detail within `NEAR_METRES`, nothing coarser than stride 2 within
    // `MID_METRES`.
    let (near, mid) = (steps(NEAR_METRES), steps(MID_METRES));
    let mut out = LOD_RINGS.map(|r| r * RING_STEPS);
    if let Some(r) = body_radius.filter(|r| *r > 0.0) {
        for (i, frac) in BODY_RING_FRACTION.iter().enumerate() {
            out[i] = out[i].min(steps(r * frac)).max((i as i32 + 1) * RING_STEPS);
        }
    }
    out[0] = out[0].max(near);
    out[1] = out[1].max(mid);
    // Rings must stay strictly ordered, a chunk apart, or `lod_for`'s
    // hysteresis compares against a boundary that is behind it.
    out[1] = out[1].max(out[0] + RING_STEPS);
    out[2] = out[2].max(out[1] + RING_STEPS);
    out
}

/// A celestial terrain switches to its sphere impostor beyond this many body
/// radii of camera distance (~2° of angular diameter — chunk meshes would be
/// a handful of pixels).
const IMPOSTOR_RADII: f64 = 60.0;
/// …and back to its meshes only inside this fraction of that (42 radii). The
/// switch frees every chunk mesh and the way back remeshes the whole body, so
/// a camera parked near the line must not flip it.
const IMPOSTOR_LEAVE: f64 = 0.7;

/// Whether a celestial terrain of `radius` is drawn as its impostor when its
/// LOD centre is `dist` away, given whether it was last frame: past `enter`
/// of its radii ([`impostor_enter_radii`]).
fn impostor_next(was: bool, dist: f64, radius: f64, enter: f64) -> bool {
    let enter = radius.max(1.0) * enter;
    if was { dist > enter * IMPOSTOR_LEAVE } else { dist > enter }
}

/// How many of its radii from the camera a body starts drawing as its
/// impostor: sixty, unless a game set its own rule (`terrain.impostorAt`).
/// A rule in pixels is the distance its disc is that many pixels across at,
/// `px_per_radian` being the view's pixels per radian of height.
fn impostor_enter_radii(rule: Option<floptle_script::ImpostorRule>, px_per_radian: f64) -> f64 {
    match rule {
        None => IMPOSTOR_RADII,
        Some(floptle_script::ImpostorRule::Radii(n)) => n.max(0.0),
        Some(floptle_script::ImpostorRule::Pixels(px)) => 2.0 * px_per_radian / px.max(1e-3),
    }
}

/// G1 residency (docs/subsystems/large-world-space.md): a cold celestial terrain's
/// field starts loading (background) when the camera comes inside this many body
/// radii — outside the impostor flip at 60, so the field is always resident
/// before its meshes could possibly draw. Evict sits farther out again, so
/// load/evict can never thrash and every transition happens while the body is
/// an impostor (visually invisible).
const RESIDENT_LOAD_RADII: f64 = 80.0;
/// A resident celestial terrain beyond this many body radii is evicted: saved to
/// disk first when its field changed (edit mode), then dropped to [`ColdTerrain`].
const RESIDENT_EVICT_RADII: f64 = 110.0;
/// Emergency: something is inside this many radii of a still-cold body (teleport,
/// summon, warp overshoot) — load synchronously, a hitch beats falling through.
pub(crate) const RESIDENT_SYNC_RADII: f64 = 5.0;

/// A celestial terrain with no field in RAM (G1 residency). The body still
/// orbits on rails and draws as its impostor sphere from the cached color.
pub(crate) struct ColdTerrain {
    pub id: u32,
    pub color: [f32; 3],
}

/// Where a cold terrain's field comes from when it streams in (G2): a `.cfield`
/// on disk (save-slot or project), or on-demand generation from the node's
/// RON genspec — the galaxy path, where unvisited worlds have no file at all.
enum TerrainSource {
    File(PathBuf),
    Generate(String),
}

/// One in-flight background terrain load/generation (G1/G2 streaming).
pub(crate) struct TerrainLoadJob {
    pub e: Entity,
    pub name: String,
    pub started: floptle_core::time::Instant,
    pub rx: std::sync::mpsc::Receiver<Option<EditorTerrain>>,
}

/// How far out, in its radii, a body's ground can reach: what
/// `terrain.ready` reports as not made while the body is not resident.
const UNMADE_REACH: f64 = 1.5;

/// How far round the player, in chunks, a planet streamed in is made before it
/// is ready; the rest is made on approach, nearest first.
const NEAR_ON_LOAD_CHUNKS: f32 = 3.0;

/// Milliseconds of script terrain edits applied a frame (at least one edit),
/// unless the game sets another with `terrain.editBudget`.
const TERRAIN_EDIT_BUDGET_MS: f64 = floptle_script::TERRAIN_EDIT_BUDGET_MS;
/// …and at most this many shadow-proxy cells refreshed after them at that
/// budget, about a millisecond in a release build; a raised budget refreshes
/// as many more.
const TERRAIN_EDIT_PROXY_CELLS: usize = 4_000;

/// Where a script terrain op lands, worked out the frame it was made.
#[derive(Clone, Copy)]
pub(crate) struct OpTarget {
    e: Entity,
    /// The op's point in the terrain's own frame.
    local: Vec3,
    /// The terrain's scale, world units to its own.
    ts: f32,
    /// A flatten's plane normal, in the terrain's frame.
    flat_normal: Option<Vec3>,
}

/// An op this many voxels across its radius or more is made off the frame:
/// one 20 m flatten was 100 to 400 ms of a frame under load, past any
/// budget, since a budget is checked between ops and cannot split one.
const BIG_OP_VOXELS: f32 = 4.0;

#[cfg(test)]
thread_local! {
    /// Big ops ([`BIG_OP_VOXELS`]) made on the calling thread rather than on
    /// a worker. Test-only.
    static BIG_OPS_ON_FRAME: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The most ops handed to an op worker ahead of what has landed. The worker
/// makes them one after another while the frames go on, and each frame lands
/// what it has made: one op a frame was 1,240 frames for a loading screen's
/// grade, the worker idle most of each.
const OP_JOB_AHEAD: usize = 32;

/// Voxels past an op's reach that its write may make ground in, where the
/// ground there has not been made yet (`ChunkField::realize_voxels` reads 8
/// past the box it writes).
const OP_REALIZE_VOXELS: f32 = 9.0;

/// Big script terrain ops being made on a worker, in order, on a copy of
/// their terrain's field. The queue waits behind them, so ops land in order,
/// and until each lands every query reads the field as it was before it.
pub(crate) struct OpJob {
    e: Entity,
    /// The worker's next ops: more are handed over as they reach the front of
    /// the queue, and the worker ends once this is dropped.
    tx: std::sync::mpsc::Sender<(floptle_script::TerrainOp, OpTarget)>,
    rx: std::sync::mpsc::Receiver<OpDone>,
    /// Ops handed over and not landed yet.
    in_flight: usize,
    /// Ops handed over, and how many of them the worker has made: what a
    /// test waits on.
    #[cfg(test)]
    handed: usize,
    #[cfg(test)]
    made: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// A big op, made: the copy it was made on, what it wrote, the chunks it
/// made from nothing, and the proxy cells over its box sampled from it.
struct OpDone {
    op: floptle_script::TerrainOp,
    at: OpTarget,
    field: floptle_field::ChunkField,
    touched: Vec<[i32; 3]>,
    /// Chunks not made yet that the write made first (exactly as the
    /// generator makes them), written or not.
    realized: Vec<[i32; 3]>,
    changed: Option<([i32; 3], [i32; 3])>,
    measured: Option<floptle_field::SculptYield>,
    region: floptle_field::DenseRegion,
}

/// Make a script op on `field` (in its terrain's frame): the chunks it wrote,
/// and for a sculpt what it moved. The one place an op is made, inline or on
/// a worker, so the two cannot come out different.
fn make_op(field: &mut floptle_field::ChunkField, op: &floptle_script::TerrainOp, at: &OpTarget) -> (Vec<[i32; 3]>, Option<floptle_field::SculptYield>) {
    use floptle_field::{Brush, BrushProfile};
    use floptle_script::TerrainOpMode as M;
    let OpTarget { local, ts, flat_normal, .. } = *at;
    let r_local = op.radius / ts;
    let profile = BrushProfile::default();
    match op.mode {
        M::Flatten => (field.flatten_to_plane(local, flat_normal.unwrap_or(Vec3::Y), r_local, op.strength, profile), None),
        M::Raise | M::Lower | M::Smooth => {
            let brush = match op.mode {
                M::Raise => Brush::Raise,
                M::Lower => Brush::Lower,
                _ => Brush::Smooth,
            };
            let (touched, y) = field.sculpt_measured(brush, local, r_local, op.strength, profile);
            (touched, Some(y))
        }
        M::Paint(c) => (field.paint(local, r_local, op.strength, c, profile), None),
        M::PaintTexture(slot) => (field.paint_texture(local, r_local, slot), None),
    }
}

/// A script terrain op waiting its turn, with where it lands; `None` when no
/// terrain was near enough when it was made.
pub(crate) struct QueuedEdit {
    op: floptle_script::TerrainOp,
    at: Option<OpTarget>,
}

/// Chunks being made again on a worker for one terrain: put away while the
/// camera was far, and wanted again as it comes near.
pub(crate) struct PromotionJob {
    e: Entity,
    coords: Vec<[i32; 3]>,
    rx: std::sync::mpsc::Receiver<Vec<([i32; 3], floptle_field::Generated)>>,
}

/// The LOD ring past which a planet's ground can be put away
/// ([`floptle_field::ChunkField::evict`]): beyond it chunks are drawn at
/// stride 4 or 8, whose corners are the stand-in's own points, and it holds
/// what a player can walk to in a few seconds.
const FAR_RING: usize = 1;
/// The coarsest level drawn from voxels a chunk must hold: finer than stride
/// 4, a chunk put away is made again first.
const FAR_FROM_LOD: u8 = 2;
/// A chunk is put away once the camera is this many quarter-chunk steps past
/// [`FAR_RING`] from it…
const EVICT_PAST_RING: i32 = 2 * RING_STEPS;
/// …and made again once it is within this many, a chunk sooner than any finer
/// stride asks for its voxels.
const PROMOTE_PAST_RING: i32 = RING_STEPS;
/// The most moving bodies whose ground streaming and detail follow, besides the
/// camera: the nearest to it.
const MOVING_BODY_ANCHORS: usize = 32;

/// Chunks a promotion job makes.
const PROMOTE_PER_JOB: usize = 4;
/// The far-chunk scan walks every chunk of a planet; it runs every this many
/// frames.
pub(crate) const FAR_SCAN_FRAMES: u64 = 8;
/// Memory let go of goes back to the system at most this often.
const TRIM_EVERY_SECS: f64 = 2.0;

/// Hand memory the program has let go of back to the system.
///
/// The system allocator keeps what is freed for the program to use again,
/// and gives the system back little of it: freed in the middle of its heaps,
/// and in the heaps of the worker threads that generated a world, it stays
/// in the process. A 650 m world held 1.2 GB more than it used once it had
/// generated, and putting nine tenths of its chunks away freed 400 MB that
/// the process went on holding (Solar measured 2.36 against 2.23 GB, for a
/// field ten times smaller). Trimmed, the same world came down from 1.14 GB
/// to 0.67 GB. Only glibc keeps memory this way; elsewhere it does nothing.
pub(crate) fn give_memory_back() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only returns free pages; it touches no live allocation.
    unsafe {
        libc::malloc_trim(0);
    }
}
/// …and puts away at most this many chunks of a terrain a scan.
const EVICT_PER_SCAN: usize = 96;

/// One in-flight background checkpoint (`terrain.flush()` → the save slot):
/// the field encodes a few chunks per frame on the main thread (it can't leave
/// it — scripts keep digging it), then the finished blob writes on a thread.
pub(crate) struct TerrainSaveJob {
    pub e: Entity,
    /// Body name at job start — console lines outlive the entity.
    pub name: String,
    /// Slot file the blob lands in (captured at start: save dir can't change
    /// under a running job).
    pub path: PathBuf,
    /// `terrain_edit_stamps` counter at job start. Still equal when the encode
    /// finishes ⇒ the blob is a clean snapshot and the dirty flag clears; an
    /// edit raced in ⇒ the blob is torn (valid, just mixed generations) — it
    /// still writes (newer than any previous file) but the field stays dirty
    /// so the next checkpoint re-saves it whole.
    pub stamp: u64,
    pub state: TerrainSaveState,
}

pub(crate) enum TerrainSaveState {
    /// Amortized `FieldSaver` encoding — driven a budget of chunks per frame.
    Encoding(floptle_field::FieldSaver),
    /// Blob handed to a writer thread; the channel reports bytes written.
    Writing(std::sync::mpsc::Receiver<Result<usize, String>>),
}

/// Chunks encoded per frame while checkpointing (~1 ms of RLE walking — the
/// whole point: a dug-up planet serializes over a few dozen frames instead of
/// freezing one).
const CHECKPOINT_CHUNKS_PER_FRAME: usize = 48;
/// A field edited more recently than this is skipped by the checkpoint picker —
/// saves happen in the quiet moments between digs, never under the shovel.
const CHECKPOINT_QUIET_SECS: f64 = 1.5;
/// …unless it has been waiting this long (someone digging non-stop): start
/// anyway — a torn snapshot beats a checkpoint that never happens.
const CHECKPOINT_FORCE_SECS: f64 = 20.0;

/// Run a load to completion, trying each candidate source in order — a
/// truncated/corrupt field file falls back to the next source (usually the
/// genspec, which regenerates the body deterministically) instead of failing
/// the whole stream: a bad file must never take a world offline when its
/// recipe is right there on the node. File read+parse or full procgen, plus
/// the shadow-proxy derivation; runs on background threads (and blockingly
/// for file sources at the emergency radius). None = every candidate failed.
///
/// `genspec` is the body's recipe, passed apart from the sources: a load that
/// may not generate (a batch owns the cores) still needs it, to find a whole-
/// field save's edits. Dropped with the Generate source, an old save loaded,
/// never learnt its edits, and was written back whole on every save.
///
/// `near`, where the player is in the body's own frame: a generation makes
/// only the ground within a few chunks of it, and stands the rest in coarse,
/// to be made on approach ([`Editor::stream_far_chunks`]). `None` makes it
/// all.
///
/// `generator` is the project's planet generator version, what a recipe that
/// names none is made by ([`resolve_spec`]).
///
/// `view` is where the meshes the landing frame would make are made round,
/// here, with the field ([`EditorTerrain::premesh`]).
fn load_terrain_from(
    sources: Vec<TerrainSource>,
    genspec: Option<String>,
    near: Option<Vec3>,
    generator: u32,
    view: Option<LandingView>,
) -> Option<EditorTerrain> {
    let make = |fill: &floptle_field::procgen::PlanetFill| match near {
        Some(p) => floptle_field::procgen::generate_planet_near(fill, p, NEAR_ON_LOAD_CHUNKS * floptle_field::CHUNK as f32 * fill.voxel),
        None => floptle_field::procgen::generate_planet(fill),
    };
    // A saved field of a planet whose caves wait knows which chunks are still
    // to make but not how: the body's genspec, when it has one, does.
    let genspec = genspec.or_else(|| {
        sources.iter().find_map(|s| match s {
            TerrainSource::Generate(spec) => Some(spec.clone()),
            TerrainSource::File(_) => None,
        })
    });
    let spec = genspec.as_deref().and_then(|s| resolve_spec(s, generator));
    for src in sources {
        let field = match src {
            TerrainSource::File(path) => floptle_vfs::read(&path)
                .ok()
                .and_then(|b| floptle_field::ChunkField::from_bytes(&b))
                .and_then(|f| {
                    if f.is_delta() {
                        // Only the edits: laid over the ground they were dug
                        // into, made again from the recipe the save carries.
                        let (fill, text) = resolve_spec(f.spec()?, generator.max(DELTA_SAVES_SINCE))?;
                        let mut f = f.lay_over(make(&fill));
                        f.set_spec(text);
                        return Some(f);
                    }
                    let mut f = f;
                    if let Some((fill, text)) = spec.as_ref() {
                        // A save from before edits were tracked: what differs
                        // from a fresh generation is what was dug, and from
                        // here on only that is saved.
                        f.set_spec(text.clone());
                        f.adopt_generation(&floptle_field::procgen::generate_planet(fill));
                    } else if f.pending_count() > 0
                        && let Some(source) = spec.as_ref().and_then(|(fill, _)| floptle_field::procgen::pending_source_for(fill))
                    {
                        f.set_pending_source(source);
                    }
                    Some(f)
                }),
            TerrainSource::Generate(text) => resolve_spec(&text, generator).map(|(fill, text)| {
                let mut f = make(&fill);
                f.set_spec(text);
                f
            }),
        };
        if let Some(field) = field {
            let mut t = EditorTerrain::new(field);
            if let Some(v) = view {
                t.premesh(&v);
            }
            return Some(t);
        }
    }
    None
}

/// A field read from a save holding only its edits, laid over the ground they
/// were dug into, whose chunks are made again from the recipe the save
/// carries as the camera comes near them. A whole field comes back as it is.
/// `None` when the recipe does not parse.
fn whole_field(f: floptle_field::ChunkField, generator: u32) -> Option<floptle_field::ChunkField> {
    if !f.is_delta() {
        return Some(f);
    }
    let (fill, text) = resolve_spec(f.spec()?, generator.max(DELTA_SAVES_SINCE))?;
    // Opening a scene, on the frame: nothing is made but the edits, and the
    // rest of the world stands in coarse until the camera comes near it.
    let nowhere = Vec3::splat(f32::MAX);
    let mut f = f.lay_over(floptle_field::procgen::generate_planet_near(&fill, nowhere, 0.0));
    f.set_spec(text);
    Some(f)
}

/// The generator version saves of only a planet's edits began with: both of
/// the first version's changes shipped in the same release as they did, so
/// such a save is never older, whatever its project says.
const DELTA_SAVES_SINCE: u32 = floptle_field::procgen::RIVERS_FOLLOW_DRAINAGE;

/// A planet recipe read from text, made by the version it names or, naming
/// none, by `generator`: the project's. With the text it is kept as from here,
/// version and all, so a save made from it says which rules made its ground.
fn resolve_spec(text: &str, generator: u32) -> Option<(floptle_field::procgen::PlanetFill, String)> {
    let mut fill: floptle_field::procgen::PlanetFill = ron::from_str(text).ok()?;
    if fill.generator.is_some() {
        return Some((fill, text.to_string()));
    }
    fill.generator = Some(generator);
    let text = ron::to_string(&fill).ok()?;
    Some((fill, text))
}

/// Stable hash of a genspec string — recorded in the `.meta` sidecar when a
/// field is written, so a project field file is only trusted for a body whose
/// genspec still matches. Regenerating a system reuses terrain ids: without
/// this, the old system's leftover `<scene>.<id>.cfield` would load as the new
/// body's terrain (the wrong planet entirely). Save-slot files skip the check —
/// a slot belongs to one galaxy seed by construction (the game's contract).
fn genspec_hash(spec: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in spec.as_bytes() {
        h = h.wrapping_mul(33) ^ u64::from(*b);
    }
    h
}

/// A genspec body's impostor color without generating anything: its primary
/// surface paint as it is drawn (close enough for a sub-pixel-to-few-pixel
/// sphere). `means` is the palette's, see [`drawn_color`].
fn genspec_impostor_color(spec: &str, means: &[[f32; 3]]) -> Option<[f32; 3]> {
    let fill: floptle_field::procgen::PlanetFill = ron::from_str(spec).ok()?;
    let [r, g, b] = fill.surface_a.color.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8);
    Some(drawn_color([r, g, b, fill.surface_a.slot], means))
}

/// A voxel's paint as the ground draws it, on average: its tint, and for a
/// textured slot the tint times the mean of the slot's palette texture, lifted
/// as the terrain shader lifts it. `means` holds each palette layer's mean
/// colour, linear (the layers are sampled as sRGB); a slot past them draws
/// white, as the shader's blank layer does. A ball drawn in the tint alone
/// was olive over brown ground.
pub(crate) fn drawn_color(c: [u8; 4], means: &[[f32; 3]]) -> [f32; 3] {
    let tint = [c[0], c[1], c[2]].map(|v| f32::from(v) / 255.0);
    if c[3] == 0 || c[3] == 255 {
        return tint;
    }
    let m = means.get(usize::from(c[3]) - 1).copied().unwrap_or([1.0; 3]);
    [tint[0] * m[0] * 1.6, tint[1] * m[1] * 1.6, tint[2] * m[2] * 1.6]
}

/// The mean colour of one palette layer, linear.
pub(crate) fn layer_mean(t: &floptle_render::TextureData) -> [f32; 3] {
    let lin = |v: u8| {
        let c = f32::from(v) / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    let mut sum = [0.0f64; 3];
    let n = (t.pixels.len() / 4).max(1);
    for p in t.pixels.as_chunks::<4>().0 {
        for k in 0..3 {
            sum[k] += f64::from(lin(p[k]));
        }
    }
    sum.map(|v| (v / n as f64) as f32)
}

/// The body's average surface color for its impostor sphere: 26 rays from
/// outside toward the center, averaging the color the ground is drawn where
/// each first hits ([`drawn_color`], with the palette's `means`).
/// Field-space — celestial terrain fields are authored centered on the origin.
pub(crate) fn impostor_surface_color(field: &floptle_field::ChunkField, radius: f32, means: &[[f32; 3]]) -> [f32; 3] {
    let mut sum = [0.0f32; 3];
    let mut n = 0.0f32;
    for x in -1..=1i32 {
        for y in -1..=1i32 {
            for z in -1..=1i32 {
                if x == 0 && y == 0 && z == 0 {
                    continue;
                }
                let d = Vec3::new(x as f32, y as f32, z as f32).normalize();
                let start = d * (radius * 1.6 + 8.0);
                if let Some(hit) = field.raycast(start, -d, radius * 3.2) {
                    let c = drawn_color(field.color(hit), means);
                    sum[0] += c[0];
                    sum[1] += c[1];
                    sum[2] += c[2];
                    n += 1.0;
                }
            }
        }
    }
    if n == 0.0 {
        return [0.75, 0.75, 0.78];
    }
    [sum[0] / n, sum[1] / n, sum[2] / n]
}

/// The ring a distance lands in, no hysteresis — for chunks with no current lod.
fn raw_lod(dist: i32, rings: [i32; 3]) -> u8 {
    if dist <= rings[0] {
        0
    } else if dist <= rings[1] {
        1
    } else if dist <= rings[2] {
        2
    } else {
        3
    }
}

/// The ring a distance lands in, with ±1 chunk of hysteresis against the chunk's
/// current lod so camera drift across a boundary can't thrash remeshing.
fn lod_for(dist: i32, cur: u8, rings: [i32; 3]) -> u8 {
    let raw = raw_lod(dist, rings);
    if raw == cur {
        cur
    } else if raw > cur {
        // Coarsen only once clearly past the boundary above the current ring.
        if dist > rings[(cur as usize).min(2)] + RING_STEPS {
            raw
        } else {
            cur
        }
    } else {
        // Refine as soon as it is inside the finer ring. Only coarsening waits:
        // a ring of one chunk refined only at distance zero, so the chunk ahead
        // of a walking player stayed coarse until they were standing in it.
        raw
    }
}

/// A queued remesh: the scratch is gathered on the main thread (it owns the field);
/// the worker meshes from the scratch alone and never touches editor state (T4).
pub(crate) struct RemeshJob {
    pub entity: Entity,
    pub coord: [i32; 3],
    pub lod: u8,
    pub epoch: u64,
    pub scratch: JobScratch,
    /// A collision mesh, bucketed this many units across, rather than a drawn
    /// one: no skirt (as collision it would be an invisible wall round every
    /// chunk), and the bucketing done here too.
    pub collision: Option<f32>,
}

/// What a remesh job meshes from: the voxels round its chunk, gathered, or
/// the field to gather them from on the worker. A coarse chunk reads a dense
/// block of the field half again its size on every side (half a million
/// voxels at stride 8), and gathered on the frame, a world that had just
/// landed spent 190 ms a frame (debug) on it for seconds.
pub(crate) enum JobScratch {
    Ready(floptle_field::MeshScratch),
    /// A copy of the field (its chunks are shared), and the stride.
    Field(std::sync::Arc<floptle_field::ChunkField>, i32),
}

/// Drawn chunks meshed, by where their voxels were gathered: `[on a worker,
/// on the frame]`.
#[cfg(test)]
pub(crate) static DRAWN_GATHERS: [std::sync::atomic::AtomicUsize; 2] =
    [std::sync::atomic::AtomicUsize::new(0), std::sync::atomic::AtomicUsize::new(0)];

impl JobScratch {
    fn into_scratch(self, coord: [i32; 3]) -> floptle_field::MeshScratch {
        match self {
            JobScratch::Ready(s) => s,
            JobScratch::Field(f, stride) => floptle_field::scratch_for_chunk(&f, coord, stride),
        }
    }
}

pub(crate) struct RemeshDone {
    pub entity: Entity,
    pub coord: [i32; 3],
    pub lod: u8,
    pub epoch: u64,
    pub mesh: floptle_field::ChunkMesh,
    /// The bucketed collision triangles, for a collision job.
    pub collision: Option<floptle_physics::CollisionChunk>,
}

/// The background remesh worker (P4): one thread, jobs in / meshes out over
/// channels — the same shape as the audio decode worker. Dropping the sender on
/// editor exit ends the thread.
pub(crate) struct TerrainWorker {
    tx: std::sync::mpsc::Sender<RemeshJob>,
    rx: std::sync::mpsc::Receiver<RemeshDone>,
    /// The worker's two ends, where there is no worker thread to hold them —
    /// see [`Self::pump`].
    #[cfg(target_arch = "wasm32")]
    inline: (std::sync::mpsc::Receiver<RemeshJob>, std::sync::mpsc::Sender<RemeshDone>),
    /// Jobs sent and not yet drained — the main thread caps this so a big LOD
    /// migration streams in nearest-first instead of flooding the queue.
    pub in_flight: usize,
    /// Results [`Self::wait`] took off the channel, for the next `try_recv`.
    early: Vec<RemeshDone>,
}

/// At most this many queued-but-unfinished jobs. Each job's scratch is ~1 MB at full
/// detail and ~4 MB at stride 8, so the cap also bounds transient memory.
const WORKER_IN_FLIGHT_CAP: usize = 16;

/// Subtracted from a dirty (brush/script-edited) chunk's queue priority: edits sort
/// ahead of every LOD migration and bypass the in-flight cap — a stale mesh under
/// the player is worse than a deep queue.
const DIRTY_PRIORITY_BOOST: i32 = 1_000_000;

/// How close to a body's centre, in body radii, counts as being on it.
///
/// Generous — an aircraft at altitude, or a ship on approach, is still landing
/// on the thing under it and still wants that ground first.
const ON_BODY_RADII: f64 = 3.0;

/// Where a chunk sits in the meshing queue: **metres from the camera**, so that
/// chunks belonging to different terrains can be compared at all.
///
/// One queue is shared by every resident terrain. Keyed on chunk distance in
/// each terrain's own local frame, a chunk three chunks from the camera on the
/// planet under your feet would tie with one three chunks from the camera on a
/// planet twelve thousand units away, and `HashMap` order would pick the
/// worker slot. Distances in different terrains are only comparable once they
/// are in the same units.
pub(crate) fn chunk_priority(
    coord: [i32; 3],
    chunk_world: f32,
    anchor: DVec3,
    rot: Quat,
    cam_world: DVec3,
    on_body: bool,
) -> i32 {
    let mid = Vec3::new(coord[0] as f32 + 0.5, coord[1] as f32 + 0.5, coord[2] as f32 + 0.5);
    let world = anchor + (rot * (mid * chunk_world)).as_dvec3();
    // Saturating: a body on the far side of a solar system is millions of units
    // away and must not wrap into the front of the queue.
    let metres = (world - cam_world).length().min(i32::MAX as f64 / 4.0) as i32;
    if on_body { metres } else { metres.saturating_add(OFF_BODY_PENALTY) }
}

/// Frames after an edit that its collision meshes are due: they land then
/// whether or not the worker finished sooner (see `pump_collision_meshes`).
const COLLISION_MESH_FRAMES: u64 = 2;

/// Added to every chunk of a body the camera is not on.
///
/// Metres alone gets one case wrong: standing between two worlds, a chunk under
/// your feet and a chunk on the horizon of the world you are landing on are
/// similar distances away, and only one of them is holding you up. Bigger than
/// any distance a streaming body is at (past sixty radii it is an impostor and
/// queues nothing at all), smaller than the dirty boost, so a live sculpt still
/// outranks everything.
const OFF_BODY_PENALTY: i32 = 100_000;

impl TerrainWorker {
    pub(crate) fn spawn() -> Self {
        let (jtx, jrx) = std::sync::mpsc::channel::<RemeshJob>();
        let (dtx, drx) = std::sync::mpsc::channel::<RemeshDone>();
        // A few threads share the queue. One alone was the mining laser's
        // hitch on a busy machine: a bite dirties up to eight collision chunks
        // at a millisecond or two each, due two frames later, and when the one
        // thread had not finished them the frame waited for it.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let threads = std::thread::available_parallelism().map_or(2, |n| (n.get() / 4).clamp(1, 4));
            let jrx = std::sync::Arc::new(std::sync::Mutex::new(jrx));
            for _ in 0..threads {
                let (jrx, dtx) = (jrx.clone(), dtx.clone());
                crate::worker::spawn("terrain-remesh", move || {
                    loop {
                        // Take one job with the queue held, run it with the queue free.
                        let job = match jrx.lock() {
                            Ok(q) => q.recv(),
                            Err(_) => break,
                        };
                        let Ok(job) = job else { break };
                        if !Self::run(job, &dtx) {
                            break; // editor gone
                        }
                    }
                });
            }
            Self { tx: jtx, rx: drx, in_flight: 0, early: Vec::new() }
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self { tx: jtx, rx: drx, in_flight: 0, early: Vec::new(), inline: (jrx, dtx) }
        }
    }

    /// One job: mesh the scratch and hand the result back. `false` when the
    /// receiving side is gone.
    fn run(job: RemeshJob, done: &std::sync::mpsc::Sender<RemeshDone>) -> bool {
        #[cfg(test)]
        if job.collision.is_none() {
            let on_frame = matches!(job.scratch, JobScratch::Ready(_));
            DRAWN_GATHERS[on_frame as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let scratch = job.scratch.into_scratch(job.coord);
        let mesh = floptle_field::mesh_scratch(&scratch, job.collision.is_none());
        let collision = job.collision.map(|cell| floptle_physics::CollisionChunk::from_mesh(&mesh, cell));
        done.send(RemeshDone { entity: job.entity, coord: job.coord, lod: job.lod, epoch: job.epoch, mesh, collision })
            .is_ok()
    }

    /// A browser has no worker thread: every job queued so far is run now,
    /// on this frame, and its answer is waiting in the same channel the
    /// desktop drains a frame later.
    #[cfg(target_arch = "wasm32")]
    fn pump(&mut self) {
        while let Ok(job) = self.inline.0.try_recv() {
            if !Self::run(job, &self.inline.1) {
                break;
            }
        }
    }

    pub(crate) fn send(&mut self, job: RemeshJob) {
        if self.tx.send(job).is_ok() {
            self.in_flight += 1;
        }
        #[cfg(target_arch = "wasm32")]
        self.pump();
    }

    /// Block until the next result is in the channel (not taking it), or a
    /// short while passes. On the web the jobs ran when they were sent.
    pub(crate) fn wait(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Ok(d) = self.rx.recv_timeout(std::time::Duration::from_millis(50)) {
            self.early.push(d);
        }
    }

    pub(crate) fn try_recv(&mut self) -> Option<RemeshDone> {
        if let Some(d) = self.early.pop() {
            self.in_flight = self.in_flight.saturating_sub(1);
            return Some(d);
        }
        match self.rx.try_recv() {
            Ok(d) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                Some(d)
            }
            Err(_) => None,
        }
    }
}

/// The per-slot triplanar scales, packed the way the field globals want them:
/// [`floptle_render::TERRAIN_SLOTS`] multipliers, four to a `vec4`.
///
/// A missing or nonsensical entry becomes `1.0` rather than `0.0` — the list is
/// grown lazily and an older project has no scales saved at all, and a zero
/// there would collapse every terrain texture to a single texel instead of
/// leaving it tiling as it was.
pub(crate) fn terrain_scale_lanes(scales: &[f32]) -> [[f32; 4]; 8] {
    let mut out = [[1.0f32; 4]; 8];
    for i in 0..floptle_render::TERRAIN_SLOTS as usize {
        let v = scales.get(i).copied().unwrap_or(1.0);
        out[i / 4][i % 4] = if v.is_finite() && v > 0.0 { v.clamp(0.01, 100.0) } else { 1.0 };
    }
    out
}

impl Editor {
    /// Seconds since the editor started — a monotonic clock for anything that
    /// animates on wall time rather than on the play session's `play_t`, which
    /// restarts with every Play.
    ///
    /// `None` before there is a window, and an `Option` rather
    /// than a zero: a clock that never advances is not "time zero", it is no
    /// time at all, and anything measuring an age against a frozen zero would
    /// read as *permanently* mid-animation. For the terrain dissolve that
    /// means terrain that is never drawn, which is the one outcome worth
    /// making unrepresentable.
    pub(crate) fn now(&self) -> Option<f32> {
        self.started.map(|s| s.elapsed().as_secs_f32())
    }

    /// Keep every terrain's render meshes in sync with its field + the camera (P4).
    ///
    /// Per frame: (1) drain finished worker meshes (stale epochs drop); (2) on a
    /// structural change (load / new / fill / undo) re-plan every chunk; (3) brush/
    /// script-dirtied chunks remesh — the near ring synchronously (sculpting must
    /// feel instant), the rest through the worker; (4) resident chunks whose LOD
    /// ring changed (with hysteresis) re-queue; (5) queued work tops up the worker
    /// nearest-first under the in-flight cap. Called right after `sync_terrain_gpu`
    /// keeps the shadow atlas fed.
    pub(crate) fn sync_terrain_meshes(&mut self, full_rebuild: bool, cam_world: DVec3) {
        self.terrain_scan_frame = self.terrain_scan_frame.wrapping_add(1);
        // Stamped onto every chunk that arrives this frame, so a wave of them
        // dissolves in together rather than each on its own schedule.
        let now = self.now();
        if self.terrain_worker.is_none() && !self.terrains.is_empty() {
            self.terrain_worker = Some(TerrainWorker::spawn());
        }
        // Moving bodies hold the ground under them at full detail too: what a
        // body lands on is drawn as it collides, however far from the camera.
        let bodies = self.moving_bodies();
        let px_per_radian = self.view_px_per_radian();
        let slot_means = self.terrain_slot_means.clone();
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return;
        };
        // Drop render meshes for terrains that no longer exist (deleted nodes).
        // cold terrains count as live: their render entry is the impostor sphere.
        let live: Vec<Entity> = self
            .terrains
            .keys()
            .chain(self.terrain_cold.keys())
            .copied()
            .collect();
        self.terrain_render.retain(|e, r| {
            if live.contains(e) {
                return true;
            }
            r.free_all(raster);
            r.born.clear();
            false
        });
        self.terrain_chunks_dirty.retain(|e, _| live.contains(e));
        // A destroyed node's cold entry goes too (a generator replacing a system).
        {
            let world = &self.world;
            self.terrain_cold.retain(|e, _| world.is_alive(*e));
        }

        // ---- 1: land finished worker meshes (stale results drop silently) ----
        if let Some(w) = self.terrain_worker.as_mut() {
            while let Some(done) = w.try_recv() {
                let Some(render) = self.terrain_render.get_mut(&done.entity) else { continue };
                if render.pending.get(&done.coord) != Some(&(done.lod, done.epoch)) {
                    continue; // superseded while in flight
                }
                render.pending.remove(&done.coord);
                if done.mesh.is_empty() {
                    if let Some((mid, _)) = render.slots.remove(&done.coord) {
                        raster.free_dynamic(mid);
                    }
                    render.forget_far(done.coord);
                    render.born.remove(&done.coord);
                    render.empty.insert(done.coord);
                } else {
                    render.empty.remove(&done.coord);
                    upload_chunk(gpu, raster, render, done.coord, &done.mesh, done.lod, now);
                }
            }
        }

        // ---- 2..4: plan work per terrain ----
        // Candidates gather into (priority = chunk distance, entity, coord, lod);
        // sync work happens immediately, async work is topped up at the end.
        let mut queue: Vec<(i32, Entity, [i32; 3], u8)> = Vec::new();
        // Meshes made with a field that has just landed, taken for this
        // pass: drawn instead of meshed here. What a terrain's scan does not
        // use is dropped once its scan has run (the ground round the player
        // is drawn by then); a terrain whose scan waits keeps its own.
        let mut premeshed: HashMap<Entity, HashMap<[i32; 3], floptle_field::ChunkMesh>> = self
            .terrains
            .iter_mut()
            .filter(|(_, t)| !t.premeshed.is_empty())
            .map(|(&e, t)| (e, std::mem::take(&mut t.premeshed)))
            .collect();
        let mut scanned: Vec<Entity> = Vec::new();
        // Chunks with no mesh that this frame meshes at full detail itself.
        let mut near_left = if self.playing { NEAR_MESHES_A_FRAME } else { usize::MAX };
        for (&e, terrain) in &self.terrains {
            // Hidden: nothing drawn, so nothing to plan. Its meshes stay for
            // when it is shown again.
            if floptle_core::is_hidden(&self.world, e) || floptle_core::is_disabled(&self.world, e) {
                continue;
            }
            let structural = full_rebuild || !self.terrain_render.contains_key(&e) || self.terrain_replan.remove(&e);
            let dirty = self.terrain_chunks_dirty.remove(&e);
            let render = self.terrain_render.entry(e).or_default();
            let wt = floptle_core::world_transform(&self.world, e);
            let (anchor, rot, ts) =
                (wt.translation, wt.rotation.normalize(), wt.scale.x.max(1e-6));

            // Far-body impostor gate: a celestial terrain seen from far enough
            // that its whole disc is a couple of degrees wide stops streaming
            // entirely and frees its resident chunk meshes; push_terrain_instances
            // draws the shaded sphere instead, with hysteresis (see
            // `impostor_next`) so orbiting along the threshold can't thrash
            // evict/remesh cycles.
            if let Some(cb) = self.world.get::<floptle_core::CelestialBody>(e) {
                let cam_dist = (cam_world - anchor).length();
                let was = render.impostor;
                let rule = match self.world.get::<Matter>(e) {
                    Some(Matter::Terrain { id, .. }) => self.terrain_impostor_rules.get(id).copied(),
                    _ => None,
                };
                render.impostor = impostor_next(was, cam_dist, cb.body_radius, impostor_enter_radii(rule, px_per_radian));
                if render.impostor != was {
                    // The SDF shadow/AO atlas excludes impostor bodies — re-lay it
                    // out. Only the atlas: re-planning every terrain's meshes
                    // dropped the jobs in flight for all of them.
                    self.terrain_atlas_dirty = true;
                }
                // The body's surface colour is wanted whether it is far enough to
                // BE an impostor or close enough to be streaming — a streaming
                // body draws the same colour as a backstop under its arriving
                // chunks. Sampled once, on whichever comes first.
                if render.impostor_color.is_none() {
                    render.impostor_color =
                        Some(impostor_surface_color(&terrain.field, cb.body_radius as f32, &slot_means));
                }
                if render.impostor {
                    render.pending.clear();
                    render.empty.clear();
                    render.free_all(raster);
                    render.born.clear();
                    scanned.push(e);
                    continue;
                }
            } else {
                render.impostor = false;
            }
            let chunk_units = floptle_field::CHUNK as f32 * terrain.field.voxel();
            // Rings sized to the body when there is one: on a
            // world you can walk around, "24 chunks away" is the far side of it.
            let rings = rings_for_body(
                self.world
                    .get::<floptle_core::CelestialBody>(e)
                    .map(|cb| cb.body_radius * ts as f64),
                (chunk_units * ts) as f64,
            );
            // Camera into the field's local frame (rotation + uniform scale), so
            // LOD rings follow the terrain wherever its node puts it.
            let cl = (rot.inverse() * (cam_world - anchor).as_vec3()) / (chunk_units * ts);
            // LOD ring selection, in this terrain's chunk units — which is the
            // right unit for a ring, and the wrong one for a queue position.
            // How far the camera is from the nearest point of the chunk, rounded
            // up: zero inside it. The chunk ahead is as near as its near face,
            // not as near as its index.
            let dist_of = |c: [i32; 3]| chunk_ring_dist(c, cl);
            // Queue position, in metres, so chunks from different terrains can
            // be compared at all.
            //
            // One queue is shared by every resident terrain, so the key is not
            // `dist_of` (chunk distance in each terrain's own local frame): that
            // ties a chunk three chunks away on the planet under your feet with
            // one three chunks away on a planet twelve thousand units off, and
            // lets `terrains` map order pick which loads.
            // …and the ground you are standing on outranks everything, before any
            // per-chunk comparison. Two bodies can otherwise interleave when you
            // are between them, which is the one case metres alone gets wrong:
            // the chunk under your feet and a chunk on the horizon of the world
            // you are landing on are similar distances, and only one of them is
            // holding you up.
            let standing_on = self
                .world
                .get::<floptle_core::CelestialBody>(e)
                .is_none_or(|cb| (cam_world - anchor).length() < cb.body_radius * ON_BODY_RADII);
            let chunk_world = chunk_units * ts;
            let prio_of = |c: [i32; 3]| {
                chunk_priority(c, chunk_world, anchor, rot, cam_world, standing_on)
            };

            if structural {
                // Everything re-plans: drop pending (their epochs are now stale by
                // construction — the results check `pending`), free slots the field
                // no longer fills, and let the coverage scan below re-queue the rest.
                render.pending.clear();
                render.empty.clear();
                let has: std::collections::HashSet<[i32; 3]> =
                    terrain.field.chunk_coords().into_iter().collect();
                render.slots.retain(|c, (mid, _)| {
                    if has.contains(c) {
                        true
                    } else {
                        raster.free_dynamic(*mid);
                        false
                    }
                });
                let gone: Vec<[i32; 3]> = render.far.keys().filter(|c| !has.contains(*c)).copied().collect();
                for c in gone {
                    render.forget_far(c);
                }
                // A chunk the field no longer fills gives its stamp back with
                // its slot, so coming back later is an arrival and fades again.
                render.born.retain(|c, _| has.contains(c));
            }

            // Brush / script edits. The editor's brush meshes near chunks
            // synchronously (sculpting must feel instant). In Play every edit
            // goes through the worker, the chunk drawing its old mesh until the
            // new one lands a frame or two later: a mining laser biting every
            // eighth of a second spent 3+ ms of each bite's frame meshing on the
            // main thread. Dirty jobs bypass the in-flight cap (a stale mesh is
            // worse than a deep queue).
            let sync_near = !self.playing;
            let body_eyes: Vec<Vec3> =
                bodies.iter().map(|b| (rot.inverse() * (*b - anchor).as_vec3()) / (chunk_units * ts)).collect();
            let under_body = |c: [i32; 3]| {
                let lo = Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32);
                body_eyes.iter().any(|cl| {
                    let gap = (lo - *cl).max(*cl - (lo + Vec3::ONE)).max(Vec3::ZERO);
                    (gap.length() * RING_STEPS as f32).ceil() as i32 <= rings[0]
                })
            };
            let dug = |c: [i32; 3]| (terrain.field.is_generated() && terrain.field.is_edited(c)) || under_body(c);
            if let Some(mut d) = dirty {
                d.sort_unstable();
                d.dedup();
                for coord in d {
                    render.empty.remove(&coord);
                    // Made before this edit: not the chunk any more.
                    if let Some(p) = premeshed.get_mut(&e) {
                        p.remove(&coord);
                    }
                    let dist = dist_of(coord);
                    let cur = render.slots.get(&coord).map(|&(_, l)| l);
                    // A dug chunk is remeshed at full detail whatever it was
                    // drawn at: a coarse remesh of a pit draws a mound over it,
                    // or nothing. On a generated world that holds as far as it
                    // is drawn at all, since the coarse levels cannot draw a
                    // pit narrower than their cells; elsewhere every chunk is
                    // the author's and it holds near the player.
                    let lod = match cur {
                        _ if dug(coord) => 0,
                        Some(c) if raw_lod(dist, rings) > 0 => c,
                        _ => raw_lod(dist, rings),
                    };
                    if lod == 0 && sync_near {
                        render.pending.remove(&coord); // a sync mesh supersedes any job
                        // Skirted like every drawn chunk: a full-detail chunk next to a
                        // coarse one needs its own curtain to close the seam from its side.
                        let cm = floptle_field::mesh_chunk(&terrain.field, coord, 1, true);
                        if cm.is_empty() {
                            if let Some((mid, _)) = render.slots.remove(&coord) {
                                raster.free_dynamic(mid);
                            }
                            render.forget_far(coord);
                            render.born.remove(&coord);
                            render.empty.insert(coord);
                        } else {
                            upload_chunk(gpu, raster, render, coord, &cm, 0, now);
                        }
                    } else {
                        queue.push((prio_of(coord) - DIRTY_PRIORITY_BOOST, e, coord, lod));
                    }
                }
            }

            // Coverage: data chunks with no mesh, no job, and no known-empty verdict
            // (fresh loads, chunks a dig just created, structural re-plans) stream in
            // by distance. LOD migration for resident chunks rides the same queue
            // (hysteresis inside lod_for).
            //
            // Throttled: this walks every chunk of the field (a big planet has
            // tens of thousands), so each terrain scans on a 4-frame rotation —
            // a 3-frame queueing delay is invisible next to worker latency,
            // and the per-frame cost of huge worlds drops 4×. Structural
            // re-plans scan immediately (their bookkeeping was just reset);
            // dirty chunks never wait (handled above, outside this scan).
            if !structural
                && !(self.terrain_scan_frame.wrapping_add(e.index() as u64)).is_multiple_of(4)
            {
                continue;
            }
            scanned.push(e);
            for coord in terrain.field.chunk_coords() {
                match render.slots.get(&coord) {
                    None => {
                        if render.pending.contains_key(&coord)
                            || render.empty.contains(&coord)
                        {
                            continue;
                        }
                        let d = dist_of(coord);
                        if terrain.field.is_evicted(coord) && raw_lod(d, rings) < FAR_FROM_LOD {
                            continue; // made again first (`stream_far_chunks`)
                        }
                        if raw_lod(d, rings) == 0 {
                            // The ground around the player is drawn this frame: a
                            // fresh load (or a dig that created a chunk) draws the
                            // mesh made with its field, or meshes it now, a few
                            // a frame in the game. The rest go to the workers
                            // first of everything.
                            let made = premeshed.get_mut(&e).and_then(|p| p.remove(&coord));
                            #[cfg(test)]
                            NEAR_MESHES.with(|n| {
                                let mut v = n.get();
                                v[if made.is_some() { 1 } else if near_left > 0 { 0 } else { 2 }] += 1;
                                n.set(v);
                            });
                            let cm = match made {
                                Some(m) => m,
                                None if near_left > 0 => {
                                    near_left -= 1;
                                    floptle_field::mesh_chunk(&terrain.field, coord, 1, true)
                                }
                                None => {
                                    queue.push((prio_of(coord) - DIRTY_PRIORITY_BOOST, e, coord, 0));
                                    continue;
                                }
                            };
                            if cm.is_empty() {
                                render.empty.insert(coord);
                            } else {
                                upload_chunk(gpu, raster, render, coord, &cm, 0, now);
                            }
                        } else {
                            let lod = if dug(coord) { 0 } else { raw_lod(d, rings) };
                            // A chunk put away is drawn coarse from its
                            // stand-in, and finer once it is made again.
                            if lod < FAR_FROM_LOD && terrain.field.is_evicted(coord) {
                                continue;
                            }
                            queue.push((prio_of(coord), e, coord, lod));
                        }
                    }
                    Some(&(_, cur)) => {
                        let want = if dug(coord) { 0 } else { lod_for(dist_of(coord), cur, rings) };
                        if want < FAR_FROM_LOD && terrain.field.is_evicted(coord) {
                            continue;
                        }
                        if want != cur && !render.pending.contains_key(&coord) {
                            queue.push((prio_of(coord), e, coord, want));
                        }
                    }
                }
            }
        }

        for (e, p) in premeshed {
            if !scanned.contains(&e)
                && let Some(t) = self.terrains.get_mut(&e)
            {
                t.premeshed = p;
            }
        }

        // The far chunks that changed this frame, merged into their groups'
        // meshes before anything draws them.
        for (e, render) in self.terrain_render.iter_mut() {
            if render.groups_dirty.is_empty() {
                continue;
            }
            let Some(t) = self.terrains.get(e) else { continue };
            render.merge_far_groups(gpu, raster, floptle_field::CHUNK as f32 * t.field.voxel());
        }

        // ---- 5: top up the worker — dirty edits first (uncapped), then nearest ----
        if let Some(w) = self.terrain_worker.as_mut() {
            queue.sort_unstable_by_key(|&(d, ..)| d);
            // One copy of each terrain's field a frame for its jobs to gather
            // from: a table of shared chunks, not the voxels.
            let mut snaps: HashMap<Entity, std::sync::Arc<floptle_field::ChunkField>> = HashMap::new();
            for (prio, e, coord, lod) in queue {
                let dirty = prio <= -DIRTY_PRIORITY_BOOST / 2;
                if !dirty && w.in_flight >= WORKER_IN_FLIGHT_CAP {
                    break; // the rest re-plans next frame
                }
                let Some(terrain) = self.terrains.get(&e) else { continue };
                let Some(render) = self.terrain_render.get_mut(&e) else { continue };
                self.terrain_epoch += 1;
                render.pending.insert(coord, (lod, self.terrain_epoch));
                w.send(RemeshJob {
                    entity: e,
                    coord,
                    lod,
                    epoch: self.terrain_epoch,
                    scratch: JobScratch::Field(
                        snaps.entry(e).or_insert_with(|| std::sync::Arc::new(terrain.field.clone())).clone(),
                        1 << lod,
                    ),
                    collision: None,
                });
            }
        }
    }

}

/// Append every terrain's chunk-mesh instances to the raster draw list. The model matrix
/// places the field-space chunk vertices via the node's f64 anchor, exactly as
/// `fill_terrain_volumes` places the shadow/AO volume — so the drawn mesh and the marched
/// field coincide.
///
/// A free function taking explicit fields, like `fill_terrain_volumes` /
/// `push_mesh_instances`: the render loop has already borrowed `self.raster` mutably out
/// of `self`, so no `&self` method may run there. `base_mat` is computed before that borrow
/// (`terrain_material`), and `raster` is passed for `dyn_paint_base` (the per-chunk color).
#[allow(clippy::too_many_arguments)] // the render loop's borrow split forces a free fn
pub(crate) fn push_terrain_instances(
    terrain_render: &HashMap<Entity, TerrainRender>,
    terrains: &HashMap<Entity, EditorTerrain>,
    world: &floptle_core::World,
    raster: &floptle_render::Raster,
    base_mat: &MaterialParams,
    cam_world: DVec3,
    view_proj: Mat4,
    sphere_mesh: MeshId,
    now: Option<f32>,
    instances: &mut Vec<(MeshId, Option<floptle_render::TexId>, floptle_render::InstanceRaw)>,
) {
    // Frustum planes (Gribb–Hartmann) in camera-relative space — the same
    // space the instance matrices are built in. Chunks cull per
    // bounding sphere: a big planet close up otherwise submits thousands of
    // draws for chunks behind the camera and below the horizon.
    let m = view_proj.transpose();
    let frustum: [Vec4; 6] = [
        m.w_axis + m.x_axis, // left
        m.w_axis - m.x_axis, // right
        m.w_axis + m.y_axis, // bottom
        m.w_axis - m.y_axis, // top
        m.w_axis + m.z_axis, // near
        m.w_axis - m.z_axis, // far
    ];
    let in_frustum = |p: Vec3, r: f32| {
        frustum.iter().all(|pl| {
            let n = Vec3::new(pl.x, pl.y, pl.z);
            let len = n.length().max(1e-6);
            (n.dot(p) + pl.w) / len > -r
        })
    };
    // Occluder-sphere test: is the chunk (center p, radius rc, camera-relative)
    // fully hidden behind a solid ball (center c, radius big_r) the camera sits
    // outside of? Exact point-behind-sphere against a conservatively shrunk
    // ball (R − rc) — a chunk that peeks past the limb never culls.
    let occluded = |p: Vec3, rc: f32, c: Vec3, big_r: f32| -> bool {
        let r_eff = big_r - rc;
        if r_eff <= 0.0 || c.length_squared() <= big_r * big_r {
            return false; // occluder too small, or camera inside it (caves)
        }
        let vlen = p.length().max(1e-6);
        let dir = p / vlen;
        let tc = c.dot(dir); // sphere center's depth along the chunk ray
        if tc <= 0.0 {
            return false; // occluder behind the camera relative to this ray
        }
        let d2 = c.length_squared() - tc * tc;
        if d2 >= r_eff * r_eff {
            return false; // ray misses the shrunk ball
        }
        vlen > tc + (r_eff * r_eff - d2).sqrt() // chunk past the FAR surface
    };
    for (&e, render) in terrain_render {
        // A body a game hid (or switched off) draws nothing of its own: a game
        // drawing its own far worlds hides the engine's.
        if floptle_core::is_hidden(world, e) || floptle_core::is_disabled(world, e) {
            continue;
        }
        // Far-body impostor: one shaded sphere at the body's position. The
        // positional star lights it with the correct terminator; beyond real
        // scale it grows to hold ~a couple of pixels so distant planets read
        // as bright dots instead of vanishing.
        if render.impostor {
            let Some(cb) = world.get::<floptle_core::CelestialBody>(e) else { continue };
            let wt = floptle_core::world_transform(world, e);
            let rel = wt.translation - cam_world;
            let dist = rel.length();
            let r_eff = cb.body_radius.max(dist * 0.0022);
            // uv_sphere(0.85): the registered primitive sphere's radius.
            let scale = (r_eff / 0.85) as f32;
            let model = Mat4::from_scale_rotation_translation(
                Vec3::splat(scale),
                Quat::IDENTITY,
                rel.as_vec3(),
            );
            let mp = MaterialParams::flat(render.impostor_color.unwrap_or([0.75, 0.75, 0.78]));
            instances.push((sphere_mesh, None, floptle_render::instance_of_mat(model, &mp)));
            continue;
        }
        let wt = floptle_core::world_transform(world, e);
        // Streaming backstop. A chunk that has been queued but
        // not yet meshed draws nothing at all, so the player sees space through
        // the ground — "I can see through unloaded terrain". While a body is
        // still streaming, fill the holes with one shaded sphere at its
        // Occluder radius: the ball it already declares as being wholly inside
        // itself, which is exactly the property needed here. It can never poke
        // through real ground, and real ground hides it the moment it lands.
        if !render.pending.is_empty()
            && let Some(cb) = world.get::<floptle_core::CelestialBody>(e)
            && cb.occluder_radius > 0.0
        {
            let rel = (wt.translation - cam_world).as_vec3();
            let scale = (cb.occluder_radius as f32 * wt.scale.x.max(1e-6)) / 0.85;
            let model = Mat4::from_scale_rotation_translation(
                Vec3::splat(scale),
                Quat::IDENTITY,
                rel,
            );
            let mp = MaterialParams::flat(render.impostor_color.unwrap_or([0.55, 0.55, 0.58]));
            instances.push((sphere_mesh, None, floptle_render::instance_of_mat(model, &mp)));
        }
        if render.slots.is_empty() {
            continue;
        }
        let scale = wt.scale.x.max(1e-6);
        let rot = wt.rotation.normalize();
        let rel = (wt.translation - cam_world).as_vec3();
        // The node's full placement: rotation + uniform scale finally apply to
        // terrain (physics converts through the same frame — see ChunkTerrain).
        let model = Mat4::from_scale_rotation_translation(Vec3::splat(scale), rot, rel);
        // Per-chunk culling geometry: chunk edge in world units, bounding-sphere
        // radius padded for skirts and the coarse overlap layer, and the body's occluder
        // ball when declared.
        let chunk_units = terrains
            .get(&e)
            .map(|t| floptle_field::CHUNK as f32 * t.field.voxel())
            .unwrap_or(0.0)
            * scale;
        // A coarse chunk meshes up to a quarter chunk past its + faces and hangs skirts
        // below its rim, so its bound is the half-diagonal of a chunk grown by a
        // quarter on every side: 0.866 × 1.5.
        let chunk_r = chunk_units * 1.3;
        let occ_r = world
            .get::<floptle_core::CelestialBody>(e)
            .map(|cb| cb.occluder_radius as f32 * scale)
            .unwrap_or(0.0);
        // Far chunks draw in their groups, a group to a draw.
        let group_units = chunk_units * FAR_GROUP as f32;
        for (g, &mid) in &render.groups {
            if chunk_units > 0.0 {
                let local = Vec3::new(g[0] as f32 + 0.5, g[1] as f32 + 0.5, g[2] as f32 + 0.5) * (group_units / scale);
                let center = rel + rot * (local * scale);
                // Half the group's diagonal, and the reach of its members' skirts.
                let r = group_units * 0.87 + chunk_units * 0.45;
                if !in_frustum(center, r) || (occ_r > 0.0 && occluded(center, r, rel, occ_r)) {
                    continue;
                }
            }
            let mut mp = *base_mat;
            mp.terrain_paint_base = raster.dyn_paint_base(mid);
            mp.terrain_splat = true;
            instances.push((mid, None, floptle_render::instance_of_mat(model, &mp)));
        }
        for (coord, &(mid, _)) in &render.slots {
            if render.far.contains_key(coord) {
                continue;
            }
            if chunk_units > 0.0 {
                let local = Vec3::new(
                    (coord[0] as f32 + 0.5) * chunk_units / scale,
                    (coord[1] as f32 + 0.5) * chunk_units / scale,
                    (coord[2] as f32 + 0.5) * chunk_units / scale,
                );
                let center = rel + rot * (local * scale);
                if !in_frustum(center, chunk_r)
                    || (occ_r > 0.0 && occluded(center, chunk_r, rel, occ_r))
                {
                    continue;
                }
            }
            let mut mp = *base_mat;
            mp.terrain_paint_base = raster.dyn_paint_base(mid);
            // Splat: interpret the chunk color's alpha as a palette slot + triplanar-sample
            // the terrain palette (bound to the raster in `set_terrain_palette`).
            mp.terrain_splat = true;
            // Dissolve-in for a chunk that just arrived. The
            // alpha lane is free on terrain — the shader forces terrain opaque
            // because its vertex alpha is a palette slot — so this rides an
            // existing lane, which matters when the raster budget is full at
            // 16/16. An unstamped chunk is fully opaque, so nothing that was
            // already on screen flickers when this ships.
            if let (Some(now), Some(&born)) = (now, render.born.get(coord)) {
                mp.alpha = chunk_fade(now, born);
            }
            instances.push((mid, None, floptle_render::instance_of_mat(model, &mp)));
        }
    }
}

/// How long a newly resident chunk takes to dissolve fully in, in seconds.
///
/// Short enough that it reads as the thing arriving rather than as fog, long
/// enough to cover the arrival of the chunks behind it — a terrain streams in
/// waves, and a fade shorter than the gap between waves just moves the pop.
const CHUNK_FADE_SECS: f32 = 0.35;

/// How opaque a chunk that first became resident at `born` is at `now` — the
/// `color.a` the shader dissolves against.
///
/// Clamped at both ends. A `born` in the future reads as fully opaque rather
/// than as the start of a fade: only a clock that went backwards can produce
/// one, and a chunk stuck invisible is a worse failure than one that never
/// faded. `born == now` is the ordinary first frame, and starts at zero.
pub(crate) fn chunk_fade(now: f32, born: f32) -> f32 {
    let age = now - born;
    if age < 0.0 {
        return 1.0;
    }
    (age / CHUNK_FADE_SECS).clamp(0.0, 1.0)
}

/// Register (or overwrite) one chunk's dynamic slot in a terrain's render set,
/// recording the LOD the mesh was extracted at — and, for a chunk arriving from
/// nothing, the moment it arrived (see [`TerrainRender::born`]).
///
/// `now` is `None` when there is no clock yet, and then nothing is stamped: an
/// unstamped chunk draws opaque, so a terrain built before the window exists is
/// simply there rather than frozen at the start of a dissolve.
fn upload_chunk(
    gpu: &floptle_render::Gpu,
    raster: &mut floptle_render::Raster,
    render: &mut TerrainRender,
    coord: [i32; 3],
    cm: &floptle_field::ChunkMesh,
    lod: u8,
    now: Option<f32>,
) {
    // The coarsest chunks are drawn in groups, from a copy of their mesh;
    // anything finer draws on its own.
    if lod == FAR_LOD {
        render.far.insert(coord, cm.clone());
        render.groups_dirty.insert(far_group_of(coord));
    } else {
        render.forget_far(coord);
    }
    let data = floptle_render::chunk_mesh_data(cm);
    // Before the match, because two of its three arms are also "already
    // resident" — an LOD swap and an outgrown slot are both re-meshes of
    // something the player can already see, and neither should dissolve.
    if let Some(now) = now
        && !render.slots.contains_key(&coord)
    {
        render.born.insert(coord, now);
    }
    match render.slots.get(&coord).copied() {
        Some((mid, _)) if raster.replace_dynamic(gpu, mid, &data) => {
            render.slots.insert(coord, (mid, lod));
        }
        Some((mid, _)) => {
            // Outgrew its slot (rare): drop and re-register at the new size.
            raster.free_dynamic(mid);
            let id = raster.register_dynamic(
                gpu,
                data.vertices.len() as u32,
                data.indices.len() as u32,
                true,
            );
            raster.replace_dynamic(gpu, id, &data);
            render.slots.insert(coord, (id, lod));
        }
        None => {
            let id = raster.register_dynamic(
                gpu,
                data.vertices.len() as u32,
                data.indices.len() as u32,
                true,
            );
            raster.replace_dynamic(gpu, id, &data);
            render.slots.insert(coord, (id, lod));
        }
    }
}

/// The cubic voxel edge to import (migrate) a legacy dense terrain at.
///
/// two constraints, and the tighter (coarser) wins:
///   1. Source detail — the median of the three axis resolutions. Using the *min* (my
///      first cut) is catastrophic for a stretched legacy field: the 18:1 Y-stretch makes
///      one axis ~0.36 units, and meshing the 578×578 footprint at 0.36 is hundreds of
///      millions of voxels — it floods the terrain color store (2^24 verts) and takes
///      forever. The median tracks the real content scale, not the thinnest artifact axis.
///   2. A footprint budget — surface-nets vertex count scales with the two largest extents'
///      area over voxel², so bound that area to a safe cell count. This is the hard backstop
///      that guarantees no field, however pathological, can blow the store.
///
/// A small terrain is detail-limited (median wins); a big one is budget-limited (area wins).
fn terrain_voxel_size(baked: &floptle_field::BakedSdf) -> f32 {
    let [w, h, d] = baked.dims;
    let mut axis = [
        2.0 * baked.half_extent[0] / (w.max(2) - 1) as f32,
        2.0 * baked.half_extent[1] / (h.max(2) - 1) as f32,
        2.0 * baked.half_extent[2] / (d.max(2) - 1) as f32,
    ];
    axis.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = axis[1];

    // Footprint = the two largest world extents (a terrain is a wide, shallow slab; its
    // surface — and thus its vertex count — scales with this area / voxel²). Cap the cell
    // count so the mesh stays well under the store and the remesh budget.
    let mut ext = [2.0 * baked.half_extent[0], 2.0 * baked.half_extent[1], 2.0 * baked.half_extent[2]];
    ext.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    const MAX_SURFACE_CELLS: f32 = 1_000_000.0; // ~1 M verts worst case, far under 2^24
    let by_area = (ext[1] * ext[2] / MAX_SURFACE_CELLS).sqrt();

    median.max(by_area).clamp(0.25, 16.0)
}

/// Bitmask of terrain palette slots whose texture asked for Pixelated filtering
/// (bit i = slot i). Packed into `terrain_tint.w` — an exact small int in f32, the same
/// idiom `rim.w` uses for tiling flags. `TERRAIN_SLOTS` is 12, so it fits easily.
///
/// A free function, not a method: the render loop holds `self.gpu.as_mut()` and friends,
/// so `&self` is unavailable there — but borrowing these two fields is fine.
pub(crate) fn terrain_nearest_mask(
    textures: &[String],
    settings: &std::collections::HashMap<String, crate::assets::TexSetting>,
    project_root: &std::path::Path,
) -> u32 {
    let mut mask = 0u32;
    for (i, path) in textures.iter().enumerate().take(32) {
        if path.is_empty() {
            continue;
        }
        let s = crate::assets::tex_setting(settings, project_root, path);
        if s.filter == crate::assets::FilterMode::Pixelated {
            mask |= 1 << i;
        }
    }
    mask
}

impl Editor {
    /// Focus (re-adding if closed) the Terrain dock tab.
    #[cfg(feature = "editor-ui")]
    pub(crate) fn focus_terrain(&mut self) {
        if let Some(dock) = self.dock_state.as_mut() {
            focus_terrain_tab(dock);
        }
    }

    /// Bake shape `key`'s shadow occluder on a worker: the model read, its
    /// vertices turned and scaled, voxelised at 128 along its longest axis.
    fn kick_occluder_bake(&mut self, key: &OccKey) {
        // Project-resolved: read raw, every mesh shadow occluder in an
        // exported build failed to load and the scene lost its shadows.
        let file = crate::project::resolve_asset_path(&self.project_root, &key.0);
        let k = key.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        crate::worker::spawn("occluder-bake", move || {
            #[cfg(test)]
            made_on("occluder");
            let started = Instant::now();
            let Ok(model) = floptle_assets::gltf_import::geometry(&file) else {
                let _ = tx.send(None);
                return;
            };
            let rot = Quat::from_xyzw(k.1[0] as f32 / 1000.0, k.1[1] as f32 / 1000.0, k.1[2] as f32 / 1000.0, k.1[3] as f32 / 1000.0)
                .normalize();
            let s = Vec3::new(k.2[0] as f32 / 1000.0, k.2[1] as f32 / 1000.0, k.2[2] as f32 / 1000.0);
            // Rotation + scale applied to the vertices (like the physics
            // colliders); translation stays in the per-frame f64 anchor.
            let m = Mat4::from_scale_rotation_translation(s, rot, Vec3::ZERO);
            let mut verts: Vec<[f32; 3]> = Vec::new();
            let mut indices: Vec<u32> = Vec::new();
            for part in &model.parts {
                let base = verts.len() as u32;
                verts.extend(part.mesh.vertices.iter().map(|v| m.transform_point3(Vec3::from(v.pos)).to_array()));
                indices.extend(part.mesh.indices.iter().map(|i| i + base));
            }
            // 128 voxels along the longest axis: a whole-map bake lands well
            // under a second and keeps doorways/rooms resolvable (the user's
            // ~80-unit map → ~0.6-unit voxels).
            let baked = std::sync::Arc::new(floptle_field::bake_occluder(&verts, &indices, 128));
            let _ = tx.send(Some((baked, (indices.len() / 3) as u64, started.elapsed().as_secs_f64() * 1e3)));
        });
        self.occluder_bake_jobs.insert(key.clone(), rx);
    }

    /// Put the occluder bakes made off the frame into the cache, where the
    /// next refresh finds them.
    fn land_occluder_bakes(&mut self) {
        if self.occluder_bake_jobs.is_empty() {
            return;
        }
        let mut landed = Vec::new();
        self.occluder_bake_jobs.retain(|k, rx| match rx.try_recv() {
            Ok(r) => {
                landed.push((k.clone(), r));
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                landed.push((k.clone(), None));
                false
            }
        });
        for (key, r) in landed {
            match r {
                Some((baked, tris, ms)) => {
                    self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!(
                            "baked shadow occluder for {} ({tris} tris → {}×{}×{} voxels, {ms:.0} ms, off the frame)",
                            key.0, baked.dims[0], baked.dims[1], baked.dims[2]
                        ),
                        None,
                    );
                    self.occluder_bakes += 1;
                    self.occluder_cache.insert(key, baked);
                }
                None => {
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!("shadow occluder: failed to load {}", key.0),
                        None,
                    );
                    self.occluder_failed.insert(key);
                }
            }
        }
    }

    /// Keep the shadow-occluder bakes in sync with the scene's static collider
    /// meshes (Collidable / MeshCollider on a `Matter::Mesh` node, no RigidBody —
    /// dynamic bodies cast via their shape proxies instead). Each eligible mesh
    /// bakes once per (asset, rotation, scale) into an unsigned occluder volume
    /// (`bake_occluder`), cached so duplicates and pure moves are free. Returns
    /// true when the set changed and the atlas needs re-uploading; per-node
    /// "casts shadows" / visibility toggles are applied at fill time (no rebake).
    ///
    /// A turn or resize waits until it settles. A bake is a 128-voxel pass over
    /// the whole mesh plus a re-upload of the atlas, about 100 ms, so a script
    /// that scales a prop every frame (a flinch, a throb) would pay it every
    /// frame. While a node is still changing it keeps casting with its last
    /// bake, and is baked once it has held still for [`OCCLUDER_SETTLE_FRAMES`].
    /// A shape that is already baked (back to its old scale) is taken at once.
    pub(crate) fn refresh_mesh_occluders(&mut self) -> bool {
        self.land_occluder_bakes();
        // The desired (entity → key) set this frame.
        let mut desired: Vec<(Entity, OccKey)> = Vec::new();
        let ents: Vec<(Entity, String)> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Mesh { asset_path } => Some((e, asset_path.clone())),
                _ => None,
            })
            .collect();
        for (e, path) in ents {
            let static_collider = (self.world.get::<floptle_core::Collidable>(e).is_some()
                || self.world.get::<floptle_core::MeshCollider>(e).is_some())
                && self.world.get::<floptle_core::RigidBody>(e).is_none();
            if !static_collider {
                continue;
            }
            // "Casts shadows" off: no bake, no atlas space, no instance. A prop
            // switched off here frees the budget for the buildings.
            if self.world.get::<floptle_core::CastShadow>(e).is_some_and(|c| !c.0) {
                continue;
            }
            let wt = floptle_core::world_transform(&self.world, e);
            let q = |v: f32| (v * 1000.0).round() as i32;
            let key: OccKey = (
                path,
                [q(wt.rotation.x), q(wt.rotation.y), q(wt.rotation.z), q(wt.rotation.w)],
                [q(wt.scale.x), q(wt.scale.y), q(wt.scale.z)],
            );
            // Against the newest set asked for, which may not be drawn yet.
            let latest = self.occluders_wanted.as_ref().unwrap_or(&self.mesh_occluders);
            let key = match latest.get(&e) {
                Some((old, _)) if *old != key && !self.occluder_cache.contains_key(&key) => {
                    let held = match self.occluder_settling.get_mut(&e) {
                        Some((k, n)) if *k == key => {
                            *n += 1;
                            *n
                        }
                        _ => {
                            self.occluder_settling.insert(e, (key.clone(), 0));
                            0
                        }
                    };
                    if held < OCCLUDER_SETTLE_FRAMES {
                        old.clone()
                    } else {
                        self.occluder_settling.remove(&e);
                        key
                    }
                }
                _ => {
                    self.occluder_settling.remove(&e);
                    key
                }
            };
            desired.push((e, key));
        }
        let latest = self.occluders_wanted.as_ref().unwrap_or(&self.mesh_occluders);
        let unchanged = desired.len() == latest.len()
            && desired
                .iter()
                .all(|(e, key)| latest.get(e).is_some_and(|(k, _)| k == key));
        if unchanged {
            return false;
        }

        let mut next: HashMap<Entity, (OccKey, std::sync::Arc<floptle_field::BakedSdf>)> =
            HashMap::new();
        let mut bake: Vec<OccKey> = Vec::new();
        for (e, key) in desired {
            if let Some(b) = self.occluder_cache.get(&key) {
                next.insert(e, (key, b.clone()));
                continue;
            }
            // Baked on a worker: the node keeps casting with the bake it had
            // (none, for a shape new to it) until this one is in.
            if let Some(old) = latest.get(&e) {
                next.insert(e, old.clone());
            }
            if !self.occluder_bake_jobs.contains_key(&key) && !self.occluder_failed.contains(&key) && !bake.contains(&key) {
                bake.push(key);
            }
        }
        for key in &bake {
            self.kick_occluder_bake(key);
        }
        let latest = self.occluders_wanted.as_ref().unwrap_or(&self.mesh_occluders);
        if next.len() == latest.len() && next.iter().all(|(e, (k, _))| latest.get(e).is_some_and(|(lk, _)| lk == k)) {
            // Only bakes on their way: the set asked for is the one there.
            return false;
        }
        // Drop cache entries nothing references anymore (a resized/removed map).
        self.occluder_cache.retain(|k, _| next.values().any(|(nk, _)| nk == k));
        self.occluder_settling.retain(|e, _| next.contains_key(e));
        // With an atlas to fill, the set drawn changes when the atlas holding
        // it is whole (`sync_terrain_gpu`); until then the old bakes cast.
        if self.raymarch.is_some() {
            self.occluders_wanted = Some(next);
        } else {
            self.mesh_occluders = next;
        }
        true
    }

    // ---- terrain sculpting --------------------------------------------------
    /// Once per frame (with the Sculpt tool): cast the cursor ray at the terrain,
    /// build the brush telegraph (ring + normal), and — if a stroke is queued —
    /// apply the brush. Editing is throttled here to one stroke per frame so a fast
    /// drag doesn't stall on the per-voxel work + GPU re-upload.
    #[cfg(feature = "editor-ui")]
    pub(crate) fn terrain_frame_update(&mut self) {
        self.terrain_viz = None;
        if self.tool != Tool::Sculpt || self.terrains.is_empty() || !self.cursor_over_scene() {
            return;
        }
        let (Some(cursor), Some(gpu)) = (self.cursor, self.gpu.as_ref()) else { return };
        let cam = self.camera.render_camera();
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let vp = cam.view_proj(w / h);
        let inv = vp.inverse();
        let ndc = Vec2::new(cursor.x / w * 2.0 - 1.0, 1.0 - cursor.y / h * 2.0);
        let near = inv * Vec4::new(ndc.x, ndc.y, 0.0, 1.0);
        let far = inv * Vec4::new(ndc.x, ndc.y, 1.0, 1.0);
        let ro_rel = near.truncate() / near.w;
        let rd = (far.truncate() / far.w - ro_rel).normalize();
        let rd_a = [rd.x, rd.y, rd.z];

        // Each field is in its node's local frame (translation + rotation +
        // uniform scale) — transform the cursor ray into each and brush the one
        // whose surface it hits nearest the camera. `hit` stays local; world
        // positions reconstruct through the same frame.
        type BrushPick = (Entity, Vec3, (DVec3, Quat, f32), f64);
        let entities: Vec<Entity> = self.terrains.keys().copied().collect();
        let mut best: Option<BrushPick> = None;
        for e in entities {
            let frame = self.terrain_world_frame_of(e);
            let (origin, rot, ts) = frame;
            let inv = rot.inverse();
            let ro = (inv * (cam.world_position + ro_rel.as_dvec3() - origin).as_vec3()) / ts;
            let rd_l = (inv * Vec3::from(rd_a)).normalize_or_zero();
            if let Some(hit) = self.terrains[&e].field.raycast(ro, rd_l, 4096.0 / ts) {
                let hitw = origin + (rot * (hit * ts)).as_dvec3();
                let dist = (hitw - cam.world_position).length();
                if best.as_ref().is_none_or(|b| dist < b.3) {
                    best = Some((e, hit, frame, dist));
                }
            }
        }
        let Some((active, hit, (origin, trot, tscale), _)) = best else {
            return;
        };
        self.active_terrain = Some(active);
        let n = (trot * self.terrains[&active].field.grad(hit)).normalize_or_zero();
        let radius = self.terrain_brush.radius;

        // Telegraph: a ring of `radius` around the hit in the surface tangent plane.
        let hitw = origin + (trot * (hit * tscale)).as_dvec3();
        let t1 = n.cross(if n.y.abs() > 0.9 { Vec3::X } else { Vec3::Y }).normalize_or_zero();
        let t2 = n.cross(t1);
        let mut ring = Vec::with_capacity(40);
        for i in 0..40 {
            let a = i as f32 / 40.0 * std::f32::consts::TAU;
            let wp = hitw + ((t1 * a.cos() + t2 * a.sin()) * radius).as_dvec3();
            if let Some(s) = project(wp, cam.world_position, vp, w, h) {
                ring.push(s);
            }
        }
        let normal = match (
            project(hitw, cam.world_position, vp, w, h),
            project(hitw + (n * (radius * 0.7)).as_dvec3(), cam.world_position, vp, w, h),
        ) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        };
        self.terrain_viz = Some(TerrainViz { ring, normal });

        // Apply a dab — but only when the cursor has moved ~a third of the brush
        // along the surface since the last one, or after a short interval if held
        // still. This spaces strokes like a real paint tool instead of dumping one
        // every frame (which at high FPS made the brush impossible to control).
        let due = if self.sculpting {
            let now = Instant::now();
            let moved = self
                .last_dab_pos
                .is_none_or(|p| (hitw - p).length() as f32 >= radius * self.terrain_brush.spacing.max(0.02));
            let timed = self
                .last_dab_time
                .is_none_or(|t| (now - t).as_secs_f32() >= 0.10);
            if moved || timed {
                self.last_dab_pos = Some(hitw);
                self.last_dab_time = Some(now);
                true
            } else {
                false
            }
        } else {
            false
        };
        if due {
            let brush = self.terrain_brush;
            // Brush radius is world units; the field works in local units.
            let r_local = brush.radius / tscale;
            let id = match self.world.get::<Matter>(active) {
                Some(Matter::Terrain { id, .. }) => *id,
                _ => 0,
            };
            let flat_normal = self.flatten_normal(active, hit, None);
            let terrain = self.terrains.get_mut(&active).unwrap();
            // Capture the pre-dab chunks into the stroke's undo record — lazily, only
            // the chunks this dab could touch that aren't already captured. The whole
            // stroke stays a single undo step of a few MB, not a whole-field snapshot.
            let candidates = terrain.field.chunks_in_box(hit, r_local * 1.5);
            let snap = terrain.field.snapshot_chunks(&candidates);
            match &mut self.stroke_snapshot {
                Some((sid, undo)) if *sid == id => undo.merge(snap),
                _ => self.stroke_snapshot = Some((id, snap)),
            }
            // Apply the brush to the authority field. No growth step: the sparse field
            // is unbounded, so sculpting near an edge just allocates chunks (the whole
            // `ensure_contains`/`grow` bug class is gone with the dense grid).
            let is_paint = matches!(brush.mode, floptle_field::Brush::Paint);
            let touched = match brush.mode {
                floptle_field::Brush::Paint if brush.tex_slot >= 0 => {
                    terrain.field.paint_texture(hit, r_local, brush.tex_slot as u8 + 1)
                }
                floptle_field::Brush::Paint => {
                    terrain.field.paint(hit, r_local, brush.strength, brush.color, brush.profile)
                }
                floptle_field::Brush::Flatten => {
                    terrain.field.flatten_to_plane(hit, flat_normal, r_local, brush.strength, brush.profile)
                }
                m => terrain.field.sculpt(m, hit, r_local, brush.strength, brush.profile),
            };
            if !touched.is_empty() {
                self.stroke_dabbed = true; // mark this stroke as worth an undo step
                // Shadow-proxy refresh + atlas partial upload + chunk remesh queue.
                // (A dab outside the proxy's box clamps — the proxy is re-derived at
                // stroke end when bounds outgrow it; see `end_sculpt_stroke`.)
                let geom = !is_paint; // sculpt changes geometry (resync wireframe + collider)
                // Sculpting while playing must reach the sim's collider copy too —
                // this path only fed the renderer, so a mid-Play brush stroke left
                // bodies standing on the old invisible surface.
                if geom {
                    self.mirror_terrain_chunks_to_sim(active, &touched);
                }
                self.queue_terrain_dirty(active, hit, r_local, geom, touched);
            }
        }
    }

    /// Drain + apply the terrain edits scripts queued this pass (`terrain.sculpt/
    /// dig/paint/paintTexture` — Terrain 2.0 P6). Call after reclaiming the sim's
    /// colliders and before stepping physics, so a dig affects the same tick.
    pub(crate) fn drain_script_terrain_ops(&mut self) {
        let ops = self.script_host.take_terrain_ops();
        self.apply_script_terrain_ops(ops);
    }

    /// Take a frame's script terrain ops, and apply this frame's share of
    /// everything taken so far: the work of [`Self::drain_script_terrain_ops`],
    /// for ops from anywhere.
    ///
    /// Ops land in the order they were made, [`TERRAIN_EDIT_BUDGET_MS`] of
    /// them a frame (or the game's `terrain.editBudget`) and at least one,
    /// and the rest wait for the next frame. Big ones, and ones over ground
    /// not made yet, are made on a worker one after another while the frames
    /// go on ([`Self::op_goes_off_frame`]), and each frame lands what it has
    /// made.
    /// A game grading a base site made 1,119 flattens in one frame, and that
    /// frame took seconds. An op is applied whole before the next starts, so
    /// what a query reads is always the ground after some number of whole
    /// ops; `terrain.pending()` says how many are still to land.
    pub(crate) fn apply_script_terrain_ops(&mut self, ops: Vec<floptle_script::TerrainOp>) {
        for (id, rule) in self.script_host.take_terrain_impostors() {
            match rule {
                Some(rule) => self.terrain_impostor_rules.insert(id, rule),
                None => self.terrain_impostor_rules.remove(&id),
            };
        }
        // Each op is placed in its terrain's own frame now, the frame it was
        // made in: one carried to a later frame lands where it was aimed on a
        // planet that has moved on its rails since (5 to 6 m off, before).
        for op in ops {
            let at = self.resolve_terrain_op(&op);
            self.terrain_op_queue.push_back(QueuedEdit { op, at });
        }
        if self.terrain_op_queue.is_empty() && self.terrain_op_job.is_none() {
            self.script_host.set_terrain_edits_queued(0);
            return;
        }
        // The ground under a scatter chunk just moved, so the props standing on
        // it are at the old height. Forget those chunks and they re-settle onto
        // the new surface — SC3's "digging the ground out from under one drops
        // or despawns it", for free, because placement was never remembered in
        // the first place.
        let scatter = !self.script_host.scatter_sources().is_empty();
        let mut dirty: Vec<(DVec3, f64)> = Vec::new();
        // One collider copy and one remesh/shadow refresh per terrain for the
        // frame's ops: a game laying 32 small flattens in a frame paid for 32
        // of each, and none of it showed in any `perf` bucket.
        let t0 = floptle_core::profile::Span::new();
        let started = floptle_core::time::Instant::now();
        let mut batch: HashMap<Entity, TerrainBatch> = HashMap::new();
        let budget = self.script_host.terrain_edit_budget_ms();
        let proxy_cap = (TERRAIN_EDIT_PROXY_CELLS as f64 * (budget / TERRAIN_EDIT_BUDGET_MS).max(1.0)) as usize;
        // The big ops made on a worker land first: everything behind them waited.
        let part = floptle_core::profile::Span::new();
        self.land_op_job(&mut dirty, scatter, started, budget);
        let landing_ms = part.ms();
        let part = floptle_core::profile::Span::new();
        while let Some(front) = self.terrain_op_queue.front() {
            // An edit waits for its terrain to land: a field streaming in or
            // being generated replaces the one there now, and would take the
            // edit with it. The edits after it wait their turn behind it.
            if let Some(at) = &front.at
                && self.terrain_landing(at.e)
            {
                break;
            }
            let off = front.at.as_ref().is_some_and(|at| self.op_goes_off_frame(&front.op, at));
            // While ops are being made on a worker nothing lands on the frame,
            // or it would land ahead of them: the next big ones for the same
            // terrain go to the worker behind them, a few ahead at most.
            if let Some(job) = &self.terrain_op_job {
                if !off || front.at.is_none_or(|at| at.e != job.e) || job.in_flight >= OP_JOB_AHEAD {
                    break;
                }
                let Some(QueuedEdit { op, at: Some(at) }) = self.terrain_op_queue.pop_front() else { break };
                self.feed_op_job(op, at);
                continue;
            }
            let Some(QueuedEdit { op, at }) = self.terrain_op_queue.pop_front() else { break };
            // Too big for any frame's budget, or over ground not made yet:
            // made off the frame, and the rest wait behind it.
            if off && let Some(at) = at {
                self.start_op_job(op, at);
                continue;
            }
            if scatter && op.affects_geometry() {
                dirty.push((DVec3::new(op.pos[0], op.pos[1], op.pos[2]), op.radius as f64));
            }
            if let Some(at) = at {
                self.apply_resolved_op(&op, &at, &mut batch);
            }
            // The frame also pays for refreshing the shadow proxy over the
            // boxes the ops touched, which grows with their spread rather
            // than their number: a spiral of flattens merges into one box
            // tens of metres across.
            if self.terrain_edit_ms + started.elapsed().as_secs_f64() * 1e3 >= budget || self.proxy_cells(&batch) > proxy_cap {
                break;
            }
        }
        // The budget is the frame's: the frame pass and every tick share it.
        self.terrain_edit_ms += started.elapsed().as_secs_f64() * 1e3;
        let applying_ms = part.ms();
        let part = floptle_core::profile::Span::new();
        for (e, mut b) in batch {
            b.touched.sort_unstable();
            b.touched.dedup();
            if b.geom {
                self.mirror_terrain_chunks_to_sim(e, &b.touched);
            }
            let mut touched = Some(b.touched);
            for (lo, hi) in b.boxes {
                let c = (lo + hi) * 0.5;
                let r = ((hi - lo) * 0.5).max_element();
                self.queue_terrain_dirty(e, c, r, b.geom, touched.take().unwrap_or_default());
            }
        }
        let ms = t0.ms();
        self.profile_record(floptle_core::profile::Bucket::Terrain, ms);
        {
            // Inside `terrainEdits`, which of its three steps a long one was.
            let mut p = self.script_host.profile().borrow_mut();
            p.record_part("terrainEdits", ms);
            p.record_part("terrainEditsLanding", landing_ms);
            p.record_part("terrainEditsApplying", applying_ms);
            p.record_part("terrainEditsAfter", part.ms());
        }
        if !dirty.is_empty() {
            let sources: Vec<floptle_core::scatter::ScatterSource> =
                self.script_host.scatter_sources().clone();
            for (p, r) in dirty {
                self.scatter_cache.invalidate_near(&sources, p, r);
            }
        }
        self.script_host.set_terrain_edits_queued(self.terrain_ops_waiting());
    }

    /// Script terrain ops taken and not landed: queued, or being made on a
    /// worker.
    pub(crate) fn terrain_ops_waiting(&self) -> usize {
        self.terrain_op_queue.len() + self.terrain_op_job.as_ref().map_or(0, |j| j.in_flight)
    }

    /// Is an op made on a worker rather than on the frame? One this many
    /// voxels across its radius ([`BIG_OP_VOXELS`]) costs more than any
    /// frame's budget, and so does one over ground not made yet: the write
    /// makes that ground first, chunks of tens of milliseconds each, on
    /// whichever thread it runs on: a 3 m flatten over ground a fresh world
    /// had not made took 120 ms of its frame (debug), against 0.1 ms handed
    /// to the worker. A grade's first cuts are small and high over the site.
    fn op_goes_off_frame(&self, op: &floptle_script::TerrainOp, at: &OpTarget) -> bool {
        let Some(t) = self.terrains.get(&at.e) else { return false };
        let (f, r) = (&t.field, op.radius / at.ts);
        if r >= BIG_OP_VOXELS * f.voxel() {
            return true;
        }
        let reach = Vec3::splat(r + f.band() + f.voxel() * OP_REALIZE_VOXELS);
        !f.pending_in_box(at.local - reach, at.local + reach).is_empty()
    }

    /// Start a worker for big ops on a copy of `op`'s terrain's field (the
    /// chunks are shared, so the copy costs a table of pointers), and hand it
    /// `op`. The proxy cells over each op's box are sampled there too:
    /// refreshing them was most of the rest of an op's frame.
    fn start_op_job(&mut self, op: floptle_script::TerrainOp, at: OpTarget) {
        let Some(t) = self.terrains.get(&at.e) else { return };
        let mut field = t.field.clone();
        let _ = field.take_changed();
        let (dims, center, half) = (t.shadow.dims, t.shadow.center, t.shadow.half_extent);
        let (band, voxel) = (t.field.band(), t.field.voxel());
        let (tx, work) = std::sync::mpsc::channel::<(floptle_script::TerrainOp, OpTarget)>();
        let (done, rx) = std::sync::mpsc::channel();
        #[cfg(test)]
        let made = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        #[cfg(test)]
        let count = made.clone();
        crate::worker::spawn("terrain-op", move || {
            for (op, at) in work {
                let reach = Vec3::splat(op.radius / at.ts + band + voxel);
                let near = Vec3::splat(voxel * OP_REALIZE_VOXELS);
                let (lo, hi) = (at.local - reach, at.local + reach);
                let unmade = field.pending_in_box(lo - near, hi + near);
                let (touched, measured) = make_op(&mut field, &op, &at);
                let changed = field.take_changed();
                let still = field.pending_in_box(lo - near, hi + near);
                let realized = unmade.into_iter().filter(|c| !still.contains(c)).collect();
                let region = field.sample_dense_region(dims, center, half, lo, hi);
                let out = OpDone { op, at, field: field.clone(), touched, realized, changed, measured, region };
                if done.send(out).is_err() {
                    break;
                }
                #[cfg(test)]
                count.fetch_add(1, std::sync::atomic::Ordering::Release);
            }
        });
        self.terrain_op_job = Some(OpJob {
            e: at.e,
            tx,
            rx,
            in_flight: 0,
            #[cfg(test)]
            handed: 0,
            #[cfg(test)]
            made,
        });
        self.feed_op_job(op, at);
    }

    /// Hand the op worker its next op.
    fn feed_op_job(&mut self, op: floptle_script::TerrainOp, at: OpTarget) {
        let Some(job) = self.terrain_op_job.as_mut() else { return };
        // A worker that died has dropped its end; what it held is reported
        // lost when its results run dry.
        let _ = job.tx.send((op, at));
        job.in_flight += 1;
        #[cfg(test)]
        {
            job.handed += 1;
        }
    }

    /// Wait until the op worker has made everything it was handed, so that
    /// a test's frames count what a frame lands, not how fast a worker is.
    #[cfg(test)]
    fn wait_for_op_worker(&self) {
        let Some(job) = &self.terrain_op_job else { return };
        let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while job.made.load(std::sync::atomic::Ordering::Acquire) < job.handed {
            assert!(std::time::Instant::now() < until, "the op worker never finished");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Land the big ops the worker has made, in order and while the frame's
    /// budget lasts (at least one): their chunks into the field, each in one
    /// step, their proxy cells and reports, and then the sim's copy and the
    /// remesh queue once for them all. The job ends once all it was handed
    /// has landed. Nothing to do while the next is still being made.
    fn land_op_job(&mut self, dirty: &mut Vec<(DVec3, f64)>, scatter: bool, started: floptle_core::time::Instant, budget: f64) {
        let Some(e) = self.terrain_op_job.as_ref().map(|j| j.e) else { return };
        let mut one: HashMap<Entity, TerrainBatch> = HashMap::new();
        let mut proxy: Option<([u32; 3], [u32; 3])> = None;
        let mut realized: Vec<[i32; 3]> = Vec::new();
        while let Some(job) = self.terrain_op_job.as_mut() {
            if job.in_flight == 0 {
                break;
            }
            let done = match job.rx.try_recv() {
                Ok(done) => done,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // The worker died: what it held is lost, and the queue goes on.
                    job.in_flight = 0;
                    self.console.push(floptle_script::LogLevel::Error, "a terrain edit failed to apply and was dropped".into(), None);
                    break;
                }
            };
            job.in_flight -= 1;
            let OpDone { op, at, field, touched, realized: made, changed, measured, region } = done;
            if scatter && op.affects_geometry() {
                dirty.push((DVec3::new(op.pos[0], op.pos[1], op.pos[2]), op.radius as f64));
            }
            let Some(t) = self.terrains.get_mut(&e) else { continue };
            let mut coords = touched.clone();
            coords.extend_from_slice(&made);
            if let Some((lo, hi)) = changed {
                coords.extend(field.chunks_holding(lo, hi));
            }
            coords.sort_unstable();
            coords.dedup();
            t.field.adopt_edit(&field, &coords, changed);
            let (mn, mx) = region.write_into(&mut t.shadow);
            proxy = Some(match proxy {
                Some((a, b)) => ([a[0].min(mn[0]), a[1].min(mn[1]), a[2].min(mn[2])], [b[0].max(mx[0]), b[1].max(mx[1]), b[2].max(mx[2])]),
                None => (mn, mx),
            });
            realized.extend(made);
            // Its report and chunks, as an inline op's; its proxy is already done.
            self.landed_op(&op, &at, touched, measured, &mut one);
            if self.terrain_edit_ms + started.elapsed().as_secs_f64() * 1e3 >= budget {
                break;
            }
        }
        if self.terrain_op_job.as_ref().is_some_and(|j| j.in_flight == 0) {
            self.terrain_op_job = None;
        }
        let Some((mn, mx)) = proxy else { return };
        let mut b = one.remove(&e).unwrap_or_default();
        // Ground the edits made is new geometry, whatever the edits were (a
        // paint over stand-in ground): the sim's copy takes it too.
        b.geom |= !realized.is_empty();
        b.touched.extend(realized);
        b.touched.sort_unstable();
        b.touched.dedup();
        if b.geom {
            self.mirror_terrain_chunks_to_sim(e, &b.touched);
        }
        self.note_region_dirty(e, mn, mx, b.geom);
        self.terrain_chunks_dirty.entry(e).or_default().extend(b.touched);
        self.touch_terrain_edit(e);
    }

    /// Queue a rewritten proxy box for upload. Each terrain keeps its own
    /// boxes, merged only with boxes they nearly overlap: one box spanning
    /// every edit of a frame uploaded all the air between them.
    fn note_region_dirty(&mut self, e: Entity, mn: [u32; 3], mx: [u32; 3], geom: bool) {
        let Some(key) = self.terrains.get(&e).map(|t| t.shadow_key) else { return };
        let d = self.terrain_region_dirty.entry(e).or_default();
        if d.key != key {
            // Boxes of a proxy since replaced: the new one uploads whole.
            *d = ShadowDirty { key, ..Default::default() };
        }
        d.add(mn, mx);
        d.geom |= geom;
    }

    /// Is a field for terrain `e` on its way, to replace the one there now: a
    /// load from a file or its genspec, or a `generatePlanet` fill?
    fn terrain_landing(&self, e: Entity) -> bool {
        self.terrain_load_jobs.iter().any(|j| j.e == e)
            || matches!(self.world.get::<Matter>(e), Some(Matter::Terrain { id, .. }) if self.planet_gen_pending.contains(id))
    }

    /// How many shadow-proxy cells refreshing the batch's boxes would sample.
    fn proxy_cells(&self, batch: &HashMap<Entity, TerrainBatch>) -> usize {
        let mut cells = 0usize;
        for (e, b) in batch {
            let Some(t) = self.terrains.get(e) else { continue };
            let size = Vec3::from(t.shadow.half_extent) * 2.0;
            let cell = Vec3::new(
                size.x / t.shadow.dims[0].max(1) as f32,
                size.y / t.shadow.dims[1].max(1) as f32,
                size.z / t.shadow.dims[2].max(1) as f32,
            );
            let pad = Vec3::splat(t.field.band() + t.field.voxel());
            for (lo, hi) in &b.boxes {
                let n = ((*hi - *lo + pad * 2.0) / cell).ceil().max(Vec3::ONE);
                cells += (n.x * n.y * n.z) as usize;
            }
        }
        cells
    }

    /// Apply one script terrain op (world coords) to the nearest terrain: the
    /// authority field, the sim's collider copy (geometry ops), the chunk remesh
    /// queue, and the shadow-proxy region — the same pipeline as an editor brush dab.
    /// Play-mode only state: Stop restores the pre-Play fields (`play_terrains`), so
    /// script edits never leak into the authored scene.
    #[cfg(test)]
    fn apply_terrain_op(&mut self, op: &floptle_script::TerrainOp, batch: &mut HashMap<Entity, TerrainBatch>) {
        if let Some(at) = self.resolve_terrain_op(op) {
            self.apply_resolved_op(op, &at, batch);
        }
    }

    /// Where a script terrain op lands: the nearest terrain to its world
    /// point, and the point in that terrain's own frame, as things stand now.
    /// `None` when no terrain's surface is near enough.
    fn resolve_terrain_op(&self, op: &floptle_script::TerrainOp) -> Option<OpTarget> {
        use floptle_script::TerrainOpMode as M;
        let pos = DVec3::new(op.pos[0], op.pos[1], op.pos[2]);
        // Nearest terrain by |world distance| at the op position — each field is
        // converted through its node's full frame (rotation + uniform scale), so
        // ops land right on placed/tilted/scaled terrains too.
        let mut best: Option<(Entity, Vec3, f32, f32)> = None;
        for &e in self.terrains.keys() {
            let (_, _, ts) = self.terrain_world_frame_of(e);
            let local = self.terrain_world_to_local(e, pos);
            let d = self.terrains[&e].field.d(local).abs() * ts;
            if best.as_ref().is_none_or(|b| d < b.2) {
                best = Some((e, local, d, ts));
            }
        }
        let (e, local, d, ts) = best?;
        // Too far from every surface: a mis-aimed op must not edit a random field.
        if d > op.radius + self.terrains[&e].field.band() * 2.0 * ts {
            return None;
        }
        let flat_normal = (op.mode == M::Flatten).then(|| self.flatten_normal(e, local, op.normal));
        Some(OpTarget { e, local, ts, flat_normal })
    }

    /// Apply a script terrain op at the place [`Self::resolve_terrain_op`]
    /// found for it.
    fn apply_resolved_op(&mut self, op: &floptle_script::TerrainOp, at: &OpTarget, batch: &mut HashMap<Entity, TerrainBatch>) {
        let Some(t) = self.terrains.get_mut(&at.e) else { return };
        let unmade = t.field.pending_count();
        #[cfg(test)]
        if op.radius / at.ts >= BIG_OP_VOXELS * t.field.voxel() {
            BIG_OPS_ON_FRAME.with(|n| n.set(n.get() + 1));
        }
        let (touched, measured) = make_op(&mut t.field, op, at);
        // Ground made on the frame by an edit that should have gone to the
        // worker ([`Self::op_goes_off_frame`]): tens of milliseconds a chunk.
        // Counted, and said once, so a frame it costs names its cause.
        let made = unmade.saturating_sub(t.field.pending_count());
        if made > 0 {
            self.terrain_inline_made += made;
            if !std::mem::replace(&mut self.terrain_inline_warned, true) {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("terrain: an edit made {made} chunk(s) of ground on the frame instead of off it"),
                    None,
                );
            }
        }
        self.landed_op(op, at, touched, measured, batch);
        // Ground the edit made is new geometry, whatever the edit was: a
        // paint over stand-in ground made it exactly in the field and left
        // the sim's copy a stand-in, read as not ready for good.
        if made > 0 {
            batch.entry(at.e).or_default().geom = true;
        }
    }

    /// What follows an op made, inline or on a worker: its report to the
    /// script, and its chunks into the frame's batch.
    fn landed_op(
        &mut self,
        op: &floptle_script::TerrainOp,
        at: &OpTarget,
        touched: Vec<[i32; 3]>,
        measured: Option<floptle_field::SculptYield>,
        batch: &mut HashMap<Entity, TerrainBatch>,
    ) {
        use floptle_script::TerrainOpMode as M;
        let OpTarget { e, local, ts, .. } = *at;
        let r_local = op.radius / ts;
        // Report before the empty-touch bail: a dab that moved nothing has to
        // report zero rather than nothing, or a game cannot tell "I dug air"
        // from "the report is still coming". Volumes are measured
        // in the field's local units, so a scaled terrain converts by scale³.
        if op.id != 0
            && let Some(y) = measured.or_else(|| (op.mode == M::Flatten).then(Default::default))
        {
            let w = f64::from(ts).powi(3);
            self.script_host.push_terrain_yield(floptle_script::TerrainYield {
                id: op.id,
                removed: y.removed * w,
                added: y.added * w,
                untextured: y.untextured * w,
                slots: y.slots.into_iter().map(|(s, v)| (s, v * w)).collect(),
            });
        }
        if touched.is_empty() {
            return;
        }
        let geom = !matches!(op.mode, M::Paint(_) | M::PaintTexture(_));
        // Geometry edits reach the sim's collider copy when the batch lands, so
        // collision agrees with the drawn surface this tick (color never
        // affects collision).
        let b = batch.entry(e).or_default();
        b.add_box(local - Vec3::splat(r_local), local + Vec3::splat(r_local));
        b.geom |= geom;
        b.touched.extend(touched);
    }

    /// The plane a flatten levels to, in terrain `e`'s local frame: the one the
    /// script named, else the radial up on a planet (a `CelestialBody` terrain
    /// is centred on its node), else `+Y`.
    pub(crate) fn flatten_normal(&self, e: Entity, local: Vec3, asked: Option<[f64; 3]>) -> Vec3 {
        if let Some(n) = asked {
            let (_, rot, _) = self.terrain_world_frame_of(e);
            return (rot.inverse() * DVec3::from(n).as_vec3()).try_normalize().unwrap_or(Vec3::Y);
        }
        if self.world.get::<floptle_core::CelestialBody>(e).is_some() {
            return local.try_normalize().unwrap_or(Vec3::Y);
        }
        Vec3::Y
    }



    /// Make the play sim's collider copy of terrain `e` agree with the authority
    /// field over `touched` chunks — by cloning those chunks (plus the one-chunk
    /// renormalize ring writes spill into), not by re-running the edit. A re-run
    /// can drift; a copy cannot, and a player standing on a stale invisible
    /// surface is exactly what drift looks like. Call after every authority
    /// geometry write while a sim exists: script ops, the editor brush during
    /// Play, fills, undo. Loudly warns (once per Play) if the sim has no
    /// matching terrain collider — a silent no-op here is unfindable later.
    pub(crate) fn mirror_terrain_chunks_to_sim(&mut self, e: Entity, touched: &[[i32; 3]]) {
        if touched.is_empty() || self.sim.is_none() {
            return;
        }
        let Some(t) = self.terrains.get_mut(&e) else { return };
        // What the edit wrote, to the voxel. The collider copy takes the chunks
        // holding it and forgets the meshes that read it: a dig writes a few
        // voxels of one chunk, and copying that chunk's whole neighbourhood and
        // re-meshing every collision chunk next to it was most of a bite's frame.
        let voxel = t.field.voxel();
        let (copy, stale, written) = match t.field.take_changed() {
            Some((lo, hi)) => {
                let mut copy = t.field.chunks_holding(lo, hi);
                copy.extend_from_slice(touched);
                copy.sort_unstable();
                copy.dedup();
                // The voxels written, as a box in the terrain's frame, a
                // voxel more each side for the surface between them, and no
                // wider than the chunks the edit touched: the record of
                // what changed reaches back to whatever wrote the field
                // before, a whole generated world the first time.
                let at = |i: [i32; 3], k: f32| Vec3::new(i[0] as f32 + k, i[1] as f32 + k, i[2] as f32 + k) * voxel;
                let side = floptle_field::CHUNK as f32 * voxel;
                let corner = |c: &[i32; 3]| Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32) * side;
                let (mut wlo, mut whi) = (at(lo, -1.0), at(hi, 2.0));
                if !touched.is_empty() {
                    let tlo = touched.iter().map(corner).fold(Vec3::splat(f32::MAX), Vec3::min) - Vec3::splat(voxel);
                    let thi = touched.iter().map(corner).fold(Vec3::splat(f32::MIN), Vec3::max) + Vec3::splat(side + voxel);
                    (wlo, whi) = (wlo.max(tlo), whi.min(thi));
                }
                (copy, t.field.chunks_reading(lo, hi, 1), (wlo, whi))
            }
            // No record of the write: every chunk touched and its neighbours.
            None => {
                let mut region: Vec<[i32; 3]> = Vec::new();
                for c in touched {
                    for dz in -1..=1 {
                        for dy in -1..=1 {
                            for dx in -1..=1 {
                                region.push([c[0] + dx, c[1] + dy, c[2] + dz]);
                            }
                        }
                    }
                }
                region.sort_unstable();
                region.dedup();
                let side = floptle_field::CHUNK as f32 * voxel;
                let corner = |c: &[i32; 3]| Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32) * side;
                let lo = region.iter().map(corner).fold(Vec3::splat(f32::MAX), Vec3::min);
                let hi = region.iter().map(corner).fold(Vec3::splat(f32::MIN), Vec3::max) + Vec3::splat(side);
                (region.clone(), region, (lo, hi))
            }
        };
        let t = &self.terrains[&e];
        // Recorded as the chunks it copies, so particles resting elsewhere
        // on the level are left alone.
        match self.sim.as_mut().and_then(|s| s.terrain_collider_mut_at(e.index(), &copy)) {
            Some(collider) => {
                collider.field.copy_chunks_from(&t.field, &copy);
                // The stale collision meshes are rebuilt off this thread and the
                // old triangles stand until they land, a frame or two later,
                // as the old drawn mesh does. A chunk never meshed for
                // collision needs nothing: it meshes from the new field when
                // something first stands in it.
                let worker = self.collision_worker.get_or_insert_with(TerrainWorker::spawn);
                for c in stale {
                    if !collider.is_meshed(c) {
                        continue;
                    }
                    self.terrain_epoch += 1;
                    self.collision_pending.insert((e, c), (self.terrain_epoch, self.collision_frame + COLLISION_MESH_FRAMES));
                    worker.send(RemeshJob {
                        entity: e,
                        coord: c,
                        lod: 0,
                        epoch: self.terrain_epoch,
                        scratch: JobScratch::Ready(floptle_field::scratch_for_chunk(&t.field, c, 1)),
                        collision: Some(t.field.voxel()),
                    });
                }
            }
            None => {
                if !self.terrain_mirror_warned {
                    self.terrain_mirror_warned = true;
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!(
                            "terrain edit on node #{} couldn't reach the physics sim \
                             (no matching terrain collider) — collision may not match \
                             the surface until Play restarts",
                            e.index()
                        ),
                        None,
                    );
                }
            }
        }
        // The marks over what was written are told the box, not the chunks.
        self.decals_note_dug(e, written.0, written.1);
    }

    /// Every terrain chunk a worker is making again right now, by node.
    pub(crate) fn chunks_being_made(&self) -> impl Iterator<Item = (Entity, [i32; 3])> + '_ {
        self.terrain_promotions.iter().flat_map(|j| j.coords.iter().map(move |&c| (j.e, c)))
    }

    /// Are the collision meshes of the terrain under a ball at `pos` (world),
    /// `reach` metres across, all built? The ones no query has built yet are
    /// handed to the collision worker, as a dig's are, and land within
    /// [`COLLISION_MESH_FRAMES`]; until then this is false. The worker
    /// gathers their voxels too, from a copy of the field that shares its
    /// chunks. For work that can wait a frame or two, such as laying a mark
    /// again: a query reaching a chunk nobody meshed meshes it whole on the
    /// spot, tens of milliseconds.
    pub(crate) fn collision_meshed_under(&mut self, pos: DVec3, reach: f64) -> bool {
        let Some(sim) = self.sim.as_ref() else { return true };
        let mut want: Vec<(Entity, [i32; 3])> = Vec::new();
        for (&e, t) in &self.terrains {
            let Some(col) = sim.world.colliders.iter().find(|c| c.eid == Some(e.index())).and_then(|c| c.shape.chunk_terrain()) else {
                continue;
            };
            let (anchor, rot, ts) = self.terrain_world_frame_of(e);
            let local = (rot.inverse() * (pos - anchor).as_vec3()) / ts;
            let r = Vec3::splat(reach as f32 / ts);
            for c in t.field.chunks_in_world_box(local - r, local + r) {
                // Only a chunk holding data has a surface to mesh.
                if t.field.has_data(c) && !col.is_meshed(c) {
                    want.push((e, c));
                }
            }
        }
        let mut ready = true;
        let mut snaps: HashMap<Entity, std::sync::Arc<floptle_field::ChunkField>> = HashMap::new();
        for (e, c) in want {
            ready = false;
            if self.collision_pending.contains_key(&(e, c)) {
                continue;
            }
            let t = &self.terrains[&e];
            let worker = self.collision_worker.get_or_insert_with(TerrainWorker::spawn);
            self.terrain_epoch += 1;
            self.collision_pending.insert((e, c), (self.terrain_epoch, self.collision_frame + COLLISION_MESH_FRAMES));
            worker.send(RemeshJob {
                entity: e,
                coord: c,
                lod: 0,
                epoch: self.terrain_epoch,
                scratch: JobScratch::Field(snaps.entry(e).or_insert_with(|| std::sync::Arc::new(t.field.clone())).clone(), 1),
                collision: Some(t.field.voxel()),
            });
        }
        ready
    }

    /// Hand the terrain colliders the collision meshes the worker has built,
    /// once a frame, before any tick steps.
    ///
    /// A mesh lands as soon as it is ready, and one that is due lands this frame
    /// even if the frame has to wait for it, so a run of `floptle run` lands
    /// every mesh on the same frame as the last: physics after a dig plays out
    /// the same twice. Due [`COLLISION_MESH_FRAMES`] after the edit, which the
    /// worker beats by far.
    pub(crate) fn pump_collision_meshes(&mut self) {
        self.collision_frame += 1;
        // A new frame: its script terrain edits start a fresh budget.
        self.terrain_edit_ms = 0.0;
        let Some(w) = self.collision_worker.as_mut() else { return };
        if self.collision_pending.is_empty() {
            return;
        }
        let frame = self.collision_frame;
        let t0 = floptle_core::profile::Span::new();
        let deadline = floptle_core::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            while let Some(done) = w.try_recv() {
                let key = (done.entity, done.coord);
                if self.collision_pending.get(&key).map(|p| p.0) != Some(done.epoch) {
                    continue; // a later edit of the chunk superseded it
                }
                self.collision_pending.remove(&key);
                if let (Some(collider), Some(built)) =
                    (self.sim.as_mut().and_then(|s| s.terrain_collider_mut_at(done.entity.index(), &[done.coord])), done.collision)
                {
                    collider.adopt(done.coord, built);
                }
                // The marks over it were told of the edit that stales it, and
                // wait for this mesh before they ask about their ground (the
                // decals' `decals_note_terrain_edit`, borrowing apart).
                if let Some(sim) = self.sim.as_ref() {
                    self.decal_batches.handled.insert(sim.world.level_edits);
                }
            }
            let due = self.collision_pending.values().any(|p| p.1 <= frame);
            if !due || floptle_core::time::Instant::now() >= deadline {
                break;
            }
            w.wait();
        }
        let ms = t0.ms();
        self.profile_record(floptle_core::profile::Bucket::Terrain, ms);
        // Named inside `terrain`: a slow frame here is the collider's rebuilt
        // triangles landing, or the frame waiting for them.
        self.script_host.profile().borrow_mut().record_part("collisionRebuild", ms);
    }

    /// Queue the render/shadow refresh for a terrain write at `local` (node-local):
    /// refresh the shadow proxy over the write box + merge the atlas's partial-upload
    /// region, and queue the touched chunks for remesh. Shared by the editor brush
    /// dab and the script terrain ops.
    pub(crate) fn queue_terrain_dirty(
        &mut self,
        e: Entity,
        local: Vec3,
        radius: f32,
        geom: bool,
        touched: Vec<[i32; 3]>,
    ) {
        let Some(t) = self.terrains.get_mut(&e) else { return };
        let pad = t.field.band() + t.field.voxel();
        let (wmin, wmax) =
            (local - Vec3::splat(radius + pad), local + Vec3::splat(radius + pad));
        let (mn, mx) = t.field.refresh_dense_region(&mut t.shadow, wmin, wmax);
        self.note_region_dirty(e, mn, mx, geom);
        self.terrain_chunks_dirty.entry(e).or_default().extend(touched);
        self.touch_terrain_edit(e); // an eviction must save this field first
    }

    /// Record a field edit: dirty-for-disk + a fresh edit stamp. every path that
    /// changes a field's voxels must come through here (brush/script dabs, undo
    /// swaps, generation adopts) — the stamp is how a background checkpoint
    /// knows its snapshot raced an edit, and how the picker finds quiet fields.
    pub(crate) fn touch_terrain_edit(&mut self, e: Entity) {
        self.terrain_disk_dirty.insert(e);
        self.terrain_edit_clock += 1;
        self.terrain_edit_stamps.insert(e, (self.terrain_edit_clock, floptle_core::time::Instant::now()));
    }

    /// End-of-stroke bookkeeping (mouse-up): if the stroke pushed the field past its
    /// shadow proxy's box, re-derive the proxy and re-upload the whole volume set —
    /// amortized to once per stroke, never per dab.
    pub(crate) fn end_sculpt_stroke(&mut self) {
        let Some(active) = self.active_terrain else { return };
        let Some(t) = self.terrains.get_mut(&active) else { return };
        let Some((lo, hi)) = t.field.bounds() else { return };
        let blo = Vec3::from(t.shadow.center) - Vec3::from(t.shadow.half_extent);
        let bhi = Vec3::from(t.shadow.center) + Vec3::from(t.shadow.half_extent);
        if lo.cmplt(blo).any() || hi.cmpgt(bhi).any() {
            t.rebuild_shadow();
            self.terrain_gpu_dirty = true;
        }
    }

    /// Create a fresh flat terrain as a new scene node (you can have any number). It
    /// is placed at the cursor's ground point; its field is in the node's local space.
    /// `cfg` (from the "New terrain" dialog) sizes the starting slab and paints it with
    /// a color/texture up front — the sparse field is unbounded, so this is a seed to
    /// sculpt out from, not a boundary (the slab occupies `-thickness..0` in local Y,
    /// surface at the node's height).
    #[cfg(feature = "editor-ui")]
    pub(crate) fn create_terrain(&mut self, cfg: &NewTerrainCfg) {
        self.record();
        let id = self.next_terrain_id;
        self.next_terrain_id += 1;
        let pos = self.cursor_world();
        let half_xz = cfg.size_xz.max(0.1) * 0.5;
        let thickness = cfg.thickness.max(0.5);
        let mut field = floptle_field::ChunkField::new(self.terrain_voxel.clamp(0.25, 16.0));
        field.fill_slab(
            Vec3::new(-half_xz, -thickness, -half_xz),
            Vec3::new(half_xz, 0.0, half_xz),
            0.0,
            cfg.color,
        );
        // The dialog's dropdown hands over an asset-tree path (spelled however the
        // editor was launched) — normalize to the portable project-relative form
        // before it lands in the palette file.
        let tex = crate::assets::asset_rel_path(&cfg.texture, &self.project_root);
        if let Some(slot) = self.ensure_texture_slot(&tex) {
            field.fill_texture(slot + 1);
        }
        let e = self.world.spawn();
        self.world.insert(e, Transform { translation: pos, ..Transform::IDENTITY });
        let n = self.terrains.len() + 1;
        self.world.insert(e, Name(format!("Terrain {n}")));
        self.world.insert(e, Matter::Terrain { id, collision: Default::default() });
        self.terrains.insert(e, EditorTerrain::new(field));
        self.active_terrain = Some(e);
        self.terrain_gpu_dirty = true;
        self.select_single(e);
    }

    /// Resolve a texture asset path to a terrain-palette slot (0-based), assigning it
    /// to the first empty slot if it isn't already in the palette. `None` for an empty
    /// path (no texture wanted) or a full palette with no matching existing slot.
    pub(crate) fn ensure_texture_slot(&mut self, path: &str) -> Option<u8> {
        if path.is_empty() {
            return None;
        }
        let path = crate::assets::asset_rel_path(path, &self.project_root);
        if let Some(i) = self.terrain_textures.iter().position(|p| *p == path) {
            return Some(i as u8);
        }
        let i = self.terrain_textures.iter().position(|p| p.is_empty())?;
        self.terrain_textures[i] = path;
        self.terrain_textures_dirty = true;
        Some(i as u8)
    }

    /// Where a terrain node's field is stored — one `.cfield` per terrain id, per
    /// scene (the Terrain 2.0 sparse format).
    pub(crate) fn terrain_field_path_id(&self, id: u32) -> PathBuf {
        self.sidecar_root().join("terrain").join(format!("{}.{id}.cfield", self.scene_name))
    }

    /// Stems of `.cfield` files carrying this terrain id under a different scene
    /// name. Every per-scene file is keyed by the scene's stem, so a rename of
    /// the `.ron` alone leaves the real data sitting here under the old name —
    /// which is the difference between "your terrain is gone" and "your terrain
    /// is one rename away".
    pub(crate) fn orphaned_field_stems(&self, id: u32) -> Vec<String> {
        let suffix = format!(".{id}.cfield");
        let Ok(entries) = floptle_vfs::read_dir(self.sidecar_root().join("terrain")) else {
            return Vec::new();
        };
        let mut out: Vec<String> = entries
            .into_iter()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let stem = name.strip_suffix(&suffix)?;
                (stem != self.scene_name).then(|| stem.to_string())
            })
            .collect();
        out.sort();
        out
    }

    /// The legacy dense field path for the same terrain — read-only migration source.
    pub(crate) fn terrain_tfield_path_id(&self, id: u32) -> PathBuf {
        self.sidecar_root().join("terrain").join(format!("{}.{id}.tfield", self.scene_name))
    }

    /// The tiny residency sidecar next to a terrain's `.cfield`: the impostor
    /// color ("r g b", linear floats), so a cold body can draw its sphere
    /// without ever touching the multi-MB field.
    pub(crate) fn terrain_meta_path_id(&self, id: u32) -> PathBuf {
        self.sidecar_root().join("terrain").join(format!("{}.{id}.meta", self.scene_name))
    }

    /// Meta v2: line 1 = "r g b" (impostor color), line 2 = the genspec hash the
    /// field file was written under (absent for bodies with no genspec).
    pub(crate) fn write_terrain_meta(&self, id: u32, color: [f32; 3], spec_hash: Option<u64>) {
        let p = self.terrain_meta_path_id(id);
        if let Some(dir) = p.parent() {
            let _ = floptle_vfs::create_dir_all(dir);
        }
        let mut text = format!("{} {} {}", color[0], color[1], color[2]);
        if let Some(h) = spec_hash {
            text.push_str(&format!("\n{h}"));
        }
        let _ = floptle_vfs::write(p, text);
    }

    fn read_terrain_meta(&self, id: u32) -> Option<[f32; 3]> {
        let text = floptle_vfs::read_to_string(self.terrain_meta_path_id(id)).ok()?;
        let mut it = text.split_whitespace().map(|s| s.parse::<f32>());
        match (it.next(), it.next(), it.next()) {
            (Some(Ok(r)), Some(Ok(g)), Some(Ok(b))) => Some([r, g, b]),
            _ => None,
        }
    }

    /// The view's height in pixels per radian: how big a planet is on screen
    /// at a distance. The game's camera in Play, the editor's otherwise.
    pub(crate) fn view_px_per_radian(&self) -> f64 {
        let h = self.gpu.as_ref().map_or(1080.0, |g| f64::from(g.config.height.max(1)));
        let fov = if self.playing {
            floptle_core::active_camera(&self.world)
                .and_then(|c| match self.world.get::<Matter>(c) {
                    Some(Matter::Camera { fov_y, ortho: false, .. }) => Some(f64::from(*fov_y)),
                    _ => None,
                })
                .unwrap_or(60f64.to_radians())
        } else {
            60f64.to_radians()
        };
        h / (2.0 * (fov.clamp(0.01, 3.0) * 0.5).tan())
    }

    /// The palette's layers changed: take each one's mean colour, and draw
    /// every planet's impostor in the colours its ground now draws. A resident
    /// body samples its field again; a cold one made from a genspec with no
    /// colour saved takes its recipe's again.
    pub(crate) fn set_terrain_slot_means(&mut self, means: Vec<[f32; 3]>) {
        if means == self.terrain_slot_means {
            return;
        }
        self.terrain_slot_means = means;
        for e in self.terrains.keys() {
            if let Some(r) = self.terrain_render.get_mut(e) {
                r.impostor_color = None;
            }
        }
        let cold: Vec<(Entity, u32)> = self.terrain_cold.iter().map(|(&e, c)| (e, c.id)).collect();
        for (e, id) in cold {
            if self.read_terrain_meta(id).is_some() {
                continue;
            }
            let Some(color) = self
                .world
                .get::<floptle_core::TerrainGen>(e)
                .and_then(|g| genspec_impostor_color(&g.0, &self.terrain_slot_means))
            else {
                continue;
            };
            if let Some(c) = self.terrain_cold.get_mut(&e) {
                c.color = color;
            }
            if let Some(r) = self.terrain_render.get_mut(&e) {
                r.impostor_color = Some(color);
            }
        }
    }

    /// The genspec hash the field file was written under (meta line 2, if any).
    fn read_terrain_meta_hash(&self, id: u32) -> Option<u64> {
        let text = floptle_vfs::read_to_string(self.terrain_meta_path_id(id)).ok()?;
        text.split_whitespace().nth(3)?.parse().ok()
    }

    /// The genspec-hash stamp for a node's current meta write: hash of its
    /// genspec, or None for a purely authored body.
    pub(crate) fn terrain_spec_hash_of(&self, e: Entity) -> Option<u64> {
        self.world.get::<floptle_core::TerrainGen>(e).map(|g| genspec_hash(&g.0))
    }

    // ---- background checkpoints (terrain.flush) -------------------------------

    /// Per-frame driver for background checkpoints, one job at a time: absorb
    /// `terrain.flush()` requests into the queue, drive the in-flight
    /// encode/write, start the next quiet field. The encode runs on the main
    /// thread (the field can't leave it — scripts keep digging) but only
    /// [`CHECKPOINT_CHUNKS_PER_FRAME`] chunks per frame; the file write runs
    /// on a thread. The synchronous ancestor of this froze the game ~1s per
    /// autosave on a dug-up planet — the player must never feel a checkpoint.
    pub(crate) fn step_terrain_checkpoint(&mut self) {
        // 1. terrain.flush() → queue every dirty resident field (dedup).
        if self.script_host.take_terrain_flush() {
            if self.script_host.terrain_save_dir().is_some() {
                let now = floptle_core::time::Instant::now();
                let dirty: Vec<Entity> = self
                    .terrains
                    .keys()
                    .copied()
                    .filter(|e| self.terrain_disk_dirty.contains(e))
                    .collect();
                let mut queued = 0usize;
                for e in dirty {
                    let already = self.terrain_flush_queue.iter().any(|(q, _)| *q == e)
                        || self.terrain_save_job.as_ref().is_some_and(|j| j.e == e);
                    if !already {
                        self.terrain_flush_queue.push((e, now));
                        queued += 1;
                    }
                }
                if queued > 0 {
                    self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!("Δ checkpoint: {queued} field(s) queued — saving in the background"),
                        None,
                    );
                }
            } else {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    "terrain.flush(): no save slot set (terrain.saveDir) — nothing written"
                        .into(),
                    None,
                );
            }
        }

        // 2. Drive the in-flight job.
        if let Some(mut job) = self.terrain_save_job.take() {
            match job.state {
                TerrainSaveState::Writing(rx) => match rx.try_recv() {
                    Ok(Ok(bytes)) => self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!(
                            "Δ checkpointed {} → save slot ({:.1} MB)",
                            job.name,
                            bytes as f64 / 1e6
                        ),
                        None,
                    ),
                    Ok(Err(err)) => {
                        self.terrain_disk_dirty.insert(job.e); // NOT safely on disk
                        self.console.push(
                            floptle_script::LogLevel::Error,
                            format!("Δ checkpoint of {} FAILED: {err}", job.name),
                            None,
                        );
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        job.state = TerrainSaveState::Writing(rx);
                        self.terrain_save_job = Some(job);
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.terrain_disk_dirty.insert(job.e);
                        self.console.push(
                            floptle_script::LogLevel::Error,
                            format!("Δ checkpoint writer for {} died — field kept dirty", job.name),
                            None,
                        );
                    }
                },
                TerrainSaveState::Encoding(mut saver) => {
                    let Some(t) = self.terrains.get(&job.e) else {
                        return; // field left RAM mid-encode (the evictor saved it) — drop
                    };
                    if !t.field.save_step(&mut saver, CHECKPOINT_CHUNKS_PER_FRAME) {
                        job.state = TerrainSaveState::Encoding(saver);
                        self.terrain_save_job = Some(job);
                        return;
                    }
                    let bytes = saver.finish();
                    // Clean snapshot ⇒ the file will match RAM: clear dirty now
                    // (an edit after this line re-dirties via its new stamp).
                    // Torn (an edit raced the encode) ⇒ blob is valid but mixes
                    // generations: still write it — newer than any previous
                    // file — but keep the field dirty for the next checkpoint.
                    let clean = self.terrain_edit_stamps.get(&job.e).map(|(s, _)| *s)
                        == Some(job.stamp);
                    if clean {
                        self.terrain_disk_dirty.remove(&job.e);
                    }
                    let path = job.path.clone();
                    let (tx, rx) = std::sync::mpsc::channel();
                    crate::worker::spawn("terrain-save", move || {
                        let res = (|| {
                            if let Some(dir) = path.parent() {
                                floptle_vfs::create_dir_all(dir)
                                    .map_err(|e| format!("create {dir:?}: {e}"))?;
                            }
                            floptle_vfs::write(&path, &bytes)
                                .map_err(|e| format!("write {path:?}: {e}"))?;
                            Ok(bytes.len())
                        })();
                        let _ = tx.send(res);
                    });
                    job.state = TerrainSaveState::Writing(rx);
                    self.terrain_save_job = Some(job);
                }
            }
        }

        // 3. Start the next job — quiet fields only (edited ≥1.5s ago), forced
        //    past the age cap so non-stop digging can't starve checkpoints.
        if self.terrain_save_job.is_some() || self.terrain_flush_queue.is_empty() {
            return;
        }
        let now = floptle_core::time::Instant::now();
        let stamps = &self.terrain_edit_stamps;
        let pick = self.terrain_flush_queue.iter().position(|(e, since)| {
            let quiet = stamps.get(e).is_none_or(|(_, at)| {
                now.duration_since(*at).as_secs_f64() >= CHECKPOINT_QUIET_SECS
            });
            quiet || now.duration_since(*since).as_secs_f64() >= CHECKPOINT_FORCE_SECS
        });
        let Some(i) = pick else { return };
        let (e, _) = self.terrain_flush_queue.remove(i);
        // Stale entries drop silently: written by a sync path, evicted, node
        // destroyed, or the slot closed — nothing left to checkpoint.
        if !self.terrain_disk_dirty.contains(&e) || !self.world.is_alive(e) {
            return;
        }
        let Some(t) = self.terrains.get(&e) else { return };
        let Some(sd) = self.script_host.terrain_save_path() else { return };
        let Some(Matter::Terrain { id, .. }) = self.world.get::<Matter>(e).cloned() else { return };
        let name = self
            .world
            .get::<floptle_core::Name>(e)
            .map(|n| n.0.clone())
            .unwrap_or_else(|| format!("terrain {id}"));
        let path =
            sd.join(format!("{}.{id}.cfield", self.scene_name));
        let stamp = self.terrain_edit_stamps.get(&e).map(|(s, _)| *s).unwrap_or(0);
        self.terrain_save_job = Some(TerrainSaveJob {
            e,
            name,
            path,
            stamp,
            state: TerrainSaveState::Encoding(t.field.begin_save()),
        });
    }

    /// Land or cancel the in-flight background checkpoint before a synchronous
    /// writer (Stop, eviction, scene switch) touches the same files: an encode
    /// just drops (the sync path writes fresher data anyway), an in-flight
    /// write joins — an older blob must never land after a newer sync write.
    pub(crate) fn settle_terrain_checkpoint(&mut self) {
        let Some(job) = self.terrain_save_job.take() else { return };
        if let TerrainSaveState::Writing(rx) = job.state {
            match rx.recv() {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    self.terrain_disk_dirty.insert(job.e);
                    self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("Δ checkpoint of {} FAILED: {err}", job.name),
                        None,
                    );
                }
                Err(_) => {
                    self.terrain_disk_dirty.insert(job.e);
                    self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("Δ checkpoint writer for {} died — field kept dirty", job.name),
                        None,
                    );
                }
            }
        }
    }

    /// Synchronously write every dirty resident field to the save slot — the
    /// exit-path guarantee (Stop, scene switch out of a slot): whatever the
    /// background pipeline was mid-way through, the player's edits are on disk
    /// when this returns. No-op without a slot.
    pub(crate) fn flush_slot_terrains_sync(&mut self) {
        let Some(sd) = self.script_host.terrain_save_path() else { return };
        self.settle_terrain_checkpoint();
        self.terrain_flush_queue.clear();
        let _ = self.script_host.take_terrain_flush(); // absorbed: all writes now
        let dirty: Vec<(Entity, u32)> = self
            .terrains
            .keys()
            .filter(|e| self.terrain_disk_dirty.contains(e))
            .filter_map(|&e| match self.world.get::<Matter>(e) {
                Some(Matter::Terrain { id, .. }) => Some((e, *id)),
                _ => None,
            })
            .collect();
        let mut wrote = 0usize;
        for (e, id) in dirty {
            let path =
                sd.join(format!("{}.{id}.cfield", self.scene_name));
            if let Some(dir) = path.parent() {
                let _ = floptle_vfs::create_dir_all(dir);
            }
            if let Some(t) = self.terrains.get(&e)
                && floptle_vfs::write(&path, t.field.to_bytes()).is_ok()
            {
                self.terrain_disk_dirty.remove(&e);
                wrote += 1;
            }
        }
        if wrote > 0 {
            self.console.push(
                floptle_script::LogLevel::Debug,
                format!("Δ flushed {wrote} terrain field(s) to the save slot"),
                None,
            );
        }
    }

    // ---- G1 residency (docs/subsystems/large-world-space.md) ---------------------

    /// The world-streaming work a frame does, with none of the drawing: hand
    /// queued `terrain.generatePlanet` fills to the generator, drive residency,
    /// and step the background checkpoint.
    ///
    /// It exists because a frame is not the only thing that steps this world.
    /// `floptle run` has no frame at all, and without this it never released the
    /// Play-start streaming hold ([`begin_play_terrain_hold`] auto-pauses until
    /// the ground exists). A paused session runs no fixed tick, so no rails, no
    /// physics and `dt == 0` — the run stepped its full span, reported the full
    /// span of simulated time, and had simulated **none** of it. Every script's
    /// `time` sat at zero and `space.bodies()` was empty, which reads as a game
    /// whose world was never built rather than as a runner that never started.
    ///
    /// The order is the frame's order and matters: generates before residency,
    /// or residency adopts a freshly created body as cold and streams a stale
    /// same-id field into it. Keep the two in step.
    ///
    /// No GPU: this is field/RAM bookkeeping. Meshing and upload are the frame's
    /// half and stay there.
    pub(crate) fn pump_world_streaming(&mut self) {
        let anchor = self.streaming_anchor();
        self.drain_terrain_generates();
        self.poll_terrain_generates();
        self.update_terrain_residency(anchor);
        self.stream_far_chunks(self.lod_center(anchor));
        self.step_terrain_checkpoint();
        self.publish_terrain_busy();
    }

    /// Where the moving bodies of the play session are, in world space, the
    /// nearest [`MOVING_BODY_ANCHORS`] to `near` first: each holds the ground
    /// under it whole and drawn at full detail.
    pub(crate) fn moving_bodies(&self) -> Vec<DVec3> {
        let Some(sim) = self.sim.as_ref() else { return Vec::new() };
        let near = self.lod_center(self.streaming_anchor());
        let mut v = sim.moving_body_positions();
        v.sort_by(|a, b| (*a - near).length_squared().total_cmp(&(*b - near).length_squared()));
        v.truncate(MOVING_BODY_ANCHORS);
        v
    }

    /// Where streaming is centred before any `terrain.lodAnchor`: the game's
    /// camera in Play, the editor's otherwise.
    fn streaming_anchor(&self) -> DVec3 {
        if self.playing {
            floptle_core::active_camera(&self.world)
                .map(|e| floptle_core::world_transform(&self.world, e).translation)
                .unwrap_or(self.camera.position)
        } else {
            self.camera.position
        }
    }

    /// Keep a planet's far ground put away and its near ground whole.
    ///
    /// A generated planet holds every chunk of its surface shell at full
    /// resolution, which is gigabytes at a kilometre and more. A chunk the
    /// camera is well past [`FAR_RING`] from is drawn at stride 4 or 8, from
    /// corners its stand-in holds, so it is put away, in the field, the
    /// physics sim's copy and Play's snapshot alike, and a few KB stand in for
    /// it. As the camera comes back, the chunk is made again on a worker,
    /// exactly as the generator made it, a chunk sooner than any finer mesh
    /// wants it; its coarse mesh stands until then. Chunks anybody has edited
    /// are never put away.
    pub(crate) fn stream_far_chunks(&mut self, cam_world: DVec3) {
        let t0 = floptle_core::profile::Span::new();
        self.stream_far_chunks_now(cam_world);
        // Named inside `terrain`: the frame after a big world lands put
        // thousands of its chunks away at once, and nothing said so.
        self.script_host.profile().borrow_mut().record_part("terrainStreaming", t0.ms());
        self.give_memory_back_soon();
    }

    /// Hand the memory let go of back to the system, off the frame and at
    /// most every [`TRIM_EVERY_SECS`], once something has been let go of.
    fn give_memory_back_soon(&mut self) {
        if !self.terrain_trim_due || self.terrain_trimmed.is_some_and(|t| t.elapsed().as_secs_f64() < TRIM_EVERY_SECS) {
            return;
        }
        self.terrain_trim_due = false;
        self.terrain_trimmed = Some(floptle_core::time::Instant::now());
        crate::worker::spawn("memory-trim", give_memory_back);
    }

    fn stream_far_chunks_now(&mut self, cam_world: DVec3) {
        // Land what the workers made.
        let mut landed = Vec::new();
        self.terrain_promotions.retain(|j| match j.rx.try_recv() {
            Ok(made) => {
                landed.push((j.e, made));
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
        });
        for (e, made) in landed {
            let Some(t) = self.terrains.get_mut(&e) else { continue };
            let coords: Vec<[i32; 3]> = made.iter().map(|(c, _)| *c).collect();
            let region = t.field.adopt_generated(made);
            // Recorded as the chunks made, not "anywhere": what rests on the
            // ground elsewhere is not disturbed by it, and a mark laid on
            // the exact ground these chunks bring back is not laid again.
            if let Some(col) = self.sim.as_mut().and_then(|s| s.terrain_collider_mut_at(e.index(), &coords)) {
                col.field.copy_chunks_from(&t.field, &coords);
                col.invalidate_meshes(&region);
            }
            self.terrain_chunks_dirty.entry(e).or_default().extend(region);
            self.decals_note_landed(e, coords);
        }

        self.prepare_asked_ground();
        self.terrain_far_frame = self.terrain_far_frame.wrapping_add(1);
        if !self.terrain_far_frame.is_multiple_of(FAR_SCAN_FRAMES) {
            return;
        }
        let bodies = self.moving_bodies();
        let entities: Vec<Entity> = self.terrains.keys().copied().collect();
        for e in entities {
            let t = &self.terrains[&e];
            if t.field.pending_source().is_none() || !t.field.is_generated() || t.field.spec().is_none() {
                continue;
            }
            let wt = floptle_core::world_transform(&self.world, e);
            let (anchor, rot, ts) = (wt.translation, wt.rotation.normalize(), wt.scale.x.max(1e-6));
            let chunk_units = floptle_field::CHUNK as f32 * t.field.voxel();
            let rings = rings_for_body(
                self.world.get::<floptle_core::CelestialBody>(e).map(|cb| cb.body_radius * ts as f64),
                (chunk_units * ts) as f64,
            );
            // The camera, and every moving body: ground is held whole under
            // whatever may land on it, wherever the camera is. A body three
            // kilometres from the detail's centre otherwise came down on the
            // coarse stand-in, metres off the ground it showed.
            let to_local = |w: DVec3| (rot.inverse() * (w - anchor).as_vec3()) / (chunk_units * ts);
            let eyes: Vec<Vec3> = std::iter::once(to_local(cam_world)).chain(bodies.iter().map(|b| to_local(*b))).collect();
            let dist_of = |c: [i32; 3]| {
                let lo = Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32);
                eyes.iter()
                    .map(|cl| {
                        let gap = (lo - *cl).max(*cl - (lo + Vec3::ONE)).max(Vec3::ZERO);
                        (gap.length() * RING_STEPS as f32).ceil() as i32
                    })
                    .min()
                    .unwrap_or(i32::MAX)
            };
            let in_flight: std::collections::HashSet<[i32; 3]> = self
                .terrain_promotions
                .iter()
                .filter(|j| j.e == e)
                .flat_map(|j| j.coords.iter().copied())
                .collect();
            let (mut far, mut near) = (Vec::new(), Vec::new());
            for c in t.field.chunk_coords() {
                let d = dist_of(c);
                if t.field.is_evicted(c) {
                    if d <= rings[FAR_RING] + PROMOTE_PAST_RING && !in_flight.contains(&c) {
                        near.push((d, c));
                    }
                } else if d > rings[FAR_RING] + EVICT_PAST_RING
                    && t.field.evictable(c)
                    && !self.terrain_prepared.get(&e).is_some_and(|k| k.contains(&c))
                {
                    far.push((d, c));
                }
            }
            // Made again nearest first, a few at a time on workers.
            near.sort_unstable();
            let source = t.field.pending_source();
            let jobs = self.terrain_promotions.iter().filter(|j| j.e == e).count();
            // Half the machine: a chunk takes tens of milliseconds to make
            // exactly, and a resumed planet has hundreds near the player.
            let most = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(1));
            if let Some(source) = source {
                for batch in near.chunks(PROMOTE_PER_JOB).take(most.saturating_sub(jobs)) {
                    let coords: Vec<[i32; 3]> = batch.iter().map(|(_, c)| *c).collect();
                    let (tx, rx) = std::sync::mpsc::channel();
                    let (src, work) = (source.clone(), coords.clone());
                    crate::worker::spawn("terrain-promote", move || {
                        let _ = tx.send(work.into_iter().map(|c| (c, src.generate(c))).collect());
                    });
                    self.terrain_promotions.push(PromotionJob { e, coords, rx });
                }
            }
            if far.is_empty() {
                continue;
            }
            // The farthest first, and a few a scan: a world that has just
            // landed is almost all far, and putting it all away was one frame.
            far.sort_unstable_by_key(|&(d, _)| std::cmp::Reverse(d));
            let far: Vec<[i32; 3]> = far.into_iter().take(EVICT_PER_SCAN).map(|(_, c)| c).collect();
            let evicted = self.terrains.get_mut(&e).map(|t| t.field.evict(&far)).unwrap_or_default();
            if evicted.is_empty() {
                continue;
            }
            self.terrain_trim_due = true;
            // The sim's copy and Play's snapshot share these chunks: put away
            // there too, or nothing is freed.
            let coords: Vec<[i32; 3]> = evicted.iter().map(|v| v.coord()).collect();
            if let Some(col) = self.sim.as_mut().and_then(|s| s.terrain_collider_mut_at(e.index(), &coords)) {
                col.field.apply_evictions(&evicted);
                col.invalidate_meshes(&coords);
            }
            self.decals_note_evicted(e, &coords);
            if let (Some(Matter::Terrain { id, .. }), Some((fields, _))) = (self.world.get::<Matter>(e), self.play_terrains.as_mut())
                && let Some((_, f)) = fields.iter_mut().find(|(i, _)| i == id)
            {
                f.apply_evictions(&evicted);
            }
        }
    }

    /// `terrain.prepare`: find the stand-in chunks each ask reaches, and hand
    /// them to workers a few at a time, beside the ones made on approach. A
    /// chunk made this way is kept, and so is every chunk round the ask: put
    /// away again it would be a stand-in, and `terrain.ready` would go back to
    /// false under the game that asked. (Only the chunks still to make were
    /// kept, and ground made on approach before the ask was put away again
    /// after it.)
    ///
    /// An ask over a world still being made waits for it: one taken while
    /// the world was streaming in reached no field, and was dropped.
    fn prepare_asked_ground(&mut self) {
        for ask in self.script_host.take_terrain_prepares() {
            if self.terrain_prepare_asks.len() < 256 {
                self.terrain_prepare_asks.push((ask, std::collections::HashSet::new()));
            }
        }
        if !self.terrain_prepare_asks.is_empty() {
            let unmade = self.terrain_unmade_regions();
            let mut asks = std::mem::take(&mut self.terrain_prepare_asks);
            for (ask, landed) in &mut asks {
                let p = DVec3::from(ask.center);
                for (&e, t) in &self.terrains {
                    // A world being made again answers with the field about
                    // to be replaced: the ask waits for the new one.
                    let generating = matches!(self.world.get::<Matter>(e), Some(Matter::Terrain { id, .. }) if self.planet_gen_pending.contains(id));
                    if generating || !landed.insert(e) || t.field.pending_source().is_none() {
                        continue;
                    }
                    // In the frame the script read this world in, when it did:
                    // an orbiting world has moved since.
                    let (anchor, rot, ts) = match ask.frames.iter().find(|f| f.0 == e.index()) {
                        Some(&(_, a, q)) => (DVec3::from(a), Quat::from_array(q).normalize(), self.terrain_world_frame_of(e).2),
                        None => self.terrain_world_frame_of(e),
                    };
                    let ts = ts.max(1e-6);
                    let local = (rot.inverse() * (p - anchor).as_vec3()) / ts;
                    // The box `terrain.ready` checks, and two voxels more.
                    let reach = Vec3::splat(ask.radius as f32 / ts + t.field.voxel() * 4.0);
                    let kept = self.terrain_prepared.entry(e).or_default();
                    let pending = t.field.pending_in_box(local - reach, local + reach);
                    for c in t.field.chunks_in_world_box(local - reach, local + reach) {
                        kept.insert(c);
                    }
                    for c in pending {
                        if !self.terrain_prepare_wanted.contains(&(e, c)) {
                            self.terrain_prepare_wanted.push((e, c));
                        }
                    }
                }
            }
            // Kept while ground not made yet reaches it; landed on every world
            // that is, it is done.
            asks.retain(|(ask, _)| unmade.iter().any(|u| u.touches(ask.center, ask.radius)));
            self.terrain_prepare_asks = asks;
        }
        if self.terrain_prepare_wanted.is_empty() {
            return;
        }
        let most = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(1));
        while self.terrain_promotions.len() < most && !self.terrain_prepare_wanted.is_empty() {
            let e = self.terrain_prepare_wanted[0].0;
            let Some(source) = self.terrains.get(&e).and_then(|t| t.field.pending_source()) else {
                self.terrain_prepare_wanted.retain(|(x, _)| *x != e);
                continue;
            };
            let take = self.terrain_prepare_wanted.iter().take(PROMOTE_PER_JOB).take_while(|(x, _)| *x == e).count();
            let coords: Vec<[i32; 3]> = self.terrain_prepare_wanted.drain(..take).map(|(_, c)| c).collect();
            let (tx, rx) = std::sync::mpsc::channel();
            let work = coords.clone();
            crate::worker::spawn("terrain-prepare", move || {
                let _ = tx.send(work.into_iter().map(|c| (c, source.generate(c))).collect());
            });
            self.terrain_promotions.push(PromotionJob { e, coords, rx });
        }
    }

    /// Answer `terrain.busy()` for the coming tick: true while the background
    /// terrain worker has a whole-body fill running or a field streaming in.
    ///
    /// A game that generates its world as the player travels needs this to
    /// pace itself. Both kinds of work share one background budget, so a game
    /// that queues the next star system whenever it likes queues it behind the
    /// ground somebody is currently standing on.
    pub(crate) fn publish_terrain_busy(&mut self) {
        let busy = self.terrain_worker_busy();
        self.script_host.set_terrain_busy(busy);
        let unmade = self.terrain_unmade_regions();
        self.script_host.set_terrain_unmade(unmade);
        let in_use = self.terrain_save_slot_in_use();
        self.script_host.set_terrain_save_dir_in_use(in_use);
    }

    /// Could anything still read or write the save slot? A terrain body in the
    /// world (resident or put away: streaming reads the slot first and writes
    /// edits back), or a checkpoint still being written. `terrain.deleteSaveDir`
    /// refuses the active slot only while this holds, so a game back at its
    /// menu can delete the save it just left.
    ///
    /// Asked whether or not a slot is set, because a game may set one and
    /// delete another in the same pass. The world is scanned only when no
    /// field is resident, and stops at the first terrain body.
    pub(crate) fn terrain_save_slot_in_use(&self) -> bool {
        !self.terrains.is_empty()
            || self.terrain_save_job.is_some()
            || !self.terrain_flush_queue.is_empty()
            || self.world.query::<Matter>().any(|(_, m)| matches!(m, Matter::Terrain { .. }))
    }

    /// The ground the world has not made yet, for `terrain.ready` and
    /// `terrain.raycast`: every terrain body put away cold, streaming in, or
    /// waiting for a `generatePlanet` fill, as a sphere holding all of it.
    ///
    /// Until its field lands, such a body is either missing from the colliders
    /// a query reads or is answered by the field it is about to replace, and
    /// both read as open air down to its core. A terrain with no field that is
    /// not a celestial body has no extent to report, and is left out.
    pub(crate) fn terrain_unmade_regions(&self) -> Vec<floptle_script::UnmadeRegion> {
        let mut out = Vec::new();
        for (e, m) in self.world.query::<Matter>() {
            let Matter::Terrain { id, .. } = m else { continue };
            let generating = self.planet_gen_pending.contains(id);
            // Not resident: put away, streaming in, or not yet picked up by
            // the residency driver at all (a body made this frame, or Play's
            // first pass before the streamer has run).
            if !generating && self.terrains.contains_key(&e) {
                continue;
            }
            let radius = match (generating.then(|| self.planet_gen_radius.get(id)).flatten(), self.world.get::<floptle_core::CelestialBody>(e)) {
                (Some(r), _) => f64::from(*r),
                (None, Some(cb)) => cb.body_radius,
                (None, None) => continue,
            };
            let wt = floptle_core::world_transform(&self.world, e);
            // Relief rises above the nominal radius: half as much again holds
            // every mountain a generated world is given.
            let r = radius.max(1.0) * f64::from(wt.scale.x.abs().max(1e-6)) * UNMADE_REACH;
            out.push(floptle_script::UnmadeRegion { center: wt.translation.to_array(), radius: r });
        }
        out
    }

    /// Does the background terrain worker have anything to do — a whole-body
    /// fill running or queued, or a field streaming in?
    ///
    /// one predicate, because two callers must not disagree about it:
    /// `terrain.busy()` answers a game with it, and `settle_world_streaming`
    /// waits on it. They did disagree — the wait watched only the streaming
    /// half, so `shot` would photograph a planet that was still generating as
    /// its impostor sphere and report that everything had settled. That is the
    /// exact failure `settle_world_streaming` exists to prevent, arriving
    /// through the door it left open.
    pub(crate) fn terrain_worker_busy(&self) -> bool {
        self.planet_gen_job.is_some()
            || !self.planet_gen_pending.is_empty()
            || !self.terrain_load_jobs.is_empty()
    }

    /// Is the world still arriving — terrain generating or streaming, chunk
    /// meshes queued that the view will show, or models waiting their turn?
    ///
    /// What a browser build holds its loading screen up for: the first
    /// frames of a terrain scene otherwise show the sky with the ground
    /// filling in under it, which reads as a game that is broken rather than
    /// one that is still loading.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn world_loading(&self) -> bool {
        self.terrain_worker_busy()
            || self.terrain_render.values().any(|r| !r.pending.is_empty())
            || !self.model_queue.is_empty()
    }

    /// Hold a headless loop while the background terrain threads work, until
    /// they are idle or `deadline` passes, and answer how long it held.
    ///
    /// A windowed frame gives those threads about sixteen milliseconds of wall
    /// time; `run` and `shot --after` step in microseconds, so a world being
    /// generated or streamed would otherwise never finish inside the span.
    /// Those two verbs are its only callers, and it sleeps, so it is built with them.
    #[cfg(feature = "editor-ui")]
    pub(crate) fn wait_for_terrain_workers(&mut self, deadline: floptle_core::time::Instant) -> std::time::Duration {
        let began = floptle_core::time::Instant::now();
        while self.terrain_worker_busy() && floptle_core::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(4));
            self.pump_world_streaming();
        }
        began.elapsed()
    }

    /// Mesh every resident terrain chunk the view can see and wait for it, for
    /// the one-shot verbs that have no frame loop to do it over time. Returns
    /// whether it settled inside `budget`.
    ///
    /// [`Self::settle_world_streaming`] brings the fields in; this is the other
    /// half — the triangles. The frame loop runs [`Self::sync_terrain_meshes`]
    /// before every draw, so in the windowed editor the ground is meshed as a
    /// matter of course. A verb that draws one frame with no loop in front of
    /// it has to run the pump itself, or the field is resident, the collider
    /// is live, the character stands on the ground — and the picture shows it
    /// standing on nothing, because not one chunk mesh ever reached the GPU.
    /// The same silent shape as the map meshes and the map paint that `shot`
    /// already syncs by hand.
    ///
    /// The first pass is a full re-plan (every chunk queued at once); the near
    /// ring meshes synchronously inside it and the rest goes through the
    /// worker, which is capped in flight and topped up by later passes — and
    /// the coverage scan itself runs every fourth pass per terrain. So this
    /// pumps until several consecutive passes find nothing pending, not until
    /// the first one does.
    pub(crate) fn settle_terrain_meshes(
        &mut self,
        cam_world: DVec3,
        budget: std::time::Duration,
    ) -> bool {
        let deadline = floptle_core::time::Instant::now() + budget;
        let mut quiet = 0u32;
        let mut first = true;
        loop {
            self.sync_terrain_meshes(first, cam_world);
            first = false;
            let pending = self.terrain_render.values().any(|r| !r.pending.is_empty());
            if pending {
                quiet = 0;
            } else {
                quiet += 1;
                // Past the scan throttle with nothing queued: settled.
                if quiet > 4 {
                    return true;
                }
                continue;
            }
            if floptle_core::time::Instant::now() >= deadline {
                return false;
            }
            #[cfg(not(target_arch = "wasm32"))]
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
    }

    /// Stream the world in and wait for it, for the one-shot verbs that have no
    /// frame loop to do it over time. Returns whether it settled inside `budget`.
    ///
    /// A celestial terrain arrives on a background thread, so a renderer that
    /// opens a scene and draws it immediately draws every planet as its impostor
    /// sphere — a smooth ball where the ground, the rocks and the buildings
    /// should be. That picture is not a slightly worse picture: `shot` exists to
    /// be believed, and a photograph of a world that had not loaded yet is a
    /// confident answer to a question nobody asked.
    ///
    /// Settled = the terrain worker had nothing to do across a full pass —
    /// nothing generating, nothing queued to generate, nothing streaming in.
    /// The extra pass matters because the pass that lands the last load is also
    /// the pass that may kick the next one.
    pub(crate) fn settle_world_streaming(
        &mut self,
        anchor: DVec3,
        budget: std::time::Duration,
    ) -> bool {
        // Residency anchors on the editor camera outside Play, and for a shot
        // the view being photographed is the presence in the world.
        self.camera.position = anchor;
        let deadline = floptle_core::time::Instant::now() + budget;
        let mut quiet = false;
        loop {
            self.pump_world_streaming();
            if !self.terrain_worker_busy() {
                if quiet {
                    return true;
                }
                // The confirming pass, taken immediately and without spending
                // any of the budget: nothing is in flight, so there is nothing
                // to wait for — this pass only asks whether the last one kicked
                // anything off. Deadline-checking here instead would report a
                // world with no terrain at all as "still streaming" whenever the
                // budget was tight, and `shot` would print a warning about
                // impostors that are not in the picture.
                quiet = true;
                continue;
            }
            quiet = false;
            // Out of budget with work still in flight is the honest false: the
            // caller is about to photograph something unfinished and has to be
            // told so.
            if floptle_core::time::Instant::now() >= deadline {
                return false;
            }
            // The work is on other threads; this one only has to let them finish.
            // (In a browser the work ran inline when it was queued, so the loop
            // above finds it done and there is nothing to wait for.)
            #[cfg(not(target_arch = "wasm32"))]
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    }

    /// Per-frame residency driver: land finished background loads, kick loads
    /// for cold bodies something is approaching, evict residents left behind.
    ///
    /// During Play the anchors are the world positions of every dynamic body
    /// (ship, astronaut, debris), the point streaming centres on (the game's
    /// camera, or its `terrain.lodAnchor` while one is set), plus any bodies
    /// the game explicitly warmed (`terrain.warm` — the map's focused planet).
    /// A body loads as any anchor nears it and is put away only once every
    /// one has left it far behind, so a map that pulls the camera across the
    /// system never unloads the planet the player stands on (that evicted the
    /// ground under the player's feet and dropped them through the world).
    /// The camera counts because what it nears has to have ground to draw: a
    /// belt rock the camera flew up to with no body beside it stayed a plain
    /// ball, its ground never made. In edit mode the editor camera is your
    /// presence, so it anchors there.
    /// The impostor flip stays camera-based — that's visual LOD (screen size),
    /// a different question from which fields are in RAM.
    ///
    /// Runs outside the render borrows (it may rebuild the sim on a mid-Play
    /// arrival/eviction) — called right before `sync_terrain_meshes` each frame.
    pub(crate) fn update_terrain_residency(&mut self, cam_world: DVec3) {
        // Gameplay anchors + this frame's warm requests (immediate mode — the
        // map re-warms its focus every frame while open).
        let anchors: Vec<DVec3> = if self.playing {
            self.world
                .query::<floptle_core::RigidBody>()
                .map(|(e, _)| floptle_core::world_transform(&self.world, e).translation)
                .chain(std::iter::once(self.lod_center(self.streaming_anchor())))
                .collect()
        } else {
            vec![cam_world]
        };
        // (terrain.flush() checkpoints are handled by `step_terrain_checkpoint`
        // — background, amortized. Exit paths use `flush_slot_terrains_sync`.)
        let warm_names = self.script_host.take_terrain_warm();
        let warm: std::collections::HashSet<Entity> = if warm_names.is_empty() {
            Default::default()
        } else {
            self.world
                .query::<floptle_core::Name>()
                .filter(|(_, n)| warm_names.contains(&n.0))
                .map(|(e, _)| e)
                .collect()
        };
        let near = |p: DVec3, reach: f64| anchors.iter().any(|a| (*a - p).length() < reach);
        // What could fall through ground not there yet: in Play the bodies,
        // which come first among the anchors, and not the camera, which only
        // has to wait to draw it. In edit mode the camera is all there is.
        let bodies = if self.playing { anchors.len() - 1 } else { anchors.len() };
        let body_near = |p: DVec3, reach: f64| anchors[..bodies].iter().any(|a| (*a - p).length() < reach);
        // A body a game keeps meshed farther out than the default loads and is
        // put away as much farther out, so its ground is in before it draws.
        let px_per_radian = self.view_px_per_radian();
        let rules = self.terrain_impostor_rules.clone();
        let reach = |id: u32| (impostor_enter_radii(rules.get(&id).copied(), px_per_radian) / IMPOSTOR_RADII).max(1.0);

        // 0. Adopt terrain bodies born at runtime: a game's loading screen builds
        //    its galaxy with createNode + setTerrainGen, and those nodes went
        //    through no scene load — they were in neither the resident nor the
        //    cold set, so nothing drew or streamed them (planets that were only
        //    their atmosphere, warm/TAB focuses that never landed). Anything
        //    untracked becomes cold exactly like adopt_terrain would make it —
        //    unless its first generation is queued (the generation queue owns
        //    those until the fill lands).
        let untracked: Vec<(Entity, u32)> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Terrain { id, .. } => Some((e, *id)),
                _ => None,
            })
            .filter(|(e, id)| {
                !self.terrains.contains_key(e)
                    && !self.terrain_cold.contains_key(e)
                    && !self.planet_gen_pending.contains(id)
                    && self.world.get::<floptle_core::CelestialBody>(*e).is_some()
            })
            .collect();
        for (e, id) in untracked {
            let has_file = floptle_vfs::exists(self.terrain_field_path_id(id));
            let color = (has_file.then(|| self.read_terrain_meta(id)).flatten())
                .or_else(|| {
                    self.world
                        .get::<floptle_core::TerrainGen>(e)
                        .map(|g| genspec_impostor_color(&g.0, &self.terrain_slot_means).unwrap_or([0.62, 0.62, 0.68]))
                })
                // A file with no meta and no genspec is still loadable — a
                // neutral sphere beats an invisible planet.
                .or_else(|| has_file.then_some([0.62, 0.62, 0.68]));
            let Some(color) = color else { continue }; // nothing to ever load: skip
            self.next_terrain_id = self.next_terrain_id.max(id + 1);
            let render = self.terrain_render.entry(e).or_default();
            render.impostor = true;
            render.impostor_color = Some(color);
            self.terrain_cold.insert(e, ColdTerrain { id, color });
        }
        // 1. Land finished loads (parse/generate + shadow proxy all happened on
        //    the thread). Failures are loud and final: a body whose stream died
        //    (bad genspec, unreadable file, a panicked generation = disconnected
        //    channel) leaves the cold set entirely — silent retry-forever was
        //    exactly the "I focused it and it never appeared" bug.
        let mut landed: Vec<(Entity, EditorTerrain, String, f64)> = Vec::new();
        let mut failed: Vec<(Entity, String)> = Vec::new();
        self.terrain_load_jobs.retain(|job| match job.rx.try_recv() {
            Ok(Some(t)) => {
                landed.push((
                    job.e,
                    t,
                    job.name.clone(),
                    job.started.elapsed().as_secs_f64(),
                ));
                false
            }
            Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                failed.push((job.e, job.name.clone()));
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
        });
        for (e, t, name, secs) in landed {
            // Accept as long as the body still lacks a resident field — cold
            // membership isn't required (the generation queue may have adopted
            // or a state edge dropped the cold entry mid-flight).
            if self.world.is_alive(e) && !self.terrains.contains_key(&e) {
                self.finish_terrain_load(e, t);
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!("Δ {name} terrain ready ({secs:.1}s)"),
                    None,
                );
            }
        }
        for (e, name) in failed {
            self.terrain_cold.remove(&e); // impostor forever; never retry-loop
            self.console.push(
                floptle_script::LogLevel::Error,
                format!(
                    "Δ {name} terrain FAILED to stream (bad genspec, unreadable field, \
                     or the generation crashed) — the body stays an impostor"
                ),
                None,
            );
        }

        // 2. Kick background loads for cold bodies inside an anchor's load radius
        //    or explicitly warmed (the map's focused planet loads however far it
        //    is), plus a blocking emergency load if a body is practically on one.
        let mut to_sync: Vec<(Entity, u32)> = Vec::new();
        let mut to_load: Vec<(Entity, u32)> = Vec::new();
        for (&e, cold) in &self.terrain_cold {
            let Some(cb) = self.world.get::<floptle_core::CelestialBody>(e) else { continue };
            // Heal the impostor render entry — it is the body's visual while cold
            // (covers any path that dropped it, e.g. a scene-switch edge).
            let render = self.terrain_render.entry(e).or_default();
            if !render.impostor {
                render.impostor = true;
                render.impostor_color = Some(cold.color);
            }
            let r = cb.body_radius.max(1.0);
            let p = floptle_core::world_transform(&self.world, e).translation;
            // The blocking emergency load is for mid-play surprises (teleports,
            // summons) — during the Play-start hold the same bodies stream in
            // the background instead (the run is paused; nothing can fall).
            // A camera alone near a cold body streams it in the background: a
            // scene loaded with the view beside an authored planet read and
            // parsed its 15 MB field on the frame, with nothing there to fall.
            if !self.play_stream_hold && body_near(p, r * RESIDENT_SYNC_RADII) {
                to_sync.push((e, cold.id));
            } else if warm.contains(&e) || near(p, r * RESIDENT_LOAD_RADII * reach(cold.id)) {
                to_load.push((e, cold.id));
            }
        }
        for (e, id) in to_sync {
            self.load_terrain_blocking(e, id);
        }
        for (e, id) in to_load {
            self.kick_terrain_load(e, id);
        }

        // 3. Evict residents every anchor has left far behind (celestials only —
        //    flat level terrains have no meaningful radius and stay resident).
        //    Warmed bodies are exempt however far away they are.
        let mut to_evict: Vec<(Entity, u32)> = Vec::new();
        for &e in self.terrains.keys() {
            if warm.contains(&e) {
                continue;
            }
            let Some(cb) = self.world.get::<floptle_core::CelestialBody>(e) else { continue };
            let Some(Matter::Terrain { id, .. }) = self.world.get::<Matter>(e) else { continue };
            let r = cb.body_radius.max(1.0);
            let p = floptle_core::world_transform(&self.world, e).translation;
            if !near(p, r * RESIDENT_EVICT_RADII * reach(*id)) {
                to_evict.push((e, *id));
            }
        }
        for (e, id) in to_evict {
            self.evict_terrain_to_cold(e, id);
        }

        // 4. Release the Play-start streaming hold once nothing required (a cold
        //    body someone is standing on) remains — the game starts itself the
        //    moment the ground exists. Keeps re-kicking until then (the 2-job cap
        //    frees up as loads land; failures drop out of the cold set above, so
        //    a dead stream can't hold Play hostage).
        if self.play_stream_hold && self.playing {
            let need = self.required_unready_terrains();
            if need.is_empty() {
                self.play_stream_hold = false;
                self.paused = false;
                self.toast = Some(("▶  World ready".into(), 2.0));
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    "▶ world streamed in — play resumed".into(),
                    None,
                );
            } else {
                for (e, id, cold) in need {
                    if cold {
                        self.kick_terrain_load(e, id);
                    } // mid-generation bodies land via the generation queue
                }
            }
        }
    }

    /// The ordered candidate sources for a cold terrain (G2) — the loader tries
    /// them in sequence, so a corrupt file degrades to regeneration instead of
    /// taking the world offline:
    ///   1. the game's save-slot file (player-edited state; trusted as-is —
    ///      a slot belongs to one galaxy by the game's own contract),
    ///   2. the project file (authored/cached) — but only if it was written
    ///      under the node's current genspec (meta hash line): regeneration
    ///      reuses terrain ids, and a stale file from the previous system must
    ///      not load as the new body's terrain,
    ///   3. the genspec itself (deterministic on-demand generation).
    ///
    /// Empty = nothing to load from (stays an impostor).
    fn resolve_terrain_source(&self, e: Entity, id: u32) -> Vec<TerrainSource> {
        let mut out = Vec::new();
        let genspec = self.world.get::<floptle_core::TerrainGen>(e).map(|g| g.0.clone());
        if let Some(sd) = self.script_host.terrain_save_path() {
            let p = sd
                .join(format!("{}.{id}.cfield", self.scene_name));
            if floptle_vfs::exists(&p) {
                out.push(TerrainSource::File(p));
            }
        }
        let p = self.terrain_field_path_id(id);
        if floptle_vfs::exists(&p) {
            let trusted = match &genspec {
                None => true, // purely authored body: the file IS the truth
                Some(g) => self.read_terrain_meta_hash(id) == Some(genspec_hash(g)),
            };
            if trusted {
                out.push(TerrainSource::File(p));
            }
        }
        if let Some(g) = genspec {
            out.push(TerrainSource::Generate(g));
        }
        out
    }

    /// The save-slot destination for an edited field, when the game set one.
    fn terrain_save_slot_path(&self, id: u32) -> Option<PathBuf> {
        self.script_host.terrain_save_path().map(|sd| {
            sd.join(format!("{}.{id}.cfield", self.scene_name))
        })
    }

    /// Spawn a background load/generate job for a cold terrain (capped at 2 in
    /// flight; duplicates are no-ops). Reads + parses a file, or generates the
    /// whole planet from its genspec — either way the shadow proxy derives on
    /// the thread too, so the main thread never hitches (the no-stutter rule).
    pub(crate) fn kick_terrain_load(&mut self, e: Entity, id: u32) {
        if self.terrain_load_jobs.iter().any(|j| j.e == e)
            || self.terrain_load_jobs.len() >= 2
        {
            return;
        }
        let mut sources = self.resolve_terrain_source(e, id);
        let genspec = self.world.get::<floptle_core::TerrainGen>(e).map(|g| g.0.clone());
        // Where the player is, in the body's frame: what is made first.
        let view = self.landing_view(e);
        let near = Some(view.near);
        // A running `terrain.generatePlanet` batch owns generation: while the
        // game's spawn planet (or any explicit fill) is being built, residency
        // must not start more generations behind it — the player is standing
        // on (or waiting for) the batch's world, and every competing generate
        // steals its cores. Fast file loads stay allowed; genspec-only bodies
        // simply stay cold impostors and re-kick once the batch lands.
        if self.planet_gen_job.is_some() || !self.planet_gen_pending.is_empty() {
            sources.retain(|s| matches!(s, TerrainSource::File(_)));
            if sources.is_empty() {
                return; // no console line: this retries every frame until then
            }
        }
        if sources.is_empty() {
            // Nothing to load from at all (no file, no genspec): stop trying.
            self.terrain_cold.remove(&e);
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!("Δ terrain id {id}: no field file and no genspec — impostor only"),
                None,
            );
            return;
        }
        let name = self
            .world
            .get::<floptle_core::Name>(e)
            .map(|n| n.0.clone())
            .unwrap_or_else(|| format!("terrain {id}"));
        self.console.push(
            floptle_script::LogLevel::Debug,
            match &sources[0] {
                TerrainSource::File(_) => format!("Δ streaming {name} in (field file)…"),
                TerrainSource::Generate(_) => {
                    format!("Δ streaming {name} in (generating from its genspec)…")
                }
            },
            None,
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let generator = self.project.terrain_generator_resolved();
        crate::worker::spawn("terrain-load", move || {
            let _ = tx.send(load_terrain_from(sources, genspec, near, generator, Some(view)));
        });
        self.terrain_load_jobs.push(TerrainLoadJob {
            e,
            name,
            started: floptle_core::time::Instant::now(),
            rx,
        });
    }

    /// Make terrain `e`'s shadow proxy on a worker, from a copy of its field
    /// (the chunks are shared, so the copy is a table, not the voxels). It
    /// lands in [`Self::land_terrain_proxies`]; until then the stand-in
    /// casts nothing.
    pub(crate) fn kick_terrain_proxy(&mut self, e: Entity) {
        let Some(t) = self.terrains.get(&e) else { return };
        let field = t.field.clone();
        let key = t.shadow_key;
        let stamp = self.terrain_edit_stamps.get(&e).map_or(0, |s| s.0);
        self.terrain_proxy_jobs.retain(|j| j.e != e);
        let (tx, rx) = std::sync::mpsc::channel();
        crate::worker::spawn("terrain-proxy", move || {
            let _ = tx.send(shadow_proxy_of(&field));
        });
        self.terrain_proxy_jobs.push(ProxyJob { e, key, stamp, rx });
    }

    /// Put the proxies made off the frame in place, each under a new key, and
    /// have the atlas laid again around them (within its budget, the old
    /// slots drawn until the new atlas is whole). One made from a field
    /// edited since is made again from the field as it is now.
    pub(crate) fn land_terrain_proxies(&mut self) {
        let mut landed = Vec::new();
        self.terrain_proxy_jobs.retain(|j| match j.rx.try_recv() {
            Ok(baked) => {
                landed.push((j.e, j.key, j.stamp, baked));
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
        });
        for (e, key, stamp, baked) in landed {
            let now = self.terrain_edit_stamps.get(&e).map_or(0, |s| s.0);
            let Some(t) = self.terrains.get_mut(&e) else { continue };
            if t.shadow_key != key {
                continue;
            }
            if now != stamp {
                self.kick_terrain_proxy(e);
                continue;
            }
            t.shadow = baked;
            t.shadow_key = next_shadow_key();
            self.terrain_atlas_dirty = true;
        }
    }

    /// Land everything the frame loop lands over frames for the shadow atlas
    /// and the terrain palette, and lay the atlas whole: for a verb that draws
    /// one picture with no frames before it. Bounded, so a GPU that never
    /// finishes ends in a picture rather than a hang.
    #[cfg(feature = "editor-ui")]
    pub(crate) fn settle_terrain_gpu(&mut self) {
        let until = floptle_core::time::Instant::now() + std::time::Duration::from_secs(30);
        self.sync_terrain_palette(true);
        loop {
            // Lands the proxies and the occluder bakes that are in, and lays
            // the atlas a budget further.
            self.sync_terrain_gpu();
            if let Some(g) = self.gpu.as_ref() {
                g.queue.submit(None);
                let _ = g.device.poll(wgpu::PollType::wait_indefinitely());
            }
            let jobs = !self.terrain_proxy_jobs.is_empty() || !self.occluder_bake_jobs.is_empty();
            let building = self.raymarch.as_ref().is_some_and(|r| r.atlas_building());
            let waiting = self.terrain_gpu_dirty || self.terrain_atlas_dirty || !self.terrain_region_dirty.is_empty();
            if self.gpu.is_none() || !(jobs || building || waiting) || floptle_core::time::Instant::now() >= until {
                break;
            }
            if jobs {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }

    /// Where the player is in terrain `e`'s frame, for what is made with a
    /// field on its way to `e`: the ground nearest first, and the meshes
    /// round it ([`EditorTerrain::premesh`]).
    pub(crate) fn landing_view(&self, e: Entity) -> LandingView {
        let wt = floptle_core::world_transform(&self.world, e);
        let ts = wt.scale.x.max(1e-6);
        let cam = self.lod_center(self.streaming_anchor());
        LandingView {
            near: (wt.rotation.normalize().inverse() * (cam - wt.translation).as_vec3()) / ts,
            body_radius: self.world.get::<floptle_core::CelestialBody>(e).map(|cb| cb.body_radius * ts as f64),
            scale: ts,
        }
    }

    /// Blocking load — the mid-play emergency inside `RESIDENT_SYNC_RADII`
    /// (teleports, summons). Only a file loads synchronously; a body whose only
    /// source is its genspec delegates to the background (a 10-second
    /// generation must never freeze a frame — Play start covers the common
    /// case with the streaming hold instead).
    pub(crate) fn load_terrain_blocking(&mut self, e: Entity, id: u32) -> bool {
        let sources = self.resolve_terrain_source(e, id);
        match sources.first() {
            None => {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!(
                        "Δ terrain id {id}: no field file and no genspec — impostor only"
                    ),
                    None,
                );
                self.terrain_cold.remove(&e); // don't retry every frame
                false
            }
            Some(TerrainSource::Generate(_)) => {
                self.kick_terrain_load(e, id);
                false
            }
            // A body with a recipe generates on every load: a file of edits is
            // laid over a fresh generation, and a whole-field file is compared
            // with one to find its edits. Neither belongs in a frame.
            Some(TerrainSource::File(_)) if self.world.get::<floptle_core::TerrainGen>(e).is_some() => {
                self.kick_terrain_load(e, id);
                false
            }
            Some(TerrainSource::File(_)) => {
                // Synchronously try the file candidates only — a corrupt file
                // must fall back to the background chain (which ends in the
                // genspec), never to an in-frame generation.
                // A file of edits is laid over a generation, so it goes to the
                // background too.
                // Named: the one terrain load a frame still waits for, because
                // something stands on the ground it holds.
                let t0 = floptle_core::profile::Span::new();
                let field = sources.iter().find_map(|s| match s {
                    TerrainSource::File(p) => floptle_vfs::read(p).ok().and_then(|b| floptle_field::ChunkField::from_bytes(&b)),
                    TerrainSource::Generate(_) => None,
                });
                self.script_host.profile().borrow_mut().record_part("terrainLoad", t0.ms());
                match field {
                    Some(f) if !f.is_delta() => {
                        self.terrain_load_jobs.retain(|j| j.e != e); // job now stale
                        // Its collider is needed now; its shadow is not.
                        self.finish_terrain_load(e, EditorTerrain::proxy_later(f));
                        self.kick_terrain_proxy(e);
                        true
                    }
                    _ => {
                        self.kick_terrain_load(e, id);
                        false
                    }
                }
            }
        }
    }

    /// A field arrived (background or blocking): make it resident. During Play the
    /// sim rebuilds so the body's collider exists — the established mid-Play path.
    fn finish_terrain_load(&mut self, e: Entity, t: EditorTerrain) {
        // What a world arriving costs the frame, named as a generated one's is.
        let t0 = floptle_core::profile::Span::new();
        // Making it let go of a great deal: see `give_memory_back`.
        self.terrain_trim_due = true;
        // A whole-field save that found its edits on loading is written again
        // at the next save, as edits: nothing dug since would otherwise leave
        // the old file, hundreds of MB, in the slot for good.
        let converted = t.field.was_converted();
        self.terrain_cold.remove(&e);
        self.terrains.insert(e, t);
        if converted {
            self.touch_terrain_edit(e);
        }
        // Its shadow volume joins the atlas, and its meshes plan from scratch;
        // every other terrain's stay as they are.
        self.terrain_atlas_dirty = true;
        self.terrain_replan.insert(e);
        // Its colour is taken from the ground itself now it is here: a colour
        // saved by an older engine was the paint's tint alone.
        if let Some(r) = self.terrain_render.get_mut(&e) {
            r.impostor_color = None;
        }
        // The render entry stays in impostor mode; the streaming loop flips it by
        // distance (still beyond 60 r at load time, so nothing pops).
        if self.playing {
            self.play_loaded_terrains.insert(e);
            self.land_terrain_in_sim(e);
            self.console.push(
                floptle_script::LogLevel::Debug,
                "Δ terrain streamed in — collision live".into(),
                None,
            );
        }
        self.script_host.profile().borrow_mut().record_part("planetLanded", t0.ms());
    }

    /// Drop a far resident to cold: save the field first if it changed (edit
    /// mode — a dug cave 110 radii away is never lost), cache the impostor
    /// color in the `.meta` sidecar, keep the render entry as a pure impostor.
    /// During Play nothing saves (Stop reverts terrain anyway).
    fn evict_terrain_to_cold(&mut self, e: Entity, id: u32) {
        // Beyond the evict radius the body has been an impostor for a while, so
        // its GPU slots are already free — if not (a hysteresis edge), wait.
        if self
            .terrain_render
            .get(&e)
            .is_some_and(|r| !r.slots.is_empty() || !r.pending.is_empty())
        {
            return;
        }
        // A background checkpoint mid-flight on this body races the eviction
        // save (same file) — settle it first; its queue entries are moot.
        if self.terrain_save_job.as_ref().is_some_and(|j| j.e == e) {
            self.settle_terrain_checkpoint();
        }
        self.terrain_flush_queue.retain(|(q, _)| *q != e);
        let Some(t) = self.terrains.get(&e) else { return };
        let color = self
            .terrain_render
            .get(&e)
            .and_then(|r| r.impostor_color)
            .unwrap_or_else(|| {
                let r = self
                    .world
                    .get::<floptle_core::CelestialBody>(e)
                    .map(|c| c.body_radius as f32)
                    .unwrap_or(50.0);
                impostor_surface_color(&t.field, r, &self.terrain_slot_means)
            });
        // Edited fields save before dropping. Destination (G2): the game's
        // save-slot dir when set (player state — writable even during Play,
        // that's the whole point of a save slot), else the project file (edit-
        // mode authoring). Playing with no save slot = drop without saving
        // (Stop reverts terrain anyway — today's Play semantics).
        if self.terrain_disk_dirty.contains(&e) {
            let dest = self
                .terrain_save_slot_path(id)
                .or_else(|| (!self.playing).then(|| self.terrain_field_path_id(id)));
            if let Some(path) = dest {
                if let Some(dir) = path.parent() {
                    let _ = floptle_vfs::create_dir_all(dir);
                }
                if floptle_vfs::write(&path, t.field.to_bytes()).is_err() {
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!("terrain id {id}: eviction save FAILED — keeping it resident"),
                        None,
                    );
                    return; // never drop unsaved edits
                }
                self.terrain_disk_dirty.remove(&e);
            }
        }
        if !self.playing {
            let hash = self.terrain_spec_hash_of(e);
            self.write_terrain_meta(id, color, hash);
        }
        self.terrains.remove(&e);
        self.terrain_prepared.remove(&e);
        self.terrain_prepare_wanted.retain(|(x, _)| *x != e);
        self.play_loaded_terrains.remove(&e);
        let render = self.terrain_render.entry(e).or_default();
        render.impostor = true;
        render.impostor_color = Some(color);
        self.terrain_cold.insert(e, ColdTerrain { id, color });
        self.terrain_gpu_dirty = true; // shadow atlas re-lays out without it
        // Mid-Play the sim still holds this body's collider, and with the field
        // gone nothing re-anchors it — a frozen collider drifting away from its
        // orbiting planet. Rebuild without it (symmetric with the load path).
        if self.playing {
            self.rebuild_sim();
        }
    }

    /// The legacy single-terrain field path (migrated to the id-keyed name on load).
    pub(crate) fn legacy_terrain_field_path(&self) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.tfield", self.scene_name))
    }

    /// After loading a scene, adopt every terrain node + load its field from disk.
    /// Order: `.cfield` (Terrain 2.0) → legacy dense `.tfield` (auto-migrated into the
    /// sparse store, old scenes just work) → a fresh flat slab. Call once `scene_name`
    /// is set.
    pub(crate) fn adopt_terrain(&mut self) {
        // A checkpoint mid-flight for the old scene must land before its
        // entities are forgotten (entity ids recycle across scene loads).
        self.settle_terrain_checkpoint();
        self.terrain_flush_queue.clear();
        self.terrain_edit_stamps.clear();
        self.terrains.clear();
        self.terrain_cold.clear();
        self.terrain_disk_dirty.clear();
        self.terrain_load_jobs.clear();
        self.terrain_proxy_jobs.clear();
        self.terrain_prepare_asks.clear();
        self.terrain_prepare_wanted.clear();
        self.terrain_prepared.clear();
        self.play_loaded_terrains.clear();
        self.active_terrain = None;
        self.terrain_slots.clear();
        let nodes: Vec<(Entity, u32)> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Terrain { id, .. } => Some((e, *id)),
                _ => None,
            })
            .collect();
        let mut max_id = 0u32;
        let mut missing: Vec<(u32, String)> = Vec::new();
        let single = nodes.len() == 1;
        for (e, id) in nodes {
            max_id = max_id.max(id);
            // G1/G2 residency: a celestial body starts cold — no field read, no
            // generation — whenever its impostor color is knowable up front:
            // from the meta sidecar (a field file exists), or from its genspec's
            // surface palette (the galaxy path — the body has no file anywhere
            // and generates on first approach). The per-frame residency driver
            // streams in whatever the camera is actually near, so scene open
            // gets faster as systems get bigger. Bodies with a file but no meta
            // yet (pre-G1 scenes) load eagerly below, cache their color, and go
            // cold from then on. Non-celestial terrains are always resident.
            if self.world.get::<floptle_core::CelestialBody>(e).is_some() {
                let color = if floptle_vfs::exists(self.terrain_field_path_id(id)) {
                    self.read_terrain_meta(id)
                } else {
                    // A genspec body is always cold (falling through to the eager
                    // path would give it a flat starter slab — it has no file to
                    // load); a garbled spec just gets a neutral sphere color.
                    self.world.get::<floptle_core::TerrainGen>(e).map(|g| {
                        genspec_impostor_color(&g.0, &self.terrain_slot_means).unwrap_or([0.62, 0.62, 0.68])
                    })
                };
                if let Some(color) = color {
                    let render = self.terrain_render.entry(e).or_default();
                    render.impostor = true;
                    render.impostor_color = Some(color);
                    self.terrain_cold.insert(e, ColdTerrain { id, color });
                    continue;
                }
            }
            let dense_migration = || {
                floptle_vfs::read(self.terrain_tfield_path_id(id))
                    .ok()
                    .and_then(|b| floptle_field::Terrain::from_bytes(&b))
                    // legacy single-terrain scenes stored one `<scene>.tfield`.
                    .or_else(|| {
                        if single {
                            floptle_vfs::read(self.legacy_terrain_field_path())
                                .ok()
                                .and_then(|b| floptle_field::Terrain::from_bytes(&b))
                        } else {
                            None
                        }
                    })
                    // Resample the dense grid into the sparse store at cubic voxels —
                    // this is also what retires the voxel-stretch artifact on old
                    // (18:1-stretched) fields.
                    .map(|t| {
                        floptle_field::ChunkField::from_dense(
                            &t.baked,
                            terrain_voxel_size(&t.baked),
                        )
                    })
            };
            let loaded = floptle_vfs::read(self.terrain_field_path_id(id))
                .ok()
                .and_then(|b| floptle_field::ChunkField::from_bytes(&b))
                .and_then(|f| whole_field(f, self.project.terrain_generator_resolved()))
                .or_else(dense_migration);
            // A terrain node in a saved scene that has no field on disk is the
            // shape of lost work, not of a new terrain — say so rather than
            // handing back a flat slab that looks identical to one.
            if loaded.is_none() {
                let name = self
                    .world
                    .get::<floptle_core::Name>(e)
                    .map(|n| n.0.clone())
                    .unwrap_or_else(|| format!("Terrain {id}"));
                missing.push((id, name));
            }
            let field = loaded
                // a terrain node with no/garbled field → start it flat.
                .unwrap_or_else(|| {
                    let mut f =
                        floptle_field::ChunkField::new(self.terrain_voxel.clamp(0.25, 16.0));
                    f.fill_slab(
                        Vec3::new(-16.0, -6.0, -16.0),
                        Vec3::new(16.0, 0.0, 16.0),
                        0.0,
                        [0.35, 0.6, 0.28],
                    );
                    f
                });
            // Self-heal the residency sidecar: an eagerly-loaded celestial (pre-G1
            // scene, no `.meta` yet) computes its impostor color once now, so it
            // can go cold on every later load/evict.
            if let Some(cb) = self.world.get::<floptle_core::CelestialBody>(e)
                && self.read_terrain_meta(id).is_none()
                && floptle_vfs::exists(self.terrain_field_path_id(id))
            {
                let color = impostor_surface_color(&field, cb.body_radius as f32, &self.terrain_slot_means);
                let hash = self.terrain_spec_hash_of(e);
                self.write_terrain_meta(id, color, hash);
            }
            // The shadow proxy is made off the frame: a scene with a big
            // terrain spent half a second of its opening on it.
            self.terrains.insert(e, EditorTerrain::proxy_later(field));
            self.kick_terrain_proxy(e);
        }
        self.next_terrain_id = max_id + 1;
        for (id, name) in missing {
            let want = self.terrain_field_path_id(id);
            let rel = want
                .strip_prefix(&self.project_root)
                .unwrap_or(&want)
                .to_string_lossy()
                .replace('\\', "/");
            // A field for this terrain id filed under another scene's name is the
            // fingerprint of a renamed scene: every per-scene file is keyed by the
            // stem, so the data is sitting right there under the old one.
            let orphans = self.orphaned_field_stems(id);
            let msg = if orphans.is_empty() {
                format!("Δ {name}: no terrain data at {rel} — starting flat")
            } else {
                let found: Vec<String> =
                    orphans.iter().map(|s| format!("terrain/{s}.{id}.cfield")).collect();
                format!(
                    "Δ {name}: no terrain data at {rel}, but {} exists. If this scene was \
                     renamed, its terrain did not follow — rename that file to match the \
                     scene and reopen.",
                    found.join(", ")
                )
            };
            self.console.push(floptle_script::LogLevel::Warn, msg, None);
        }
        self.terrain_gpu_dirty = !self.terrains.is_empty();
        // Restore the texture palette so painted-texture slots map to images again.
        // A slot line may end in `|glow` — that slot's texture is self-lit (the
        // cave-visibility channel); the marker rides the sidecar, not the path.
        // (cold terrains count — their fields still splat this palette on load.)
        if (!self.terrains.is_empty() || !self.terrain_cold.is_empty())
            && let Ok(text) = floptle_vfs::read_to_string(self.terrain_palette_path()) {
                let slots = floptle_render::TERRAIN_SLOTS as usize;
                let mut glow = 0u32;
                let mut scales: Vec<f32> = Vec::new();
                // One line per slot, `path` plus zero or more `|flag` suffixes.
                // Unknown suffixes are ignored rather than folded into the path:
                // this file is written by newer editors than the one reading it
                // as often as the other way round, and a slot whose texture path
                // had `|someflag` glued onto it would silently stop resolving.
                let mut palette: Vec<String> = text
                    .lines()
                    .enumerate()
                    .map(|(i, line)| {
                        let mut parts = line.split('|');
                        // An older editor wrote absolute paths here; one inside
                        // the project is read back relative, so it matches the
                        // slot a picker would give and saves portable.
                        let path = match parts.next().unwrap_or("") {
                            "" => String::new(),
                            p => floptle_scene::portable::rel_path(p, &self.project_root),
                        };
                        let mut scale = 1.0f32;
                        for flag in parts {
                            if flag == "glow" {
                                glow |= 1 << i.min(31);
                            } else if let Some(v) = flag.strip_prefix("scale=") {
                                scale = v.parse().ok().filter(|f: &f32| f.is_finite() && *f > 0.0).unwrap_or(1.0);
                            }
                        }
                        scales.push(scale);
                        path
                    })
                    .collect();
                palette.resize(slots, String::new());
                scales.resize(slots, 1.0);
                self.terrain_textures = palette;
                self.terrain_tex_scale = scales;
                self.terrain_glow_mask = glow;
                self.terrain_textures_dirty = true;
            }
    }

/// The world translation of a terrain node (places its field in world space).
    /// A terrain node's world placement: translation + rotation + uniform scale
    /// (x drives — an SDF can't stretch per-axis without breaking the distance
    /// metric). The one frame every terrain consumer (render, physics, brush,
    /// script ops, LOD) converts through, so they can never disagree.
    pub(crate) fn terrain_world_frame_of(&self, e: Entity) -> (DVec3, Quat, f32) {
        let wt = floptle_core::world_transform(&self.world, e);
        (wt.translation, wt.rotation.normalize(), wt.scale.x.max(1e-6))
    }

    /// Where terrain detail, the impostor switch and far-craft freezing are
    /// measured from: the game's `terrain.lodAnchor` during Play, when it named
    /// one that still exists, else `camera`.
    pub(crate) fn lod_center(&self, camera: DVec3) -> DVec3 {
        self.lod_anchor_world().unwrap_or(camera)
    }

    /// The game's `terrain.lodAnchor` in world space, during Play, when it
    /// names a point or a node that still exists.
    pub(crate) fn lod_anchor_world(&self) -> Option<DVec3> {
        if !self.playing {
            return None;
        }
        match self.script_host.terrain_lod_anchor()? {
            floptle_script::LodAnchor::Node(id) => self
                .world
                .entity_at(id)
                .filter(|e| self.world.get::<floptle_core::Transform>(*e).is_some())
                .map(|e| floptle_core::world_transform(&self.world, e).translation),
            floptle_script::LodAnchor::Point(p) => Some(DVec3::from(p)),
        }
    }

    /// World point → a terrain's field-local frame.
    pub(crate) fn terrain_world_to_local(&self, e: Entity, p: DVec3) -> Vec3 {
        let (anchor, rot, s) = self.terrain_world_frame_of(e);
        (rot.inverse() * (p - anchor).as_vec3()) / s
    }

    /// Which terrain a whole-terrain op (Fill) targets: the selected terrain node, or
    /// the one last sculpted, or — if there's exactly one — that terrain.
    pub(crate) fn target_terrain(&self) -> Option<Entity> {
        if let Some(&e) = self.selection.last()
            && self.terrains.contains_key(&e) {
                return Some(e);
            }
        if let Some(e) = self.active_terrain
            && self.terrains.contains_key(&e) {
                return Some(e);
            }
        if self.terrains.len() == 1 {
            return self.terrains.keys().next().copied();
        }
        None
    }

    /// Fill the raymarch globals' per-volume slots: each uploaded terrain's box,
    /// composed anchor (node f64 translation) + local center first, then
    /// camera-relative — exact at any world distance. Each volume samples
    /// its own atlas slot at native resolution; overlapping volumes fuse on the GPU
    /// with the same smin the old CPU combine used (k = 0.6).
    /// (Associated fn taking explicit fields — callers sit inside the render section
    /// where `self.gpu`/`self.egui` are mutably borrowed, so `&self` is unavailable.)
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fill_terrain_volumes(
        terrains: &HashMap<Entity, EditorTerrain>,
        slots: &[TerrainSlot],
        occluders: &HashMap<Entity, (OccKey, std::sync::Arc<floptle_field::BakedSdf>)>,
        occ_regions: &HashMap<OccKey, usize>,
        budget: u32,
        world: &floptle_core::World,
        g: &mut RaymarchGlobals,
        cam_world: DVec3,
    ) {
        g.params[2] = 0.1; // blob↔terrain blend k (the old single-field look)
        for (i, slot) in slots.iter().take(floptle_render::MAX_VOLUMES).enumerate() {
            let e = slot.e;
            // A just-deleted terrain leaves a stale slot until the atlas is
            // re-laid without it — leave it absent (w = 0).
            if !terrains.contains_key(&e) {
                continue;
            }
            let wt = floptle_core::world_transform(world, e);
            // The shadow/AO volume shader samples an axis-aligned, unit-scale box:
            // a rotated or scaled terrain would cast its unrotated shadow, which
            // reads as broken. Skip its volume instead (meshes still render + AO
            // via SSAO; revisit with per-volume rotation in the field shader).
            let placed = wt.rotation.normalize().w.abs() < 0.99999
                || (wt.scale.x - 1.0).abs() > 1e-3;
            if placed {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    floptle_say::say_err!(
                        "[terrain] a rotated/scaled terrain skips SDF sun-shadow/AO \
                         casting (v1 limitation — rendering & collision are exact)"
                    );
                }
                continue;
            }
            let anchor = wt.translation;
            // The box of the proxy the slot holds, which is the terrain's
            // own until a newer one has been uploaded whole.
            let (bc, hf) = (slot.center, slot.half);
            let cr = anchor + DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - cam_world;
            // w = 3: shadow + AO, not drawn. Terrain 2.0 draws the extracted chunk meshes
            // through the raster pass (`push_terrain_instances`); the raymarch stops
            // sphere-tracing terrain but its field keeps casting sun shadows and darkening
            // props that stand on it (that is what `w = 3` means, vs `w = 2` which would
            // drop terrain out of the AO field — trap T2).
            g.vol_center[i] = [cr.x as f32, cr.y as f32, cr.z as f32, 3.0];
            g.vol_half[i] = [hf[0], hf[1], hf[2], 0.6];
        }
        // Mesh shadow occluders take the instances after the terrains, flagged
        // shadow-only (w = 2): the shadow march folds them in, the drawn field
        // skips them. Each draws from its bake's region of the atlas, so copies
        // of one model cost one bake. When more want to cast than there are
        // instances, the ones nearest the camera win, this frame: shadows
        // follow the player around the level instead of going to whichever
        // meshes sort first by name. A node that doesn't cast (switched off, or
        // hidden) takes no instance at all.
        let first = slots.len().min(floptle_render::MAX_VOLUMES);
        let room = (budget as usize).min(floptle_render::MAX_VOLUMES).saturating_sub(first);
        struct Cand {
            gap: f64,
            id: u32,
            region: usize,
            centre: [f32; 3],
            half: [f32; 3],
        }
        let mut cands: Vec<Cand> = occluders
            .iter()
            .filter_map(|(&e, (key, b))| {
                let casts = world.get::<floptle_core::CastShadow>(e).is_none_or(|c| c.0)
                    && !floptle_core::is_hidden(world, e);
                if !casts {
                    return None;
                }
                let region = *occ_regions.get(key)?;
                let anchor = floptle_core::world_transform(world, e).translation;
                let (bc, hf) = (b.center, b.half_extent);
                let cr = anchor + DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - cam_world;
                // How far the camera is from the box, not its centre: a long
                // building the player stands beside is near however far away
                // its middle is.
                let half = DVec3::new(hf[0] as f64, hf[1] as f64, hf[2] as f64);
                let gap = (cr.abs() - half).max(DVec3::ZERO).length();
                Some(Cand {
                    gap,
                    id: e.index(),
                    region,
                    centre: [cr.x as f32, cr.y as f32, cr.z as f32],
                    half: hf,
                })
            })
            .collect();
        if cands.len() > room {
            cands.sort_by(|a, b| a.gap.total_cmp(&b.gap).then(a.id.cmp(&b.id)));
        } else {
            // Everyone fits: order only has to be stable.
            cands.sort_by_key(|c| c.id);
        }
        for (j, c) in cands.into_iter().take(room).enumerate() {
            let i = first + j;
            g.vol_center[i] = [c.centre[0], c.centre[1], c.centre[2], 2.0];
            g.vol_half[i] = [c.half[0], c.half[1], c.half[2], 0.0];
            g.vol_atlas[i][3] = (c.region + 1) as f32;
        }
    }

    /// The surface [`Material`] that drives terrain shading. Terrain uses the same
    /// lighting model as the meshes, so this picks whose lighting params (ambient,
    /// specular/reflectiveness, rim, emissive, unlit, color tint) every terrain
    /// adopts: the active terrain's material if it has one, else any terrain that has
    /// one, else a neutral matte default. Per-terrain color still comes from painting.
    pub(crate) fn terrain_material(&self) -> MaterialParams {
        let pick = self
            .active_terrain
            .filter(|e| self.world.get::<Material>(*e).is_some())
            .or_else(|| {
                self.terrains
                    .keys()
                    .copied()
                    .find(|&e| self.world.get::<Material>(e).is_some())
            });
        pick.and_then(|e| self.world.get::<Material>(e))
            .map(material_params)
            .unwrap_or_else(|| MaterialParams::flat([1.0, 1.0, 1.0]))
    }
}

#[cfg(test)]
mod occluder_tests {
    use floptle_core::math::DVec3;
    use floptle_core::{Transform, World};
    use std::collections::HashMap;

    /// **Copies of one model share a bake, and the nearest ones cast.** Forty
    /// copies of one building plus five props with shadows switched off; a
    /// budget of 32: exactly the 32 nearest buildings get an instance, every
    /// one drawing from the single shared region, and the switched-off props
    /// take none of the budget.
    #[test]
    fn copies_share_one_bake_and_the_nearest_ones_cast() {
        let bake = std::sync::Arc::new(floptle_field::BakedSdf {
            dims: [4, 4, 4],
            center: [0.0, 5.0, 0.0],
            half_extent: [3.0, 5.0, 3.0],
            distance: vec![1.0; 64],
            color: vec![[255; 4]; 64],
        });
        let key: crate::shading::OccKey = ("models/building.glb".into(), [0, 0, 0, 1000], [1000; 3]);
        let mut world = World::new();
        let mut occluders = HashMap::new();
        let mut near = Vec::new();
        for i in 0..45 {
            let e = world.spawn();
            // Placed in a shuffled order, so nearness is not spawn order.
            let d = ((i * 17) % 45) as f64 * 20.0;
            world.insert(e, Transform::from_translation(DVec3::new(d, 0.0, 0.0)));
            if i >= 40 {
                world.insert(e, floptle_core::CastShadow(false));
            } else {
                near.push((d, e.index()));
            }
            occluders.insert(e, (key.clone(), bake.clone()));
        }
        near.sort_by(|a, b| a.0.total_cmp(&b.0));
        let regions = HashMap::from([(key.clone(), 3usize)]);
        let mut g = floptle_render::RaymarchGlobals::default();
        crate::Editor::fill_terrain_volumes(&HashMap::new(), &[], &occluders, &regions, 32, &world, &mut g, DVec3::ZERO);
        let placed: Vec<usize> = (0..floptle_render::MAX_VOLUMES).filter(|&i| g.vol_center[i][3] >= 0.5).collect();
        assert_eq!(placed.len(), 32, "the budget was not filled, or overfilled");
        assert!(placed.iter().all(|&i| g.vol_atlas[i][3] == 4.0), "an instance did not draw from the shared bake");
        // The 32 placed are the 32 nearest casters.
        let mut xs: Vec<f64> = placed.iter().map(|&i| g.vol_center[i][0] as f64).collect();
        xs.sort_by(f64::total_cmp);
        let want: Vec<f64> = near[..32].iter().map(|n| n.0).collect();
        assert_eq!(xs, want, "the kept instances are not the nearest ones");

        // Everyone fits: no choice to make, all 40 casters placed.
        let mut g = floptle_render::RaymarchGlobals::default();
        crate::Editor::fill_terrain_volumes(&HashMap::new(), &[], &occluders, &regions, 64, &world, &mut g, DVec3::ZERO);
        let n = (0..floptle_render::MAX_VOLUMES).filter(|&i| g.vol_center[i][3] >= 0.5).count();
        assert_eq!(n, floptle_render::MAX_VOLUMES, "a budget past the shader's capacity must stop at it");
    }

    /// **A prop a script resizes every frame is baked once, not every frame.**
    /// A collidable mesh flinching by a few percent a frame keeps its last
    /// bake while it changes, is baked once at the size it settles on, and
    /// going back to a size it already has costs nothing.
    #[test]
    fn a_resize_every_frame_bakes_once_it_settles() {
        use floptle_assets::glb_write::{WriteMesh, WriteNode, write_glb};
        use floptle_core::math::Vec3;
        let root = std::env::temp_dir().join(format!("floptle_occ_settle_{}", std::process::id()));
        std::fs::create_dir_all(root.join("models")).unwrap();
        let mesh = WriteMesh {
            positions: vec![[-1.0, 0.0, -1.0], [1.0, 0.0, -1.0], [0.0, 2.0, 1.0], [0.0, 0.0, 1.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            indices: vec![0, 1, 2, 1, 3, 2, 3, 0, 2, 0, 3, 1],
            base_color: [1.0; 4],
            ..Default::default()
        };
        std::fs::write(root.join("models/mound.glb"), write_glb(&[WriteNode::mesh_node("m", mesh)], &[], &[], &[])).unwrap();

        let mut ed = crate::Editor { project_root: root.clone(), ..Default::default() };
        let e = ed.world.spawn();
        ed.world.insert(e, Transform::from_translation(DVec3::ZERO));
        ed.world.insert(e, floptle_core::Matter::Mesh { asset_path: "models/mound.glb".into() });
        ed.world.insert(e, floptle_core::Collidable);
        // Bakes are made off the frame: frames until none is on its way, and
        // how many of them changed the set. Bounded by time, not frames.
        let land = |ed: &mut crate::Editor| -> u32 {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
            let mut changed = 0;
            loop {
                changed += ed.refresh_mesh_occluders() as u32;
                if ed.occluder_bake_jobs.is_empty() {
                    return changed;
                }
                assert!(std::time::Instant::now() < until, "a bake never landed");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        assert_eq!(land(&mut ed), 1, "the first bake changes the set once, when it lands");
        assert_eq!(ed.occluder_bakes, 1, "the first bake");
        // A model read and voxelised on the frame was about 100 ms of it.
        assert_eq!(super::made_here("occluder"), 0, "the bake was made on the frame");

        // Flinch: a different size every frame for a second and a half,
        // decaying back to where it started.
        let mut changed = 0;
        for f in 0..90 {
            let s = 1.0 + 0.05 * (1.0 - f as f32 / 89.0) * (f as f32 * 0.7).sin().abs();
            ed.world.get_mut::<Transform>(e).unwrap().scale = Vec3::splat(s);
            changed += ed.refresh_mesh_occluders() as u32;
        }
        for _ in 0..(super::OCCLUDER_SETTLE_FRAMES + 10) {
            changed += ed.refresh_mesh_occluders() as u32;
        }
        assert_eq!(ed.occluder_bakes, 1, "a node still changing size was rebaked");
        assert_eq!(changed, 0, "a flinch that comes back to rest re-uploaded the atlas");

        // Held at one size: baked exactly once, then left alone.
        ed.world.get_mut::<Transform>(e).unwrap().scale = Vec3::splat(1.2);
        let mut changed = 0;
        for _ in 0..(super::OCCLUDER_SETTLE_FRAMES + 10) {
            changed += ed.refresh_mesh_occluders() as u32;
        }
        changed += land(&mut ed);
        assert_eq!(ed.occluder_bakes, 2, "the settled size was not baked, or baked more than once");
        assert_eq!(changed, 1, "the atlas should be re-uploaded once, when the new bake lands");
        let (key, _) = &ed.mesh_occluders[&e];
        assert_eq!(key.2, [1200; 3], "the occluder is not at the size the node settled on");

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod tests {
    use super::terrain_voxel_size;
    use super::{CHUNK_FADE_SECS, chunk_fade, chunk_priority, lod_for, raw_lod, rings_for_body, LOD_RINGS, RING_STEPS};
    use floptle_core::math::{DVec3, Quat};
    use floptle_field::BakedSdf;

    /// **A map camera does not move the terrain's centre of detail.**
    /// With an anchor on the ship, the render camera can go from 50 to 80 body
    /// radii and back and the terrain is measured from the ship throughout, so
    /// nothing remeshes or switches to its impostor. The switch itself has
    /// real hysteresis: in at 60 radii, out at 42.
    #[test]
    fn terrain_detail_follows_the_anchor_not_a_roaming_camera() {
        use super::{impostor_next, IMPOSTOR_RADII};
        let r = 150.0;
        assert!(!impostor_next(false, 59.0 * r, r, IMPOSTOR_RADII) && impostor_next(false, 61.0 * r, r, IMPOSTOR_RADII));
        assert!(impostor_next(true, 45.0 * r, r, IMPOSTOR_RADII), "a camera parked just inside 60 radii does not flip it back");
        assert!(!impostor_next(true, 41.0 * r, r, IMPOSTOR_RADII), "it returns to meshes inside 42 radii");

        let mut ed = crate::Editor::default();
        let ship = ed.world.spawn();
        ed.world.insert(ship, floptle_core::Transform::from_translation(DVec3::new(0.0, r + 2.0, 0.0)));
        let far = |k: f64| DVec3::new(0.0, 0.0, k * r);
        assert_eq!(ed.lod_center(far(80.0)), far(80.0), "outside Play the camera is the centre");
        ed.playing = true;
        assert_eq!(ed.lod_center(far(80.0)), far(80.0), "no anchor: the camera");
        ed.script_host.set_terrain_lod_anchor(Some(floptle_script::LodAnchor::Node(ship.index())));
        for k in [50.0, 80.0, 50.0] {
            let c = ed.lod_center(far(k));
            assert_eq!(c, DVec3::new(0.0, r + 2.0, 0.0), "camera at {k} R: the ship stays the centre");
            assert!(!impostor_next(false, c.length(), r, IMPOSTOR_RADII), "and the planet under it stays meshed");
        }
        ed.world.despawn(ship);
        assert_eq!(ed.lod_center(far(80.0)), far(80.0), "an anchor node that is gone hands it back to the camera");
        ed.script_host.set_terrain_lod_anchor(Some(floptle_script::LodAnchor::Point([1.0, 2.0, 3.0])));
        assert_eq!(ed.lod_center(far(80.0)), DVec3::new(1.0, 2.0, 3.0));
    }

    /// **Which plane a flatten levels to.** On a planet the radial up at
    /// the brush, on a flat terrain `+Y`, and a script's own normal turned into
    /// the terrain's frame.
    #[test]
    fn a_flatten_levels_to_the_planets_up_unless_told_otherwise() {
        use floptle_core::math::Vec3;
        let mut ed = crate::Editor::default();
        let flat = ed.world.spawn();
        ed.world.insert(flat, floptle_core::Transform::IDENTITY);
        let planet = ed.world.spawn();
        ed.world.insert(planet, floptle_core::Transform::IDENTITY);
        ed.world.insert(planet, floptle_core::CelestialBody { body_radius: 100.0, ..Default::default() });
        let at = Vec3::new(0.0, 70.0, 70.0);
        let n = ed.flatten_normal(planet, at, None);
        assert!((n - at.normalize()).length() < 1e-5, "a planet levels to its radial up, got {n}");
        assert_eq!(ed.flatten_normal(flat, at, None), Vec3::Y, "a flat terrain levels to +Y");

        let tilted = ed.world.spawn();
        ed.world.insert(
            tilted,
            floptle_core::Transform { rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2), ..floptle_core::Transform::IDENTITY },
        );
        let n = ed.flatten_normal(tilted, at, Some([0.0, 1.0, 0.0]));
        assert!((n - Vec3::X).length() < 1e-5, "world up is local +X on a node turned 90° about Z, got {n}");
    }

    /// The wait and the flag are the same question.
    ///
    /// `terrain.busy()` answers a game; `settle_world_streaming` waits before a
    /// shot. When the wait watched only the streaming half, a planet that was
    /// still generating settled instantly and was photographed as its impostor
    /// sphere — the exact failure the wait exists to prevent.
    #[test]
    fn the_wait_and_the_busy_flag_ask_the_same_question() {
        let mut ed = crate::Editor::default();
        assert!(!ed.terrain_worker_busy(), "an empty editor has no terrain work");
        // A queued whole-body fill counts, even though nothing is streaming.
        ed.planet_gen_pending.insert(3);
        assert!(ed.terrain_worker_busy(), "a queued fill is work");
        assert!(
            !ed.settle_world_streaming(DVec3::ZERO, std::time::Duration::ZERO),
            "…so the wait must not report a still-generating world as settled"
        );
        ed.planet_gen_pending.clear();
        assert!(ed.settle_world_streaming(DVec3::ZERO, std::time::Duration::from_secs(1)));
    }

    /// A world with nothing to stream settles, and says so, without spending
    /// the budget it was offered.
    ///
    /// `shot` calls this before it takes the picture and prints a warning about
    /// photographing impostors when it comes back false. A scene with no terrain
    /// in it must therefore come back true, promptly — a warning that fires on
    /// every shot of every 2D project is a warning nobody reads by the time a
    /// planet really is unfinished.
    #[test]
    fn a_world_with_nothing_to_stream_settles_at_once() {
        let mut ed = crate::Editor::default();
        let t0 = floptle_core::time::Instant::now();
        assert!(
            ed.settle_world_streaming(DVec3::ZERO, std::time::Duration::from_secs(45)),
            "an empty world is a settled world"
        );
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(1),
            "settling nothing must not wait: took {:?}",
            t0.elapsed()
        );
        // …and the same answer when it is handed no budget at all, because the
        // question "is anything in flight" does not need one.
        assert!(ed.settle_world_streaming(DVec3::ZERO, std::time::Duration::ZERO));
    }

    /// A chunk that just arrived dissolves in over its first moments, monotonically
    /// — and a chunk with no arrival stamp is simply opaque, which
    /// The reported bug, in numbers: *"I can see through unloaded terrain and it
    /// needs to prioritize loading what's right under me"*.
    ///
    /// One queue serves every terrain. Under the old key — chunk distance in
    /// each terrain's own local frame — the ground under your feet and a chunk
    /// on a planet twelve thousand units away were literally equal, and the
    /// winner was `HashMap` iteration order.
    #[test]
    fn the_ground_under_your_feet_outranks_a_planet_twelve_thousand_units_away() {
        let cam = DVec3::new(0.0, 100.0, 0.0);
        let here = DVec3::ZERO; // the body you are standing on
        let far = DVec3::new(12_000.0, 0.0, 0.0); // another world entirely
        let chunk = 48.0f32;
        let coord = [0, 2, 0]; // three chunks out in BOTH terrains' local frames

        let under_me = chunk_priority(coord, chunk, here, Quat::IDENTITY, cam, true);
        let over_there = chunk_priority(coord, chunk, far, Quat::IDENTITY, cam, false);
        assert!(
            under_me < over_there,
            "the ground under the camera queued behind a planet 12 km away \
             ({under_me} vs {over_there})"
        );
        // And it is not a near-tie decided by rounding — it is four orders of
        // magnitude, which is the difference the old key threw away.
        assert!(over_there > under_me * 100, "{under_me} vs {over_there}");
    }

    /// Within one terrain the key still orders by nearness, or the fix would
    /// have traded a cross-terrain bug for an intra-terrain one.
    #[test]
    fn nearer_chunks_still_come_first_within_one_terrain() {
        let cam = DVec3::ZERO;
        let at = |c: [i32; 3]| chunk_priority(c, 48.0, DVec3::ZERO, Quat::IDENTITY, cam, true);
        let mut d: Vec<i32> = (0..6).map(|i| at([i, 0, 0])).collect();
        let sorted = {
            let mut s = d.clone();
            s.sort_unstable();
            s
        };
        assert_eq!(d, sorted, "priority is not monotonic in distance: {d:?}");
        d.dedup();
        assert!(d.len() >= 5, "distinct distances collapsed to the same priority: {d:?}");

        // A body the camera is not on is penalised as a whole, so no chunk of it
        // can slip ahead of the ground being stood on.
        let off = chunk_priority([0, 0, 0], 48.0, DVec3::ZERO, Quat::IDENTITY, cam, false);
        assert!(off > at([200, 0, 0]), "an off-body chunk beat a distant on-body one");
    }

    /// A body on the far side of a solar system must not wrap into the front of
    /// the queue. Metres are an i32, and a real system is millions of units wide.
    #[test]
    fn an_absurdly_distant_body_does_not_wrap_to_the_front() {
        let cam = DVec3::ZERO;
        let miles_away = DVec3::new(9.0e14, 0.0, 0.0);
        let p = chunk_priority([0, 0, 0], 48.0, miles_away, Quat::IDENTITY, cam, false);
        assert!(p > 0, "distance overflowed to {p} and would be meshed first");
        assert!(p > chunk_priority([9, 9, 9], 48.0, DVec3::ZERO, Quat::IDENTITY, cam, true));
    }

    /// is what keeps every already-resident chunk from flickering on the frame this
    /// shipped.
    #[test]
    fn a_new_chunk_dissolves_in_and_an_old_one_is_just_there() {
        assert_eq!(chunk_fade(10.0, 10.0), 0.0, "born this instant: nothing of it yet");
        assert!(chunk_fade(10.0 + CHUNK_FADE_SECS * 0.5, 10.0) > 0.4);
        assert!(chunk_fade(10.0 + CHUNK_FADE_SECS * 0.5, 10.0) < 0.6);
        assert_eq!(chunk_fade(10.0 + CHUNK_FADE_SECS, 10.0), 1.0, "done fading");
        assert_eq!(chunk_fade(1000.0, 10.0), 1.0, "and it stays done");

        // Never backwards: a threshold that is not ordered in alpha turns the
        // dissolve into a flicker, which is worse than the pop it replaces.
        let mut prev = 0.0;
        for i in 0..=20 {
            let f = chunk_fade(10.0 + CHUNK_FADE_SECS * i as f32 / 20.0, 10.0);
            assert!(f >= prev, "fade went backwards at step {i}: {f} after {prev}");
            prev = f;
        }

        // A clock that restarted under a loaded scene must not leave chunks
        // invisible forever — the one failure worse than never fading.
        assert_eq!(chunk_fade(0.5, 900.0), 1.0, "a stamp in the future reads as opaque");

        // …and with no clock at all there is no stamp, so nothing is mid-fade.
        // `now()` is an Option for exactly this: a frozen zero would read as
        // "every chunk started dissolving and never finished", which draws no
        // terrain at all. An Editor with no window has no clock.
        let ed = crate::Editor::default();
        assert!(ed.now().is_none(), "no window, no clock — and so nothing to fade against");
    }

    /// A chunk coarsens only a chunk past its ring (hysteresis), so a camera
    /// drifting across a ring edge can't flip it every frame; it refines as
    /// soon as it is inside the finer ring, because waiting there kept the
    /// ground ahead of a walking player coarse until they stood in it.
    #[test]
    fn lod_rings_have_hysteresis() {
        let r = LOD_RINGS;
        let b = r[0]; // the lod0/lod1 boundary
        // Fresh chunks take the raw ring.
        assert_eq!(raw_lod(b, r), 0);
        assert_eq!(raw_lod(b + 1, r), 1);
        // A lod0 chunk up to a chunk past the boundary stays lod0…
        assert_eq!(lod_for(b + RING_STEPS, 0, r), 0);
        // …and coarsens once clearly past it.
        assert_eq!(lod_for(b + RING_STEPS + 1, 0, r), 1);
        // A lod1 chunk back inside the finer ring refines at once.
        assert_eq!(lod_for(b, 1, r), 0);
        assert_eq!(lod_for(b - 1, 1, r), 0);
        // No-change fast path.
        assert_eq!(lod_for(2, 0, r), 0);
        assert_eq!(lod_for(100, 3, r), 3);
        // Far chunks are lod3 regardless of history.
        assert_eq!(lod_for(LOD_RINGS[2] + RING_STEPS + 1, 0, r), 3);
    }

    /// Surface chunks of a sphere of `radius`, and how many of them the rings
    /// would queue at full detail from a camera standing on it.
    ///
    /// The count is the point: with the whole body of a walkable planet inside
    /// ring 0, arriving would mean surface-net meshing all
    /// of it through a 16-deep queue — the hitch, and then the pop-in as the
    /// queue drained.
    fn lod0_chunks(radius: f64, chunk: f64, rings: [i32; 3]) -> (usize, usize) {
        let n = (radius / chunk).ceil() as i32 + 1;
        // Stand on the north pole; the camera's chunk is the top of the sphere.
        let cam = [0, (radius / chunk).round() as i32, 0];
        let (mut surface, mut full) = (0, 0);
        for x in -n..=n {
            for y in -n..=n {
                for z in -n..=n {
                    // A chunk the surface passes through: its corner span
                    // straddles the radius.
                    let lo = ((x * x + y * y + z * z) as f64).sqrt() * chunk;
                    let hi = (((x.abs() + 1).pow(2) + (y.abs() + 1).pow(2) + (z.abs() + 1).pow(2))
                        as f64)
                        .sqrt()
                        * chunk;
                    if !(lo <= radius && hi >= radius) {
                        continue;
                    }
                    surface += 1;
                    // How far the camera, on the surface over the pole, is from
                    // the chunk's nearest face, in the rings' quarter chunks.
                    let eye = [0.0, radius / chunk, 0.0];
                    let gap = |c: i32, e: f64| ((c as f64 - e).max(e - (c + 1) as f64)).max(0.0);
                    let g = (gap(x, eye[0]).powi(2) + gap(y, eye[1]).powi(2) + gap(z, eye[2]).powi(2)).sqrt();
                    let d = (g * RING_STEPS as f64).ceil() as i32;
                    let _ = cam;
                    if raw_lod(d, rings) == 0 {
                        full += 1;
                    }
                }
            }
        }
        (surface, full)
    }

    /// The measurement, as a chunk count rather than a frame time — a count is
    /// what a test can hold, and the frame time follows it.
    #[test]
    fn a_walkable_planet_no_longer_meshes_its_whole_surface_at_full_detail() {
        // Solar's planets are 100–230 units; one chunk is 32 voxels × 1.5.
        let (radius, chunk) = (180.0, 48.0);
        let tight = rings_for_body(Some(radius), chunk);
        let (surface, before) = lod0_chunks(radius, chunk, LOD_RINGS.map(|r| r * RING_STEPS));
        let (_, after) = lod0_chunks(radius, chunk, tight);

        assert!(
            before * 2 > surface,
            "more than HALF the body's {surface} surface chunks used to be queued at full \
             detail from one standing position ({before})"
        );
        // What is left at full detail is the ground within 64 m of the player,
        // which has to be: about a tenth of a world this small.
        assert!(
            after * 4 < before && after * 5 < surface,
            "standing on a {radius}-unit world: {before} full-detail chunks became {after} of {surface}"
        );
        println!(
            "radius {radius}: {surface} surface chunks, full detail {before} -> {after} \
             (rings {:?} -> {tight:?})",
            LOD_RINGS
        );
        // …and the ring still covers the ground you are standing on.
        assert!(tight[0] >= 1 && tight[1] > tight[0] && tight[2] > tight[1], "{tight:?}");
    }

    /// A world big enough for the absolute rings to mean what they say is
    /// untouched — this can only ever tighten.
    #[test]
    fn a_big_world_keeps_the_rings_it_had() {
        let quarters = LOD_RINGS.map(|r| r * RING_STEPS);
        assert_eq!(rings_for_body(Some(100_000.0), 48.0), quarters);
        assert_eq!(rings_for_body(None, 48.0), quarters, "ordinary terrain has no body");
        // A tiny moon still gets a full-detail ring under your feet and two
        // coarser ones around it, rather than collapsing to nothing; the first
        // reaches 64 m and the second 120 m, as everywhere.
        let tiny = rings_for_body(Some(20.0), 48.0);
        assert_eq!(tiny, [6, 10, 14], "ordered, and never empty: {tiny:?}");
    }

    /// **What a player can touch is drawn at full detail.** On a body of any
    /// size, ground within 64 m of the camera is stride 1 and nothing within
    /// 120 m is coarser than stride 2, whatever stride a chunk was drawn at
    /// before. The rings were a fraction of the body's radius, which on a
    /// 300 m world rounded to one chunk, and a coarse chunk refined only once
    /// the camera was inside it: mountains that were not there until you walked
    /// into them, and pits that were never drawn.
    #[test]
    fn ground_near_the_camera_is_drawn_at_full_detail() {
        for radius in [40.0, 120.0, 300.0, 1500.0] {
            for chunk in [24.0, 48.0, 96.0] {
                let rings = rings_for_body(Some(radius), chunk);
                for m in 0..200 {
                    // The camera-to-chunk distance in the rings' quarter chunks.
                    let dist = (m as f64 / chunk * RING_STEPS as f64).ceil() as i32;
                    for cur in 0..=3u8 {
                        let lod = lod_for(dist, cur, rings);
                        if m as f64 <= super::NEAR_METRES {
                            assert_eq!(lod, 0, "r {radius}, chunk {chunk}: ground {m} m away drawn at lod {lod} (was {cur})");
                        } else if m as f64 <= super::MID_METRES {
                            assert!(lod <= 1, "r {radius}, chunk {chunk}: ground {m} m away drawn at lod {lod} (was {cur})");
                        }
                    }
                }
            }
        }
    }

    /// A dense field with the given world size and voxel dims — only the fields
    /// `terrain_voxel_size` reads need to be real.
    fn baked(size: [f32; 3], dims: [u32; 3]) -> BakedSdf {
        BakedSdf {
            dims,
            center: [0.0; 3],
            half_extent: [size[0] * 0.5, size[1] * 0.5, size[2] * 0.5],
            distance: vec![0.0; 1],
            color: vec![[0; 4]; 1],
        }
    }

    /// The shipped bug: `terrain_voxel_size` took the MIN axis resolution, so a stretched
    /// field (the 18:1 Y-stretch) meshed the wide footprint at its thin-axis voxel —
    /// millions of surface cells that flooded the terrain color store (2^24). The chosen
    /// voxel must keep the surface-cell count bounded for any slab shape.
    #[test]
    fn voxel_size_bounds_the_vertex_count() {
        let cases = [
            // (world size, dense dims) — the real cases + extremes.
            ([578.0, 97.5, 578.0], [289, 271, 307]), // Ty's stretched field (was 2.6 M cells)
            ([578.0, 12.0, 578.0], [64, 24, 64]),    // the 18:1 slab that shipped
            ([16.0, 6.0, 16.0], [64, 24, 64]),       // a small terrain
            ([4000.0, 100.0, 4000.0], [256, 64, 256]), // a huge map
            ([2.0, 200.0, 2.0], [24, 384, 24]),      // a tall column
        ];
        for (size, dims) in cases {
            let v = terrain_voxel_size(&baked(size, dims));
            // Surface cells ≈ (two largest extents) / voxel². This is what becomes the
            // vertex count; it must stay well under 2^24 (~16.7 M) — the store's ceiling.
            let mut ext = size;
            ext.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let cells = ext[1] * ext[2] / (v * v);
            assert!(
                cells < 4_000_000.0,
                "{size:?} @ voxel {v:.3} => {cells:.0} surface cells — near/over the color \
                 store ceiling (the min-axis bug shipped ~2.6 M here and overflowed)"
            );
            assert!(v.is_finite() && v >= 0.25, "voxel {v} out of range for {size:?}");
        }
    }
}

#[cfg(test)]
mod residency_tests {
    use super::*;

    /// **A bite of a mining laser costs the main thread a few milliseconds at
    /// most, even in a debug build.** Each bite on a generated planet cost the
    /// frame it landed in 9 ms, then 33 to 61 ms on bigger worlds, in a release
    /// build: the dug chunks re-meshed for collision on the main thread, the
    /// sim's copy of the field took deep copies of every chunk the dig reached,
    /// and the collider forgot every cached triangle. Thirty bites down a
    /// trench through the path a script's `terrain.dig` takes, timed without
    /// the collision meshes landing (they are built on worker threads).
    #[test]
    fn a_bite_costs_the_main_thread_a_few_milliseconds() {
        let fill = floptle_field::procgen::PlanetFill { seed: 6, radius: 200.0, voxel: 1.5, relief: 12.0, cave_depth: 60.0, core_r: 10.0, defer_caves: true, ..Default::default() };
        let mut ed = crate::Editor::default();
        let field = floptle_field::procgen::generate_planet(&fill);
        let ground = field.raycast(Vec3::new(0.0, 260.0, 0.0), Vec3::NEG_Y, 260.0).unwrap();
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.terrains.insert(e, EditorTerrain::new(field));
        ed.toggle_play();
        {
            // Something stands here: the collision chunks the bites reach are meshed.
            use floptle_physics::CollisionShape;
            let _ = ed.sim.as_mut().unwrap().terrain_collider_mut(e.index()).unwrap().distance(ground);
        }
        let mut each = Vec::new();
        for i in 0..30 {
            let p = ground - Vec3::new(0.0, i as f32 * 0.3, i as f32 * 0.35);
            let op = floptle_script::TerrainOp {
                pos: [p.x as f64, p.y as f64, p.z as f64],
                radius: 2.4,
                strength: 0.85,
                mode: floptle_script::TerrainOpMode::Lower,
                id: 0,
                normal: None,
            };
            let t = std::time::Instant::now();
            let mut batch: HashMap<Entity, TerrainBatch> = HashMap::new();
            ed.apply_terrain_op(&op, &mut batch);
            for (e, mut b) in batch {
                b.touched.sort_unstable();
                b.touched.dedup();
                ed.mirror_terrain_chunks_to_sim(e, &b.touched);
                let mut touched = Some(b.touched);
                for (lo, hi) in b.boxes {
                    let c = (lo + hi) * 0.5;
                    let r = ((hi - lo) * 0.5).max_element();
                    ed.queue_terrain_dirty(e, c, r, b.geom, touched.take().unwrap_or_default());
                }
            }
            each.push(t.elapsed().as_secs_f64() * 1e3);
            for _ in 0..COLLISION_MESH_FRAMES {
                ed.pump_collision_meshes();
            }
        }
        each.sort_by(f64::total_cmp);
        let p90 = each[each.len() * 9 / 10];
        // Against meshing one chunk at full detail, timed here too, so a busy
        // machine slows both: a bite costs about two of them, and with the
        // collision meshes built on the main thread it cost thirty.
        let field = &ed.terrains[&e].field;
        let c = (ground / (field.voxel() * floptle_field::CHUNK as f32)).floor().to_array().map(|v| v as i32);
        let mut mesh: Vec<f64> = (0..5)
            .map(|_| {
                let t = std::time::Instant::now();
                let _ = floptle_field::mesh_chunk(field, c, 1, false);
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        mesh.sort_by(f64::total_cmp);
        assert!(p90 < 5.0 * mesh[2], "a bite costs the main thread {p90:.2} ms (90th percentile), a chunk's mesh {:.2} ms", mesh[2]);
    }

    /// Sync terrain `e`'s meshes from `cam` until every chunk the field holds
    /// is drawn or known empty and nothing is in flight.
    fn settle(ed: &mut crate::Editor, e: Entity, cam: DVec3) {
        let all = ed.terrains[&e].field.chunk_coords().len();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            ed.sync_terrain_meshes(false, cam);
            let r = &ed.terrain_render[&e];
            if r.pending.is_empty() && r.slots.len() + r.empty.len() >= all {
                break;
            }
            assert!(std::time::Instant::now() < until, "the world never finished meshing");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// **A dug chunk of a generated world is drawn at full detail however far
    /// away it is.** The coarse levels cannot draw a pit narrower than their
    /// cells: re-meshed at its ring's stride, a 3.5 m laser trench drew 3 to
    /// 8 m above its floor, as a mound or as nothing, and every pit was
    /// invisible from 70 m. Ground no one has dug keeps its rings.
    #[test]
    fn a_dug_chunk_of_a_generated_world_is_drawn_at_full_detail_at_any_distance() {
        use floptle_field::{Brush, BrushProfile};
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let fill = floptle_field::procgen::PlanetFill { seed: 3, radius: 120.0, voxel: 1.5, relief: 6.0, cave_depth: 0.0, ..Default::default() };
        let mut field = floptle_field::procgen::generate_planet(&fill);
        let ground = field.raycast(Vec3::new(0.0, 200.0, 0.0), Vec3::NEG_Y, 200.0).unwrap();
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        // The camera on the far side of the world, so the dug ground is in the
        // coarsest ring.
        let cam = DVec3::new(0.0, -400.0, 0.0);
        let touched = field.sculpt(Brush::Lower, ground, 2.4, 1.0, BrushProfile::default());
        let c = *touched.iter().max_by_key(|c| c[1]).unwrap();
        let undug = [c[0] + 2, c[1], c[2]];
        ed.terrains.insert(e, EditorTerrain::new(field));
        ed.sync_terrain_meshes(true, cam);
        settle(&mut ed, e, cam);
        let lod_of = |ed: &crate::Editor, c: [i32; 3]| {
            let r = &ed.terrain_render[&e];
            r.pending.get(&c).map(|p| p.0).or_else(|| r.slots.get(&c).map(|s| s.1))
        };
        assert_eq!(lod_of(&ed, c), Some(0), "the dug chunk is drawn at stride {}", lod_of(&ed, c).map_or(0, |l| 1 << l));
        assert!(lod_of(&ed, undug).is_none_or(|l| l > 0), "undug ground beside it lost its ring");
    }

    /// **A world seen from orbit is drawn a few dozen draws, not a draw a
    /// chunk.** The coarsest chunks are merged [`FAR_GROUP`]³ to a mesh: a
    /// 1 km planet from orbit was 6,100 draws and most of a 24 ms frame.
    #[test]
    fn a_world_seen_from_orbit_draws_its_far_chunks_in_groups() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let fill = floptle_field::procgen::PlanetFill { seed: 6, radius: 260.0, voxel: 2.0, relief: 12.0, cave_depth: 0.0, ..Default::default() };
        let field = floptle_field::procgen::generate_planet(&fill);
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.terrains.insert(e, EditorTerrain::new(field));
        // Past the outer ring, so every chunk is at the coarsest level.
        let cam = DVec3::new(0.0, 0.0, 2600.0);
        settle(&mut ed, e, cam);
        let render = &ed.terrain_render[&e];
        let far = render.slots.values().filter(|s| s.1 == FAR_LOD).count();
        assert!(far > 100, "only {far} chunks drawn at the coarsest level");
        let view_proj = Mat4::perspective_rh(1.0, 1.0, 0.5, 10000.0) * Mat4::look_at_rh(Vec3::new(0.0, 0.0, 2600.0), Vec3::ZERO, Vec3::Y);
        let mut instances = Vec::new();
        push_terrain_instances(
            &ed.terrain_render,
            &ed.terrains,
            &ed.world,
            ed.raster.as_ref().unwrap(),
            &MaterialParams::flat([0.5, 0.5, 0.5]),
            cam,
            view_proj,
            ed.mesh_ids[floptle_core::Shape::Sphere as usize],
            None,
            &mut instances,
        );
        assert!(!instances.is_empty(), "nothing drawn");
        assert!(instances.len() * 6 < far, "{} draws for {far} far chunks", instances.len());
        // A game that draws its own far worlds hides this one: nothing of it
        // is drawn. (`node.visible = false` drew every chunk regardless.)
        ed.world.insert(e, floptle_core::Visible(false));
        let mut hidden = Vec::new();
        push_terrain_instances(
            &ed.terrain_render,
            &ed.terrains,
            &ed.world,
            ed.raster.as_ref().unwrap(),
            &MaterialParams::flat([0.5, 0.5, 0.5]),
            cam,
            view_proj,
            ed.mesh_ids[floptle_core::Shape::Sphere as usize],
            None,
            &mut hidden,
        );
        assert!(hidden.is_empty(), "a hidden world drew {} times", hidden.len());
    }

    /// **In Play a dig is meshed off the main thread.** A mining laser biting
    /// every eighth of a second spent 3+ ms of each bite's frame meshing the
    /// chunks it reached. Now the chunk goes to the worker and keeps drawing
    /// its old mesh until the new one lands; outside Play the editor's brush
    /// still meshes on the spot, because sculpting has to feel instant.
    #[test]
    fn in_play_a_dig_remeshes_on_the_worker_and_the_old_mesh_stays_until_then() {
        use floptle_field::{Brush, BrushProfile};
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let mut field = floptle_field::ChunkField::new(0.5);
        for _ in 0..20 {
            field.sculpt(Brush::Raise, Vec3::new(2.0, 0.0, 2.0), 5.0, 1.0, BrushProfile::default());
        }
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.terrains.insert(e, EditorTerrain::new(field));
        let cam = DVec3::new(2.0, 6.0, 2.0);
        ed.sync_terrain_meshes(true, cam);
        let dig = |ed: &mut crate::Editor| {
            let t = ed.terrains.get_mut(&e).unwrap();
            let touched = t.field.sculpt(Brush::Lower, Vec3::new(2.0, 2.0, 2.0), 1.5, 1.0, BrushProfile::default());
            ed.terrain_chunks_dirty.entry(e).or_default().extend(touched.iter().copied());
            touched
        };
        let mesh_of = |ed: &crate::Editor, c: [i32; 3]| ed.terrain_render.get(&e).and_then(|r| r.slots.get(&c)).map(|s| s.0);

        // Outside Play: meshed now, nothing left for the worker.
        let touched = dig(&mut ed);
        let c = *touched.iter().find(|c| mesh_of(&ed, **c).is_some()).expect("the dig reached a drawn chunk");
        ed.sync_terrain_meshes(false, cam);
        assert!(!ed.terrain_render[&e].pending.contains_key(&c), "outside Play the brush must mesh on the spot");

        // In Play: queued for the worker, the old mesh still drawn.
        ed.playing = true;
        let old = mesh_of(&ed, c);
        dig(&mut ed);
        ed.sync_terrain_meshes(false, cam);
        assert!(ed.terrain_render[&e].pending.contains_key(&c), "in Play a dig must go to the worker");
        assert_eq!(mesh_of(&ed, c), old, "the old mesh must stay drawn until the new one lands");
        // …and it does land.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ed.terrain_render[&e].pending.contains_key(&c) && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(5));
            ed.sync_terrain_meshes(false, cam);
        }
        assert!(!ed.terrain_render[&e].pending.contains_key(&c), "the worker's mesh never landed");
        assert!(mesh_of(&ed, c).is_some(), "the chunk is drawn after the remesh");
    }

    /// **In Play a dig's collision meshes are built off the main thread and land
    /// on a fixed frame.** The collider keeps its old triangles until then, as
    /// the drawn chunk keeps its old mesh, and from that frame on it stands on
    /// the dug ground. A mining laser re-meshed every collision chunk near each
    /// bite on the main thread, most of its frame.
    #[test]
    fn in_play_a_digs_collision_lands_from_the_worker_on_its_frame() {
        use floptle_field::{Brush, BrushProfile};
        use floptle_physics::CollisionShape;
        let mut ed = crate::Editor::default();
        let mut field = floptle_field::ChunkField::new(0.5);
        field.fill_slab(Vec3::new(-12.0, -6.0, -12.0), Vec3::new(12.0, 0.0, 12.0), 0.0, [0.5, 0.5, 0.5]);
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.terrains.insert(e, EditorTerrain::new(field));
        ed.toggle_play();
        assert!(ed.playing && ed.sim.is_some(), "did not enter play with a sim");
        let probe = Vec3::new(1.0, 0.6, 1.0);
        let collider_distance = |ed: &mut crate::Editor| {
            ed.sim.as_mut().unwrap().terrain_collider_mut(e.index()).expect("the terrain's collider").distance(probe)
        };
        let before = collider_distance(&mut ed);
        assert!((before - 0.6).abs() < 0.2, "the slab's top is not where it was laid: {before}");

        // The dig, through the path a script's `terrain.dig` takes.
        let touched = {
            let t = ed.terrains.get_mut(&e).unwrap();
            t.field.sculpt(Brush::Lower, Vec3::new(1.0, 0.0, 1.0), 2.0, 1.0, BrushProfile { hardness: 1.0, ..Default::default() })
        };
        ed.mirror_terrain_chunks_to_sim(e, &touched);
        assert_eq!(collider_distance(&mut ed), before, "the old triangles must stand until the new ones land");
        for _ in 0..COLLISION_MESH_FRAMES {
            ed.pump_collision_meshes();
        }
        assert!(ed.collision_pending.is_empty(), "the collision meshes never landed");
        let after = collider_distance(&mut ed);
        assert!(after > before + 0.5, "the collider does not see the hole: {before} -> {after}");
    }

    /// **A mining laser's bites cost the frames they land in little.** Each
    /// bite on a generated planet cost its frame 9 ms, then 33 to 61 ms on the
    /// bigger worlds, in a release build. Whole player frames, as a build
    /// runs them (headless, so the draw itself is skipped: the bites' cost is
    /// in the step and the terrain work around it): 60 without digging, then
    /// 60 with a bite every seventh, as a laser biting every 0.12 s does at
    /// 60 fps. The slowest bite frames stand against the slowest of the
    /// others, so a slow machine slows both; a release build holds the card's
    /// 4 ms, a debug one cannot, so the bound is the debug one.
    #[test]
    fn a_mining_lasers_bites_cost_their_frames_little() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let fill = floptle_field::procgen::PlanetFill { seed: 6, radius: 200.0, voxel: 1.5, relief: 12.0, cave_depth: 60.0, core_r: 10.0, defer_caves: true, ..Default::default() };
        let field = floptle_field::procgen::generate_planet(&fill);
        let ground = field.raycast(Vec3::new(0.0, 260.0, 0.0), Vec3::NEG_Y, 260.0).unwrap();
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.world.insert(e, floptle_core::Matter::Terrain { id: 1, collision: Default::default() });
        ed.terrains.insert(e, EditorTerrain::new(field));
        let cam = ed.world.spawn();
        ed.world.insert(cam, floptle_core::Transform::from_translation(DVec3::new(0.0, ground.y as f64 + 6.0, 12.0)));
        ed.world.insert(cam, floptle_core::Matter::Camera { fov_y: 1.0, active: true, target: String::new(), cull_mask: !0, target_w: 0, target_h: 0, target_hz: 0.0, ortho: false, ortho_height: 10.0, stack: Vec::new() });
        ed.toggle_play();
        {
            // Something stands where the laser bites, so its collision is meshed.
            use floptle_physics::CollisionShape;
            let _ = ed.sim.as_mut().unwrap().terrain_collider_mut(e.index()).unwrap().distance(ground);
        }
        // Settle: the ground around the camera meshed.
        for _ in 0..60 {
            ed.player_frame(false);
        }
        let frame = |ed: &mut crate::Editor, bite: Option<Vec3>| {
            let t = std::time::Instant::now();
            if let Some(p) = bite {
                ed.apply_script_terrain_ops(vec![floptle_script::TerrainOp {
                    pos: [p.x as f64, p.y as f64, p.z as f64],
                    radius: 2.4,
                    strength: 0.85,
                    mode: floptle_script::TerrainOpMode::Lower,
                    id: 0,
                    normal: None,
                }]);
            }
            ed.player_frame(false);
            t.elapsed().as_secs_f64() * 1e3
        };
        let mut quiet: Vec<f64> = (0..60).map(|_| frame(&mut ed, None)).collect();
        let mut bites = Vec::new();
        for i in 0..60 {
            let k = i / 7;
            let ms = frame(&mut ed, (i % 7 == 0).then(|| ground - Vec3::new(0.0, k as f32 * 0.3, k as f32 * 0.35)));
            if i % 7 == 0 {
                bites.push(ms);
            }
        }
        let p95 = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[(v.len() - 1) * 95 / 100]
        };
        let (q, b) = (p95(&mut quiet), p95(&mut bites));
        // A debug build: a bite's frame is a few milliseconds of release work,
        // 7 to 25 ms here depending on what else is running, and the
        // main-thread collision meshing this guards against was 90.
        assert!(b - q < 35.0, "the slowest bite frames took {b:.1} ms against {q:.1} ms for frames without one");
    }

    /// **A dig made while a world is still on its way lands in that world.**
    /// A game resuming a save dug into ground whose field was still being made;
    /// the field arrived, replaced the one the dig had gone into, and the rock
    /// came back. An edit now waits for its terrain to land, and the edits
    /// after it wait their turn.
    #[test]
    fn a_dig_made_while_a_world_is_on_its_way_lands_in_it() {
        let fill = floptle_field::procgen::PlanetFill { seed: 14, radius: 80.0, voxel: 1.5, relief: 4.0, cave_depth: 0.0, ..Default::default() };
        let mut ed = crate::Editor::default();
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.world.insert(e, floptle_core::Matter::Terrain { id: 1, collision: Default::default() });
        // What stands there until the world arrives: the same planet, older.
        ed.terrains.insert(e, EditorTerrain::new(floptle_field::procgen::generate_planet(&fill)));
        let ground = ed.terrains[&e].field.raycast(Vec3::new(0.0, 150.0, 0.0), Vec3::NEG_Y, 150.0).unwrap();
        ed.start_terrain_generates(vec![(1, fill)]);
        let dig = floptle_script::TerrainOp {
            pos: [ground.x as f64, ground.y as f64, ground.z as f64],
            radius: 2.0,
            strength: 1.0,
            mode: floptle_script::TerrainOpMode::Lower,
            id: 0,
            normal: None,
        };
        ed.apply_script_terrain_ops(vec![dig]);
        assert_eq!(ed.terrain_op_queue.len(), 1, "the dig went into the world about to be replaced");
        let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !ed.terrain_op_queue.is_empty() || ed.planet_gen_job.is_some() {
            assert!(std::time::Instant::now() < until, "the dig never landed");
            std::thread::sleep(std::time::Duration::from_millis(5));
            ed.pump_world_streaming();
            ed.apply_script_terrain_ops(Vec::new());
        }
        let below = ground - Vec3::Y * 1.0;
        assert!(ed.terrains[&e].field.d(below) > 0.0, "the hole is gone: the rock came back with the world");
    }

    /// **A fast body far from the camera lands on the ground.** The ground a
    /// body lands on is held whole and drawn at full detail under it wherever
    /// the camera is: twenty spheres dropped at 60 m/s on the far side of a
    /// world from the camera all come to rest on the ground, within 0.2 m of
    /// it, with the ground under each held whole in the field and in the
    /// sim's copy. Followed by the camera alone, the ground under them was put
    /// away to a coarse stand-in, and they came down on that.
    #[test]
    fn a_fast_body_far_from_the_camera_lands_on_the_ground() {
        let fill = floptle_field::procgen::PlanetFill { seed: 13, radius: 200.0, voxel: 1.5, relief: 8.0, cave_depth: 0.0, ..Default::default() };
        let mut field = floptle_field::procgen::generate_planet(&fill);
        field.set_spec("land".into());
        let exact = field.clone();
        let mut ed = crate::Editor::default();
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.world.insert(e, floptle_core::CelestialBody { body_radius: 200.0, mu: 8.0e5, ..Default::default() });
        ed.world.insert(e, floptle_core::Matter::Terrain { id: 1, collision: Default::default() });
        let dir = Vec3::new(1.0, 0.2, 0.1).normalize();
        let ground = field.raycast(dir * 300.0, -dir, 300.0).unwrap();
        ed.terrains.insert(e, EditorTerrain::new(field));
        let (u, v) = (dir.any_orthonormal_vector(), dir.cross(dir.any_orthonormal_vector()));
        let mut balls = Vec::new();
        for i in 0..20 {
            let p = ground + dir * 30.0 + u * ((i % 5) as f32 * 4.0 - 8.0) + v * ((i / 5) as f32 * 4.0 - 6.0);
            let b = ed.world.spawn();
            ed.world.insert(b, floptle_core::Transform::from_translation(p.as_dvec3()));
            ed.world.insert(b, floptle_core::RigidBody { kind: floptle_core::BodyKind::Sphere, radius: 0.5, ..Default::default() });
            balls.push(b);
        }
        // The camera on the far side of the world.
        ed.camera.position = (-dir * 600.0).as_dvec3();
        ed.toggle_play();
        assert!(ed.playing && ed.sim.is_some());
        for b in &balls {
            ed.sim.as_mut().unwrap().set_body_velocity(b.index(), -dir * 60.0);
        }
        for _ in 0..360 {
            let at = ed.lod_center(ed.camera.position);
            ed.stream_far_chunks(at);
            ed.sim.as_mut().unwrap().step_tick(1.0 / 60.0, None);
        }
        let f = &ed.terrains[&e].field;
        assert!(f.evicted_count() > 0, "nothing was put away: the camera is not far enough");
        for b in &balls {
            let p = ed.sim.as_ref().unwrap().body_snapshot(b.index()).unwrap().pos.as_vec3();
            // The ground under it is held whole, in the field and the sim's copy.
            let c = (p / (floptle_field::CHUNK as f32 * f.voxel())).floor().to_array().map(|v| v as i32);
            assert!(!f.is_evicted(c), "the ground under a ball was put away");
            let col = ed.sim.as_mut().unwrap().terrain_collider_mut(e.index()).unwrap();
            assert!(!col.field.is_evicted(c), "the sim's ground under a ball was put away");
            // And it rests on it: on the whole ground, none of it below.
            let off = exact.d(p) - 0.5;
            assert!(off.abs() < 0.2, "a ball came to rest {off:.2} m off the ground");
        }
    }

    /// **Walking round a planet keeps the ground it holds flat.** A generated
    /// planet held every chunk of its surface at full detail, gigabytes at a
    /// kilometre: the ground past the near rings is put away behind the
    /// player and made again ahead, so what is held follows the player and
    /// does not grow. Half way round a 200 m world and back, a chunk made
    /// again is the chunk it was.
    #[test]
    fn walking_round_a_planet_keeps_the_ground_it_holds_flat() {
        let fill = floptle_field::procgen::PlanetFill { seed: 12, radius: 200.0, voxel: 1.5, relief: 8.0, cave_depth: 0.0, ..Default::default() };
        let mut field = floptle_field::procgen::generate_planet(&fill);
        field.set_spec("walk".into());
        let whole = field.clone();
        let full = field.memory_bytes();
        let mut ed = crate::Editor::default();
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.world.insert(e, floptle_core::CelestialBody { body_radius: 200.0, ..Default::default() });
        ed.terrains.insert(e, EditorTerrain::new(field));
        let settle = |ed: &mut crate::Editor, cam: DVec3| {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                for _ in 0..FAR_SCAN_FRAMES {
                    ed.stream_far_chunks(cam);
                }
                if ed.terrain_promotions.is_empty() {
                    // Once more: a scan with nothing in flight starts nothing,
                    // and puts nothing more away (a scan puts away a few).
                    let away = ed.terrains[&e].field.evicted_count();
                    for _ in 0..FAR_SCAN_FRAMES {
                        ed.stream_far_chunks(cam);
                    }
                    if ed.terrain_promotions.is_empty() && ed.terrains[&e].field.evicted_count() == away {
                        break;
                    }
                }
                assert!(std::time::Instant::now() < until, "the ground never settled");
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        let mut held = Vec::new();
        for step in 0..=20 {
            let a = std::f64::consts::PI * step as f64 / 20.0;
            let cam = DVec3::new(a.sin(), a.cos(), 0.0) * 205.0;
            settle(&mut ed, cam);
            let f = &ed.terrains[&e].field;
            assert!(f.evicted_count() > 0, "nothing was put away at step {step}");
            held.push(f.memory_bytes());
        }
        let (lo, hi) = (*held.iter().min().unwrap(), *held.iter().max().unwrap());
        assert!(hi * 10 < full * 7, "the walk held {hi} bytes of the planet's {full}");
        assert!(hi * 2 < lo * 3, "what is held grew from {lo} to {hi} bytes on the walk");
        // Back where it started, the ground made again is the ground.
        settle(&mut ed, DVec3::new(0.0, 205.0, 0.0));
        let f = &ed.terrains[&e].field;
        let near: Vec<[i32; 3]> = f.chunk_coords().into_iter().filter(|c| !f.is_evicted(*c) && c[1] > 2).collect();
        assert!(!near.is_empty());
        let (mut da, mut ca, mut db, mut cb) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for c in near {
            let lo = [c[0] * floptle_field::CHUNK, c[1] * floptle_field::CHUNK, c[2] * floptle_field::CHUNK];
            whole.gather(lo, floptle_field::CHUNK as usize, &mut da, &mut ca);
            f.gather(lo, floptle_field::CHUNK as usize, &mut db, &mut cb);
            assert!(da.iter().zip(&db).all(|(x, y)| x.to_bits() == y.to_bits()), "chunk {c:?} came back different");
        }
    }

    /// **A thousand flattens land over frames, in order, and leave the ground
    /// they would all at once.** A game grading a base site made 1,119
    /// flattens in one frame, and applying them all took that frame seconds.
    /// They land a few milliseconds' worth a frame now; the slowest frame
    /// costs a few flattens, and the field they leave is the one applying them
    /// in a single batch leaves, to the bit, though the planet moves on
    /// between frames as one on its rails does (carried over with their world
    /// points, the last of them landed 5 to 6 m off).
    #[test]
    fn a_thousand_flattens_land_over_frames_and_leave_the_same_ground() {
        let fill = floptle_field::procgen::PlanetFill { seed: 8, radius: 120.0, voxel: 1.5, relief: 6.0, cave_depth: 0.0, ..Default::default() };
        let field = floptle_field::procgen::generate_planet(&fill);
        let ground = field.raycast(Vec3::new(0.0, 200.0, 0.0), Vec3::NEG_Y, 200.0).unwrap();
        let ops: Vec<floptle_script::TerrainOp> = (0..1000)
            .map(|i| {
                let (a, r) = (i as f32 * 2.399, 2.0 + (i % 40) as f32 * 0.6);
                let p = ground + Vec3::new(a.cos() * r, 0.0, a.sin() * r);
                floptle_script::TerrainOp {
                    pos: [p.x as f64, p.y as f64, p.z as f64],
                    radius: 3.0,
                    strength: 0.6,
                    mode: floptle_script::TerrainOpMode::Flatten,
                    id: 0,
                    normal: None,
                }
            })
            .collect();
        let editor = |field: &floptle_field::ChunkField| {
            let mut ed = crate::Editor::default();
            let e = ed.world.spawn();
            ed.world.insert(e, floptle_core::Transform::IDENTITY);
            ed.terrains.insert(e, EditorTerrain::new(field.clone()));
            (ed, e)
        };
        // All at once.
        let (mut once, eo) = editor(&field);
        let mut batch: HashMap<Entity, TerrainBatch> = HashMap::new();
        for op in &ops {
            once.apply_terrain_op(op, &mut batch);
        }
        // Over frames, as the engine applies them.
        let (mut ed, e) = editor(&field);
        // Each frame stands beside one flatten timed on its own straight after
        // it, on a second copy of the world, so the two share whatever else
        // the machine is doing.
        let (mut solo, _) = editor(&field);
        let solo_ops = ops.clone();
        let (mut frames, mut ratios) = (0usize, Vec::new());
        let mut first = Some(ops);
        loop {
            let t = std::time::Instant::now();
            ed.pump_collision_meshes(); // a new frame, a new budget
            ed.apply_script_terrain_ops(first.take().unwrap_or_default());
            let frame = t.elapsed().as_secs_f64();
            let t = std::time::Instant::now();
            solo.pump_collision_meshes();
            solo.apply_script_terrain_ops(vec![solo_ops[(frames * 37) % solo_ops.len()].clone()]);
            ratios.push(frame / t.elapsed().as_secs_f64());
            // The planet moves on between frames, as one on its rails does:
            // what is carried over still lands where it was aimed.
            let tr = ed.world.get_mut::<floptle_core::Transform>(e).unwrap();
            tr.translation += DVec3::new(3.0, -1.0, 2.0);
            frames += 1;
            if ed.terrain_op_queue.is_empty() {
                break;
            }
            assert!(frames < 10_000, "the edits never all landed");
        }
        assert!(frames > 2, "a thousand flattens landed in {frames} frame(s): there is no budget");
        // A frame is a few flattens and the refresh after them: two to five of
        // one here, in a debug build (9 to 50 ms a frame, depending on what
        // else is running; a few in release). All thousand in one frame were
        // forty.
        ratios.sort_by(f64::total_cmp);
        let p90 = ratios[ratios.len() * 9 / 10];
        assert!(p90 < 10.0, "a frame of edits cost {p90:.1} flattens (90th percentile)");
        let (a, b) = (&once.terrains[&eo].field, &ed.terrains[&e].field);
        let mut coords = a.all_chunk_coords();
        coords.extend(b.all_chunk_coords());
        coords.sort_unstable();
        coords.dedup();
        let (mut da, mut ca, mut db, mut cb) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for c in coords {
            let lo = [c[0] * floptle_field::CHUNK, c[1] * floptle_field::CHUNK, c[2] * floptle_field::CHUNK];
            a.gather(lo, floptle_field::CHUNK as usize, &mut da, &mut ca);
            b.gather(lo, floptle_field::CHUNK as usize, &mut db, &mut cb);
            let same = da.iter().zip(&db).all(|(x, y)| x.to_bits() == y.to_bits()) && ca == cb;
            assert!(same, "chunk {c:?} differs from applying the edits all at once");
        }
    }

    /// **A generated world lands without stalling the frame it lands in.**
    /// The shadow proxy was derived from the whole field on the main thread
    /// when a `generatePlanet` fill arrived: on a 600 m world that was one
    /// 25 s frame near the end of generation. Every frame from asking to
    /// landed, through the path the frame loop takes, stays short.
    #[test]
    fn a_generated_world_lands_without_stalling_a_frame() {
        let mut ed = crate::Editor::default();
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.world.insert(e, floptle_core::Matter::Terrain { id: 4, collision: Default::default() });
        let fill = floptle_field::procgen::PlanetFill {
            seed: 16,
            radius: 220.0,
            voxel: 1.5,
            relief: 10.0,
            cave_depth: 30.0,
            mountains: Some(floptle_field::procgen::Mountains { height: 20.0, ..Default::default() }),
            rivers: Some(floptle_field::procgen::Rivers::default()),
            defer_caves: true,
            ..Default::default()
        };
        ed.start_terrain_generates(vec![(4, fill)]);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(300);
        let mut worst = 0.0f64;
        while !ed.terrains.contains_key(&e) {
            assert!(std::time::Instant::now() < until, "the world never landed");
            let t = std::time::Instant::now();
            ed.pump_world_streaming();
            worst = worst.max(t.elapsed().as_secs_f64() * 1e3);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(worst < 100.0, "a frame of the generation took {worst:.0} ms on the main thread");
    }

    /// **An old whole-field save of a generated world comes back saving only
    /// its edits, through either way it streams in.** The emergency load read
    /// only the file, and a load kicked while a `generatePlanet` batch ran
    /// dropped the genspec with the generation, so the save never learnt what
    /// was dug and was written back whole: 682 MB, every save. Then, found, it
    /// was not marked for saving, and with nothing new dug the slot kept the
    /// 394 MB file beside the edits of the next body. Both paths, from a
    /// version-1 file of a dug planet.
    #[test]
    fn an_old_whole_field_save_saves_small_after_either_load() {
        use floptle_field::{Brush, BrushProfile};
        let fill = floptle_field::procgen::PlanetFill { seed: 9, radius: 90.0, voxel: 1.5, relief: 5.0, cave_depth: 0.0, ..Default::default() };
        let genspec = ron::to_string(&fill).unwrap();
        let mut dug = floptle_field::procgen::generate_planet(&fill);
        let ground = dug.raycast(Vec3::new(0.0, 150.0, 0.0), Vec3::NEG_Y, 150.0).unwrap();
        for i in 0..6 {
            dug.sculpt(Brush::Lower, ground - Vec3::Y * (i as f32), 2.4, 1.0, BrushProfile::default());
        }
        // Written as the engine wrote it before edits were tracked: whole.
        let old = dug.to_bytes();
        assert_eq!(&old[4..8], &1u32.to_le_bytes());
        for kicked in [false, true] {
            let dir = std::env::temp_dir().join(format!("floptle-oldsave-{}-{kicked}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let mut ed = crate::Editor { project_root: dir.clone(), scene_name: "s".into(), ..Default::default() };
            let e = ed.world.spawn();
            ed.world.insert(e, floptle_core::Transform::IDENTITY);
            ed.world.insert(e, floptle_core::Matter::Terrain { id: 1, collision: Default::default() });
            ed.world.insert(e, floptle_core::CelestialBody { body_radius: 90.0, ..Default::default() });
            ed.world.insert(e, floptle_core::TerrainGen(genspec.clone()));
            let path = ed.terrain_field_path_id(1);
            floptle_vfs::create_dir_all(path.parent().unwrap()).unwrap();
            floptle_vfs::write(&path, &old).unwrap();
            ed.write_terrain_meta(1, [0.5, 0.5, 0.5], Some(genspec_hash(&genspec)));
            if kicked {
                // A game's generation batch is running: only files may load.
                ed.planet_gen_pending.insert(99);
                ed.kick_terrain_load(e, 1);
            } else {
                assert!(!ed.load_terrain_blocking(e, 1), "a body with a recipe loaded in the frame");
            }
            let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
            while !ed.terrains.contains_key(&e) {
                assert!(std::time::Instant::now() < until, "the save never streamed in");
                std::thread::sleep(std::time::Duration::from_millis(5));
                ed.pump_world_streaming();
            }
            // Marked for the next save, which writes it as edits: nothing
            // new was dug, and a flush writes only what is marked.
            assert!(ed.terrain_disk_dirty.contains(&e), "the converted field is not written at the next save ({})", if kicked { "kicked" } else { "blocking" });
            let saved = ed.terrains[&e].field.to_bytes();
            assert_eq!(&saved[4..8], &2u32.to_le_bytes(), "saved as version {} after the {} load", saved[4], if kicked { "kicked" } else { "blocking" });
            assert!(saved.len() * 5 < old.len(), "saved {} bytes of a {}-byte field", saved.len(), old.len());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The full genspec streaming pipeline, headless: a PlanetFill serialized
    /// exactly like `node:setTerrainGen` does (ron::to_string) must round-trip
    /// through `load_terrain_from(Generate(..))` into a real, non-empty terrain
    /// — this is the contract behind "focus a planet on the map and its terrain
    /// streams in". A tiny radius keeps the test fast.
    #[test]
    fn genspec_streams_into_a_real_terrain() {
        let fill = floptle_field::procgen::PlanetFill {
            radius: 20.0,
            voxel: 1.5,
            relief: 2.0,
            cave_depth: 6.0,
            core_r: 3.0,
            seed: 1234,
            ..Default::default()
        };
        let spec = ron::to_string(&fill).expect("genspec serializes");
        let color = genspec_impostor_color(&spec, &[]).expect("impostor color from genspec");
        // The surface paint as it draws: its tint, over a blank (white) layer.
        let [r, g, b] = fill.surface_a.color.map(|c| (c * 255.0).round() as u8);
        assert_eq!(color, drawn_color([r, g, b, fill.surface_a.slot], &[]));
        let t = load_terrain_from(vec![TerrainSource::Generate(spec.clone())], None, None, 0, None)
            .expect("genspec generates a terrain");
        assert!(t.field.data_chunks() > 0, "generated field is empty");
        // The surface is really there: a ray from outside hits near the radius.
        let hit = t
            .field
            .raycast(Vec3::new(0.0, 40.0, 0.0), Vec3::new(0.0, -1.0, 0.0), 80.0)
            .expect("ray from space hits the generated surface");
        assert!(
            (hit.y - fill.radius).abs() < fill.relief + 3.0,
            "surface at {} vs radius {}",
            hit.y,
            fill.radius
        );
        // A garbled genspec fails cleanly (None → the loud-failure path), never panics.
        assert!(load_terrain_from(vec![TerrainSource::Generate("(not ron".into())], None, None, 0, None).is_none());
        assert!(genspec_impostor_color("(not ron", &[]).is_none());
        // A corrupt file falls back to the genspec instead of failing the stream —
        // the exact failure seen in practice (LFS pointer stubs where fields should be).
        let dir = std::env::temp_dir().join(format!("floptle-badfield-{}", std::process::id()));
        floptle_vfs::create_dir_all(&dir).unwrap();
        let bad = dir.join("corrupt.cfield");
        floptle_vfs::write(&bad, b"version https://git-lfs -- not a field").unwrap();
        let t2 = load_terrain_from(vec![
            TerrainSource::File(bad),
            TerrainSource::Generate(spec),
        ], None, None, 0, None)
        .expect("corrupt file falls back to genspec generation");
        assert!(t2.field.data_chunks() > 0);
        let _ = std::fs::remove_dir_all(&dir);
        // Genspec-hash gate: same spec → same hash, different spec → different.
        assert_eq!(genspec_hash("abc"), genspec_hash("abc"));
        assert_ne!(genspec_hash("abc"), genspec_hash("abd"));
    }

    /// **A recipe that names no generator is made by the project's, and keeps
    /// it from then on; one that names one keeps its own.** The text a script's
    /// `setTerrainGen` writes without a version is the text it wrote before
    /// versions (its hash, which trusts a saved field for the body, does not
    /// move); a field made from it carries the project's version, so its save
    /// says which rules made its ground. And a save of only edits is never
    /// older than such saves are, whatever its project says.
    #[test]
    fn a_recipe_without_a_version_is_made_by_the_projects_generator() {
        use floptle_field::procgen::{PlanetFill, DEEP_ROCK_NEEDS_CAVES};
        let read = |f: &floptle_field::ChunkField| ron::from_str::<PlanetFill>(f.spec().unwrap()).unwrap().generator;
        let fill = PlanetFill { seed: 4, radius: 30.0, voxel: 2.0, relief: 3.0, cave_depth: 0.0, generator: None, ..Default::default() };
        let text = ron::to_string(&fill).unwrap();
        assert!(!text.contains("generator"), "an unversioned recipe's text changed: {text}");

        let (made, kept) = resolve_spec(&text, DEEP_ROCK_NEEDS_CAVES).unwrap();
        assert_eq!(made.generator, Some(DEEP_ROCK_NEEDS_CAVES));
        assert_eq!(resolve_spec(&kept, 0).unwrap().1, kept, "a named version was not kept");

        let t = load_terrain_from(vec![TerrainSource::Generate(text.clone())], None, None, DEEP_ROCK_NEEDS_CAVES, None).unwrap();
        assert_eq!(read(&t.field), Some(DEEP_ROCK_NEEDS_CAVES), "the field forgot the version that made it");

        // An edits-only save whose recipe names none, in a project that names
        // none either.
        let mut g = floptle_field::procgen::generate_planet(&PlanetFill { generator: Some(DELTA_SAVES_SINCE), ..fill.clone() });
        g.set_spec(text);
        let ground = g.raycast(Vec3::new(0.0, 60.0, 0.0), Vec3::NEG_Y, 60.0).unwrap();
        g.sculpt(floptle_field::Brush::Lower, ground, 2.0, 1.0, floptle_field::BrushProfile::default());
        let f = floptle_field::ChunkField::from_bytes(&g.to_bytes()).unwrap();
        assert!(f.is_delta());
        let back = whole_field(f, 0).unwrap();
        assert_eq!(read(&back), Some(DELTA_SAVES_SINCE), "an edits-only save read as older than such saves are");
    }

    /// **A planet's impostor is the colour its ground draws, palette texture
    /// and all.** The ball was drawn in the paint's tint alone, so a world of
    /// a pale tint over a brown texture showed as a pale ball over brown
    /// ground. Now each palette layer's mean colour multiplies the tint, as
    /// the terrain shader does; a genspec body's colour without a field does
    /// the same, and a change of palette takes every resident body's again.
    #[test]
    fn a_planets_impostor_is_the_colour_its_ground_draws() {
        use floptle_field::procgen::{LayerPaint, PlanetFill};
        let near = |a: [f32; 3], b: [f32; 3]| (0..3).all(|k| (a[k] - b[k]).abs() < 0.01);
        // Layer 0 white, layer 1 a brown texture: slot 2 draws layer 1.
        let brown = [0.30, 0.18, 0.08];
        let means = vec![[1.0; 3], brown];
        let tint = 204.0 / 255.0;
        let want = brown.map(|m| tint * m * 1.6);

        let fill = PlanetFill { seed: 2, radius: 40.0, voxel: 2.0, relief: 3.0, cave_depth: 0.0, ..Default::default() };
        let mut field = floptle_field::procgen::generate_planet(&fill);
        field.fill_color([0.8; 3]);
        field.fill_texture(2);
        let got = impostor_surface_color(&field, 40.0, &means);
        assert!(near(got, want), "the ball is {got:?}, its ground draws {want:?}");
        assert!(near(impostor_surface_color(&field, 40.0, &[]), [tint * 1.6; 3]), "a blank layer draws white");

        let spec = PlanetFill { surface_a: LayerPaint { slot: 2, color: [0.8; 3] }, ..fill.clone() };
        let got = genspec_impostor_color(&ron::to_string(&spec).unwrap(), &means).unwrap();
        assert!(near(got, want), "a genspec ball is {got:?}, its ground draws {want:?}");

        // sRGB mid grey is a fifth of full light, linear.
        let grey = floptle_render::TextureData { pixels: [128u8, 128, 128, 255].repeat(16), width: 4, height: 4 };
        assert!(near(layer_mean(&grey), [0.2159; 3]), "{:?}", layer_mean(&grey));

        // A palette change takes a resident body's colour again.
        let mut ed = crate::Editor::default();
        let e = ed.world.spawn();
        ed.terrains.insert(e, EditorTerrain::new(field));
        ed.terrain_render.entry(e).or_default().impostor_color = Some([0.8; 3]);
        ed.set_terrain_slot_means(vec![layer_mean(&grey)]);
        assert_eq!(ed.terrain_render[&e].impostor_color, None, "a palette change left the old colour");
    }

    /// **A game decides when one of its planets becomes a ball.**
    /// `terrain.impostorAt(id, { radii = n })` switches it past `n` of its
    /// radii; `{ pixels = n }` once its disc is smaller than that on screen. A
    /// body with no rule keeps the sixty radii it always had; the switch was a
    /// fixed constant before, and a world 42 to 60 radii away drew all its
    /// chunks however small it was.
    #[test]
    fn a_game_decides_when_its_planet_draws_as_a_ball() {
        let r = 50.0;
        assert_eq!(impostor_enter_radii(None, 900.0), IMPOSTOR_RADII, "the default moved");
        assert_eq!(impostor_enter_radii(Some(floptle_script::ImpostorRule::Radii(5.0)), 900.0), 5.0);
        // Ten pixels at 900 px a radian: a disc of 2r is ten pixels at 180 r.
        let px = impostor_enter_radii(Some(floptle_script::ImpostorRule::Pixels(10.0)), 900.0);
        assert!((px - 180.0).abs() < 1e-9, "{px}");

        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let fill = floptle_field::procgen::PlanetFill { seed: 3, radius: r as f32, voxel: 2.0, relief: 3.0, cave_depth: 0.0, ..Default::default() };
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.world.insert(e, floptle_core::Matter::Terrain { id: 7, collision: Default::default() });
        ed.world.insert(e, floptle_core::CelestialBody { body_radius: r, ..Default::default() });
        ed.terrains.insert(e, EditorTerrain::new(floptle_field::procgen::generate_planet(&fill)));
        let cam = DVec3::new(0.0, 0.0, 10.0 * r);
        ed.sync_terrain_meshes(true, cam);
        assert!(!ed.terrain_render[&e].impostor, "a world ten radii away is a ball with no rule set");

        ed.terrain_impostor_rules.insert(7, floptle_script::ImpostorRule::Radii(5.0));
        ed.sync_terrain_meshes(false, cam);
        assert!(ed.terrain_render[&e].impostor, "a world past the radii its game set is not a ball");
        let view_proj = Mat4::perspective_rh(1.0, 1.0, 0.5, 10000.0) * Mat4::look_at_rh(cam.as_vec3(), Vec3::ZERO, Vec3::Y);
        let mut drawn = Vec::new();
        push_terrain_instances(
            &ed.terrain_render,
            &ed.terrains,
            &ed.world,
            ed.raster.as_ref().unwrap(),
            &MaterialParams::flat([0.5, 0.5, 0.5]),
            cam,
            view_proj,
            ed.mesh_ids[floptle_core::Shape::Sphere as usize],
            None,
            &mut drawn,
        );
        assert_eq!(drawn.len(), 1, "a ball is one draw");

        // By size on screen: the disc at ten radii is 2 / 10 of a radian.
        let disc = 0.2 * ed.view_px_per_radian();
        ed.terrain_impostor_rules.insert(7, floptle_script::ImpostorRule::Pixels(disc * 0.5));
        ed.sync_terrain_meshes(false, cam);
        assert!(!ed.terrain_render[&e].impostor, "a world twice the pixels its game set is still a ball");
        ed.terrain_impostor_rules.insert(7, floptle_script::ImpostorRule::Pixels(disc * 2.0));
        ed.sync_terrain_meshes(false, cam);
        assert!(ed.terrain_render[&e].impostor, "a world half the pixels its game set is not a ball");
    }


    /// **One big edit is made off the frame, and lands whole.** A single
    /// 20 m flatten cost 100 to 400 ms of one frame under load: the budget is
    /// checked between edits and cannot split one. It is made on a worker,
    /// on a copy of the field, and taken into the field in one step, so a
    /// query in the frames between reads the ground as it was before it; the
    /// small edits behind it wait their turn; and the ground left is the
    /// ground making them all at once leaves. Counted, not timed: the frame it
    /// lands in also lands the six small edits queued behind it, so a ratio
    /// to one small flatten read 5.7 on a CI runner with nothing wrong.
    #[test]
    fn one_big_flatten_is_made_off_the_frame_and_lands_whole() {
        let fill = floptle_field::procgen::PlanetFill { seed: 8, radius: 150.0, voxel: 1.5, relief: 6.0, cave_depth: 0.0, ..Default::default() };
        let field = floptle_field::procgen::generate_planet(&fill);
        let ground = field.raycast(Vec3::new(0.0, 250.0, 0.0), Vec3::NEG_Y, 250.0).unwrap();
        let flatten = |p: Vec3, radius: f32| floptle_script::TerrainOp {
            pos: [p.x as f64, p.y as f64, p.z as f64],
            radius,
            strength: 0.6,
            mode: floptle_script::TerrainOpMode::Flatten,
            id: 0,
            normal: None,
        };
        let mut ops = vec![flatten(ground + Vec3::Y * 3.0, 20.0)];
        ops.extend((0..6).map(|i| flatten(ground + Vec3::new(i as f32 * 2.0, 1.0, 0.0), 3.0)));
        let editor = || {
            let mut ed = crate::Editor::default();
            let e = ed.world.spawn();
            ed.world.insert(e, floptle_core::Transform::IDENTITY);
            ed.terrains.insert(e, EditorTerrain::new(field.clone()));
            (ed, e)
        };
        let (mut once, eo) = editor();
        let mut batch: HashMap<Entity, TerrainBatch> = HashMap::new();
        for op in &ops {
            once.apply_terrain_op(op, &mut batch);
        }

        let (mut ed, e) = editor();
        let watch = field.chunks_holding(
            [(ground.x / 1.5) as i32, (ground.y / 1.5) as i32, (ground.z / 1.5) as i32],
            [(ground.x / 1.5) as i32, (ground.y / 1.5) as i32, (ground.z / 1.5) as i32],
        )[0];
        let lo = [watch[0] * floptle_field::CHUNK, watch[1] * floptle_field::CHUNK, watch[2] * floptle_field::CHUNK];
        let (mut before, mut c0, mut now, mut c1) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        field.gather(lo, floptle_field::CHUNK as usize, &mut before, &mut c0);
        let (mut frames, mut waited) = (0usize, 0usize);
        let inline_before = BIG_OPS_ON_FRAME.with(|n| n.get());
        let mut first = Some(ops);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            ed.pump_collision_meshes();
            ed.apply_script_terrain_ops(first.take().unwrap_or_default());
            frames += 1;
            if ed.terrain_op_job.is_some() {
                // In flight: the field reads as it did before the edit.
                waited += 1;
                assert_eq!(ed.terrain_op_queue.len(), 6, "an edit behind the big one went ahead of it");
                ed.terrains[&e].field.gather(lo, floptle_field::CHUNK as usize, &mut now, &mut c1);
                assert!(now.iter().zip(&before).all(|(a, b)| a.to_bits() == b.to_bits()), "a query read the big edit half made");
            }
            if ed.terrain_op_queue.is_empty() && ed.terrain_op_job.is_none() {
                break;
            }
            assert!(std::time::Instant::now() < until, "the edits never landed");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let inline = BIG_OPS_ON_FRAME.with(|n| n.get()) - inline_before;
        assert_eq!(inline, 0, "the big flatten was made on the frame ({frames} frames)");
        assert!(waited > 0, "the big flatten was never in flight");
        let (a, b) = (&once.terrains[&eo].field, &ed.terrains[&e].field);
        let mut coords = a.all_chunk_coords();
        coords.extend(b.all_chunk_coords());
        coords.sort_unstable();
        coords.dedup();
        let (mut da, mut ca, mut db, mut cb) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for c in coords {
            let lo = [c[0] * floptle_field::CHUNK, c[1] * floptle_field::CHUNK, c[2] * floptle_field::CHUNK];
            a.gather(lo, floptle_field::CHUNK as usize, &mut da, &mut ca);
            b.gather(lo, floptle_field::CHUNK as usize, &mut db, &mut cb);
            let same = da.iter().zip(&db).all(|(x, y)| x.to_bits() == y.to_bits()) && ca == cb;
            assert!(same, "chunk {c:?} differs from making the edits all at once");
        }
        // Its proxy cells, sampled off the frame, are the ones refreshing them
        // in place from the field it left would write.
        let t = &ed.terrains[&e];
        let mut fresh = t.shadow.clone();
        let r = 20.0 + t.field.band() + t.field.voxel();
        t.field.refresh_dense_region(&mut fresh, ground + Vec3::Y * 3.0 - Vec3::splat(r), ground + Vec3::Y * 3.0 + Vec3::splat(r));
        assert!(fresh.distance.iter().zip(&t.shadow.distance).all(|(a, b)| a.to_bits() == b.to_bits()) && fresh.color == t.shadow.color, "the proxy over the big edit is stale");
    }


    /// Every chunk of two fields, compared to the bit: what a test of edits
    /// landed over frames checks against the same edits made at once.
    fn assert_same_chunks(a: &floptle_field::ChunkField, b: &floptle_field::ChunkField, what: &str) {
        let mut coords = a.all_chunk_coords();
        coords.extend(b.all_chunk_coords());
        coords.sort_unstable();
        coords.dedup();
        let (mut da, mut ca, mut db, mut cb) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for c in coords {
            let lo = [c[0] * floptle_field::CHUNK, c[1] * floptle_field::CHUNK, c[2] * floptle_field::CHUNK];
            a.gather(lo, floptle_field::CHUNK as usize, &mut da, &mut ca);
            b.gather(lo, floptle_field::CHUNK as usize, &mut db, &mut cb);
            let same = da.iter().zip(&db).all(|(x, y)| x.to_bits() == y.to_bits()) && ca == cb;
            assert!(same, "chunk {c:?} differs from {what}");
        }
    }

    fn flatten_op(p: Vec3, radius: f32) -> floptle_script::TerrainOp {
        floptle_script::TerrainOp {
            pos: [p.x as f64, p.y as f64, p.z as f64],
            radius,
            strength: 0.6,
            mode: floptle_script::TerrainOpMode::Flatten,
            id: 0,
            normal: None,
        }
    }

    fn editor_on(field: &floptle_field::ChunkField) -> (crate::Editor, Entity) {
        let mut ed = crate::Editor::default();
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.terrains.insert(e, EditorTerrain::new(field.clone()));
        (ed, e)
    }

    /// Frames, as the frame loop runs them, until every op has landed; after
    /// each, the op worker is let finish what it holds, so the count is what
    /// a frame lands and not how fast the machine is.
    fn frames_to_land(ed: &mut crate::Editor, ops: Vec<floptle_script::TerrainOp>) -> usize {
        let (mut frames, mut first) = (0usize, Some(ops));
        loop {
            ed.pump_collision_meshes();
            ed.apply_script_terrain_ops(first.take().unwrap_or_default());
            frames += 1;
            if ed.terrain_op_queue.is_empty() && ed.terrain_op_job.is_none() {
                return frames;
            }
            ed.wait_for_op_worker();
            assert!(frames < 100_000, "the edits never all landed");
        }
    }

    /// **Big edits are made ahead on the worker, and a frame lands all it has
    /// made.** A big op was made on a worker one at a time, and the next was
    /// handed over only once the last had landed: one a frame, whatever the
    /// frame rate, so a loading screen's 1,240-op grade took 21.7 s at 60 fps
    /// and 50 s at 20, the worker idle most of each frame. The worker is
    /// handed up to [`OP_JOB_AHEAD`] at a time now. Counted in frames, with
    /// the worker let finish each frame and the budget out of the way: 64
    /// ops land in a few frames (one a frame was 64), in order, and leave
    /// the ground making them all at once leaves.
    #[test]
    fn big_edits_are_made_ahead_and_land_many_a_frame() {
        let fill = floptle_field::procgen::PlanetFill { seed: 8, radius: 150.0, voxel: 1.5, relief: 6.0, cave_depth: 0.0, ..Default::default() };
        let field = floptle_field::procgen::generate_planet(&fill);
        let ground = field.raycast(Vec3::new(0.0, 250.0, 0.0), Vec3::NEG_Y, 250.0).unwrap();
        let n = 64;
        let ops: Vec<floptle_script::TerrainOp> = (0..n)
            .map(|i| {
                let (a, r) = (i as f32 * 2.399, 2.0 + (i % 16) as f32 * 1.5);
                flatten_op(ground + Vec3::new(a.cos() * r, 0.5, a.sin() * r), 6.0)
            })
            .collect();
        let (mut once, eo) = editor_on(&field);
        assert!(once.op_goes_off_frame(&ops[0], &once.resolve_terrain_op(&ops[0]).unwrap()), "the fixture's ops are not big");
        let mut batch: HashMap<Entity, TerrainBatch> = HashMap::new();
        for op in &ops {
            once.apply_terrain_op(op, &mut batch);
        }
        let (mut ed, e) = editor_on(&field);
        ed.script_host.set_terrain_edit_budget(Some(1000.0));
        let frames = frames_to_land(&mut ed, ops);
        eprintln!("{n} big edits landed in {frames} frames");
        assert!(frames * 8 <= n, "{n} big edits took {frames} frames");
        assert_same_chunks(&once.terrains[&eo].field, &ed.terrains[&e].field, "making the edits all at once");
    }

    /// **The budget sets how many small edits a frame lands, and a game can
    /// raise it.** Small edits land on the frame while
    /// [`TERRAIN_EDIT_BUDGET_MS`] lasts; `terrain.editBudget` raises it, for
    /// a loading screen that covers the view. Counted in frames: 800 small
    /// flattens land in far fewer frames than 800 at the default, and in
    /// fewer again at ten times the budget.
    #[test]
    fn the_edit_budget_sets_how_many_small_edits_a_frame_lands() {
        let fill = floptle_field::procgen::PlanetFill { seed: 8, radius: 120.0, voxel: 1.5, relief: 6.0, cave_depth: 0.0, ..Default::default() };
        let field = floptle_field::procgen::generate_planet(&fill);
        let ground = field.raycast(Vec3::new(0.0, 200.0, 0.0), Vec3::NEG_Y, 200.0).unwrap();
        let n = 800;
        let ops: Vec<floptle_script::TerrainOp> = (0..n)
            .map(|i| {
                let (a, r) = (i as f32 * 2.399, (i % 8) as f32 * 0.4);
                flatten_op(ground + Vec3::new(a.cos() * r, 0.3, a.sin() * r), 1.5)
            })
            .collect();
        let (mut ed, _) = editor_on(&field);
        let t = std::time::Instant::now();
        let at_default = frames_to_land(&mut ed, ops.clone());
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let (mut ed, _) = editor_on(&field);
        ed.script_host.set_terrain_edit_budget(Some(TERRAIN_EDIT_BUDGET_MS * 10.0));
        let raised = frames_to_land(&mut ed, ops);
        eprintln!("{n} small edits: {at_default} frames at the default budget ({ms:.0} ms), {raised} at ten times it");
        assert!(at_default * 4 <= n, "{n} small edits took {at_default} frames at the default budget");
        assert!(raised * 2 <= at_default, "ten times the budget took {raised} frames, against {at_default}");
    }

    /// **An edit over ground not made yet is made off the frame.** A world
    /// that has just streamed in has made only the ground near the player,
    /// and an edit over the rest makes it first, chunks of tens of
    /// milliseconds each, on the frame: a fresh world's first grade had a
    /// 504 ms frame, 450 of it `terrainEdits`, and its first ops are small
    /// cuts high over the site. Such an edit goes to the worker however small it is:
    /// the frame it is taken in makes no ground (counted in chunks still not
    /// made), and it lands as making it on the frame would have.
    #[test]
    fn an_edit_over_ground_not_made_yet_is_made_off_the_frame() {
        let fill = floptle_field::procgen::PlanetFill { seed: 8, radius: 150.0, voxel: 1.5, relief: 6.0, cave_depth: 0.0, ..Default::default() };
        let whole = floptle_field::procgen::generate_planet(&fill);
        let top = whole.raycast(Vec3::new(0.0, 250.0, 0.0), Vec3::NEG_Y, 250.0).unwrap();
        let side = whole.raycast(Vec3::new(250.0, 0.0, 0.0), Vec3::NEG_X, 250.0).unwrap();
        // Made round the top only, as a world streamed in under a player is.
        let field = floptle_field::procgen::generate_planet_near(&fill, top, floptle_field::CHUNK as f32 * fill.voxel);
        let op = flatten_op(side, 3.0);
        let reach = Vec3::splat(3.0 + field.band() + field.voxel());
        let before = field.pending_in_box(side - reach, side + reach).len();
        assert!(before > 0, "the fixture's edit is over ground already made");
        // Made on the frame, as before: it makes the ground there first.
        let (mut once, eo) = editor_on(&field);
        let t = std::time::Instant::now();
        let mut batch: HashMap<Entity, TerrainBatch> = HashMap::new();
        once.apply_terrain_op(&op, &mut batch);
        let inline_ms = t.elapsed().as_secs_f64() * 1e3;
        assert!(once.terrains[&eo].field.pending_in_box(side - reach, side + reach).len() < before, "making it on the frame made no ground");
        let (mut ed, e) = editor_on(&field);
        let t = std::time::Instant::now();
        ed.pump_collision_meshes();
        ed.apply_script_terrain_ops(vec![op]);
        let frame_ms = t.elapsed().as_secs_f64() * 1e3;
        eprintln!("an edit over ground not made yet: {inline_ms:.1} ms made on the frame, the frame taking it {frame_ms:.1} ms");
        let left = ed.terrains[&e].field.pending_in_box(side - reach, side + reach).len();
        assert_eq!(left, before, "the frame that took the edit made {} chunks of ground", before - left);
        assert!(ed.terrain_op_job.is_some(), "the edit was not handed to the worker");
        frames_to_land(&mut ed, Vec::new());
        assert!(ed.terrains[&e].field.pending_in_box(side - reach, side + reach).len() < before, "the ground the worker made did not land");
        assert_same_chunks(&once.terrains[&eo].field, &ed.terrains[&e].field, "making the edit on the frame");
    }

    /// **Ground a paint made reaches the sim's copy.** An edit over stand-in
    /// ground makes that ground first. A paint changes no shape, so its
    /// chunks were not copied to the physics sim, and the sim's copy, the one
    /// scripts read, kept the stand-in there: `terrain.ready` said false over
    /// it for good (the field said it was made, so `prepare` had nothing to
    /// make) and bodies stood on the coarse copy. Counted: after the paint,
    /// the sim's copy has the same chunks still to make as the field.
    #[test]
    fn ground_a_paint_made_reaches_the_sims_copy() {
        let fill = floptle_field::procgen::PlanetFill { seed: 8, radius: 150.0, voxel: 1.5, relief: 6.0, cave_depth: 0.0, ..Default::default() };
        let whole = floptle_field::procgen::generate_planet(&fill);
        let top = whole.raycast(Vec3::new(0.0, 250.0, 0.0), Vec3::NEG_Y, 250.0).unwrap();
        let side = whole.raycast(Vec3::new(250.0, 0.0, 0.0), Vec3::NEG_X, 250.0).unwrap();
        let mut field = floptle_field::procgen::generate_planet_near(&fill, top, floptle_field::CHUNK as f32 * fill.voxel);
        field.set_spec("test".into());
        let (mut ed, e) = editor_on(&field);
        ed.world.insert(e, floptle_core::CelestialBody { body_radius: 150.0, ..Default::default() });
        ed.world.insert(e, floptle_core::Matter::Terrain { id: 1, collision: Default::default() });
        ed.toggle_play();
        let reach = Vec3::splat(12.0);
        let before = ed.terrains[&e].field.pending_in_box(side - reach, side + reach).len();
        assert!(before > 0, "the fixture's paint is over ground already made");
        let paint = floptle_script::TerrainOp {
            pos: [side.x as f64, side.y as f64, side.z as f64],
            radius: 3.0,
            strength: 1.0,
            mode: floptle_script::TerrainOpMode::Paint([0.9, 0.1, 0.1]),
            id: 0,
            normal: None,
        };
        frames_to_land(&mut ed, vec![paint]);
        let made = ed.terrains[&e].field.pending_in_box(side - reach, side + reach);
        assert!(made.len() < before, "the paint made no ground");
        let sim = ed.sim.as_mut().unwrap().terrain_collider_mut(e.index()).unwrap().field.pending_in_box(side - reach, side + reach);
        assert_eq!(sim, made, "the sim's copy kept {} stand-in chunks the paint made", sim.len().saturating_sub(made.len()));
    }

    /// **A grade sent the moment a big world lands makes no ground on any
    /// frame.** A fresh world has made only the ground round the player, and a
    /// write over the rest makes it first, tens of milliseconds a chunk; an op
    /// that should have gone to the worker and did not was one 625 ms frame
    /// of Solar's. Counted, on a world Solar's size during Play: 1,200 small
    /// sculpts and flattens over made and stand-in ground, sent at once, make
    /// no chunk on the frame that takes them or on any frame after, and all
    /// land.
    #[test]
    fn a_grade_on_a_fresh_world_makes_no_ground_on_the_frame() {
        let fill = floptle_field::procgen::PlanetFill { seed: 8, radius: 922.0, voxel: 1.5, relief: 6.0, cave_depth: 20.0, defer_caves: true, ..Default::default() };
        let t = std::time::Instant::now();
        let top = Vec3::new(0.0, 928.0, 0.0);
        let mut field = floptle_field::procgen::generate_planet_near(&fill, top, NEAR_ON_LOAD_CHUNKS * floptle_field::CHUNK as f32 * fill.voxel);
        field.set_spec("test".into());
        let gen_ms = t.elapsed().as_secs_f64() * 1e3;
        let ground = field.raycast(Vec3::new(0.0, 1000.0, 0.0), Vec3::NEG_Y, 400.0).unwrap();
        let ops: Vec<floptle_script::TerrainOp> = (0..1200)
            .map(|i| {
                let (a, r) = (i as f32 * 2.399, (i % 40) as f32 * 5.0);
                let p = ground + Vec3::new(a.cos() * r, 0.5, a.sin() * r);
                let mode = match i % 3 { 0 => floptle_script::TerrainOpMode::Flatten, 1 => floptle_script::TerrainOpMode::Lower, _ => floptle_script::TerrainOpMode::Raise };
                floptle_script::TerrainOp { pos: [p.x as f64, p.y as f64, p.z as f64], radius: 1.5, strength: 0.6, mode, id: i as u64 + 1, normal: None }
            })
            .collect();
        let (mut ed, e) = editor_on(&field);
        ed.world.insert(e, floptle_core::CelestialBody { body_radius: 922.0, ..Default::default() });
        ed.world.insert(e, floptle_core::Matter::Terrain { id: 1, collision: Default::default() });
        ed.toggle_play();
        assert!(ed.sim.is_some(), "the fixture is not in Play");
        let t = std::time::Instant::now();
        let c = field.clone();
        let clone_ms = t.elapsed().as_secs_f64() * 1e3;
        drop(c);
        let pending0 = ed.terrains[&e].field.pending_count();
        let t = std::time::Instant::now();
        ed.pump_collision_meshes();
        ed.apply_script_terrain_ops(ops);
        let first = t.elapsed().as_secs_f64() * 1e3;
        let made_first = pending0 - ed.terrains[&e].field.pending_count();
        let (mut worst, mut frames) = (0.0f64, 1);
        while !(ed.terrain_op_queue.is_empty() && ed.terrain_op_job.is_none()) && frames < 5000 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            let t = std::time::Instant::now();
            ed.pump_collision_meshes();
            ed.apply_script_terrain_ops(Vec::new());
            worst = worst.max(t.elapsed().as_secs_f64() * 1e3);
            frames += 1;
        }
        floptle_say::say!("GRADE gen {gen_ms:.0} ms, clone {clone_ms:.1} ms, chunks {} pending {pending0}; submit frame {first:.1} ms making {made_first} chunks; worst later {worst:.1} ms over {frames} frames", ed.terrains[&e].field.chunk_coords().len());
        assert_eq!(made_first, 0, "the frame that took the grade made {made_first} chunks of ground");
        assert_eq!(ed.terrain_inline_made, 0, "the grade made {} chunks of ground on frames", ed.terrain_inline_made);
        assert!(ed.terrain_op_queue.is_empty() && ed.terrain_op_job.is_none(), "the grade never landed in {frames} frames");
    }

    /// **A world that has just landed puts its far ground away a little at a
    /// time.** It is almost all far from the player, and every chunk of it
    /// was put away in the first scan: on Solar's scale-3 worlds that was a
    /// 320 to 720 ms frame with no part named. A scan now puts away at most
    /// [`EVICT_PER_SCAN`] chunks, farthest first, and the scans go on until
    /// the far ground is all away.
    #[test]
    fn a_landed_world_puts_its_far_ground_away_a_little_at_a_time() {
        let fill = floptle_field::procgen::PlanetFill { seed: 5, radius: 300.0, voxel: 1.5, relief: 6.0, cave_depth: 0.0, ..Default::default() };
        let mut field = floptle_field::procgen::generate_planet(&fill);
        field.set_spec("test".into());
        let mut ed = crate::Editor::default();
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.world.insert(e, floptle_core::CelestialBody { body_radius: 300.0, ..Default::default() });
        ed.terrains.insert(e, EditorTerrain::new(field));
        let cam = DVec3::new(0.0, 310.0, 0.0);
        let (mut scans, mut most, mut last) = (0, 0, 0);
        for _ in 0..FAR_SCAN_FRAMES * 400 {
            ed.stream_far_chunks(cam);
            let now = ed.terrains[&e].field.evicted_count();
            if now != last {
                scans += 1;
                most = most.max(now - last);
                last = now;
            }
        }
        assert!(last > EVICT_PER_SCAN * 3, "only {last} chunks were put away: the fixture is not far enough");
        assert!(most <= EVICT_PER_SCAN, "one scan put {most} chunks away");
        assert!(scans > 3, "{last} chunks went in {scans} scan(s)");
        // And the scans went on until the far ground was all away.
        let t = &ed.terrains[&e].field;
        let left = t.chunk_coords().into_iter().filter(|c| t.evictable(*c)).count();
        assert!(left < t.chunk_coords().len() / 2, "{left} chunks were never put away");
    }

    /// **Ground a game asked for stays made, even what was made already.**
    /// `terrain.prepare` kept the chunks it had to make, so `terrain.ready`
    /// would not go back to false under the game; ground already made when it
    /// asked was not kept, and was put away with the rest of the far side,
    /// and the box read not ready again. Counted: a whole world, a prepare
    /// far from the camera while every chunk is still made, and every far
    /// scan after it; not one chunk in the box is put away, while hundreds
    /// beside it are.
    #[test]
    fn ground_asked_for_is_kept_even_where_it_was_made_already() {
        let fill = floptle_field::procgen::PlanetFill { seed: 5, radius: 300.0, voxel: 1.5, relief: 6.0, cave_depth: 0.0, ..Default::default() };
        let mut field = floptle_field::procgen::generate_planet(&fill);
        field.set_spec("test".into());
        let mut ed = crate::Editor::default();
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.world.insert(e, floptle_core::CelestialBody { body_radius: 300.0, ..Default::default() });
        ed.terrains.insert(e, EditorTerrain::new(field));
        let site = Vec3::new(0.0, -300.0, 0.0);
        let reach = Vec3::splat(60.0 + 1.5 * 2.0);
        assert!(ed.terrains[&e].field.pending_in_box(site - reach, site + reach).is_empty(), "the fixture's site is not all made");
        ed.terrain_prepare_asks.push((
            floptle_script::PrepareAsk { center: site.as_dvec3().to_array(), radius: 60.0, frames: Vec::new() },
            std::collections::HashSet::new(),
        ));
        let cam = DVec3::new(0.0, 310.0, 0.0);
        for _ in 0..FAR_SCAN_FRAMES * 200 {
            ed.stream_far_chunks(cam);
        }
        let t = &ed.terrains[&e].field;
        assert!(t.evicted_count() > 200, "only {} chunks were put away: the fixture is not far enough", t.evicted_count());
        let gone = t.pending_in_box(site - reach, site + reach).len();
        assert_eq!(gone, 0, "{gone} chunks of the ground asked for were put away");
    }

    /// **A world's chunks are gathered for meshing off the frame.** A coarse
    /// chunk is meshed from a dense block of the field half again its size on
    /// every side, half a million voxels, and up to sixteen were gathered on
    /// the frame each frame while a world that had just landed was meshed:
    /// 190 ms a frame for seconds (debug). The workers gather from a shared
    /// copy now, so a world's drawn chunks reach them with nothing gathered.
    /// Counted, not timed: a ratio of frame to gather read 13 under a loaded
    /// test run against 17.5 gathered on the frame.
    #[test]
    fn a_new_worlds_chunks_are_gathered_off_the_frame() {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let fill = floptle_field::procgen::PlanetFill { seed: 5, radius: 300.0, voxel: 1.5, relief: 6.0, cave_depth: 0.0, ..Default::default() };
        let field = floptle_field::procgen::generate_planet(&fill);
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.world.insert(e, floptle_core::CelestialBody { body_radius: 300.0, ..Default::default() });
        ed.terrains.insert(e, EditorTerrain::new(field));
        let cam = DVec3::new(0.0, 310.0, 0.0);
        let (off, on) = (DRAWN_GATHERS[0].load(Relaxed), DRAWN_GATHERS[1].load(Relaxed));
        ed.sync_terrain_meshes(true, cam);
        for _ in 0..40 {
            ed.sync_terrain_meshes(false, cam);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let off = DRAWN_GATHERS[0].load(Relaxed) - off;
        let on = DRAWN_GATHERS[1].load(Relaxed) - on;
        assert!(off > 16, "only {off} chunks were meshed: the fixture is not meshing a world");
        assert_eq!(on, 0, "{on} drawn chunks were gathered on the frame ({off} on a worker)");
    }


    /// **Ground put away goes back to the system.** Putting nine tenths of a
    /// 650 m world's chunks away freed 400 MB that the process went on
    /// holding: the system allocator keeps what is freed. Handed back, the
    /// process is smaller by most of what the ground held.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn ground_put_away_goes_back_to_the_system() {
        // Measured in a process of its own: the other tests in this one
        // allocate hundreds of megabytes while it measures.
        if std::env::var_os("FLOPTLE_TEST_ALONE").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "terrain_edit::residency_tests::ground_put_away_goes_back_to_the_system", "--test-threads=1"])
                .env("FLOPTLE_TEST_ALONE", "1")
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success() && said.contains("1 passed"), "{said}");
            return;
        }
        fn rss() -> f64 {
            let s = std::fs::read_to_string("/proc/self/statm").unwrap();
            s.split_whitespace().nth(1).unwrap().parse::<f64>().unwrap() * 4096.0
        }
        let fill = floptle_field::procgen::PlanetFill { seed: 3, radius: 450.0, voxel: 1.5, relief: 8.0, cave_depth: 0.0, ..Default::default() };
        let mut field = floptle_field::procgen::generate_planet(&fill);
        field.set_spec("test".into());
        give_memory_back();
        let held = field.memory_bytes() as f64;
        let before = rss();
        let far: Vec<[i32; 3]> = field.chunk_coords().into_iter().filter(|c| c[1] < 6).collect();
        field.evict(&far);
        let freed = held - field.memory_bytes() as f64;
        give_memory_back();
        let back = before - rss();
        assert!(freed > 50e6, "the fixture frees only {:.0} MB", freed / 1e6);
        assert!(back > freed * 0.4, "{:.0} MB of ground was put away and the process is {:.0} MB smaller", freed / 1e6, back / 1e6);
    }

    /// **A world generated over a terrain draws none of what stood there
    /// before.** A scene opens on a starter slab and its script generates a
    /// planet over it; the slab's chunk meshes stayed, merged into the new
    /// world's far groups, and drew inside it. Every chunk drawn is one the
    /// world holds.
    #[test]
    fn a_world_generated_over_a_terrain_draws_none_of_the_old_one() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Transform::IDENTITY);
        ed.world.insert(e, floptle_core::Matter::Terrain { id: 1, collision: Default::default() });
        ed.world.insert(e, floptle_core::CelestialBody { body_radius: 120.0, ..Default::default() });
        let mut slab = floptle_field::ChunkField::new(1.5);
        slab.fill_slab(Vec3::new(-24.0, -6.0, -24.0), Vec3::new(24.0, 6.0, 24.0), 6.0, [0.35, 0.6, 0.28]);
        ed.terrains.insert(e, EditorTerrain::new(slab));
        let cam = DVec3::new(0.0, 0.0, 450.0);
        for _ in 0..40 {
            ed.sync_terrain_meshes(false, cam);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!ed.terrain_render[&e].slots.is_empty(), "the slab never meshed");
        let fill = floptle_field::procgen::PlanetFill { seed: 6, radius: 120.0, voxel: 1.5, relief: 6.0, cave_depth: 0.0, ..Default::default() };
        ed.start_terrain_generates(vec![(1, fill)]);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while ed.planet_gen_job.is_some() {
            ed.poll_terrain_generates();
            assert!(std::time::Instant::now() < until, "the world never landed");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for _ in 0..200 {
            ed.sync_terrain_meshes(false, cam);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (r, f) = (&ed.terrain_render[&e], &ed.terrains[&e].field);
        assert!(!r.slots.is_empty(), "the world never meshed");
        let stale: Vec<_> = r.slots.keys().filter(|c| !f.has_data(**c)).collect();
        assert!(stale.is_empty(), "chunks of what stood there before still draw: {stale:?}");
    }
}
