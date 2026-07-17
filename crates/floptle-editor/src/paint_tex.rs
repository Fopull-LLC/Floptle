//! Texture painting — the resolution-independent companion to vertex painting.
//!
//! A brush stamps into a per-node paint texture that renders as a **transparent overlay**
//! on top of the node, so painted detail is independent of the mesh's polygon count (fine
//! detail on a flat low-poly wall) while the node underneath keeps rendering EXACTLY as it
//! always did — same mesh, same UVs, same textures, same tiling, same vertex colors.
//!
//! # Why an overlay (and not a baked canvas)
//!
//! The first cut seeded a canvas by RESAMPLING the node's base texture into the paint
//! atlas. That changes the base's look: the atlas texel grid is aligned per-triangle and
//! sized per-world-area, so a nearest-sampled pixel-art texture came back with its texels
//! at a different angle and scale (Ty: "the angle of the pixels seems completely
//! different"). An overlay never touches the base render — the paint texture starts fully
//! TRANSPARENT, dabs deposit color + alpha, and the GPU alpha-blends it over the ordinary
//! draw. Unpainted texels contribute nothing, so the base is pixel-exact by construction.
//!
//! The overlay draws through the ordinary transparent pass: its instance alpha rides just
//! under the opaque cutoff, and the transparent pipeline depth-tests LESS-EQUAL, so the
//! coplanar overlay (identical positions → byte-identical depth) lands exactly on its
//! surface without z-fighting, and is still occluded by anything actually in front.
//!
//! # Why a per-triangle ATLAS
//!
//! Painting into the mesh's own UVs REPEATS: level meshes reuse UV space (one texture
//! tiles across many faces), so a single dab appears everywhere those UVs repeat. The
//! overlay instead renders through a generated **unique** UV set where every triangle owns
//! its own patch of the texture ([`crate::paint_mesh::MeshAtlas`]), packed by its real
//! (flattened) shape at a shared texel density — a dab lands in exactly one patch, and a
//! long thin face gets a long thin patch (no stretch).
//!
//! # Smooth across faces
//!
//! A dab paints in WORLD space: for every texel of every triangle the brush sphere touches,
//! it reconstructs the surface point and weights by world distance to the cursor (see
//! [`crate::paint_mesh::for_each_cell_texel`]). Texels on either side of a shared edge
//! reconstruct the same world point, so paint flows across the edge with no visible seam.
//!
//! # Shading + erase
//!
//! The node's vertex colors keep multiplying the overlay, so paint sits in the same light
//! as the surface it covers: imported COLOR_0 rides the atlas mesh's own paint block, and
//! brush vertex paint is mirrored into an atlas-ordered block
//! ([`Editor::sync_tex_paint_mirrors`]). The ⌫ Erase brush pulls the paint's alpha back to
//! zero — revealing the live base, not a copy of it.
//!
//! # Identity + undo + save
//!
//! Keyed by a stable `TexturePaint { id }` component (not `Entity`, which `restore()`
//! invalidates), exactly like vertex paint — so undo survives a World rebuild. The painted
//! images round-trip to `<project>/paint/<scene>.tpaint` (see `paint_tex_io`); the atlas
//! layout is deterministic, so a reload rebuilds it identically and the saved pixels line up.

use floptle_core::math::{Mat4, Vec3};
use floptle_core::{Entity, TexturePaint};
use floptle_render::{MeshId, TexFilter, TexId, TexSampling, TextureData, TexWrap};

use crate::paint_mesh::MeshAtlas;
use crate::Editor;

/// Paint textures sample nearest + clamp: crisp retro pixels, and (with the atlas padding)
/// no filtering bleed across the packed triangle patches.
const PAINT_SAMPLING: TexSampling = TexSampling { filter: TexFilter::Pixelated, wrap: TexWrap::Clamp };

/// A fresh canvas texel: fully transparent (the overlay shows nothing), with WHITE rgb so
/// the Multiply/Darken blend modes see the identity when paint first lands on a texel.
const CLEAR_TEXEL: [u8; 4] = [255, 255, 255, 0];

/// The overlay's instance alpha: just under the renderer's opaque cutoff (0.999), so the
/// overlay routes to the alpha-blended transparent pass. Visually indistinguishable from 1.
const OVERLAY_ALPHA: f32 = 0.998;

