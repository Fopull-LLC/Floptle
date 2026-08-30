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
use std::time::Instant;
use crate::dock::{focus_terrain_tab};
use crate::gizmo::{Tool};
use crate::shading::{OccKey, material_params};
use crate::terrain_ui::{NewTerrainCfg};
use crate::viz::{TerrainViz, project};
use crate::{Editor};

/// One editable terrain (Terrain 2.0 / P3): the sparse unbounded [`ChunkField`] is THE
/// authority — brushes write it, physics collides it, saves serialize it, the mesher
/// extracts the drawn surface from it. The dense `shadow` proxy is DERIVED from it at a
/// capped resolution purely to feed the GPU shadow/AO atlas (until the P5 clipmap).
pub(crate) struct EditorTerrain {
    pub field: floptle_field::ChunkField,
    pub shadow: floptle_field::BakedSdf,
}

/// Longest-axis cell cap for the shadow proxy. Soft sun shadows are forgiving of a
/// coarse field; primary visibility (the unforgiving part) is the chunk meshes.
pub(crate) const TERRAIN_SHADOW_MAX_DIM: u32 = 192;

impl EditorTerrain {
    /// Wrap a field, deriving its shadow proxy.
    pub(crate) fn new(field: floptle_field::ChunkField) -> Self {
        let shadow = shadow_proxy_of(&field);
        Self { field, shadow }
    }

    /// Re-derive the shadow proxy from the current field (structural change / undo /
    /// bounds outgrown). The empty-field proxy is a tiny inert box.
    pub(crate) fn rebuild_shadow(&mut self) {
        self.shadow = shadow_proxy_of(&self.field);
    }
}

fn shadow_proxy_of(field: &floptle_field::ChunkField) -> floptle_field::BakedSdf {
    field.to_dense(TERRAIN_SHADOW_MAX_DIM).unwrap_or(floptle_field::BakedSdf {
        dims: [2, 2, 2],
        center: [0.0; 3],
        half_extent: [0.5; 3],
        distance: vec![1.0; 8],
        color: vec![[128, 128, 128, 255]; 8],
    })
}

/// The GPU residency of one terrain's chunk meshes. Chunk vertices are FIELD-space, so
/// every chunk shares one camera-relative instance matrix and the triplanar material
/// stays continuous.
#[derive(Default)]
pub(crate) struct TerrainRender {
    /// One dynamic raster slot per non-empty chunk (+ the LOD it was meshed at),
    /// keyed by chunk coord so a sculpt can re-mesh just the chunks it touched and
    /// free the ones that emptied.
    pub slots: HashMap<[i32; 3], (MeshId, u8)>,
    /// When each chunk FIRST became resident, in seconds on the editor's clock —
    /// what [`chunk_fade`] measures the dissolve-in against (`floptle/0067`).
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
    /// Data chunks whose last mesh came out EMPTY (a band remnant with no zero
    /// crossing). Without this the coverage scan would re-queue them every frame
    /// forever; a brush dirtying the chunk clears its entry.
    pub empty: std::collections::HashSet<[i32; 3]>,
    /// FAR-BODY IMPOSTOR mode: the camera is so far from this (celestial) terrain
    /// that chunk meshes would be sub-pixel noise — streaming stops, resident
    /// chunks are evicted, and the body draws as one shaded sphere instead
    /// (radial star light gives it the correct terminator for free).
    pub impostor: bool,
    /// The impostor sphere's albedo — the body's average surface color, sampled
    /// from the field once on the first switch to impostor mode.
    pub impostor_color: Option<[f32; 3]>,
}

/// P4 LOD ring radii, in CHUNKS of Chebyshev distance from the camera's chunk:
/// within `RINGS[l]` → stride `2^l`; beyond the last ring → stride 8. One chunk
/// ≈ 48 units at the default 1.5-unit voxel.
///
/// These are the radii for a world big enough that "24 chunks away" is over the
/// horizon. On a SMALL body they are not — see [`rings_for_body`].
const LOD_RINGS: [i32; 3] = [4, 10, 24];

/// What fraction of a body's RADIUS each ring should reach, on a body small
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

/// The LOD rings to use for a terrain, given the body radius it belongs to
/// (`None` for ordinary, non-celestial terrain).
///
/// Never LARGER than the absolute rings, so a big world is untouched: this can
/// only tighten. Never smaller than one chunk per ring either — a body you can
/// stand on always gets a ring of full detail under your feet and two coarser
/// ones around it, however small it is.
fn rings_for_body(body_radius: Option<f64>, chunk_units: f64) -> [i32; 3] {
    let Some(r) = body_radius.filter(|r| *r > 0.0) else { return LOD_RINGS };
    let chunk = chunk_units.max(1e-3);
    let mut out = LOD_RINGS;
    for (i, frac) in BODY_RING_FRACTION.iter().enumerate() {
        let by_body = ((r * frac) / chunk).ceil() as i32;
        out[i] = out[i].min(by_body).max(i as i32 + 1);
    }
    // Rings must stay strictly ordered or `lod_for`'s hysteresis compares
    // against a boundary that is behind it.
    out[1] = out[1].max(out[0] + 1);
    out[2] = out[2].max(out[1] + 1);
    out
}

/// A celestial terrain switches to its sphere impostor beyond this many body
/// radii of camera distance (~2° of angular diameter — chunk meshes would be
/// a handful of pixels).
const IMPOSTOR_RADII: f64 = 60.0;

/// G1 RESIDENCY (docs/subsystems/large-world-space.md): a COLD celestial terrain's
/// field starts loading (background) when the camera comes inside this many body
/// radii — outside the impostor flip at 60, so the field is always resident
/// before its meshes could possibly draw. Evict sits farther out again, so
/// load/evict can never thrash and every transition happens while the body is
/// an impostor (visually invisible).
const RESIDENT_LOAD_RADII: f64 = 80.0;
/// A RESIDENT celestial terrain beyond this many body radii is evicted: saved to
/// disk first when its field changed (edit mode), then dropped to [`ColdTerrain`].
const RESIDENT_EVICT_RADII: f64 = 110.0;
/// Emergency: something is INSIDE this many radii of a still-cold body (teleport,
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
    pub started: std::time::Instant,
    pub rx: std::sync::mpsc::Receiver<Option<EditorTerrain>>,
}

/// One in-flight background CHECKPOINT (`terrain.flush()` → the save slot):
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
    /// still writes (newer than any previous file) but the field STAYS dirty
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
/// A field edited more recently than this is SKIPPED by the checkpoint picker —
/// saves happen in the quiet moments between digs, never under the shovel.
const CHECKPOINT_QUIET_SECS: f64 = 1.5;
/// …unless it has been waiting this long (someone digging non-stop): start
/// anyway — a torn snapshot beats a checkpoint that never happens.
const CHECKPOINT_FORCE_SECS: f64 = 20.0;

/// Run a load to completion, trying each candidate source IN ORDER — a
/// truncated/corrupt field file falls back to the next source (usually the
/// genspec, which regenerates the body deterministically) instead of failing
/// the whole stream: a bad file must never take a world offline when its
/// recipe is right there on the node. File read+parse or full procgen, plus
/// the shadow-proxy derivation; runs on background threads (and blockingly
/// for file sources at the emergency radius). None = every candidate failed.
fn load_terrain_from(sources: Vec<TerrainSource>) -> Option<EditorTerrain> {
    for src in sources {
        let field = match src {
            TerrainSource::File(path) => std::fs::read(&path)
                .ok()
                .and_then(|b| floptle_field::ChunkField::from_bytes(&b)),
            TerrainSource::Generate(spec) => ron::from_str(&spec)
                .ok()
                .map(|fill: floptle_field::procgen::PlanetFill| {
                    floptle_field::procgen::generate_planet(&fill)
                }),
        };
        if let Some(field) = field {
            return Some(EditorTerrain::new(field));
        }
    }
    None
}

/// Stable hash of a genspec string — recorded in the `.meta` sidecar when a
/// field is written, so a PROJECT field file is only trusted for a body whose
/// genspec still matches. Regenerating a system reuses terrain ids: without
/// this, the OLD system's leftover `<scene>.<id>.cfield` would load as the NEW
/// body's terrain (the wrong planet entirely). Save-slot files skip the check —
/// a slot belongs to one galaxy seed by construction (the game's contract).
fn genspec_hash(spec: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in spec.as_bytes() {
        h = h.wrapping_mul(33) ^ u64::from(*b);
    }
    h
}

