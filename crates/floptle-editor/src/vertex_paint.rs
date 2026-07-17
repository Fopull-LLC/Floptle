//! The vertex-paint stroke: cursor ray → mesh hit → brush dab → `vpaint` upload.
//!
//! Modelled on `terrain_edit::terrain_frame_update` (same ray build, same telegraph,
//! same dab spacing, same lazy per-stroke snapshot) — deliberately, so there is one
//! way brushes behave in this editor rather than two.
//!
//! Where it differs from terrain, and why:
//!   * it raycasts TRIANGLES (`paint_mesh`), not an SDF field;
//!   * it is hard-gated off during Play — see `paint_gate` below;
//!   * blocks are copy-on-write, so painting a duplicated prop forks rather than
//!     bleeding into the original (proposal §9.0).

use std::time::Instant;

use floptle_core::math::{Vec2, Vec3, Vec4};
use floptle_core::{Entity, Matter, VertexPaint};

use crate::gizmo::Tool;
use crate::paint_ui::PaintMode;
use crate::viz::project;
use crate::{Editor, Snapshot};

/// The brush telegraph — a ring on the surface under the cursor, in screen space.
pub(crate) struct PaintViz {
    pub(crate) ring: Vec<Vec2>,
}

/// The paint value that leaves the surface unchanged. Brush paint modulates 2×, so
/// mid-grey (0.5) is neutral, not white — a fresh block filled with this looks untouched
/// until a dab lands, and "clear"/⌫ Erase return to it. Alpha stays fully opaque.
/// `pub(crate)`: the texture-paint mirror blocks fill with it too.
pub(crate) const NEUTRAL_PAINT: [u8; 4] = [128, 128, 128, 255];

/// A node's paint: one block per mesh part.
#[derive(Clone, Debug, Default)]
pub(crate) struct PaintBlocks {
    /// `(base, vertex_count)` per part, parallel to the asset's `parts`.
    pub(crate) parts: Vec<(u32, u32)>,
}

impl Editor {
    /// Bring the 🖌 Paint tab to the front (re-adding it if closed), so choosing the
    /// tool never leaves the brush controls hidden behind another tab.
    pub(crate) fn focus_paint(&mut self) {
        if let Some(dock) = self.dock_state.as_mut() {
            crate::dock::focus_paint_tab(dock);
        }
    }

    /// The mesh-asset key a node paints against, and its part count. Primitives share
    /// ONE `MeshId` per shape, so they key by shape name — the paint still lands
    /// per-node because the block is per-node, not per-mesh.
    pub(crate) fn paint_key(&self, e: Entity) -> Option<(String, bool)> {
        match self.world.get::<Matter>(e) {
            Some(Matter::Mesh { asset_path }) => Some((asset_path.clone(), false)),
            Some(Matter::Primitive { shape, .. }) => Some((format!("@prim/{}", *shape as u8), true)),
            _ => None,
        }
    }

    /// Ensure the CPU geometry for a paintable node is cached, returning its key.
    pub(crate) fn ensure_paint_mesh_pub(&mut self, e: Entity) -> Option<String> {
        self.ensure_paint_mesh(e)
    }

    fn ensure_paint_mesh(&mut self, e: Entity) -> Option<String> {
        let (key, is_prim) = self.paint_key(e)?;
        if self.paint_meshes.get(&key).is_some() {
            return Some(key);
        }
        let data = if is_prim {
            let shape = match self.world.get::<Matter>(e) {
                Some(Matter::Primitive { shape, .. }) => *shape,
                _ => return None,
            };
            vec![crate::matter_catalog::primitive_mesh(shape)]
        } else {
            // The editor keeps no CPU geometry (MeshAsset holds only MeshIds), so the
            // brush re-imports ONCE here and caches — never per dab.
            let path = self.resolve_asset_path(&key);
            match floptle_assets::import(&path) {
                Ok(m) => m.parts.into_iter().map(|p| p.mesh).collect(),
                // Cache the FAILURE too. This runs for every mesh node every frame the
                // Paint tool is active — an uncached failure would re-hit the disk each
                // frame forever.
                Err(_) => Vec::new(),
            }
        };
        let empty = data.is_empty();
        self.paint_meshes.get_or_build(&key, || data);
        (!empty).then_some(key)
    }