/// One part's paint image + its GPU texture + the atlas mesh it renders through.
pub(crate) struct PaintPartTex {
    /// CPU source of truth (RGBA, row-major, `edge²` texels). Alpha is REAL coverage:
    /// 0 = unpainted (base shows through), 255 = solid paint.
    pub(crate) pixels: Vec<u8>,
    pub(crate) edge: u32,
    pub(crate) tex: TexId,
    /// The unique-UV overlay mesh (same positions/normals as the original part).
    pub(crate) atlas: MeshId,
    /// Per-triangle atlas geometry — the brush paints through this in world space.
    pub(crate) cells: Vec<crate::paint_mesh::AtlasCell>,
    /// Original vertex id per atlas vertex — the vertex-paint remap (see mirror sync).
    pub(crate) orig_vids: Vec<u32>,
    /// The atlas mesh's own paint block (imported COLOR_0, carried over at registration;
    /// 0 = the mesh had none). ×1 multiply, like any imported block.
    pub(crate) mesh_vp: u32,
    /// Atlas-ordered MIRROR of the node's brush vertex-paint block (0 = none). Rebuilt by
    /// `sync_tex_paint_mirrors` whenever vertex paint changes; ×2 modulate, like the brush.
    pub(crate) node_vp: u32,
}

/// A node's texture paint: one entry per mesh part.
pub(crate) struct PaintTex {
    pub(crate) parts: Vec<PaintPartTex>,
    /// The `Editor::vpaint_epoch` value the vertex-paint mirrors were last built at.
    /// Starts at `u64::MAX` (never a real epoch) so the first sync always runs.
    pub(crate) mirror_epoch: u64,
}

impl Editor {
    /// The node's texture-paint id, assigning a fresh one (and the component) on first use.
    /// The next id is `max(existing) + 1`, queried live (like `next_paint_id`) — so it never
    /// collides with ids loaded from a scene.
    fn ensure_tex_paint_id(&mut self, e: Entity) -> u32 {
        if let Some(tp) = self.world.get::<TexturePaint>(e) {
            return tp.id;
        }
        let id = self
            .world
            .query::<TexturePaint>()
            .map(|(_, p)| p.id)
            .max()
            .map_or(1, |m| m + 1);
        self.world.insert(e, TexturePaint { id });
        id
    }

