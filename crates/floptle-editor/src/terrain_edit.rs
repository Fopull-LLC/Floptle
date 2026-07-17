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

/// The DERIVED render mesh for one terrain (ADR terrain-mesh / P2). The dense field stays
/// the authority (physics, sculpt, save, and the atlas that feeds shadows + AO); this is
/// only what the raster pass DRAWS. Chunk vertices are FIELD-space, so every chunk shares
/// one camera-relative instance matrix and the triplanar material stays continuous.
#[derive(Default)]
pub(crate) struct TerrainRender {
    /// The sparse SDF the mesher extracts from. Re-derived from the dense field on change.
    pub field: floptle_field::ChunkField,
    /// One dynamic raster slot per non-empty chunk, keyed by chunk coord so a sculpt can
    /// re-mesh just the chunks it touched and free the ones that emptied.
    pub slots: HashMap<[i32; 3], MeshId>,
}

impl Editor {
    /// Rebuild every terrain's render mesh whose dense field changed. Cheap when nothing
    /// changed (the guard is the same `terrain_gpu_dirty` / region-dirty the atlas upload
    /// already tracks). Full rebuild on structural change; the sculpt fast-path re-meshes
    /// only the dabbed chunks. Called right after `sync_terrain_gpu` keeps the atlas fed.
    pub(crate) fn sync_terrain_meshes(
        &mut self,
        full_rebuild: bool,
        region: Option<(Entity, [u32; 3], [u32; 3])>,
    ) {
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

        for (&e, terrain) in &self.terrains {
            let structural = full_rebuild || !self.terrain_render.contains_key(&e);
            let region_box = region.filter(|(re, ..)| *re == e);
            if !structural && region_box.is_none() {
                continue;
            }
            let render = self.terrain_render.entry(e).or_default();

            // Which chunks to (re)mesh, and whether to prune slots the field no longer fills.
            let (coords, prune) = if structural {
                // Full re-derive: a cubic-voxel ChunkField resampled off the dense grid.
                // Resampling to cubic voxels is also what quietly retires the voxel-stretch
                // artifact on old (18:1-stretched) fields — they mesh at true cubic detail.
                let voxel = terrain_voxel_size(&terrain.baked);
                render.field = floptle_field::ChunkField::from_dense(&terrain.baked, voxel);
                (render.field.chunk_coords(), true)
            } else {
                // Regional: re-derive ONLY the dabbed box from the dense authority and
                // re-mesh just its chunks — what keeps sculpting a big terrain smooth
                // (a full resample per dab would be O(whole field)).
                let (_, mn, mx) = region_box.expect("region present");
                let (wmin, wmax) = voxel_index_world_box(&terrain.baked, mn, mx);
                let touched = render.field.refresh_from_dense_region(&terrain.baked, wmin, wmax);
                (touched, false)
            };

            for coord in coords {
                let cm = floptle_field::mesh_chunk(&render.field, coord, 1, false);
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
                    render.field.chunk_coords().into_iter().collect();
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

/// World-space AABB (in the dense field's frame) of a box given as DENSE voxel indices —
/// the conversion the sculpt fast-path needs to hand `refresh_from_dense_region` a region.
/// Mirrors `Terrain::voxel_world`: voxel `i` centers at `center - half + (i+0.5)/dims·2half`.
fn voxel_index_world_box(
    baked: &floptle_field::BakedSdf,
    mn: [u32; 3],
    mx: [u32; 3],
) -> (Vec3, Vec3) {
    let c = baked.center;
    let hf = baked.half_extent;
    let [w, h, d] = baked.dims;
    let f = |i: u32, n: u32, ci: f32, hi: f32| ci - hi + (i as f32 + 0.5) / n.max(1) as f32 * 2.0 * hi;
    let lo = Vec3::new(f(mn[0], w, c[0], hf[0]), f(mn[1], h, c[1], hf[1]), f(mn[2], d, c[2], hf[2]));
    let hi = Vec3::new(f(mx[0], w, c[0], hf[0]), f(mx[1], h, c[1], hf[1]), f(mx[2], d, c[2], hf[2]));
    (lo.min(hi), lo.max(hi))
}

/// The cubic voxel edge to mesh a dense terrain at.
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
        let mut best: Option<(Entity, [f32; 3], DVec3, f64)> = None;
        for e in entities {
            let origin = self.terrain_world_origin(e);
            let ro_local = cam.world_position + ro_rel.as_dvec3() - origin;
            let ro = [ro_local.x as f32, ro_local.y as f32, ro_local.z as f32];
            if let Some(hit) = self.terrains[&e].raycast(ro, rd_a) {
                let hitw = DVec3::new(hit[0] as f64, hit[1] as f64, hit[2] as f64) + origin;
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
        let nrm = self.terrains[&active].normal(hit);
        let radius = self.terrain_brush.radius;

        // Telegraph: a ring of `radius` around the hit in the surface tangent plane.
        let hitw = DVec3::new(hit[0] as f64, hit[1] as f64, hit[2] as f64) + origin;
        let n = Vec3::new(nrm[0], nrm[1], nrm[2]);
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
            // Capture the pre-stroke field once per stroke, keyed by terrain id, so
            // the whole stroke is a single (restorable) undo step.
            if self.stroke_snapshot.is_none() {
                let id = match self.world.get::<Matter>(active) {
                    Some(Matter::Terrain { id }) => *id,
                    _ => 0,
                };
                if let Some(t) = self.terrains.get(&active) {
                    self.stroke_snapshot = Some((id, t.to_bytes()));
                }
            }
            let terrain = self.terrains.get_mut(&active).unwrap();
            // Infinite terrain: grow the field outward when the brush nears an edge,
            // so the slab has no fixed bounds. (Skip for Paint — painting never
            // extends the shape.) Growth keeps voxel size constant.
            let is_paint = matches!(brush.mode, floptle_field::Brush::Paint);
            // Growing the bounds reallocates the grid (dims change) → must take the full
            // path. `resized` is checked below to decide partial vs full.
            let resized = if !is_paint { terrain.ensure_contains(hit, brush.radius * 1.5) } else { false };
            // Apply the brush; collect the voxel sub-box it actually changed (paint =
            // its brush box; sculpt = the box of cells whose distance moved).
            let region = match brush.mode {
                floptle_field::Brush::Paint if brush.tex_slot >= 0 => {
                    terrain.paint_texture(hit, brush.radius, brush.tex_slot as u8 + 1);
                    Some(terrain.brush_range(hit, brush.radius))
                }
                floptle_field::Brush::Paint => {
                    terrain.paint(hit, brush.radius, brush.strength, brush.color, brush.profile);
                    Some(terrain.brush_range(hit, brush.radius))
                }
                m => terrain.sculpt(m, hit, brush.radius, brush.strength, brush.profile),
            };
            self.stroke_dabbed = true; // mark this stroke as worth an undo step
            // Fast path: a single terrain that didn't resize uploads only the dabbed box
            // (no full re-clone + re-upload — that's the paint/sculpt lag). A resize, an
            // empty change, or multiple terrains fall back to a full rebuild.
            match region {
                Some([mn, mx]) if self.terrains.len() == 1 && !resized => {
                    let hi = [mx[0] + 1, mx[1] + 1, mx[2] + 1];
                    let geom = !is_paint; // sculpt changes geometry (resync wireframe + collider)
                    self.terrain_region_dirty = Some(match self.terrain_region_dirty {
                        Some((e, omn, omx, og)) if e == active => (
                            active,
                            [omn[0].min(mn[0]), omn[1].min(mn[1]), omn[2].min(mn[2])],
                            [omx[0].max(hi[0]), omx[1].max(hi[1]), omx[2].max(hi[2])],
                            og || geom,
                        ),
                        _ => (active, mn, hi, geom),
                    });
                }
                _ => self.terrain_gpu_dirty = true,
            }
        }
    }

    /// Voxel dims for the current detail setting over the terrain box (≈2:1:2).
    pub(crate) fn terrain_dims(&self) -> [u32; 3] {
        // The legacy default slab (16 × 6 × 16 half-extents) — kept for adopt_terrain's
        // "terrain node with no field" fallback.
        self.terrain_dims_for([16.0, 6.0, 16.0])
    }

    /// Voxel grid for a slab of `half` half-extents: `detail` cells along the LONGEST
    /// axis, and every other axis sized to the SAME voxel edge — i.e. cubic cells.
    ///
    /// The old policy was `[d, d*3/8, d]`: a fixed COUNT with a hardcoded 8:3 aspect,
    /// which never consulted the terrain's actual size. Two consequences, both of which
    /// shipped:
    ///
    /// * **Stretched cells.** A slab whose real aspect isn't 8:3 got anisotropic voxels
    ///   — a 578 × 12 terrain came out 9.17 × 0.50 × 9.17, i.e. **18:1**. Trilinear
    ///   interpolation across cells that stretched is visibly faceted: you see the
    ///   lattice as dark quad lines, and terraced steps edge-on.
    /// * **Detail meant nothing at scale.** 64 cells whether the terrain is 16 units or
    ///   578. The slider looked like it controlled quality; past a few dozen units it
    ///   couldn't.
    ///
    /// Cubic cells fix the facets. They do NOT make a huge slab fine — 578 units at
    /// `detail` 192 is still ~3 units/voxel — which is exactly why
    /// [`Self::terrain_voxel_size`] is surfaced in the UI: a terrain this big wants
    /// several blended volumes, not one coarse one, and the number has to say so.
    pub(crate) fn terrain_dims_for(&self, half: [f32; 3]) -> [u32; 3] {
        crate::terrain_ui::terrain_dims_for_size(
            [2.0 * half[0], 2.0 * half[1], 2.0 * half[2]],
            self.terrain_detail,
        )
    }

    /// The world-space voxel edge of a terrain field, per axis. Anisotropy here is
    /// what shows up as a visible lattice, so the UI reports it.
    pub(crate) fn terrain_voxel_size(field: &floptle_field::Terrain) -> [f32; 3] {
        let b = &field.baked;
        [0, 1, 2].map(|i| 2.0 * b.half_extent[i] / (b.dims[i].max(2) - 1) as f32)
    }

    /// Create a fresh flat terrain as a NEW scene node (you can have any number). It
    /// is placed at the cursor's ground point so multiple terrains can be laid out
    /// and blended; its field is centered in the node's local space. `cfg` (from the
    /// "New terrain" dialog) sizes the flat slab and paints it with a color/texture
    /// up front — a flat field renders exactly right at any voxel density (trilinear
    /// interpolation of a plane is exact), so a huge open field is just as clean as a
    /// tiny patch; `terrain_dims()`/detail only matters once you start sculpting bumps.
    pub(crate) fn create_terrain(&mut self, cfg: &NewTerrainCfg) {
        self.record();
        let id = self.next_terrain_id;
        self.next_terrain_id += 1;
        let pos = self.cursor_world();
        let half_xz = cfg.size_xz.max(0.1) * 0.5;
        let half_y = cfg.thickness.max(0.1) * 0.5;
        let mut field = floptle_field::Terrain::flat(
            self.terrain_dims_for([half_xz, half_y, half_xz]),
            [0.0, 0.0, 0.0],
            [half_xz, half_y, half_xz],
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
        self.terrains.insert(e, field);
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

    /// Where a terrain node's field is stored — one file per terrain id, per scene.
    pub(crate) fn terrain_field_path_id(&self, id: u32) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.{id}.tfield", self.scene_name))
    }

    /// The legacy single-terrain field path (migrated to the id-keyed name on load).
    pub(crate) fn legacy_terrain_field_path(&self) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.tfield", self.scene_name))
    }

    /// After loading a scene, adopt every terrain node + load its field from disk
    /// (id-keyed, with a one-time legacy fallback). Call once `scene_name` is set.
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
            let field = std::fs::read(self.terrain_field_path_id(id))
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
                // a terrain node with no/garbled field → start it flat.
                .unwrap_or_else(|| {
                    floptle_field::Terrain::flat(
                        self.terrain_dims(),
                        [0.0, 0.0, 0.0],
                        [16.0, 6.0, 16.0],
                        0.0,
                        [0.35, 0.6, 0.28],
                    )
                });
            self.terrains.insert(e, field);
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
        terrains: &HashMap<Entity, floptle_field::Terrain>,
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
            let bc = t.baked.center;
            let hf = t.baked.half_extent;
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