    /// How many nodes share `id` — the copy-on-write test. Paste/duplicate copies the
    /// KEY, so two nodes can legitimately point at one block; the first dab on either
    /// must fork instead of editing both.
    fn paint_sharers(&self, id: u32) -> usize {
        self.world
            .query::<VertexPaint>()
            .filter(|(_, p)| p.id == id)
            .count()
    }

    fn next_paint_id(&self) -> u32 {
        self.world
            .query::<VertexPaint>()
            .map(|(_, p)| p.id)
            .max()
            .map_or(1, |m| m + 1)
    }

    /// The node's paint blocks, allocating (white) or forking (copy-on-write) as
    /// needed. Returns the blocks and the id actually in force.
    fn paint_blocks_for(&mut self, e: Entity, key: &str) -> Option<(u32, PaintBlocks)> {
        let parts = self.paint_meshes.part_count(key);
        if parts == 0 {
            return None;
        }
        let existing = self.world.get::<VertexPaint>(e).copied();

        // Already ours and unshared → paint it directly.
        if let Some(vp) = existing
            && self.paint_sharers(vp.id) == 1
            && let Some(b) = self.paint_data.get(&vp.id)
        {
            return Some((vp.id, b.clone()));
        }

        // Everything read off `self` happens BEFORE the raster borrow below: once
        // `raster` is mutably borrowed, no `&self` method may be called.
        let id = self.next_paint_id();
        let src = existing.and_then(|vp| self.paint_data.get(&vp.id).cloned());
        let counts: Vec<u32> = (0..parts).map(|p| self.paint_meshes.vertex_count(key, p)).collect();

        let mut blocks = PaintBlocks::default();
        {
            let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
                return None;
            };
            for (p, &n) in counts.iter().enumerate() {
                let base = match &src {
                    // FORK: copy the shared block so the original keeps its paint.
                    Some(sb) if sb.parts.len() > p => {
                        let colors = raster.paint_block(sb.parts[p].0, sb.parts[p].1);
                        raster.paint_alloc_from(gpu, &colors)
                    }
                    // Fresh: mid-grey is the identity under the 2× modulate, so the node
                    // looks untouched until a dab actually lands.
                    _ => raster.paint_alloc(gpu, n, NEUTRAL_PAINT),
                };
                if base == 0 {
                    return None; // store full — alloc_paint already logged
                }
                blocks.parts.push((base, n));
            }
        }
        self.world.insert(e, VertexPaint { id });
        self.paint_data.insert(id, blocks.clone());
        Some((id, blocks))
    }

    /// True when the paint tool may run at all. Play is a HARD gate, not a courtesy:
    /// `push_history` no-ops while playing (history.rs:17-29), and Stop does not revert
    /// paint — so a Play-time stroke would persist while being un-undoable. Terrain
    /// merely tolerates that; paint refuses.
    pub(crate) fn paint_gate(&self) -> bool {
        self.tool == Tool::Paint && !self.playing && self.cursor_over_scene()
    }

    /// Per-frame stroke driver — called beside `terrain_frame_update`.
    pub(crate) fn vertex_paint_frame_update(&mut self) {
        self.paint_viz = None;
        if !self.paint_gate() {
            return;
        }
        let (Some(cursor), Some(gpu)) = (self.cursor, self.gpu.as_ref()) else { return };
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let vp = cam.view_proj(w / h);
        let inv = vp.inverse();
        let ndc = Vec2::new(cursor.x / w * 2.0 - 1.0, 1.0 - cursor.y / h * 2.0);
        let near = inv * Vec4::new(ndc.x, ndc.y, 0.0, 1.0);
        let far = inv * Vec4::new(ndc.x, ndc.y, 1.0, 1.0);
        let ro_rel = near.truncate() / near.w;
        let rd = (far.truncate() / far.w - ro_rel).normalize();

        // Nearest paintable node under the cursor. Every candidate is tested in its own
        // LOCAL space (the mesh cache stores object-space geometry), so the ray is
        // pushed through each node's inverse world transform rather than the geometry
        // being transformed — one matrix inverse beats N vertex transforms.
        let candidates: Vec<Entity> = self
            .world
            .query::<Matter>()
            .filter(|(_, m)| matches!(m, Matter::Mesh { .. } | Matter::Primitive { .. }))
            .map(|(e, _)| e)
            .collect();

        // (entity, mesh key, part, hit-local, normal-local, world dist)
        type Cand = (Entity, String, usize, Vec3, Vec3, f64);
        let mut best: Option<Cand> = None;
        for &e in &candidates {
            let Some(key) = self.ensure_paint_mesh(e) else { continue };
            let wt = floptle_core::world_transform(&self.world, e);
            let model = floptle_core::math::Mat4::from_scale_rotation_translation(
                wt.scale,
                wt.rotation,
                (wt.translation - cam.world_position).as_vec3(),
            );
            let minv = model.inverse();
            let ro_l = (minv * Vec4::new(ro_rel.x, ro_rel.y, ro_rel.z, 1.0)).truncate();
            let rd_l = (minv * Vec4::new(rd.x, rd.y, rd.z, 0.0)).truncate();
            let len = rd_l.length();
            if len < 1e-9 {
                continue; // degenerate scale — not paintable
            }
            let Some(hit) = self.paint_meshes.raycast(&key, ro_l, rd_l / len, 1e5) else {
                continue;
            };
            // Back to world for the depth compare, so a near small prop beats a far big one.
            let hw = model * Vec4::new(hit.pos.x, hit.pos.y, hit.pos.z, 1.0);
            let dist = (hw.truncate()).length() as f64;
            if best.as_ref().is_none_or(|b| dist < b.5) {
                best = Some((e, key, hit.part, hit.pos, hit.normal, dist));
            }
        }
        let Some((active, key, part, hit_local, nrm_local, _)) = best else {
            return;
        };

        // Telegraph: a ring around the hit in the surface tangent plane (local space,
        // projected through the node's transform — so it hugs a scaled/rotated prop).
        let wt = floptle_core::world_transform(&self.world, active);
        let model = floptle_core::math::Mat4::from_scale_rotation_translation(
            wt.scale,
            wt.rotation,
            (wt.translation - cam.world_position).as_vec3(),
        );
        let radius = self.vertex_brush.radius;
        let n = nrm_local.normalize_or_zero();
        let t1 = n.cross(if n.y.abs() > 0.9 { Vec3::X } else { Vec3::Y }).normalize_or_zero();
        let t2 = n.cross(t1);
        let mut ring = Vec::with_capacity(40);
        for i in 0..40 {
            let a = i as f32 / 40.0 * std::f32::consts::TAU;
            let lp = hit_local + (t1 * a.cos() + t2 * a.sin()) * radius;
            let wp = (model * Vec4::new(lp.x, lp.y, lp.z, 1.0)).truncate();
            if let Some(s) = project(wp.as_dvec3() + cam.world_position, cam.world_position, vp, w, h) {
                ring.push(s);
            }
        }
        self.paint_viz = Some(PaintViz { ring });

        // Eyedropper acts on click, not on drag — sampling every frame would make the
        // brush color jitter as the cursor moves.
        if self.vertex_brush.mode == PaintMode::Sample {
            if self.painting
                && let Some(vp_c) = self.world.get::<VertexPaint>(active).copied()
                && let Some(b) = self.paint_data.get(&vp_c.id)
                && let Some(&(base, count)) = b.parts.get(part)
            {
                let near = self.paint_meshes.in_radius(&key, part, hit_local, radius);
                if let Some(&(i, _)) = near.first()
                    && i < count
                {
                    let Some(raster) = self.raster.as_ref() else { return };
                    let c = raster.paint_get(base, i);
                    self.vertex_brush.color =
                        [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0];
                }
                self.painting = false; // one-shot
            }
            return;
        }

        // Dab spacing — copied from the sculpt brush (terrain_edit.rs:228-245) because
        // it's already tuned: without it a fast machine dumps a dab per frame and the
        // stroke is uncontrollable.
        let hit_world = (model * Vec4::new(hit_local.x, hit_local.y, hit_local.z, 1.0))
            .truncate()
            .as_dvec3()
            + cam.world_position;
        if !self.painting {
            return;
        }
        let brush_spacing = (self.vertex_brush.radius * self.vertex_brush.spacing).max(1e-4)
            / self.vertex_brush.radius.max(1e-4);
        let now = Instant::now();
        let moved = self
            .last_dab_pos
            .is_none_or(|p| (hit_world - p).length() as f32 >= radius * brush_spacing);
        let timed = self.last_dab_time.is_none_or(|t| (now - t).as_secs_f32() >= 0.10);
        if !(moved || timed) {
            return;
        }
        self.last_dab_pos = Some(hit_world);
        self.last_dab_time = Some(now);

        // TEXTURE target: the dab is a world-space SPHERE, and it paints EVERY paintable
        // surface it touches — all parts, all nodes — not just the ray hit. That is what
        // makes corner shading work: a stroke along a wall-floor seam shades both surfaces
        // in one pass, darkest at the seam (painted ambient occlusion, the retro baked
        // look). Texture paint doesn't touch the vertex blocks below. The FIRST time a
        // stroke touches each node, its pre-stroke images are banked for the stroke's ONE
        // undo step (`None` = that node had no paint yet, so undo removes it).
        if self.vertex_brush.target == crate::paint_ui::PaintTarget::Texture {
            // The brush centre in camera-relative world space — what every node's model
            // matrix maps its object space into.
            let center = (model * Vec4::new(hit_local.x, hit_local.y, hit_local.z, 1.0)).truncate();
            let erase = self.vertex_brush.mode == PaintMode::Erase;
            for &e2 in &candidates {
                // Erasing an unpainted node is a no-op — don't seed a canvas just to erase.
                let pre_id = self.world.get::<floptle_core::TexturePaint>(e2).map(|p| p.id);
                if erase && pre_id.is_none() {
                    continue;
                }
                let Some(key2) = self.ensure_paint_mesh(e2) else { continue };
                let wt2 = floptle_core::world_transform(&self.world, e2);
                let model2 = floptle_core::math::Mat4::from_scale_rotation_translation(
                    wt2.scale,
                    wt2.rotation,
                    (wt2.translation - cam.world_position).as_vec3(),
                );
                // Cheap whole-node reject: the sphere must reach some part's world bounds —
                // without this every dab would walk every mesh's triangle cells.
                let parts_n = self.paint_meshes.part_count(&key2);
                let touched = (0..parts_n).any(|p| {
                    self.paint_meshes
                        .part_bounds(&key2, p)
                        .is_some_and(|(mn, mx)| sphere_touches_bounds(center, radius, mn, mx, model2))
                });
                if !touched {
                    continue;
                }
                // First touch of this node in the stroke: bank its pre-images for undo.
                if let Some(id) = pre_id
                    && !self.tex_stroke_snapshot.contains_key(&id)
                {
                    let snap = self.tex_paint_snapshot(id);
                    self.tex_stroke_snapshot.insert(id, snap);
                }
                self.texture_paint_dab(e2, &key2, center, model2, radius, rd);
                if pre_id.is_none()
                    && let Some(new_id) =
                        self.world.get::<floptle_core::TexturePaint>(e2).map(|p| p.id)
                {
                    self.tex_stroke_snapshot.entry(new_id).or_insert(None);
                }
            }
            return;
        }

        // Same for vertices: erase returns toward neutral, so a node with no paint block
        // has nothing to erase — skip rather than allocate one.
        if self.vertex_brush.mode == PaintMode::Erase
            && self.world.get::<VertexPaint>(active).is_none()
        {
            return;
        }
        let Some((id, blocks)) = self.paint_blocks_for(active, &key) else { return };
        let Some(&(base, count)) = blocks.parts.get(part) else { return };

        // Everything that reads `self` happens up front: once `raster` is mutably
        // borrowed below, no `&self` method may be called.
        let brush = self.vertex_brush;
        let near = self.paint_meshes.in_radius(&key, part, hit_local, radius);
        if near.is_empty() {
            return;
        }
        // View direction in LOCAL space, for the backface test.
        let view_l = (model.inverse() * Vec4::new(-rd.x, -rd.y, -rd.z, 0.0)).truncate();
        // Pre-resolve each candidate's facing so the loop needn't touch the mesh cache.
        let facing: Vec<bool> = near
            .iter()
            .map(|&(i, _)| {
                self.paint_meshes
                    .get(&key)
                    .and_then(|ps| ps.get(part))
                    .and_then(|pp| pp.verts.get(i as usize))
                    .map(|v| Vec3::from(v.normal).dot(view_l) > 0.0)
                    .unwrap_or(true)
            })
            .collect();
        let snap_needed = self.paint_stroke_snapshot.is_none();
        let block_list = blocks.parts.clone();

        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return;
        };

        // Lazily snapshot the pre-stroke colors ONCE per stroke — lazily because until
        // the ray lands we don't know WHICH node is being painted. Banked on LMB-up.
        let snapshot = snap_needed.then(|| {
            let per_part: Vec<Vec<[u8; 4]>> =
                block_list.iter().map(|&(b, c)| raster.paint_block(b, c)).collect();
            (id, per_part)
        });

        // Smooth averages against the PRE-dab colors: sampling as we write would let the
        // iteration order bias the result.
        let pre: Vec<[u8; 4]> = if brush.mode == PaintMode::Smooth {
            near.iter().map(|&(i, _)| raster.paint_get(base, i)).collect()
        } else {
            Vec::new()
        };
        let smooth_target = {
            let mut acc = [0f32; 4];
            for c in &pre {
                for (ch, a) in acc.iter_mut().enumerate() {
                    *a += c[ch] as f32;
                }
            }
            let n = pre.len().max(1) as f32;
            [acc[0] / n, acc[1] / n, acc[2] / n, acc[3] / n]
        };

        let (mut lo, mut hi) = (u32::MAX, 0u32);
        for (k, &(i, d)) in near.iter().enumerate() {
            if i >= count || (!brush.backfaces && !facing[k]) {
                continue;
            }
            // The shared brush profile (hardness + falloff shape) — the same one the
            // terrain brush runs, so "hard edge" means the same thing in both tools.
            let w = brush.profile.weight(d, radius) * brush.strength;
            if w <= 0.0 {
                continue;
            }
            let src = match brush.mode {
                PaintMode::Smooth => smooth_target,
                // Erase returns toward the neutral (untouched) value.
                PaintMode::Erase => [128.0, 128.0, 128.0, 255.0],
                _ => [
                    brush.color[0] * 255.0,
                    brush.color[1] * 255.0,
                    brush.color[2] * 255.0,
                    brush.alpha * 255.0,
                ],
            };
            let cur = raster.paint_get(base, i);
            let mut out = cur;
            for (ch, o) in out.iter_mut().enumerate() {
                if !brush.channels[ch] {
                    continue; // masked-off channels keep whatever was already there
                }
                // Blend picks the full-strength TARGET; the brush weight then lerps
                // toward it — so every mode answers to strength/falloff identically.
                // Smooth averages and Erase restores, so both mix regardless of blend.
                let target = match brush.mode {
                    PaintMode::Smooth | PaintMode::Erase => src[ch],
                    _ => brush.blend.apply(cur[ch] as f32, src[ch]),
                };
                let val = cur[ch] as f32 + (target - cur[ch] as f32) * w;
                *o = val.round().clamp(0.0, 255.0) as u8;
            }
            if out != cur {
                raster.paint_set(base, i, out);
                lo = lo.min(i);
                hi = hi.max(i);
            }
        }
        if lo <= hi {
            // One upload for the dab's touched range, not one per vertex.
            raster.paint_flush(gpu, base, lo, hi);
            self.paint_stroke_dabbed = true;
            self.vpaint_epoch += 1; // texture-paint mirrors resync (paint_tex)
        }
        if let Some(sn) = snapshot {
            self.paint_stroke_snapshot = Some(sn);
        }
    }

    /// Flood every part of the selected node with the brush color. With ⌫ Erase active it
    /// floods NEUTRAL instead — the whole node back to unpainted.
    pub(crate) fn paint_fill_selected(&mut self) {
        if self.playing {
            return;
        }
        let sel: Vec<Entity> = self.selection.clone();
        for e in sel {
            let b = self.vertex_brush;
            // Erase-filling an unpainted node is a no-op — don't allocate blocks for it.
            if b.mode == PaintMode::Erase && self.world.get::<VertexPaint>(e).is_none() {
                continue;
            }
            let Some(key) = self.ensure_paint_mesh(e) else { continue };
            let Some((id, blocks)) = self.paint_blocks_for(e, &key) else { continue };
            let fill = if b.mode == PaintMode::Erase {
                NEUTRAL_PAINT
            } else {
                [
                    (b.color[0] * 255.0).round() as u8,
                    (b.color[1] * 255.0).round() as u8,
                    (b.color[2] * 255.0).round() as u8,
                    (b.alpha * 255.0).round() as u8,
                ]
            };
            let snap = {
                let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
                    return;
                };
                let snap: Vec<Vec<[u8; 4]>> =
                    blocks.parts.iter().map(|&(bb, c)| raster.paint_block(bb, c)).collect();
                for &(base, count) in &blocks.parts {
                    for i in 0..count {
                        let mut out = raster.paint_get(base, i);
                        for (ch, o) in out.iter_mut().enumerate() {
                            if b.channels[ch] {
                                *o = fill[ch];
                            }
                        }
                        raster.paint_set(base, i, out);
                    }
                    if count > 0 {
                        raster.paint_flush(gpu, base, 0, count - 1);
                    }
                }
                snap
            };
            self.push_history(Snapshot::VertexPaint(id, snap));
            self.vpaint_epoch += 1;
        }
    }

    /// Strip paint from the selected node — back to the plain unpainted look.
    pub(crate) fn paint_clear_selected(&mut self) {
        if self.playing {
            return;
        }
        let sel: Vec<Entity> = self.selection.clone();
        for e in sel {
            let Some(vp) = self.world.get::<VertexPaint>(e).copied() else { continue };
            let Some(blocks) = self.paint_data.get(&vp.id).cloned() else { continue };
            let snap = {
                let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
                    return;
                };
                let snap: Vec<Vec<[u8; 4]>> =
                    blocks.parts.iter().map(|&(bb, c)| raster.paint_block(bb, c)).collect();
                for &(base, count) in &blocks.parts {
                    for i in 0..count {
                        raster.paint_set(base, i, NEUTRAL_PAINT);
                    }
                    if count > 0 {
                        raster.paint_flush(gpu, base, 0, count - 1);
                    }
                }
                snap
            };
            self.push_history(Snapshot::VertexPaint(vp.id, snap));
            self.vpaint_epoch += 1;
            // The component stays: the block is already allocated, and dropping it would
            // strand that range in the store. Mid-grey IS unpainted, visually (2× neutral).
        }
    }

    /// Bank the finished stroke as ONE undo step (called on LMB-up).
    pub(crate) fn end_paint_stroke(&mut self) {
        if let Some((id, snap)) = self.paint_stroke_snapshot.take()
            && self.paint_stroke_dabbed
        {
            self.push_history(Snapshot::VertexPaint(id, snap));
        }
        // Texture stroke: ONE undo step carrying every touched node's pre-stroke images
        // (`None` = that node had no paint before, so undo removes it) — the sphere brush
        // can cross several nodes in a stroke, and Ctrl+Z must take the whole stroke back.
        if !self.tex_stroke_snapshot.is_empty() {
            let entries: Vec<_> = self.tex_stroke_snapshot.drain().collect();
            self.push_history(Snapshot::TexPaint(entries));
        }
        self.paint_stroke_dabbed = false;
        self.last_dab_pos = None;
        self.last_dab_time = None;
    }
}

/// Does the brush sphere touch a part's transformed bounds? Conservative (the world AABB of
/// the local box's 8 corners is looser than the true oriented box) — that's fine, it only
/// gates the per-triangle work, which rejects precisely.
fn sphere_touches_bounds(
    center: Vec3,
    radius: f32,
    mn: Vec3,
    mx: Vec3,
    model: floptle_core::math::Mat4,
) -> bool {
    let mut wmin = Vec3::splat(f32::INFINITY);
    let mut wmax = Vec3::splat(f32::NEG_INFINITY);
    for i in 0..8 {
        let c = Vec3::new(
            if i & 1 == 0 { mn.x } else { mx.x },
            if i & 2 == 0 { mn.y } else { mx.y },
            if i & 4 == 0 { mn.z } else { mx.z },
        );
        let w = model.transform_point3(c);
        wmin = wmin.min(w);
        wmax = wmax.max(w);
    }
    (center.clamp(wmin, wmax) - center).length_squared() <= radius * radius
}