    /// Build a node's per-part paint textures + atlas meshes on first paint. The canvas is
    /// fully TRANSPARENT — an overlay over the node's ordinary render, which never changes.
    /// Returns the id, or `None` if the node has no paintable mesh. `pub(crate)` so a scene
    /// reload can rebuild the (deterministic) atlas before overwriting it with saved pixels.
    pub(crate) fn ensure_paint_tex(&mut self, e: Entity, key: &str) -> Option<u32> {
        let id = self.ensure_tex_paint_id(e);
        if self.paint_tex.contains_key(&id) {
            return Some(id);
        }
        let parts = self.paint_meshes.part_count(key);
        if parts == 0 {
            return None;
        }
        // Build atlases before the raster borrow.
        let atlases: Vec<MeshAtlas> =
            (0..parts).filter_map(|p| self.paint_meshes.atlas_mesh(key, p)).collect();
        if atlases.len() != parts {
            return None; // a part had no triangles — bail rather than misalign
        }
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return None;
        };
        let mut pt = PaintTex { parts: Vec::with_capacity(parts), mirror_epoch: u64::MAX };
        for atlas in atlases {
            let edge = atlas.edge;
            let mut pixels = vec![0u8; (edge * edge * 4) as usize];
            for px in pixels.chunks_exact_mut(4) {
                px.copy_from_slice(&CLEAR_TEXEL);
            }
            let tex = raster.register_texture(
                gpu,
                &TextureData { pixels: pixels.clone(), width: edge, height: edge },
                PAINT_SAMPLING,
            );
            // Registering with the atlas's remapped COLOR_0 allocates its paint block, so
            // the overlay's paint is shaded by the mesh's imported per-vertex look.
            let mesh_id = raster.register(gpu, &atlas.mesh, None);
            let mesh_vp = raster.mesh_paint_base(mesh_id);
            pt.parts.push(PaintPartTex {
                pixels,
                edge,
                tex,
                atlas: mesh_id,
                cells: atlas.cells,
                orig_vids: atlas.orig_vids,
                mesh_vp,
                node_vp: 0,
            });
        }
        self.paint_tex.insert(id, pt);
        Some(id)
    }

    /// Keep each texture-painted node's vertex paint shading its OVERLAY: the node's brush
    /// block is indexed by ORIGINAL vertex id, but the atlas mesh has its own (unshared)
    /// vertices — so an atlas-ordered MIRROR block is maintained per part and remapped
    /// through `orig_vids` whenever vertex paint changes (`vpaint_epoch` bumps on every
    /// mutation: dab, fill, clear, undo, reload). Runs once per frame; a no-op when nothing
    /// changed.
    pub(crate) fn sync_tex_paint_mirrors(&mut self) {
        let epoch = self.vpaint_epoch;
        let painted: Vec<(Entity, u32)> = self
            .world
            .query::<TexturePaint>()
            .map(|(e, tp)| (e, tp.id))
            .collect();
        for (e, id) in painted {
            if self.paint_tex.get(&id).is_none_or(|pt| pt.mirror_epoch == epoch) {
                continue;
            }
            // The node's brush vertex-paint blocks (per part), if it has any.
            let src = self
                .world
                .get::<floptle_core::VertexPaint>(e)
                .copied()
                .and_then(|vp| self.paint_data.get(&vp.id).cloned());
            let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
                return;
            };
            let Some(pt) = self.paint_tex.get_mut(&id) else { continue };
            pt.mirror_epoch = epoch;
            for (i, pp) in pt.parts.iter_mut().enumerate() {
                let Some(&(sbase, scount)) = src.as_ref().and_then(|b| b.parts.get(i)) else {
                    pp.node_vp = 0; // no brush paint (any more) — fall back to COLOR_0
                    continue;
                };
                let n = pp.orig_vids.len() as u32;
                if n == 0 {
                    continue;
                }
                if pp.node_vp == 0 {
                    pp.node_vp = raster.paint_alloc(gpu, n, crate::vertex_paint::NEUTRAL_PAINT);
                    if pp.node_vp == 0 {
                        continue; // store full — alloc_paint already logged
                    }
                }
                for (j, &vid) in pp.orig_vids.iter().enumerate() {
                    let c = if vid < scount {
                        raster.paint_get(sbase, vid)
                    } else {
                        crate::vertex_paint::NEUTRAL_PAINT
                    };
                    raster.paint_set(pp.node_vp, j as u32, c);
                }
                raster.paint_flush(gpu, pp.node_vp, 0, n - 1);
            }
        }
    }

    /// Stamp one texture-paint dab onto ONE node — but onto EVERY part and triangle the
    /// brush sphere (`center`, `radius`) touches, weighted by each texel's reconstructed
    /// surface point's distance to the cursor. Combined with the stroke driver calling this
    /// for every node in the sphere, a dab at a wall-floor corner shades BOTH surfaces in
    /// one pass, darkest at the seam — painted ambient occlusion, the retro baked look.
    /// `center` is the cursor hit and `model` maps object → the same (camera-relative) space.
    ///
    /// `view` is the camera ray direction: triangles facing AWAY are skipped (unless the
    /// brush's back-faces switch is on), so the sphere can't bleed through a thin wall onto
    /// its far side — the same rule the vertex brush applies per vertex.
    ///
    /// Paint deposits color AND alpha (coverage) — the overlay blends over the base by that
    /// alpha, so a soft brush edge fades the paint out over the untouched surface. ⌫ Erase
    /// pulls the alpha back toward zero, revealing the live base.
    pub(crate) fn texture_paint_dab(
        &mut self,
        e: Entity,
        key: &str,
        center: Vec3,
        model: Mat4,
        radius: f32,
        view: Vec3,
    ) {
        let Some(id) = self.ensure_paint_tex(e, key) else { return };
        let brush = self.vertex_brush;
        let erase = brush.mode == crate::paint_ui::PaintMode::Erase;
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else { return };
        let Some(pt) = self.paint_tex.get_mut(&id) else { return };
        let src = [
            (brush.color[0] * 255.0).round(),
            (brush.color[1] * 255.0).round(),
            (brush.color[2] * 255.0).round(),
        ];
        let src_a = (brush.alpha * 255.0).round();
        let r2 = radius * radius;
        for pp in pt.parts.iter_mut() {
            let edge = pp.edge;
            let tex = pp.tex;
            // Disjoint field borrows: the cell list (read) and the pixel buffer (written).
            let cells = &pp.cells;
            let pixels = &mut pp.pixels;
            let mut dirty: Option<[u32; 4]> = None;
            for cell in cells {
                let p0 = model.transform_point3(Vec3::from(cell.pos[0]));
                let p1 = model.transform_point3(Vec3::from(cell.pos[1]));
                let p2 = model.transform_point3(Vec3::from(cell.pos[2]));
                // Sphere vs triangle AABB — the cheap reject that keeps this O(touched tris).
                let amin = p0.min(p1).min(p2);
                let amax = p0.max(p1).max(p2);
                if (center.clamp(amin, amax) - center).length_squared() > r2 {
                    continue;
                }
                // Facing gate: the sphere reaches through thin geometry, so a wall's far
                // side is in range — skip triangles pointing away from the camera.
                if !brush.backfaces && (p1 - p0).cross(p2 - p0).dot(view) > 0.0 {
                    continue;
                }
                crate::paint_mesh::for_each_cell_texel(cell, edge, |idx, bary| {
                    let wp = p0 * bary[0] + p1 * bary[1] + p2 * bary[2];
                    let d = (wp - center).length();
                    if d > radius {
                        return;
                    }
                    let w = brush.strength * brush.profile.weight(d, radius);
                    if w <= 0.0 {
                        return;
                    }
                    let cur_a = pixels[idx + 3] as f32;
                    if erase {
                        // Alpha toward zero; the rgb underneath is irrelevant once invisible.
                        pixels[idx + 3] = (cur_a * (1.0 - w)).round().clamp(0.0, 255.0) as u8;
                    } else {
                        for (ch, &s) in src.iter().enumerate() {
                            if !brush.channels[ch] {
                                continue;
                            }
                            let cur = pixels[idx + ch] as f32;
                            // The blend mode composes within the PAINT layer. A texel
                            // receiving its first paint takes the color directly — lerping
                            // from the clear texel's white would haze soft brush edges.
                            let target = brush.blend.apply(cur, s);
                            pixels[idx + ch] = if cur_a <= 0.0 {
                                target.round().clamp(0.0, 255.0) as u8
                            } else {
                                (cur + (target - cur) * w).round().clamp(0.0, 255.0) as u8
                            };
                        }
                        if brush.channels[3] {
                            // Coverage always MIXES toward the brush alpha — a multiply/
                            // darken against a transparent (0) texel could never deposit.
                            pixels[idx + 3] =
                                (cur_a + (src_a - cur_a) * w).round().clamp(0.0, 255.0) as u8;
                        }
                    }
                    let texel = idx as u32 / 4;
                    let (tx, ty) = (texel % edge, texel / edge);
                    dirty = Some(match dirty {
                        None => [tx, ty, tx, ty],
                        Some([x0, y0, x1, y1]) => [x0.min(tx), y0.min(ty), x1.max(tx), y1.max(ty)],
                    });
                });
            }
            // Upload only the dirty rect — re-sending the whole (up to 2048²) atlas per dab
            // would stutter the brush.
            if let Some([x0, y0, x1, y1]) = dirty {
                let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
                let mut sub = Vec::with_capacity((w * h * 4) as usize);
                for ty in y0..=y1 {
                    let row = ((ty * edge + x0) * 4) as usize;
                    sub.extend_from_slice(&pixels[row..row + (w * 4) as usize]);
                }
                raster.update_texture_region(gpu, tex, x0, y0, w, h, &sub);
            }
        }
    }

    /// The paint-image bytes of every part (for an undo snapshot).
    pub(crate) fn tex_paint_snapshot(&self, id: u32) -> Option<Vec<Vec<u8>>> {
        self.paint_tex.get(&id).map(|pt| pt.parts.iter().map(|p| p.pixels.clone()).collect())
    }

    /// Restore paint images from an undo snapshot and re-upload them.
    pub(crate) fn tex_paint_restore(&mut self, id: u32, snap: &[Vec<u8>]) {
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else { return };
        let Some(pt) = self.paint_tex.get_mut(&id) else { return };
        for (pp, bytes) in pt.parts.iter_mut().zip(snap) {
            if bytes.len() == pp.pixels.len() {
                pp.pixels.clone_from(bytes);
                raster.update_texture(
                    gpu,
                    pp.tex,
                    &TextureData { pixels: pp.pixels.clone(), width: pp.edge, height: pp.edge },
                );
            }
        }
    }

    /// Flood the selected nodes' paint textures (the ▦ Texture target's "Fill selected").
    /// One uniform DAB over every texel: the brush STRENGTH is the deposit weight, so
    /// strength 1.0 floods solid color while 0.3 lays a 30% translucent wash over the base
    /// — and repeated fills deepen it, exactly like repeated strokes. Blend mode + channel
    /// mask apply as in a dab. With ⌫ Erase active it FADES all paint by the strength
    /// instead (full strength removes it — the base shows through). One undo step per node
    /// — `None` pre-state when the node wasn't painted, so undo removes the paint entirely.
    pub(crate) fn tex_fill_selected(&mut self) {
        if self.playing {
            return;
        }
        let brush = self.vertex_brush;
        let erase = brush.mode == crate::paint_ui::PaintMode::Erase;
        let w = brush.strength.clamp(0.0, 1.0);
        let src = [
            (brush.color[0] * 255.0).round(),
            (brush.color[1] * 255.0).round(),
            (brush.color[2] * 255.0).round(),
        ];
        let src_a = (brush.alpha * 255.0).round();
        let sel: Vec<Entity> = self.selection.clone();
        for e in sel {
            // Erase-filling a node with no paint is a no-op — don't build a canvas for it.
            if erase && self.world.get::<TexturePaint>(e).is_none() {
                continue;
            }
            let Some(key) = self.ensure_paint_mesh_pub(e) else { continue };
            let pre = self
                .world
                .get::<TexturePaint>(e)
                .map(|p| p.id)
                .and_then(|id| self.tex_paint_snapshot(id));
            let Some(id) = self.ensure_paint_tex(e, &key) else { continue };
            {
                let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
                    return;
                };
                let Some(pt) = self.paint_tex.get_mut(&id) else { continue };
                for pp in pt.parts.iter_mut() {
                    for px in pp.pixels.chunks_exact_mut(4) {
                        let cur_a = px[3] as f32;
                        if erase {
                            px[3] = (cur_a * (1.0 - w)).round().clamp(0.0, 255.0) as u8;
                            continue;
                        }
                        // The dab's own math with a uniform weight (see texture_paint_dab).
                        for (ch, &s) in src.iter().enumerate() {
                            if !brush.channels[ch] {
                                continue;
                            }
                            let cur = px[ch] as f32;
                            let target = brush.blend.apply(cur, s);
                            px[ch] = if cur_a <= 0.0 {
                                target.round().clamp(0.0, 255.0) as u8
                            } else {
                                (cur + (target - cur) * w).round().clamp(0.0, 255.0) as u8
                            };
                        }
                        if brush.channels[3] {
                            px[3] = (cur_a + (src_a - cur_a) * w).round().clamp(0.0, 255.0) as u8;
                        }
                    }
                    raster.update_texture(
                        gpu,
                        pp.tex,
                        &TextureData { pixels: pp.pixels.clone(), width: pp.edge, height: pp.edge },
                    );
                }
            }
            self.push_history(crate::Snapshot::TexPaint(vec![(id, pre)]));
        }
    }

    /// Overwrite a built part's pixels wholesale (a scene reload dropping saved paint onto
    /// the freshly rebuilt atlas). Fails — leaving the clear canvas — if the layout no
    /// longer matches (`edge`/size mismatch), so stale paint can't scramble a changed mesh.
    pub(crate) fn overwrite_tex_paint_part(&mut self, id: u32, part: usize, edge: u32, pixels: Vec<u8>) -> bool {
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else { return false };
        let Some(pt) = self.paint_tex.get_mut(&id) else { return false };
        let Some(pp) = pt.parts.get_mut(part) else { return false };
        if pp.edge != edge || pixels.len() != pp.pixels.len() {
            return false;
        }
        pp.pixels = pixels;
        raster.update_texture(
            gpu,
            pp.tex,
            &TextureData { pixels: pp.pixels.clone(), width: pp.edge, height: pp.edge },
        );
        true
    }

    /// Remove a node's texture paint entirely (the ✖ Clear button / undoing the first dab):
    /// drop the images + component so nothing overlays the node's original render.
    pub(crate) fn clear_texture_paint(&mut self, e: Entity) {
        if let Some(tp) = self.world.get::<TexturePaint>(e).copied() {
            self.paint_tex.remove(&tp.id);
            self.world.remove::<TexturePaint>(e);
        }
    }
}

