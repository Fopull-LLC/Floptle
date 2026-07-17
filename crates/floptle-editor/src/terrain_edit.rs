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
    /// One dynamic raster slot per non-empty chunk, keyed by chunk coord so a sculpt can
    /// re-mesh just the chunks it touched and free the ones that emptied.
    pub slots: HashMap<[i32; 3], MeshId>,
}

impl Editor {
    /// Rebuild every terrain's render mesh whose field changed. Cheap when nothing
    /// changed. Full rebuild on structural change (load / new / fill / undo); the
    /// sculpt fast-path re-meshes only the chunks a dab actually touched (drained
    /// from `terrain_chunks_dirty`). Called right after `sync_terrain_gpu` keeps the
    /// shadow atlas fed.
    pub(crate) fn sync_terrain_meshes(&mut self, full_rebuild: bool) {
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return;
        };
        // Drop render meshes for terrains that no longer exist (deleted nodes).
        let live: Vec<Entity> = self.terrains.keys().copied().collect();
        self.terrain_render.retain(|e, r| {
            if live.contains(e) {
                return true;
            }
            for (_, mid) in r.slots.drain() {
                raster.free_dynamic(mid);
            }
            false
        });
        self.terrain_chunks_dirty.retain(|e, _| live.contains(e));

        for (&e, terrain) in &self.terrains {
            let structural = full_rebuild || !self.terrain_render.contains_key(&e);
            let dirty = self.terrain_chunks_dirty.remove(&e);
            if !structural && dirty.is_none() {
                continue;
            }
            let render = self.terrain_render.entry(e).or_default();

            // Which chunks to (re)mesh, and whether to prune slots the field no longer
            // fills. The mesher reads the AUTHORITY field directly — there is no
            // derived copy to keep in sync any more.
            let (coords, prune) = if structural {
                (terrain.field.chunk_coords(), true)
            } else {
                let mut d = dirty.unwrap_or_default();
                d.sort_unstable();
                d.dedup();
                (d, false)
            };

            for coord in coords {
                let cm = floptle_field::mesh_chunk(&terrain.field, coord, 1, false);
                if cm.is_empty() {
                    // Chunk emptied (e.g. dug fully away): free + forget its slot.
                    if let Some(mid) = render.slots.remove(&coord) {
                        raster.free_dynamic(mid);
                    }
                } else {
                    upload_chunk(gpu, raster, render, coord, &cm);
                }
            }

            if prune {
                // A full rebuild may have shed chunks entirely; free any slot whose chunk
                // no longer holds data.
                let has: std::collections::HashSet<[i32; 3]> =
                    terrain.field.chunk_coords().into_iter().collect();
                render.slots.retain(|c, mid| {
                    if has.contains(c) {
                        true
                    } else {
                        raster.free_dynamic(*mid);
                        false
                    }
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
pub(crate) fn push_terrain_instances(
    terrain_render: &HashMap<Entity, TerrainRender>,
    world: &floptle_core::World,
    raster: &floptle_render::Raster,
    base_mat: &MaterialParams,
    cam_world: DVec3,
    instances: &mut Vec<(MeshId, Option<floptle_render::TexId>, floptle_render::InstanceRaw)>,
) {
    for (&e, render) in terrain_render {
        if render.slots.is_empty() {
            continue;
        }
        let anchor = floptle_core::world_transform(world, e).translation;
        let model = Mat4::from_translation((anchor - cam_world).as_vec3());
        for &mid in render.slots.values() {
            let mut mp = *base_mat;
            mp.terrain_paint_base = raster.dyn_paint_base(mid);
            // Splat: interpret the chunk color's alpha as a palette slot + triplanar-sample
            // the terrain palette (bound to the raster in `set_terrain_palette`).
            mp.terrain_splat = true;
            instances.push((mid, None, floptle_render::instance_of_mat(model, &mp)));
        }
    }
}

/// Register (or overwrite) one chunk's dynamic slot in a terrain's render set.
fn upload_chunk(
    gpu: &floptle_render::Gpu,
    raster: &mut floptle_render::Raster,
    render: &mut TerrainRender,
    coord: [i32; 3],
    cm: &floptle_field::ChunkMesh,
) {
    let data = floptle_render::chunk_mesh_data(cm);
    match render.slots.get(&coord).copied() {
        Some(mid) if raster.replace_dynamic(gpu, mid, &data) => {}
        Some(mid) => {
            // Outgrew its slot (rare): drop and re-register at the new size.
            raster.free_dynamic(mid);
            let id = raster.register_dynamic(
                gpu,
                data.vertices.len() as u32,
                data.indices.len() as u32,
                true,
            );
            raster.replace_dynamic(gpu, id, &data);
            render.slots.insert(coord, id);
        }
        None => {
            let id = raster.register_dynamic(
                gpu,
                data.vertices.len() as u32,
                data.indices.len() as u32,
                true,
            );
            raster.replace_dynamic(gpu, id, &data);
            render.slots.insert(coord, id);
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
/// idiom `rim.w` uses for tiling flags. `TERRAIN_SLOTS` is 16, so it fits easily.
///
/// A free function, not a method: the render loop holds `self.gpu.as_mut()` and friends,
/// so `&self` is unavailable there — but borrowing these two fields is fine.
pub(crate) fn terrain_nearest_mask(
    textures: &[String],
    settings: &std::collections::HashMap<String, crate::assets::TexSetting>,
) -> f32 {
    let mut mask = 0u32;
    for (i, path) in textures.iter().enumerate().take(32) {
        if path.is_empty() {
            continue;
        }
        let s = settings.get(path).copied().unwrap_or_default();
        if s.filter == crate::assets::FilterMode::Pixelated {
            mask |= 1 << i;
        }
    }
    mask as f32
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
                let Ok(model) =
                    floptle_assets::gltf_import::import(std::path::Path::new(&key.0))
                else {
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

        // Each field is in its node's LOCAL space — raycast every terrain and brush
        // the one whose surface the cursor ray hits NEAREST the camera.
        let entities: Vec<Entity> = self.terrains.keys().copied().collect();
        let mut best: Option<(Entity, Vec3, DVec3, f64)> = None;
        for e in entities {
            let origin = self.terrain_world_origin(e);
            let ro_local = cam.world_position + ro_rel.as_dvec3() - origin;
            let ro = Vec3::new(ro_local.x as f32, ro_local.y as f32, ro_local.z as f32);
            if let Some(hit) = self.terrains[&e].field.raycast(ro, Vec3::from(rd_a), 4096.0) {
                let hitw = DVec3::new(hit.x as f64, hit.y as f64, hit.z as f64) + origin;
                let dist = (hitw - cam.world_position).length();
                if best.as_ref().is_none_or(|b| dist < b.3) {
                    best = Some((e, hit, origin, dist));
                }
            }
        }
        let Some((active, hit, origin, _)) = best else {
            return;
        };
        self.active_terrain = Some(active);
        let n = self.terrains[&active].field.grad(hit);
        let radius = self.terrain_brush.radius;

        // Telegraph: a ring of `radius` around the hit in the surface tangent plane.
        let hitw = DVec3::new(hit.x as f64, hit.y as f64, hit.z as f64) + origin;
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
            let id = match self.world.get::<Matter>(active) {
                Some(Matter::Terrain { id }) => *id,
                _ => 0,
            };
            let terrain = self.terrains.get_mut(&active).unwrap();
            // Capture the pre-dab chunks into the stroke's undo record — lazily, only
            // the chunks this dab could touch that aren't already captured. The whole
            // stroke stays a single undo step of a few MB, not a whole-field snapshot.
            let candidates = terrain.field.chunks_in_box(hit, brush.radius * 1.5);
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
                    terrain.field.paint_texture(hit, brush.radius, brush.tex_slot as u8 + 1)
                }
                floptle_field::Brush::Paint => {
                    terrain.field.paint(hit, brush.radius, brush.strength, brush.color, brush.profile)
                }
                m => terrain.field.sculpt(m, hit, brush.radius, brush.strength, brush.profile),
            };
            if !touched.is_empty() {
                self.stroke_dabbed = true; // mark this stroke as worth an undo step
                // Shadow-proxy refresh + atlas partial upload + chunk remesh queue.
                // (A dab outside the proxy's box clamps — the proxy is re-derived at
                // stroke end when bounds outgrow it; see `end_sculpt_stroke`.)
                let geom = !is_paint; // sculpt changes geometry (resync wireframe + collider)
                self.queue_terrain_dirty(active, hit, brush.radius, geom, touched);
            }
        }
    }

    /// Drain + apply the terrain edits scripts queued this pass (`terrain.sculpt/
    /// dig/paint/paintTexture` — Terrain 2.0 P6). Call after reclaiming the sim's
    /// colliders and BEFORE stepping physics, so a dig affects the same tick.
    pub(crate) fn drain_script_terrain_ops(&mut self) {
        for op in self.script_host.take_terrain_ops() {
            self.apply_terrain_op(&op);
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
        // Nearest terrain by |field distance| at the op position.
        let mut best: Option<(Entity, Vec3, f32)> = None;
        for &e in self.terrains.keys() {
            let anchor = self.terrain_world_origin(e);
            let local = (pos - anchor).as_vec3();
            let d = self.terrains[&e].field.d(local).abs();
            if best.as_ref().is_none_or(|b| d < b.2) {
                best = Some((e, local, d));
            }
        }
        let Some((e, local, d)) = best else { return };
        // Too far from every surface: a mis-aimed op must not edit a random field.
        if d > op.radius + self.terrains[&e].field.band() * 2.0 {
            return;
        }
        let profile = BrushProfile::default();
        let t = self.terrains.get_mut(&e).unwrap();
        let touched = match op.mode {
            M::Raise => t.field.sculpt(Brush::Raise, local, op.radius, op.strength, profile),
            M::Lower => t.field.sculpt(Brush::Lower, local, op.radius, op.strength, profile),
            M::Smooth => t.field.sculpt(Brush::Smooth, local, op.radius, op.strength, profile),
            M::Flatten => t.field.sculpt(Brush::Flatten, local, op.radius, op.strength, profile),
            M::Paint(c) => t.field.paint(local, op.radius, op.strength, c, profile),
            M::PaintTexture(slot) => t.field.paint_texture(local, op.radius, slot),
        };
        if touched.is_empty() {
            return;
        }
        let geom = !matches!(op.mode, M::Paint(_) | M::PaintTexture(_));
        // Mirror geometry edits into the sim's collider copy so collision agrees
        // with the drawn surface THIS tick (color never affects collision).
        if geom
            && let Some(sim) = self.sim.as_mut()
            && let Some(f) = sim.terrain_field_mut(e.index())
        {
            match op.mode {
                M::Raise => f.sculpt(Brush::Raise, local, op.radius, op.strength, profile),
                M::Lower => f.sculpt(Brush::Lower, local, op.radius, op.strength, profile),
                M::Smooth => f.sculpt(Brush::Smooth, local, op.radius, op.strength, profile),
                M::Flatten => f.sculpt(Brush::Flatten, local, op.radius, op.strength, profile),
                _ => Vec::new(),
            };
        }
        self.queue_terrain_dirty(e, local, op.radius, geom, touched);
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
        if let Some(slot) = self.ensure_texture_slot(&cfg.texture) {
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

    /// The legacy DENSE field path for the same terrain — read-only migration source.
    pub(crate) fn terrain_tfield_path_id(&self, id: u32) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.{id}.tfield", self.scene_name))
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
        self.terrains.clear();
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
        let single = nodes.len() == 1;
        for (e, id) in nodes {
            max_id = max_id.max(id);
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
            let field = std::fs::read(self.terrain_field_path_id(id))
                .ok()
                .and_then(|b| floptle_field::ChunkField::from_bytes(&b))
                .or_else(dense_migration)
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
            self.terrains.insert(e, EditorTerrain::new(field));
        }
        self.next_terrain_id = max_id + 1;
        self.terrain_gpu_dirty = !self.terrains.is_empty();
        // Restore the texture palette so painted-texture slots map to images again.
        if !self.terrains.is_empty()
            && let Ok(text) = std::fs::read_to_string(self.terrain_palette_path()) {
                let slots = floptle_render::TERRAIN_SLOTS as usize;
                let mut palette: Vec<String> = text.lines().map(|s| s.to_string()).collect();
                palette.resize(slots, String::new());
                self.terrain_textures = palette;
                self.terrain_textures_dirty = true;
            }
    }

    /// The world translation of a terrain node (places its field in world space).
    pub(crate) fn terrain_world_origin(&self, e: Entity) -> DVec3 {
        floptle_core::world_transform(&self.world, e).translation
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
            let anchor = floptle_core::world_transform(world, e).translation;
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
    use floptle_field::BakedSdf;

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