/// A genspec body's impostor color without generating anything: its surface
/// palette's primary tint (close enough for a sub-pixel-to-few-pixel sphere).
fn genspec_impostor_color(spec: &str) -> Option<[f32; 3]> {
    let fill: floptle_field::procgen::PlanetFill = ron::from_str(spec).ok()?;
    Some(fill.surface_a.color)
}

/// The body's average surface color for its impostor sphere: 26 rays from
/// outside toward the center, averaging the voxel color where each first hits.
/// Field-space — celestial terrain fields are authored centered on the origin.
pub(crate) fn impostor_surface_color(field: &floptle_field::ChunkField, radius: f32) -> [f32; 3] {
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
                    let c = field.color(hit);
                    sum[0] += c[0] as f32 / 255.0;
                    sum[1] += c[1] as f32 / 255.0;
                    sum[2] += c[2] as f32 / 255.0;
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
        if dist > rings[(cur as usize).min(2)] + 1 {
            raw
        } else {
            cur
        }
    } else {
        // Refine only once clearly inside the finer ring.
        if dist < rings[raw as usize] {
            raw
        } else {
            cur
        }
    }
}

/// A queued remesh: the scratch is gathered on the main thread (it owns the field);
/// the worker meshes from the scratch alone and never touches editor state (T4).
pub(crate) struct RemeshJob {
    pub entity: Entity,
    pub coord: [i32; 3],
    pub lod: u8,
    pub skirt: bool,
    pub epoch: u64,
    pub scratch: floptle_field::MeshScratch,
}

pub(crate) struct RemeshDone {
    pub entity: Entity,
    pub coord: [i32; 3],
    pub lod: u8,
    pub epoch: u64,
    pub mesh: floptle_field::ChunkMesh,
}

/// The background remesh worker (P4): one thread, jobs in / meshes out over
/// channels — the same shape as the audio decode worker. Dropping the sender on
/// editor exit ends the thread.
pub(crate) struct TerrainWorker {
    tx: std::sync::mpsc::Sender<RemeshJob>,
    rx: std::sync::mpsc::Receiver<RemeshDone>,
    /// Jobs sent and not yet drained — the main thread caps this so a big LOD
    /// migration streams in nearest-first instead of flooding the queue.
    pub in_flight: usize,
}

/// At most this many queued-but-unfinished jobs. Each job's scratch is ~0.5–1.2 MB,
/// so the cap also bounds transient memory.
const WORKER_IN_FLIGHT_CAP: usize = 16;

/// Subtracted from a dirty (brush/script-edited) chunk's queue priority: edits sort
/// ahead of every LOD migration AND bypass the in-flight cap — a stale mesh under
/// the player is worse than a deep queue.
const DIRTY_PRIORITY_BOOST: i32 = 1_000_000;

/// How close to a body's centre, in body radii, counts as being ON it.
///
/// Generous — an aircraft at altitude, or a ship on approach, is still landing
/// on the thing under it and still wants that ground first.
const ON_BODY_RADII: f64 = 3.0;

/// Where a chunk sits in the meshing queue: **metres from the camera**, so that
/// chunks belonging to different terrains can be compared at all
/// (`floptle/0074`).
///
/// One queue is shared by every resident terrain. The key used to be chunk
/// distance in each terrain's own local frame, so a chunk three chunks from the
/// camera on the planet under your feet tied with one three chunks from the
/// camera on a planet twelve thousand units away — and which of them got the
/// worker slot was whatever order a `HashMap` happened to iterate in. Distances
/// in different terrains are only comparable once they are in the same units.
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
    // away and must not wrap into the FRONT of the queue.
    let metres = (world - cam_world).length().min(i32::MAX as f64 / 4.0) as i32;
    if on_body { metres } else { metres.saturating_add(OFF_BODY_PENALTY) }
}

/// Added to every chunk of a body the camera is NOT on (`floptle/0074`).
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
        std::thread::Builder::new()
            .name("terrain-remesh".into())
            .spawn(move || {
                while let Ok(job) = jrx.recv() {
                    let mesh = floptle_field::mesh_scratch(&job.scratch, job.skirt);
                    if dtx
                        .send(RemeshDone {
                            entity: job.entity,
                            coord: job.coord,
                            lod: job.lod,
                            epoch: job.epoch,
                            mesh,
                        })
                        .is_err()
                    {
                        break; // editor gone
                    }
                }
            })
            .expect("spawn terrain remesh worker");
        Self { tx: jtx, rx: drx, in_flight: 0 }
    }

    pub(crate) fn send(&mut self, job: RemeshJob) {
        if self.tx.send(job).is_ok() {
            self.in_flight += 1;
        }
    }

    pub(crate) fn try_recv(&mut self) -> Option<RemeshDone> {
        match self.rx.try_recv() {
            Ok(d) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                Some(d)
            }
            Err(_) => None,
        }
    }
}

impl Editor {
    /// Seconds since the editor started — a monotonic clock for anything that
    /// animates on wall time rather than on the play session's `play_t`, which
    /// restarts with every Play.
    ///
    /// `None` before there is a window, and deliberately an `Option` rather
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
    /// script-dirtied chunks remesh — the near ring SYNCHRONOUSLY (sculpting must
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
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return;
        };
        // Drop render meshes for terrains that no longer exist (deleted nodes).
        // COLD terrains count as live: their render entry IS the impostor sphere.
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
            for (_, (mid, _)) in r.slots.drain() {
                raster.free_dynamic(mid);
            }
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
        for (&e, terrain) in &self.terrains {
            let structural = full_rebuild || !self.terrain_render.contains_key(&e);
            let dirty = self.terrain_chunks_dirty.remove(&e);
            let render = self.terrain_render.entry(e).or_default();
            let wt = floptle_core::world_transform(&self.world, e);
            let (anchor, rot, ts) =
                (wt.translation, wt.rotation.normalize(), wt.scale.x.max(1e-6));

            // FAR-BODY IMPOSTOR gate: a celestial terrain seen from far enough
            // that its whole disc is a couple of degrees wide stops streaming
            // entirely and frees its resident chunk meshes; push_terrain_instances
            // draws the shaded sphere instead. ×0.9 hysteresis so orbiting along
            // the threshold can't thrash evict/remesh cycles.
            if let Some(cb) = self.world.get::<floptle_core::CelestialBody>(e) {
                let cam_dist = (cam_world - anchor).length();
                let enter = cb.body_radius.max(1.0) * IMPOSTOR_RADII;
                let was = render.impostor;
                render.impostor =
                    if render.impostor { cam_dist > enter * 0.9 } else { cam_dist > enter };
                if render.impostor != was {
                    // The SDF shadow/AO atlas excludes impostor bodies — re-lay it out.
                    self.terrain_gpu_dirty = true;
                }
                // The body's surface colour is wanted whether it is far enough to
                // BE an impostor or close enough to be streaming — a streaming
                // body draws the same colour as a backstop under its arriving
                // chunks (`floptle/0074`). Sampled once, on whichever comes first.
                if render.impostor_color.is_none() {
                    render.impostor_color =
                        Some(impostor_surface_color(&terrain.field, cb.body_radius as f32));
                }
                if render.impostor {
                    render.pending.clear();
                    render.empty.clear();
                    for (_, (mid, _)) in render.slots.drain() {
                        raster.free_dynamic(mid);
                    }
                    render.born.clear();
                    continue;
                }
            } else {
                render.impostor = false;
            }
            let chunk_units = floptle_field::CHUNK as f32 * terrain.field.voxel();
            // Rings sized to the BODY when there is one (`floptle/0067`): on a
            // world you can walk around, "24 chunks away" is the far side of it.
            let rings = rings_for_body(
                self.world
                    .get::<floptle_core::CelestialBody>(e)
                    .map(|cb| cb.body_radius * ts as f64),
                (chunk_units * ts) as f64,
            );
            // Camera into the FIELD's local frame (rotation + uniform scale), so
            // LOD rings follow the terrain wherever its node puts it.
            let cl = (rot.inverse() * (cam_world - anchor).as_vec3()) / (chunk_units * ts);
            let cam_chunk =
                [cl.x.floor() as i32, cl.y.floor() as i32, cl.z.floor() as i32];
            // LOD ring selection, in THIS terrain's chunk units — which is the
            // right unit for a ring, and the wrong one for a queue position.
            let dist_of = |c: [i32; 3]| {
                (c[0] - cam_chunk[0])
                    .abs()
                    .max((c[1] - cam_chunk[1]).abs())
                    .max((c[2] - cam_chunk[2]).abs())
            };
            // Queue position, in METRES, so chunks from different terrains can
            // be compared at all (`floptle/0074`).
            //
            // One queue is shared by every resident terrain, and the sort key
            // used to be `dist_of` — chunk distance in each terrain's own local
            // frame. A chunk three chunks away on the planet under your feet
            // therefore tied with one three chunks away on a planet twelve
            // thousand units off, and the winner was whatever order the
            // `terrains` map happened to iterate in. The report was "I can see
            // through unloaded terrain and it needs to prioritize loading what's
            // right under me".
            // …and the ground you are STANDING on outranks everything, before any
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
                // A chunk the field no longer fills gives its stamp back with
                // its slot, so coming back later is an arrival and fades again.
                render.born.retain(|c, _| has.contains(c));
            }

            // Brush / script edits: near chunks synchronously (sculpting must feel
            // instant), far chunks through the worker — dirty jobs BYPASS the
            // in-flight cap (a stale mesh is worse than a deep queue).
            if let Some(mut d) = dirty {
                d.sort_unstable();
                d.dedup();
                for coord in d {
                    render.empty.remove(&coord);
                    let dist = dist_of(coord);
                    let cur = render.slots.get(&coord).map(|&(_, l)| l);
                    let lod = cur.unwrap_or_else(|| raw_lod(dist, rings));
                    if lod == 0 {
                        render.pending.remove(&coord); // a sync mesh supersedes any job
                        let cm = floptle_field::mesh_chunk(&terrain.field, coord, 1, false);
                        if cm.is_empty() {
                            if let Some((mid, _)) = render.slots.remove(&coord) {
                                raster.free_dynamic(mid);
                            }
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
            // by distance. LOD migration for RESIDENT chunks rides the same queue
            // (hysteresis inside lod_for).
            //
            // THROTTLED: this walks EVERY chunk of the field (a big planet has
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
            for coord in terrain.field.chunk_coords() {
                match render.slots.get(&coord) {
                    None => {
                        if render.pending.contains_key(&coord)
                            || render.empty.contains(&coord)
                        {
                            continue;
                        }
                        let d = dist_of(coord);
                        if raw_lod(d, rings) == 0 {
                            // The ground around the player never streams: a fresh
                            // load (or a dig that created a chunk) meshes it NOW.
                            let cm =
                                floptle_field::mesh_chunk(&terrain.field, coord, 1, false);
                            if cm.is_empty() {
                                render.empty.insert(coord);
                            } else {
                                upload_chunk(gpu, raster, render, coord, &cm, 0, now);
                            }
                        } else {
                            queue.push((prio_of(coord), e, coord, raw_lod(d, rings)));
                        }
                    }
                    Some(&(_, cur)) => {
                        let want = lod_for(dist_of(coord), cur, rings);
                        if want != cur && !render.pending.contains_key(&coord) {
                            queue.push((prio_of(coord), e, coord, want));
                        }
                    }
                }
            }
        }

        // ---- 5: top up the worker — dirty edits first (uncapped), then nearest ----
        if let Some(w) = self.terrain_worker.as_mut() {
            queue.sort_unstable_by_key(|&(d, ..)| d);
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
                    skirt: lod > 0,
                    epoch: self.terrain_epoch,
                    scratch: floptle_field::scratch_for_chunk(
                        &terrain.field,
                        coord,
                        1 << lod,
                    ),
                });
            }
        }
    }

}