/// Push a texture-painted node's paint OVERLAY: each part's atlas mesh with its paint
/// texture, coplanar over the base (which the caller draws normally — the base look never
/// changes). The instance alpha rides just under the opaque cutoff so the overlay routes to
/// the alpha-blended transparent pass; unpainted texels have zero alpha and show nothing.
/// A FREE function taking explicit fields — the render loop has `self.raster` borrowed out,
/// so no `&self` method may run there (the `push_terrain_instances` pattern).
pub(crate) fn push_painted_node(
    world: &floptle_core::World,
    paint_tex: &std::collections::HashMap<u32, PaintTex>,
    e: Entity,
    model: Mat4,
    base_mat: &floptle_render::MaterialParams,
    instances: &mut Vec<(MeshId, Option<TexId>, floptle_render::InstanceRaw)>,
) {
    let Some(id) = world.get::<TexturePaint>(e).map(|p| p.id) else { return };
    let Some(pt) = paint_tex.get(&id) else { return };
    // The atlas UVs are a direct 1:1 map, so any material tiling/triplanar must be OFF for
    // the overlay (the base underneath keeps its own tiling). Colour / unlit stay, so the
    // paint shades like the surface it covers.
    let mut mp = *base_mat;
    mp.tile_mode = 0;
    mp.tile = [0.0; 4];
    mp.tile_rotation = 0.0;
    mp.alpha = mp.alpha.min(OVERLAY_ALPHA);
    for pp in &pt.parts {
        // Vertex paint shades the overlay exactly like the base — same precedence as the
        // normal mesh path (`push_mesh_instances::painted`): the node's brush block (×2
        // modulate) wins, else the mesh's imported COLOR_0 (×1), else white.
        mp.paint_modulate = pp.node_vp != 0;
        mp.paint_base = if pp.node_vp != 0 { pp.node_vp } else { pp.mesh_vp };
        instances.push((pp.atlas, Some(pp.tex), floptle_render::instance_of_mat(model, &mp)));
    }
}

#[cfg(test)]
mod tests {
    use crate::paint_mesh::{for_each_cell_texel, AtlasCell, PaintMeshCache};
    use floptle_core::math::Vec3;
    use floptle_render::{MeshData, TextureData, Vertex};

    /// A flat plane subdivided into `n × n` quads over world `[0, size]²`, with UVs that TILE
    /// (`uv = pos / tile`, so they exceed 1) — the exact case that made mesh-UV painting
    /// repeat. Triangles are varied in size across the plane so a single density couldn't
    /// suit them all.
    fn plane(n: u32, size: f32, tile: f32) -> MeshData {
        let mut d = MeshData::default();
        // Non-uniform grid lines (some cells wide, some narrow) → stretched triangles.
        let line = |i: u32| {
            let f = i as f32 / n as f32;
            // ease the spacing so cell widths vary
            (f + 0.35 * (f * std::f32::consts::TAU).sin() / std::f32::consts::TAU).clamp(0.0, 1.0) * size
        };
        let vert = |x: f32, y: f32| Vertex { pos: [x, y, 0.0], normal: [0.0, 0.0, 1.0], uv: [x / tile, y / tile] };
        for gy in 0..n {
            for gx in 0..n {
                let (x0, x1) = (line(gx), line(gx + 1));
                let (y0, y1) = (line(gy), line(gy + 1));
                let b = d.vertices.len() as u32;
                d.vertices.extend([vert(x0, y0), vert(x1, y0), vert(x1, y1), vert(x0, y1)]);
                d.indices.extend([b, b + 1, b + 2, b, b + 2, b + 3]);
            }
        }
        d
    }