/// Append every terrain's chunk-mesh instances to the raster draw list. The model matrix
/// places the field-space chunk vertices via the node's f64 anchor, exactly as
/// `fill_terrain_volumes` places the shadow/AO volume — so the drawn mesh and the marched
/// field coincide (ADR-0015 camera-relative).
///
/// A FREE function taking explicit fields, like `fill_terrain_volumes` /
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
    // Frustum planes (Gribb–Hartmann) in CAMERA-RELATIVE space — the same
    // space the instance matrices are built in (ADR-0015). Chunks cull per
    // bounding sphere: a big planet close up otherwise submits THOUSANDS of
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
    // outside of? Exact point-behind-sphere against a CONSERVATIVELY shrunk
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
        // STREAMING BACKSTOP (`floptle/0074`). A chunk that has been queued but
        // not yet meshed draws nothing at all, so the player sees space through
        // the ground — "I can see through unloaded terrain". While a body is
        // still streaming, fill the holes with one shaded sphere at its
        // OCCLUDER radius: the ball it already declares as being wholly inside
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
        // The node's FULL placement: rotation + uniform scale finally apply to
        // terrain (physics converts through the same frame — see ChunkTerrain).
        let model = Mat4::from_scale_rotation_translation(Vec3::splat(scale), rot, rel);
        // Per-chunk culling geometry: chunk edge in world units, bounding-sphere
        // radius padded for skirts, and the body's occluder ball when declared.
        let chunk_units = terrains
            .get(&e)
            .map(|t| floptle_field::CHUNK as f32 * t.field.voxel())
            .unwrap_or(0.0)
            * scale;
        let chunk_r = chunk_units * 0.95; // half-diagonal (0.866) + skirt pad
        let occ_r = world
            .get::<floptle_core::CelestialBody>(e)
            .map(|cb| cb.occluder_radius as f32 * scale)
            .unwrap_or(0.0);
        for (coord, &(mid, _)) in &render.slots {
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
            // Dissolve-in for a chunk that just arrived (`floptle/0067`). The
            // alpha lane is free on terrain — the shader forces terrain opaque
            // because its VERTEX alpha is a palette slot — so this rides an
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
/// `color.a` the shader dissolves against (`floptle/0067`).
///
/// Clamped at both ends. A `born` in the FUTURE reads as fully opaque rather
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
/// TWO constraints, and the tighter (coarser) wins:
///   1. Source detail — the MEDIAN of the three axis resolutions. Using the *min* (my
///      first cut) is catastrophic for a STRETCHED legacy field: the 18:1 Y-stretch makes
///      one axis ~0.36 units, and meshing the 578×578 footprint at 0.36 is hundreds of
///      millions of voxels — it floods the terrain color store (2^24 verts) and takes
///      forever. The median tracks the real content scale, not the thinnest artifact axis.
///   2. A FOOTPRINT BUDGET — surface-nets vertex count scales with the two LARGEST extents'
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

    // Footprint = the two LARGEST world extents (a terrain is a wide, shallow slab; its
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
    pub(crate) fn focus_terrain(&mut self) {
        if let Some(dock) = self.dock_state.as_mut() {
            focus_terrain_tab(dock);
        }
    }

    /// Build every node's STATIC collider into the sim at Play. A node is a static
    /// collider if it carries `Collidable` (the "collidable" switch) or the legacy
    /// `MeshCollider` marker. The collider is auto-shaped from the node's `Matter`:
    /// a Mesh bakes its world-space triangles; a Cube/Sphere/Capsule primitive becomes
    /// a box/sphere/capsule sized to the primitive geometry × the node's scale (and
    /// oriented by its rotation). These are environment colliders, not dynamic bodies.
    /// Keep the shadow-occluder bakes in sync with the scene's static collider
    /// meshes (Collidable / MeshCollider on a `Matter::Mesh` node, no RigidBody —
    /// dynamic bodies cast via their shape proxies instead). Each eligible mesh
    /// bakes once per (asset, rotation, scale) into an unsigned occluder volume
    /// (`bake_occluder`), cached so duplicates and pure moves are free. Returns
    /// true when the SET changed and the atlas needs re-uploading; per-node
    /// "casts shadows" / visibility toggles are applied at fill time (no rebake).
    pub(crate) fn refresh_mesh_occluders(&mut self) -> bool {
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
            let wt = floptle_core::world_transform(&self.world, e);
            let q = |v: f32| (v * 1000.0).round() as i32;
            let key: OccKey = (
                path,
                [q(wt.rotation.x), q(wt.rotation.y), q(wt.rotation.z), q(wt.rotation.w)],
                [q(wt.scale.x), q(wt.scale.y), q(wt.scale.z)],
            );
            desired.push((e, key));
        }
        let unchanged = desired.len() == self.mesh_occluders.len()
            && desired
                .iter()
                .all(|(e, key)| self.mesh_occluders.get(e).is_some_and(|(k, _)| k == key));
        if unchanged {
            return false;
        }

        let mut next: HashMap<Entity, (OccKey, std::sync::Arc<floptle_field::BakedSdf>)> =
            HashMap::new();
        for (e, key) in desired {
            let baked = if let Some(b) = self.occluder_cache.get(&key) {
                b.clone()
            } else {
                // Bake: rotation + scale applied to the vertices (like the physics
                // colliders); translation stays in the per-frame f64 anchor.
                let started = Instant::now();
                // Project-resolved: read raw, every mesh shadow occluder in an
                // exported build failed to load and the scene lost its shadows.
                let file = crate::project::resolve_asset_path(&self.project_root, &key.0);
                let Ok(model) = floptle_assets::gltf_import::import(&file) else {
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!("shadow occluder: failed to load {}", key.0),
                        None,
                    );
                    continue;
                };
                let rot = Quat::from_xyzw(
                    key.1[0] as f32 / 1000.0,
                    key.1[1] as f32 / 1000.0,
                    key.1[2] as f32 / 1000.0,
                    key.1[3] as f32 / 1000.0,
                )
                .normalize();
                let s = Vec3::new(
                    key.2[0] as f32 / 1000.0,
                    key.2[1] as f32 / 1000.0,
                    key.2[2] as f32 / 1000.0,
                );
                let m = Mat4::from_scale_rotation_translation(s, rot, Vec3::ZERO);
                let mut verts: Vec<[f32; 3]> = Vec::new();
                let mut indices: Vec<u32> = Vec::new();
                for part in &model.parts {
                    let base = verts.len() as u32;
                    verts.extend(
                        part.mesh
                            .vertices
                            .iter()
                            .map(|v| m.transform_point3(Vec3::from(v.pos)).to_array()),
                    );
                    indices.extend(part.mesh.indices.iter().map(|i| i + base));
                }
                // 128 voxels along the longest axis: a whole-map bake lands well
                // under a second and keeps doorways/rooms resolvable (the user's
                // ~80-unit map → ~0.6-unit voxels).
                let baked =
                    std::sync::Arc::new(floptle_field::bake_occluder(&verts, &indices, 128));
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!(
                        "baked shadow occluder for {} ({} tris → {}×{}×{} voxels, {} ms)",
                        key.0,
                        indices.len() / 3,
                        baked.dims[0],
                        baked.dims[1],
                        baked.dims[2],
                        started.elapsed().as_millis()
                    ),
                    None,
                );
                self.occluder_cache.insert(key.clone(), baked.clone());
                baked
            };
            next.insert(e, (key, baked));
        }
        // Drop cache entries nothing references anymore (a resized/removed map).
        self.occluder_cache.retain(|k, _| next.values().any(|(nk, _)| nk == k));
        self.mesh_occluders = next;
        true
    }

    // ---- terrain sculpting --------------------------------------------------
    /// Once per frame (with the Sculpt tool): cast the cursor ray at the terrain,
    /// build the brush telegraph (ring + normal), and — if a stroke is queued —
    /// apply the brush. Editing is throttled here to one stroke per frame so a fast
    /// drag doesn't stall on the per-voxel work + GPU re-upload.
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

        // Each field is in its node's LOCAL frame (translation + rotation +
        // uniform scale) — transform the cursor ray into each and brush the one
        // whose surface it hits NEAREST the camera. `hit` stays LOCAL; world
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
            // Brush radius is WORLD units; the field works in LOCAL units.
            let r_local = brush.radius / tscale;
            let id = match self.world.get::<Matter>(active) {
                Some(Matter::Terrain { id }) => *id,
                _ => 0,
            };
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
            // Apply the brush to the AUTHORITY field. No growth step: the sparse field
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
                m => terrain.field.sculpt(m, hit, r_local, brush.strength, brush.profile),
            };
            if !touched.is_empty() {
                self.stroke_dabbed = true; // mark this stroke as worth an undo step
                // Shadow-proxy refresh + atlas partial upload + chunk remesh queue.
                // (A dab outside the proxy's box clamps — the proxy is re-derived at
                // stroke end when bounds outgrow it; see `end_sculpt_stroke`.)
                let geom = !is_paint; // sculpt changes geometry (resync wireframe + collider)
                // Sculpting WHILE PLAYING must reach the sim's collider copy too —
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
    /// colliders and BEFORE stepping physics, so a dig affects the same tick.
    pub(crate) fn drain_script_terrain_ops(&mut self) {
        let ops = self.script_host.take_terrain_ops();
        if ops.is_empty() {
            return;
        }
        // The ground under a scatter chunk just moved, so the props standing on
        // it are at the old height. Forget those chunks and they re-settle onto
        // the new surface — SC3's "digging the ground out from under one drops
        // or despawns it", for free, because placement was never remembered in
        // the first place. Collected BEFORE the edits, since the sources list
        // is borrowed from the script host and the edits do not change it.
        let dirty: Vec<(DVec3, f64)> = {
            let sources = self.script_host.scatter_sources();
            if sources.is_empty() {
                Vec::new()
            } else {
                ops.iter()
                    .filter(|o| o.affects_geometry())
                    .map(|o| (DVec3::new(o.pos[0], o.pos[1], o.pos[2]), o.radius as f64))
                    .collect()
            }
        };
        for op in ops {
            self.apply_terrain_op(&op);
        }
        if !dirty.is_empty() {
            let sources: Vec<floptle_core::scatter::ScatterSource> =
                self.script_host.scatter_sources().clone();
            for (p, r) in dirty {
                self.scatter_cache.invalidate_near(&sources, p, r);
            }
        }
    }

    /// Apply one script terrain op (world coords) to the nearest terrain: the
    /// authority field, the sim's collider copy (geometry ops), the chunk remesh
    /// queue, and the shadow-proxy region — the same pipeline as an editor brush dab.
    /// Play-mode only state: Stop restores the pre-Play fields (`play_terrains`), so
    /// script edits never leak into the authored scene.
    fn apply_terrain_op(&mut self, op: &floptle_script::TerrainOp) {
        use floptle_field::{Brush, BrushProfile};
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
        let Some((e, local, d, ts)) = best else { return };
        // Too far from every surface: a mis-aimed op must not edit a random field.
        if d > op.radius + self.terrains[&e].field.band() * 2.0 * ts {
            return;
        }
        let r_local = op.radius / ts;
        let profile = BrushProfile::default();
        let t = self.terrains.get_mut(&e).unwrap();
        let mut measured = None;
        let touched = match op.mode {
            M::Raise | M::Lower | M::Smooth | M::Flatten => {
                let brush = match op.mode {
                    M::Raise => Brush::Raise,
                    M::Lower => Brush::Lower,
                    M::Smooth => Brush::Smooth,
                    _ => Brush::Flatten,
                };
                let (touched, y) =
                    t.field.sculpt_measured(brush, local, r_local, op.strength, profile);
                measured = Some(y);
                touched
            }
            M::Paint(c) => t.field.paint(local, r_local, op.strength, c, profile),
            M::PaintTexture(slot) => t.field.paint_texture(local, r_local, slot),
        };
        // Report BEFORE the empty-touch bail: a dab that moved nothing has to
        // report zero rather than nothing, or a game cannot tell "I dug air"
        // from "the report is still coming" (floptle/0037). Volumes are measured
        // in the field's local units, so a scaled terrain converts by scale³.
        if let Some(y) = measured
            && op.id != 0
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
        // Mirror geometry edits into the sim's collider copy so collision agrees
        // with the drawn surface THIS tick (color never affects collision).
        if geom {
            self.mirror_terrain_chunks_to_sim(e, &touched);
        }
        self.queue_terrain_dirty(e, local, r_local, geom, touched);
    }

    /// Make the play sim's collider copy of terrain `e` agree with the authority
    /// field over `touched` chunks — by CLONING those chunks (plus the one-chunk
    /// renormalize ring writes spill into), not by re-running the edit. A re-run
    /// can drift; a copy cannot, and a player standing on a stale invisible
    /// surface is exactly what drift looks like. Call after EVERY authority
    /// geometry write while a sim exists: script ops, the editor brush during
    /// Play, fills, undo. Loudly warns (once per Play) if the sim has no
    /// matching terrain collider — a silent no-op here is unfindable later.
    pub(crate) fn mirror_terrain_chunks_to_sim(&mut self, e: Entity, touched: &[[i32; 3]]) {
        if touched.is_empty() || self.sim.is_none() {
            return;
        }
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
        let Some(t) = self.terrains.get(&e) else { return };
        match self.sim.as_mut().and_then(|s| s.terrain_field_mut(e.index())) {
            Some(f) => f.copy_chunks_from(&t.field, &region),
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
        self.terrain_region_dirty = Some(match self.terrain_region_dirty {
            Some((se, omn, omx, og)) if se == e => (
                e,
                [omn[0].min(mn[0]), omn[1].min(mn[1]), omn[2].min(mn[2])],
                [omx[0].max(mx[0]), omx[1].max(mx[1]), omx[2].max(mx[2])],
                og || geom,
            ),
            _ => (e, mn, mx, geom),
        });
        self.terrain_chunks_dirty.entry(e).or_default().extend(touched);
        self.touch_terrain_edit(e); // an eviction must save this field first
    }

    /// Record a field edit: dirty-for-disk + a fresh edit stamp. EVERY path that
    /// changes a field's voxels must come through here (brush/script dabs, undo
    /// swaps, generation adopts) — the stamp is how a background checkpoint
    /// knows its snapshot raced an edit, and how the picker finds quiet fields.
    pub(crate) fn touch_terrain_edit(&mut self, e: Entity) {
        self.terrain_disk_dirty.insert(e);
        self.terrain_edit_clock += 1;
        self.terrain_edit_stamps.insert(e, (self.terrain_edit_clock, std::time::Instant::now()));
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

    /// Create a fresh flat terrain as a NEW scene node (you can have any number). It
    /// is placed at the cursor's ground point; its field is in the node's local space.
    /// `cfg` (from the "New terrain" dialog) sizes the STARTING slab and paints it with
    /// a color/texture up front — the sparse field is unbounded, so this is a seed to
    /// sculpt out from, not a boundary (the slab occupies `-thickness..0` in local Y,
    /// surface at the node's height).
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
        // The dialog's dropdown hands over an asset-TREE path (spelled however the
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
        self.world.insert(e, Matter::Terrain { id });
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
        if let Some(i) = self.terrain_textures.iter().position(|p| p == path) {
            return Some(i as u8);
        }
        let i = self.terrain_textures.iter().position(|p| p.is_empty())?;
        self.terrain_textures[i] = path.to_string();
        self.terrain_textures_dirty = true;
        Some(i as u8)
    }

    /// Where a terrain node's field is stored — one `.cfield` per terrain id, per
    /// scene (the Terrain 2.0 sparse format).
    pub(crate) fn terrain_field_path_id(&self, id: u32) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.{id}.cfield", self.scene_name))
    }

    /// Stems of `.cfield` files carrying THIS terrain id under a DIFFERENT scene
    /// name. Every per-scene file is keyed by the scene's stem, so a rename of
    /// the `.ron` alone leaves the real data sitting here under the old name —
    /// which is the difference between "your terrain is gone" and "your terrain
    /// is one rename away".
    pub(crate) fn orphaned_field_stems(&self, id: u32) -> Vec<String> {
        let suffix = format!(".{id}.cfield");
        let Ok(entries) = std::fs::read_dir(self.project_root.join("terrain")) else {
            return Vec::new();
        };
        let mut out: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let stem = name.strip_suffix(&suffix)?;
                (stem != self.scene_name).then(|| stem.to_string())
            })
            .collect();
        out.sort();
        out
    }

    /// The legacy DENSE field path for the same terrain — read-only migration source.
    pub(crate) fn terrain_tfield_path_id(&self, id: u32) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.{id}.tfield", self.scene_name))
    }

    /// The tiny residency sidecar next to a terrain's `.cfield`: the impostor
    /// color ("r g b", linear floats), so a COLD body can draw its sphere
    /// without ever touching the multi-MB field.
    pub(crate) fn terrain_meta_path_id(&self, id: u32) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.{id}.meta", self.scene_name))
    }

    /// Meta v2: line 1 = "r g b" (impostor color), line 2 = the genspec hash the
    /// field file was written under (absent for bodies with no genspec).
    pub(crate) fn write_terrain_meta(&self, id: u32, color: [f32; 3], spec_hash: Option<u64>) {
        let p = self.terrain_meta_path_id(id);
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut text = format!("{} {} {}", color[0], color[1], color[2]);
        if let Some(h) = spec_hash {
            text.push_str(&format!("\n{h}"));
        }
        let _ = std::fs::write(p, text);
    }

    fn read_terrain_meta(&self, id: u32) -> Option<[f32; 3]> {
        let text = std::fs::read_to_string(self.terrain_meta_path_id(id)).ok()?;
        let mut it = text.split_whitespace().map(|s| s.parse::<f32>());
        match (it.next(), it.next(), it.next()) {
            (Some(Ok(r)), Some(Ok(g)), Some(Ok(b))) => Some([r, g, b]),
            _ => None,
        }
    }

    /// The genspec hash the field file was written under (meta line 2, if any).
    fn read_terrain_meta_hash(&self, id: u32) -> Option<u64> {
        let text = std::fs::read_to_string(self.terrain_meta_path_id(id)).ok()?;
        text.split_whitespace().nth(3)?.parse().ok()
    }

    /// The genspec-hash stamp for a node's current meta write: hash of its
    /// genspec, or None for a purely authored body.
    pub(crate) fn terrain_spec_hash_of(&self, e: Entity) -> Option<u64> {
        self.world.get::<floptle_core::TerrainGen>(e).map(|g| genspec_hash(&g.0))
    }

    // ---- background checkpoints (terrain.flush) -------------------------------

    /// Per-frame driver for BACKGROUND checkpoints, one job at a time: absorb
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
                let now = std::time::Instant::now();
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
                    // (an edit AFTER this line re-dirties via its new stamp).
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
                    std::thread::spawn(move || {
                        let res = (|| {
                            if let Some(dir) = path.parent() {
                                std::fs::create_dir_all(dir)
                                    .map_err(|e| format!("create {dir:?}: {e}"))?;
                            }
                            std::fs::write(&path, &bytes)
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

        // 3. Start the next job — QUIET fields only (edited ≥1.5s ago), forced
        //    past the age cap so non-stop digging can't starve checkpoints.
        if self.terrain_save_job.is_some() || self.terrain_flush_queue.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
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
        let Some(sd) = self.script_host.terrain_save_dir() else { return };
        let Some(Matter::Terrain { id }) = self.world.get::<Matter>(e).cloned() else { return };
        let name = self
            .world
            .get::<floptle_core::Name>(e)
            .map(|n| n.0.clone())
            .unwrap_or_else(|| format!("terrain {id}"));
        let path =
            self.project_root.join(&sd).join(format!("{}.{id}.cfield", self.scene_name));
        let stamp = self.terrain_edit_stamps.get(&e).map(|(s, _)| *s).unwrap_or(0);
        self.terrain_save_job = Some(TerrainSaveJob {
            e,
            name,
            path,
            stamp,
            state: TerrainSaveState::Encoding(t.field.begin_save()),
        });
    }

    /// Land or cancel the in-flight background checkpoint before a SYNCHRONOUS
    /// writer (Stop, eviction, scene switch) touches the same files: an encode
    /// just drops (the sync path writes fresher data anyway), an in-flight
    /// write JOINS — an older blob must never land after a newer sync write.
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

    /// Synchronously write EVERY dirty resident field to the save slot — the
    /// exit-path guarantee (Stop, scene switch out of a slot): whatever the
    /// background pipeline was mid-way through, the player's edits are on disk
    /// when this returns. No-op without a slot.
    pub(crate) fn flush_slot_terrains_sync(&mut self) {
        let Some(sd) = self.script_host.terrain_save_dir() else { return };
        self.settle_terrain_checkpoint();
        self.terrain_flush_queue.clear();
        let _ = self.script_host.take_terrain_flush(); // absorbed: all writes now
        let dirty: Vec<(Entity, u32)> = self
            .terrains
            .keys()
            .filter(|e| self.terrain_disk_dirty.contains(e))
            .filter_map(|&e| match self.world.get::<Matter>(e) {
                Some(Matter::Terrain { id }) => Some((e, *id)),
                _ => None,
            })
            .collect();
        let mut wrote = 0usize;
        for (e, id) in dirty {
            let path =
                self.project_root.join(&sd).join(format!("{}.{id}.cfield", self.scene_name));
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Some(t) = self.terrains.get(&e)
                && std::fs::write(&path, t.field.to_bytes()).is_ok()
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
    /// Play-start streaming hold ([`begin_play_terrain_hold`] auto-PAUSES until
    /// the ground exists). A paused session runs no fixed tick, so no rails, no
    /// physics and `dt == 0` — the run stepped its full span, reported the full
    /// span of simulated time, and had simulated **none** of it. Every script's
    /// `time` sat at zero and `space.bodies()` was empty, which reads as a game
    /// whose world was never built rather than as a runner that never started.
    ///
    /// The ORDER is the frame's order and matters: generates before residency,
    /// or residency adopts a freshly created body as cold and streams a stale
    /// same-id field into it. Keep the two in step.
    ///
    /// No GPU: this is field/RAM bookkeeping. Meshing and upload are the frame's
    /// half and stay there.
    pub(crate) fn pump_world_streaming(&mut self) {
        let anchor = if self.playing {
            floptle_core::active_camera(&self.world)
                .map(|e| floptle_core::world_transform(&self.world, e).translation)
                .unwrap_or(self.camera.position)
        } else {
            self.camera.position
        };
        self.drain_terrain_generates();
        self.poll_terrain_generates();
        self.update_terrain_residency(anchor);
        self.step_terrain_checkpoint();
        self.publish_terrain_busy();
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
    }

    /// Does the background terrain worker have anything to do — a whole-body
    /// fill running or queued, or a field streaming in?
    ///
    /// ONE predicate, because two callers must not disagree about it:
    /// `terrain.busy()` answers a game with it, and `settle_world_streaming`
    /// waits on it. They did disagree — the wait watched only the streaming
    /// half, so `shot` would photograph a planet that was still GENERATING as
    /// its impostor sphere and report that everything had settled. That is the
    /// exact failure `settle_world_streaming` exists to prevent, arriving
    /// through the door it left open.
    pub(crate) fn terrain_worker_busy(&self) -> bool {
        self.planet_gen_job.is_some()
            || !self.planet_gen_pending.is_empty()
            || !self.terrain_load_jobs.is_empty()
    }

    /// Stream the world in and WAIT for it, for the one-shot verbs that have no
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
        // the view being photographed IS the presence in the world.
        self.camera.position = anchor;
        let deadline = std::time::Instant::now() + budget;
        let mut quiet = false;
        loop {
            self.pump_world_streaming();
            if !self.terrain_worker_busy() {
                if quiet {
                    return true;
                }
                // The confirming pass, taken IMMEDIATELY and without spending
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
            if std::time::Instant::now() >= deadline {
                return false;
            }
            // The work is on other threads; this one only has to let them finish.
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    }

    /// Per-frame residency driver: land finished background loads, kick loads
    /// for cold bodies something is approaching, evict residents left behind.
    ///
    /// RESIDENCY IS GAMEPLAY-BASED, NOT CAMERA-BASED. During Play the anchors
    /// are the world positions of every dynamic body (ship, astronaut, debris)
    /// plus any bodies the game explicitly warmed (`terrain.warm` — the map's
    /// focused planet); the camera doesn't count — opening the map and zooming
    /// across the system must NEVER unload the planet you're standing on (that
    /// evicted the ground under Ty's feet and dropped him through the world).
    /// In edit mode the editor camera IS your presence, so it anchors there.
    /// The IMPOSTOR flip stays camera-based — that's visual LOD (screen size),
    /// a different question from which fields are in RAM.
    ///
    /// Runs OUTSIDE the render borrows (it may rebuild the sim on a mid-Play
    /// arrival/eviction) — called right before `sync_terrain_meshes` each frame.
    pub(crate) fn update_terrain_residency(&mut self, cam_world: DVec3) {
        // Gameplay anchors + this frame's warm requests (immediate mode — the
        // map re-warms its focus every frame while open).
        let anchors: Vec<DVec3> = if self.playing {
            self.world
                .query::<floptle_core::RigidBody>()
                .map(|(e, _)| floptle_core::world_transform(&self.world, e).translation)
                .collect()
        } else {
            vec![cam_world]
        };
        if anchors.is_empty() {
            return; // a playing scene with no dynamic bodies: leave residency as-is
        }
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

        // 0. Adopt terrain bodies born at RUNTIME: a game's loading screen builds
        //    its galaxy with createNode + setTerrainGen, and those nodes went
        //    through no scene load — they were in NEITHER the resident nor the
        //    cold set, so nothing drew or streamed them (planets that were only
        //    their atmosphere, warm/TAB focuses that never landed). Anything
        //    untracked becomes COLD exactly like adopt_terrain would make it —
        //    unless its FIRST generation is queued (the generation queue owns
        //    those until the fill lands).
        let untracked: Vec<(Entity, u32)> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Terrain { id } => Some((e, *id)),
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
            let has_file = self.terrain_field_path_id(id).exists();
            let color = (has_file.then(|| self.read_terrain_meta(id)).flatten())
                .or_else(|| {
                    self.world
                        .get::<floptle_core::TerrainGen>(e)
                        .map(|g| genspec_impostor_color(&g.0).unwrap_or([0.62, 0.62, 0.68]))
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
        //    the thread). Failures are LOUD and final: a body whose stream died
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
        //    is), plus a blocking emergency load if a body is practically ON one.
        let mut to_sync: Vec<(Entity, u32)> = Vec::new();
        let mut to_load: Vec<(Entity, u32)> = Vec::new();
        for (&e, cold) in &self.terrain_cold {
            let Some(cb) = self.world.get::<floptle_core::CelestialBody>(e) else { continue };
            // Heal the impostor render entry — it IS the body's visual while cold
            // (covers any path that dropped it, e.g. a scene-switch edge).
            let render = self.terrain_render.entry(e).or_default();
            if !render.impostor {
                render.impostor = true;
                render.impostor_color = Some(cold.color);
            }
            let r = cb.body_radius.max(1.0);
            let p = floptle_core::world_transform(&self.world, e).translation;
            // The blocking emergency load is for mid-play surprises (teleports,
            // summons) — during the Play-start HOLD the same bodies stream in
            // the background instead (the run is paused; nothing can fall).
            if !self.play_stream_hold && near(p, r * RESIDENT_SYNC_RADII) {
                to_sync.push((e, cold.id));
            } else if warm.contains(&e) || near(p, r * RESIDENT_LOAD_RADII) {
                to_load.push((e, cold.id));
            }
        }
        for (e, id) in to_sync {
            self.load_terrain_blocking(e, id);
        }
        for (e, id) in to_load {
            self.kick_terrain_load(e, id);
        }

        // 3. Evict residents EVERY anchor has left far behind (celestials only —
        //    flat level terrains have no meaningful radius and stay resident).
        //    Warmed bodies are exempt however far away they are.
        let mut to_evict: Vec<(Entity, u32)> = Vec::new();
        for &e in self.terrains.keys() {
            if warm.contains(&e) {
                continue;
            }
            let Some(cb) = self.world.get::<floptle_core::CelestialBody>(e) else { continue };
            let Some(Matter::Terrain { id }) = self.world.get::<Matter>(e) else { continue };
            let r = cb.body_radius.max(1.0);
            let p = floptle_core::world_transform(&self.world, e).translation;
            if !near(p, r * RESIDENT_EVICT_RADII) {
                to_evict.push((e, *id));
            }
        }
        for (e, id) in to_evict {
            self.evict_terrain_to_cold(e, id);
        }

        // 4. Release the Play-start streaming hold once nothing REQUIRED (a cold
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

    /// The ORDERED candidate sources for a cold terrain (G2) — the loader tries
    /// them in sequence, so a corrupt file degrades to regeneration instead of
    /// taking the world offline:
    ///   1. the game's save-slot file (player-edited state; trusted as-is —
    ///      a slot belongs to one galaxy by the game's own contract),
    ///   2. the project file (authored/cached) — but ONLY if it was written
    ///      under the node's CURRENT genspec (meta hash line): regeneration
    ///      reuses terrain ids, and a stale file from the previous system must
    ///      not load as the new body's terrain,
    ///   3. the genspec itself (deterministic on-demand generation).
    ///
    /// Empty = nothing to load from (stays an impostor).
    fn resolve_terrain_source(&self, e: Entity, id: u32) -> Vec<TerrainSource> {
        let mut out = Vec::new();
        let genspec = self.world.get::<floptle_core::TerrainGen>(e).map(|g| g.0.clone());
        if let Some(sd) = self.script_host.terrain_save_dir() {
            let p = self
                .project_root
                .join(&sd)
                .join(format!("{}.{id}.cfield", self.scene_name));
            if p.exists() {
                out.push(TerrainSource::File(p));
            }
        }
        let p = self.terrain_field_path_id(id);
        if p.exists() {
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
        self.script_host.terrain_save_dir().map(|sd| {
            self.project_root.join(sd).join(format!("{}.{id}.cfield", self.scene_name))
        })
    }

    /// Spawn a background load/generate job for a cold terrain (capped at 2 in
    /// flight; duplicates are no-ops). Reads + parses a file, or generates the
    /// whole planet from its genspec — either way the shadow proxy derives on
    /// the thread too, so the main thread never hitches (Ty's no-stutter rule).
    pub(crate) fn kick_terrain_load(&mut self, e: Entity, id: u32) {
        if self.terrain_load_jobs.iter().any(|j| j.e == e)
            || self.terrain_load_jobs.len() >= 2
        {
            return;
        }
        let mut sources = self.resolve_terrain_source(e, id);
        // A running `terrain.generatePlanet` batch OWNS generation: while the
        // game's spawn planet (or any explicit fill) is being built, residency
        // must not start MORE generations behind it — the player is standing
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
        std::thread::spawn(move || {
            let _ = tx.send(load_terrain_from(sources));
        });
        self.terrain_load_jobs.push(TerrainLoadJob {
            e,
            name,
            started: std::time::Instant::now(),
            rx,
        });
    }

    /// Blocking load — the mid-play emergency inside `RESIDENT_SYNC_RADII`
    /// (teleports, summons). Only a FILE loads synchronously; a body whose only
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
            Some(TerrainSource::File(_)) => {
                // Synchronously try the FILE candidates only — a corrupt file
                // must fall back to the background chain (which ends in the
                // genspec), never to an in-frame generation.
                let files: Vec<TerrainSource> = sources
                    .into_iter()
                    .filter(|s| matches!(s, TerrainSource::File(_)))
                    .collect();
                match load_terrain_from(files) {
                    Some(t) => {
                        self.terrain_load_jobs.retain(|j| j.e != e); // job now stale
                        self.finish_terrain_load(e, t);
                        true
                    }
                    None => {
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
        self.terrain_cold.remove(&e);
        self.terrains.insert(e, t);
        self.terrain_gpu_dirty = true;
        // The render entry stays in impostor mode; the streaming loop flips it by
        // distance (still beyond 60 r at load time, so nothing pops).
        if self.playing {
            self.play_loaded_terrains.insert(e);
            self.rebuild_sim();
            self.console.push(
                floptle_script::LogLevel::Debug,
                "Δ terrain streamed in — collision live".into(),
                None,
            );
        }
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
                impostor_surface_color(&t.field, r)
            });
        // Edited fields save before dropping. Destination (G2): the game's
        // save-slot dir when set (player state — writable even during Play,
        // that's the whole point of a save slot), else the project file (edit-
        // mode authoring). Playing with NO save slot = drop without saving
        // (Stop reverts terrain anyway — today's Play semantics).
        if self.terrain_disk_dirty.contains(&e) {
            let dest = self
                .terrain_save_slot_path(id)
                .or_else(|| (!self.playing).then(|| self.terrain_field_path_id(id)));
            if let Some(path) = dest {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if std::fs::write(&path, t.field.to_bytes()).is_err() {
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
        self.play_loaded_terrains.remove(&e);
        let render = self.terrain_render.entry(e).or_default();
        render.impostor = true;
        render.impostor_color = Some(color);
        self.terrain_cold.insert(e, ColdTerrain { id, color });
        self.terrain_gpu_dirty = true; // shadow atlas re-lays out without it
        // Mid-Play the sim still holds this body's collider, and with the field
        // gone nothing re-anchors it — a FROZEN collider drifting away from its
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
        // A checkpoint mid-flight for the OLD scene must land before its
        // entities are forgotten (entity ids recycle across scene loads).
        self.settle_terrain_checkpoint();
        self.terrain_flush_queue.clear();
        self.terrain_edit_stamps.clear();
        self.terrains.clear();
        self.terrain_cold.clear();
        self.terrain_disk_dirty.clear();
        self.terrain_load_jobs.clear();
        self.play_loaded_terrains.clear();
        self.active_terrain = None;
        self.terrain_slots.clear();
        let nodes: Vec<(Entity, u32)> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Terrain { id } => Some((e, *id)),
                _ => None,
            })
            .collect();
        let mut max_id = 0u32;
        let mut missing: Vec<(u32, String)> = Vec::new();
        let single = nodes.len() == 1;
        for (e, id) in nodes {
            max_id = max_id.max(id);
            // G1/G2 RESIDENCY: a celestial body starts COLD — no field read, no
            // generation — whenever its impostor color is knowable up front:
            // from the meta sidecar (a field file exists), or from its genspec's
            // surface palette (the galaxy path — the body has NO file anywhere
            // and generates on first approach). The per-frame residency driver
            // streams in whatever the camera is actually near, so scene open
            // gets FASTER as systems get bigger. Bodies with a file but no meta
            // yet (pre-G1 scenes) load eagerly below, cache their color, and go
            // cold from then on. Non-celestial terrains are always resident.
            if self.world.get::<floptle_core::CelestialBody>(e).is_some() {
                let color = if self.terrain_field_path_id(id).exists() {
                    self.read_terrain_meta(id)
                } else {
                    // A genspec body is ALWAYS cold (falling through to the eager
                    // path would give it a flat starter slab — it has no file to
                    // load); a garbled spec just gets a neutral sphere color.
                    self.world.get::<floptle_core::TerrainGen>(e).map(|g| {
                        genspec_impostor_color(&g.0).unwrap_or([0.62, 0.62, 0.68])
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
                std::fs::read(self.terrain_tfield_path_id(id))
                    .ok()
                    .and_then(|b| floptle_field::Terrain::from_bytes(&b))
                    // legacy single-terrain scenes stored one `<scene>.tfield`.
                    .or_else(|| {
                        if single {
                            std::fs::read(self.legacy_terrain_field_path())
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
            let loaded = std::fs::read(self.terrain_field_path_id(id))
                .ok()
                .and_then(|b| floptle_field::ChunkField::from_bytes(&b))
                .or_else(dense_migration);
            // A terrain node in a SAVED scene that has no field on disk is the
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
                && self.terrain_field_path_id(id).exists()
            {
                let color = impostor_surface_color(&field, cb.body_radius as f32);
                let hash = self.terrain_spec_hash_of(e);
                self.write_terrain_meta(id, color, hash);
            }
            self.terrains.insert(e, EditorTerrain::new(field));
        }
        self.next_terrain_id = max_id + 1;
        for (id, name) in missing {
            let want = self.terrain_field_path_id(id);
            let rel = want
                .strip_prefix(&self.project_root)
                .unwrap_or(&want)
                .to_string_lossy()
                .replace('\\', "/");
            // A field for this terrain id filed under ANOTHER scene's name is the
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
        // (COLD terrains count — their fields still splat this palette on load.)
        if (!self.terrains.is_empty() || !self.terrain_cold.is_empty())
            && let Ok(text) = std::fs::read_to_string(self.terrain_palette_path()) {
                let slots = floptle_render::TERRAIN_SLOTS as usize;
                let mut glow = 0u32;
                let mut palette: Vec<String> = text
                    .lines()
                    .enumerate()
                    .map(|(i, s)| match s.strip_suffix("|glow") {
                        Some(path) => {
                            glow |= 1 << i.min(31);
                            path.to_string()
                        }
                        None => s.to_string(),
                    })
                    .collect();
                palette.resize(slots, String::new());
                self.terrain_textures = palette;
                self.terrain_glow_mask = glow;
                self.terrain_textures_dirty = true;
            }
    }

    /// The world translation of a terrain node (places its field in world space).
    /// A terrain node's world placement: translation + rotation + UNIFORM scale
    /// (x drives — an SDF can't stretch per-axis without breaking the distance
    /// metric). The one frame every terrain consumer (render, physics, brush,
    /// script ops, LOD) converts through, so they can never disagree.
    pub(crate) fn terrain_world_frame_of(&self, e: Entity) -> (DVec3, Quat, f32) {
        let wt = floptle_core::world_transform(&self.world, e);
        (wt.translation, wt.rotation.normalize(), wt.scale.x.max(1e-6))
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
    /// composed anchor (node f64 translation) + local center FIRST, then
    /// camera-relative — exact at any world distance (ADR-0015). Each volume samples
    /// its own atlas slot at native resolution; overlapping volumes fuse on the GPU
    /// with the same smin the old CPU combine used (k = 0.6).
    /// (Associated fn taking explicit fields — callers sit inside the render section
    /// where `self.gpu`/`self.egui` are mutably borrowed, so `&self` is unavailable.)
    pub(crate) fn fill_terrain_volumes(
        terrains: &HashMap<Entity, EditorTerrain>,
        slots: &[Entity],
        occluders: &HashMap<Entity, (OccKey, std::sync::Arc<floptle_field::BakedSdf>)>,
        occ_slots: &[Entity],
        world: &floptle_core::World,
        g: &mut RaymarchGlobals,
        cam_world: DVec3,
    ) {
        g.params[2] = 0.1; // blob↔terrain blend k (the old single-field look)
        for (i, &e) in slots.iter().take(floptle_render::MAX_VOLUMES).enumerate() {
            // A just-deleted terrain leaves a stale slot for one frame — leave it
            // absent (w = 0); the dirty flag re-uploads the set next frame.
            let Some(t) = terrains.get(&e) else { continue };
            let wt = floptle_core::world_transform(world, e);
            // The shadow/AO volume shader samples an AXIS-ALIGNED, unit-scale box:
            // a rotated or scaled terrain would cast its UNROTATED shadow, which
            // reads as broken. Skip its volume instead (meshes still render + AO
            // via SSAO; revisit with per-volume rotation in the field shader).
            let placed = wt.rotation.normalize().w.abs() < 0.99999
                || (wt.scale.x - 1.0).abs() > 1e-3;
            if placed {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!(
                        "[terrain] a rotated/scaled terrain skips SDF sun-shadow/AO \
                         casting (v1 limitation — rendering & collision are exact)"
                    );
                }
                continue;
            }
            let anchor = wt.translation;
            let bc = t.shadow.center;
            let hf = t.shadow.half_extent;
            let cr = anchor + DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - cam_world;
            // w = 3: shadow + AO, NOT drawn. Terrain 2.0 draws the extracted chunk meshes
            // through the raster pass (`push_terrain_instances`); the raymarch stops
            // sphere-tracing terrain but its field keeps casting sun shadows and darkening
            // props that stand on it (that is what `w = 3` means, vs `w = 2` which would
            // drop terrain out of the AO field — trap T2).
            g.vol_center[i] = [cr.x as f32, cr.y as f32, cr.z as f32, 3.0];
            g.vol_half[i] = [hf[0], hf[1], hf[2], 0.6];
        }
        // Mesh shadow occluders ride the slots AFTER the terrains, flagged
        // shadow-only (w = 2): the shadow march folds them in, the drawn field
        // skips them. Per-node "casts shadows" / visibility opt-outs simply leave
        // the slot absent this frame — no re-upload needed to toggle.
        for (j, &e) in occ_slots.iter().enumerate() {
            let i = slots.len() + j;
            if i >= floptle_render::MAX_VOLUMES {
                break;
            }
            let Some((_, b)) = occluders.get(&e) else { continue };
            let casts = world.get::<floptle_core::CastShadow>(e).map(|c| c.0).unwrap_or(true)
                && !matches!(
                    world.get::<floptle_core::Visible>(e),
                    Some(floptle_core::Visible(false))
                );
            if !casts {
                continue;
            }
            let anchor = floptle_core::world_transform(world, e).translation;
            let bc = b.center;
            let hf = b.half_extent;
            let cr = anchor + DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - cam_world;
            g.vol_center[i] = [cr.x as f32, cr.y as f32, cr.z as f32, 2.0];
            g.vol_half[i] = [hf[0], hf[1], hf[2], 0.0];
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
mod tests {
    use super::terrain_voxel_size;
    use super::{CHUNK_FADE_SECS, chunk_fade, chunk_priority, lod_for, raw_lod, rings_for_body, LOD_RINGS};
    use floptle_core::math::{DVec3, Quat};
    use floptle_field::BakedSdf;

    /// The wait and the flag are the same question (`floptle/0157` + `0158`).
    ///
    /// `terrain.busy()` answers a game; `settle_world_streaming` waits before a
    /// shot. When the wait watched only the streaming half, a planet that was
    /// still GENERATING settled instantly and was photographed as its impostor
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
    /// the budget it was offered (`floptle/0157`).
    ///
    /// `shot` calls this before it takes the picture and prints a warning about
    /// photographing impostors when it comes back false. A scene with no terrain
    /// in it must therefore come back TRUE, promptly — a warning that fires on
    /// every shot of every 2D project is a warning nobody reads by the time a
    /// planet really is unfinished.
    #[test]
    fn a_world_with_nothing_to_stream_settles_at_once() {
        let mut ed = crate::Editor::default();
        let t0 = std::time::Instant::now();
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
    /// (`floptle/0067`) — and a chunk with no arrival stamp is simply opaque, which
    /// The reported bug, in numbers: *"I can see through unloaded terrain and it
    /// needs to prioritize loading what's right under me"* (`floptle/0074`).
    ///
    /// One queue serves every terrain. Under the old key — chunk distance in
    /// each terrain's OWN local frame — the ground under your feet and a chunk
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

        // A body the camera is NOT on is penalised as a whole, so no chunk of it
        // can slip ahead of the ground being stood on.
        let off = chunk_priority([0, 0, 0], 48.0, DVec3::ZERO, Quat::IDENTITY, cam, false);
        assert!(off > at([200, 0, 0]), "an off-body chunk beat a distant on-body one");
    }

    /// A body on the far side of a solar system must not WRAP into the front of
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

    /// The LOD rings hold their chunk at the boundary (±1 hysteresis) so a camera
    /// drifting across a ring edge can't flip a chunk's stride every frame.
    #[test]
    fn lod_rings_have_hysteresis() {
        let r = LOD_RINGS;
        let b = r[0]; // the lod0/lod1 boundary
        // Fresh chunks take the raw ring.
        assert_eq!(raw_lod(b, r), 0);
        assert_eq!(raw_lod(b + 1, r), 1);
        // A lod0 chunk exactly at the boundary +1 stays lod0…
        assert_eq!(lod_for(b + 1, 0, r), 0);
        // …and coarsens once clearly past it.
        assert_eq!(lod_for(b + 2, 0, r), 1);
        // A lod1 chunk at the boundary stays lod1…
        assert_eq!(lod_for(b, 1, r), 1);
        // …and refines once clearly inside.
        assert_eq!(lod_for(b - 1, 1, r), 0);
        // No-change fast path.
        assert_eq!(lod_for(2, 0, r), 0);
        assert_eq!(lod_for(100, 3, r), 3);
        // Far chunks are lod3 regardless of history.
        assert_eq!(lod_for(LOD_RINGS[2] + 2, 0, r), 3);
    }

    /// Surface chunks of a sphere of `radius`, and how many of them the rings
    /// would queue at FULL detail from a camera standing on it.
    ///
    /// The count is what `floptle/0067` asks for: on a walkable planet the whole
    /// body used to sit inside ring 0, so arriving meant surface-net meshing all
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
                    let d = (x - cam[0]).abs().max((y - cam[1]).abs()).max((z - cam[2]).abs());
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
        let (surface, before) = lod0_chunks(radius, chunk, LOD_RINGS);
        let (_, after) = lod0_chunks(radius, chunk, tight);

        assert!(
            before * 2 > surface,
            "more than HALF the body's {surface} surface chunks used to be queued at full \
             detail from one standing position ({before})"
        );
        assert!(
            after * 8 < before,
            "standing on a {radius}-unit world: {before} full-detail chunks became {after}"
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
        assert_eq!(rings_for_body(Some(100_000.0), 48.0), LOD_RINGS);
        assert_eq!(rings_for_body(None, 48.0), LOD_RINGS, "ordinary terrain has no body");
        // A tiny moon still gets a full-detail ring under your feet and two
        // coarser ones around it, rather than collapsing to nothing.
        let tiny = rings_for_body(Some(20.0), 48.0);
        assert_eq!(tiny, [1, 2, 3], "ordered, and never empty: {tiny:?}");
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

    /// The shipped bug: `terrain_voxel_size` took the MIN axis resolution, so a STRETCHED
    /// field (the 18:1 Y-stretch) meshed the wide footprint at its thin-axis voxel —
    /// millions of surface cells that flooded the terrain color store (2^24). The chosen
    /// voxel must keep the surface-cell count bounded for ANY slab shape.
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
            // vertex count; it MUST stay well under 2^24 (~16.7 M) — the store's ceiling.
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

    /// The FULL genspec streaming pipeline, headless: a PlanetFill serialized
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
        let color = genspec_impostor_color(&spec).expect("impostor color from genspec");
        assert_eq!(color, fill.surface_a.color);
        let t = load_terrain_from(vec![TerrainSource::Generate(spec.clone())])
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
        // A garbled genspec fails CLEANLY (None → the loud-failure path), never panics.
        assert!(load_terrain_from(vec![TerrainSource::Generate("(not ron".into())]).is_none());
        assert!(genspec_impostor_color("(not ron").is_none());
        // A corrupt FILE falls back to the genspec instead of failing the stream —
        // the exact failure Ty hit (LFS pointer stubs where fields should be).
        let dir = std::env::temp_dir().join(format!("floptle-badfield-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("corrupt.cfield");
        std::fs::write(&bad, b"version https://git-lfs -- not a field").unwrap();
        let t2 = load_terrain_from(vec![
            TerrainSource::File(bad),
            TerrainSource::Generate(spec),
        ])
        .expect("corrupt file falls back to genspec generation");
        assert!(t2.field.data_chunks() > 0);
        let _ = std::fs::remove_dir_all(&dir);
        // Genspec-hash gate: same spec → same hash, different spec → different.
        assert_eq!(genspec_hash("abc"), genspec_hash("abc"));
        assert_ne!(genspec_hash("abc"), genspec_hash("abd"));
    }
}