    /// A checkerboard base texture — pixel-art whose texel angle/scale must NOT change.
    fn checker(edge: u32, cells: u32) -> TextureData {
        let mut pixels = vec![0u8; (edge * edge * 4) as usize];
        for y in 0..edge {
            for x in 0..edge {
                let on = ((x * cells / edge) + (y * cells / edge)).is_multiple_of(2);
                let c = if on { [70, 90, 160, 255] } else { [200, 205, 220, 255] };
                let i = ((y * edge + x) * 4) as usize;
                pixels[i..i + 4].copy_from_slice(&c);
            }
        }
        TextureData { pixels, width: edge, height: edge }
    }

    /// 2D barycentric of `(px, py)` in the triangle's `xy` (the plane is at z=0), or `None`
    /// when outside. Used by the forward render to map a screen point to a cell.
    fn bary2d(px: f32, py: f32, a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> Option<[f32; 3]> {
        let det = (b[1] - c[1]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[1] - c[1]);
        if det.abs() < 1e-12 {
            return None;
        }
        let l0 = ((b[1] - c[1]) * (px - c[0]) + (c[0] - b[0]) * (py - c[1])) / det;
        let l1 = ((c[1] - a[1]) * (px - c[0]) + (a[0] - c[0]) * (py - c[1])) / det;
        let l2 = 1.0 - l0 - l1;
        (l0 >= -1e-4 && l1 >= -1e-4 && l2 >= -1e-4).then_some([l0, l1, l2])
    }

    /// Nearest-sample an RGBA8 image with REPEAT wrap (what the base pass does).
    fn sample_wrap(img: &TextureData, u: f32, v: f32) -> [u8; 4] {
        let (w, h) = (img.width.max(1), img.height.max(1));
        let x = (((u - u.floor()) * w as f32) as u32).min(w - 1);
        let y = (((v - v.floor()) * h as f32) as u32).min(h - 1);
        let i = ((y * w + x) * 4) as usize;
        [img.pixels[i], img.pixels[i + 1], img.pixels[i + 2], img.pixels[i + 3]]
    }

    /// The seam fix, at the CPU level: a world-space dab straddling a shared triangle edge
    /// must reach BOTH triangles' cells, so paint flows across the edge instead of stopping
    /// at it.
    #[test]
    fn a_dab_on_a_shared_edge_reaches_both_triangles() {
        let mut cache = PaintMeshCache::default();
        // Two triangles sharing the diagonal from (-1,-1) to (1,1).
        let v = |x: f32, y: f32| Vertex { pos: [x, y, 0.0], normal: [0.0, 0.0, 1.0], uv: [0.0; 2] };
        cache.get_or_build("q", || {
            vec![MeshData {
                vertices: vec![v(-1.0, -1.0), v(1.0, -1.0), v(1.0, 1.0), v(-1.0, 1.0)],
                indices: vec![0, 1, 2, 0, 2, 3],
                colors: None,
            }]
        });
        let atlas = cache.atlas_mesh("q", 0).expect("atlas");
        // The origin lies ON the shared diagonal edge. Both cells must have a texel whose
        // reconstructed surface point sits (nearly) on it.
        let closest_to_origin = |cell: &AtlasCell| {
            let mut best = f32::MAX;
            for_each_cell_texel(cell, atlas.edge, |_, bary| {
                let p = Vec3::from(cell.pos[0]) * bary[0]
                    + Vec3::from(cell.pos[1]) * bary[1]
                    + Vec3::from(cell.pos[2]) * bary[2];
                best = best.min(p.length());
            });
            best
        };
        let d0 = closest_to_origin(&atlas.cells[0]);
        let d1 = closest_to_origin(&atlas.cells[1]);
        assert!(d0 < 0.2 && d1 < 0.2, "shared edge unreachable from both cells: {d0}, {d1}");
    }

    /// THE corner-shading case: a floor and a wall meeting at a right angle (two separate
    /// parts, as they'd be two separate nodes). A brush sphere centred ON the seam must
    /// reach texels of BOTH surfaces, with weight falling off symmetrically — that's what
    /// makes one stroke along the corner shade both sides like baked ambient occlusion.
    #[test]
    fn a_corner_dab_shades_both_the_floor_and_the_wall() {
        let v = |x: f32, y: f32, z: f32, n: [f32; 3]| Vertex { pos: [x, y, z], normal: n, uv: [0.0; 2] };
        // Floor: y = 0 plane, x,z ∈ [0,4]. Wall: x = 0 plane, y,z ∈ [0,4] — meeting along x=y=0.
        let floor = MeshData {
            vertices: vec![
                v(0.0, 0.0, 0.0, [0.0, 1.0, 0.0]),
                v(4.0, 0.0, 0.0, [0.0, 1.0, 0.0]),
                v(4.0, 0.0, 4.0, [0.0, 1.0, 0.0]),
                v(0.0, 0.0, 4.0, [0.0, 1.0, 0.0]),
            ],
            indices: vec![0, 2, 1, 0, 3, 2],
            colors: None,
        };
        let wall = MeshData {
            vertices: vec![
                v(0.0, 0.0, 0.0, [1.0, 0.0, 0.0]),
                v(0.0, 4.0, 0.0, [1.0, 0.0, 0.0]),
                v(0.0, 4.0, 4.0, [1.0, 0.0, 0.0]),
                v(0.0, 0.0, 4.0, [1.0, 0.0, 0.0]),
            ],
            indices: vec![0, 2, 1, 0, 3, 2],
            colors: None,
        };
        let mut cache = PaintMeshCache::default();
        cache.get_or_build("corner", || vec![floor, wall]);
        let center = Vec3::new(0.0, 0.0, 2.0); // on the seam
        let radius = 1.0;
        // For each part: the strongest brush weight any of its texels would receive.
        let max_w = |part: usize| -> f32 {
            let atlas = cache.atlas_mesh("corner", part).expect("atlas");
            let mut best = 0.0f32;
            for cell in &atlas.cells {
                for_each_cell_texel(cell, atlas.edge, |_, bary| {
                    let p = Vec3::from(cell.pos[0]) * bary[0]
                        + Vec3::from(cell.pos[1]) * bary[1]
                        + Vec3::from(cell.pos[2]) * bary[2];
                    let d = (p - center).length();
                    if d <= radius {
                        best = best.max(1.0 - d / radius);
                    }
                });
            }
            best
        };
        // Near-full weight on BOTH (the shortfall from 1.0 is texel granularity — the
        // nearest texel centre sits a fraction of a texel off the seam), and symmetric:
        // corner shading must not favour one side.
        let (wf, ww) = (max_w(0), max_w(1));
        assert!(wf > 0.8, "the floor must get near-full weight at the seam, got {wf}");
        assert!(ww > 0.8, "the wall must get near-full weight at the seam, got {ww}");
        assert!((wf - ww).abs() < 0.15, "corner shading must be symmetric: {wf} vs {ww}");
    }

    /// THE overlay guarantee, forward-rendered to a PNG: the base render is PIXEL-EXACT
    /// wherever paint hasn't landed (the reported bug was the base texture's pixels changing
    /// angle/scale under the old baked canvas), a painted disc blends over it smoothly across
    /// faces, and an erased area returns to exactly the base. Asserted, not just eyeballed.
    #[test]
    fn overlay_leaves_the_base_render_pixel_exact() {
        let mesh = plane(7, 4.0, 1.3);
        let mut cache = PaintMeshCache::default();
        cache.get_or_build("p", || vec![mesh]);
        let atlas = cache.atlas_mesh("p", 0).expect("atlas");
        let edge = atlas.edge;
        let base_tex = checker(64, 8);

        // The paint layer: transparent canvas + a painted disc + an erased disc inside it —
        // the same math texture_paint_dab runs.
        let mut paint = vec![0u8; (edge * edge * 4) as usize];
        for px in paint.chunks_exact_mut(4) {
            px.copy_from_slice(&[255, 255, 255, 0]);
        }
        let dab = |paint: &mut [u8], center: Vec3, radius: f32, color: [f32; 3], erase: bool| {
            for cell in &atlas.cells {
                let p0 = Vec3::from(cell.pos[0]);
                let p1 = Vec3::from(cell.pos[1]);
                let p2 = Vec3::from(cell.pos[2]);
                for_each_cell_texel(cell, edge, |idx, bary| {
                    let wp = p0 * bary[0] + p1 * bary[1] + p2 * bary[2];
                    let d = (wp - center).length();
                    if d > radius {
                        return;
                    }
                    let w = 1.0 - d / radius; // linear falloff
                    let cur_a = paint[idx + 3] as f32;
                    if erase {
                        paint[idx + 3] = (cur_a * (1.0 - w)).round() as u8;
                    } else {
                        for ch in 0..3 {
                            let cur = paint[idx + ch] as f32;
                            let target = color[ch] * 255.0;
                            paint[idx + ch] = if cur_a <= 0.0 {
                                target.round() as u8
                            } else {
                                (cur + (target - cur) * w).round() as u8
                            };
                        }
                        paint[idx + 3] = (cur_a + (255.0 - cur_a) * w).round() as u8;
                    }
                });
            }
        };
        dab(&mut paint, Vec3::new(2.4, 2.2, 0.0), 1.3, [0.05, 0.02, 0.02], false);
        // Erase the middle — a short STROKE (several dabs), like the brush actually delivers:
        // each dab multiplies alpha by (1-w), so a stroke converges to exactly zero.
        for _ in 0..4 {
            dab(&mut paint, Vec3::new(2.4, 2.2, 0.0), 0.5, [0.0; 3], true);
        }

        // Forward render top-down, both passes exactly as the GPU composes them:
        // base = original mesh UVs → tiled texture; overlay = atlas UV → paint, alpha-over.
        let res = 320u32;
        let size = 4.0f32;
        let render = |with_overlay: bool| -> Vec<u8> {
            let mut out = vec![0u8; (res * res * 4) as usize];
            for oy in 0..res {
                for ox in 0..res {
                    let wx = ox as f32 / res as f32 * size;
                    let wy = (res - 1 - oy) as f32 / res as f32 * size;
                    let mut rgba = [30u8, 30, 34, 255];
                    for cell in &atlas.cells {
                        let Some(l) = bary2d(wx, wy, cell.pos[0], cell.pos[1], cell.pos[2]) else {
                            continue;
                        };
                        // BASE pass: the node's own render — original (tiling) UVs.
                        let su = cell.src_uv[0][0] * l[0] + cell.src_uv[1][0] * l[1] + cell.src_uv[2][0] * l[2];
                        let sv = cell.src_uv[0][1] * l[0] + cell.src_uv[1][1] * l[1] + cell.src_uv[2][1] * l[2];
                        rgba = sample_wrap(&base_tex, su, sv);
                        // OVERLAY pass: the paint, alpha-blended over.
                        if with_overlay {
                            let u = cell.uv[0][0] * l[0] + cell.uv[1][0] * l[1] + cell.uv[2][0] * l[2];
                            let v = cell.uv[0][1] * l[0] + cell.uv[1][1] * l[1] + cell.uv[2][1] * l[2];
                            let tx = ((u * edge as f32) as u32).min(edge - 1);
                            let ty = ((v * edge as f32) as u32).min(edge - 1);
                            let i = ((ty * edge + tx) * 4) as usize;
                            let a = paint[i + 3] as f32 / 255.0;
                            for ch in 0..3 {
                                rgba[ch] = (paint[i + ch] as f32 * a
                                    + rgba[ch] as f32 * (1.0 - a))
                                    .round() as u8;
                            }
                        }
                        break;
                    }
                    let o = ((oy * res + ox) * 4) as usize;
                    out[o..o + 4].copy_from_slice(&rgba);
                }
            }
            out
        };
        let base_only = render(false);
        let painted = render(true);

        // THE assertion: every pixel outside the dab is byte-identical to the base render,
        // and the erased center returns to it too. Only the painted ring may differ.
        let px_at = |img: &[u8], wx: f32, wy: f32| {
            let ox = ((wx / size) * res as f32) as u32;
            let oy = res - 1 - ((wy / size) * res as f32) as u32;
            let i = ((oy * res + ox) * 4) as usize;
            [img[i], img[i + 1], img[i + 2]]
        };
        let mut outside_diff = 0usize;
        for oy in 0..res {
            for ox in 0..res {
                let wx = ox as f32 / res as f32 * size;
                let wy = (res - 1 - oy) as f32 / res as f32 * size;
                let dist = ((wx - 2.4).powi(2) + (wy - 2.2).powi(2)).sqrt();
                let i = ((oy * res + ox) * 4) as usize;
                if dist > 1.35 && base_only[i..i + 3] != painted[i..i + 3] {
                    outside_diff += 1;
                }
            }
        }
        assert_eq!(outside_diff, 0, "unpainted pixels must be BYTE-IDENTICAL to the base render");
        assert_eq!(
            px_at(&painted, 2.4, 2.2),
            px_at(&base_only, 2.4, 2.2),
            "the erased center must return to exactly the base"
        );
        assert_ne!(
            px_at(&painted, 2.4, 3.1),
            px_at(&base_only, 2.4, 3.1),
            "the painted ring must actually show paint"
        );

        let path = std::env::temp_dir().join("floptle_paint_probe.png");
        floptle_assets::save_texture_png(
            &TextureData { pixels: painted, width: res, height: res },
            &path,
        )
        .expect("write probe png");
        println!("paint overlay probe atlas edge={edge}, wrote {}", path.display());
    }
}
