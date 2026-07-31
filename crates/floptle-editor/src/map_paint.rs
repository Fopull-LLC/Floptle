//! Carrying a blockout's paint across a geometry edit.
//!
//! Vertex paint is one colour per RENDER vertex and texture paint is one patch
//! per RENDER triangle — and a map mesh re-triangulates from scratch every time
//! you pull a face. So after an extrude, the block that used to be "the top
//! face's four corners" is just four numbers with nothing to attach to; applied
//! blind it lands on whatever now occupies those indices, which is paint on the
//! wrong surfaces. Dropping it instead is honest, and infuriating: touching one
//! wall would clear a level's shading.
//!
//! So the editor keeps a DURABLE NAME for every render vertex and triangle:
//!
//! * a face is named by the (sorted) set of mesh vertices it uses, hashed —
//!   stable through face reindexing, which `delete_faces` and `knife` both do;
//! * a vertex is named by `(its face, its mesh vertex)` — corners aren't shared
//!   between faces, so this is exactly one render vertex;
//! * a triangle is named by `(its face, its index within that face's fan)`.
//!
//! After a rebuild the old names are matched against the new ones and the paint
//! follows: move a vertex, assign a slot, cut a face somewhere else, delete a
//! face — the surfaces that survived keep their paint. A face whose vertex set
//! genuinely changed (the top of an extrusion, the halves of a cut) is new
//! geometry and comes back unpainted, which is the honest answer.

use std::collections::HashMap;

use floptle_core::{Entity, TexturePaint, VertexPaint};
use floptle_map::{MapMesh, SlotMesh};

use crate::paint_mesh::AtlasCell;
use crate::vertex_paint::{PaintBlocks, NEUTRAL_PAINT};
use crate::Editor;

/// A face's durable name: a hash of its sorted vertex indices.
type FaceKey = u64;
/// `(face, that face's mesh vertex)` — one render vertex.
type VertKey = (FaceKey, u32);
/// `(face, triangle index within the face's fan)` — one render triangle.
type TriKey = (FaceKey, u32);
/// A texel rectangle in an atlas: `(x, y, w, h)`.
type Rect = (u32, u32, u32, u32);

/// What every render vertex and triangle of one map mesh was, at the moment its
/// GPU parts were last built.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MapPaintIdent {
    /// Per part, per render vertex.
    verts: Vec<Vec<VertKey>>,
    /// Per part, per triangle.
    tris: Vec<Vec<TriKey>>,
}

fn face_key(mesh: &MapMesh, face: u32) -> FaceKey {
    use std::hash::{Hash, Hasher};
    let Some(f) = mesh.faces.get(face as usize) else { return 0 };
    let mut verts = f.verts.clone();
    verts.sort_unstable();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    verts.hash(&mut h);
    h.finish()
}

/// Name every render vertex and triangle of `slots` (which must be the
/// triangulation of `mesh`).
pub(crate) fn ident_of(mesh: &MapMesh, slots: &[SlotMesh]) -> MapPaintIdent {
    // One hash per FACE, not per corner — a 20k-corner blockout would otherwise
    // re-sort and re-hash the same face four times over.
    let keys: Vec<FaceKey> = (0..mesh.faces.len() as u32).map(|f| face_key(mesh, f)).collect();
    let at = |f: u32| keys.get(f as usize).copied().unwrap_or(0);
    let mut out = MapPaintIdent::default();
    for sm in slots {
        out.verts.push(sm.vert_src.iter().map(|&(f, v)| (at(f), v)).collect());
        // The fan index restarts at every new source face; `tri_faces` is
        // grouped by face, so a running counter is enough.
        let mut tris = Vec::with_capacity(sm.tri_faces.len());
        let (mut prev, mut n) = (u32::MAX, 0u32);
        for &f in &sm.tri_faces {
            n = if f == prev { n + 1 } else { 0 };
            prev = f;
            tris.push((at(f), n));
        }
        out.tris.push(tris);
    }
    out
}

/// The texel rectangle a triangle's patch occupies, grown by one texel on each
/// side to include the edge dilation the brush writes (`for_each_cell_texel`).
fn cell_rect(cell: &AtlasCell, edge: u32) -> Rect {
    let e = edge as f32;
    let xs = cell.uv.map(|u| u[0] * e);
    let ys = cell.uv.map(|u| u[1] * e);
    let lo_x = (xs.iter().copied().fold(f32::MAX, f32::min) - 1.0).floor().max(0.0) as u32;
    let hi_x = (xs.iter().copied().fold(f32::MIN, f32::max) + 1.0).ceil().min(e - 1.0) as u32;
    let lo_y = (ys.iter().copied().fold(f32::MAX, f32::min) - 1.0).floor().max(0.0) as u32;
    let hi_y = (ys.iter().copied().fold(f32::MIN, f32::max) + 1.0).ceil().min(e - 1.0) as u32;
    (lo_x, lo_y, hi_x.saturating_sub(lo_x) + 1, hi_y.saturating_sub(lo_y) + 1)
}

impl Editor {
    /// The node carrying map geometry `id`, if it is still in the scene.
    fn map_entity(&self, id: u32) -> Option<Entity> {
        self.world.query::<floptle_core::Matter>().find_map(|(e, m)| match m {
            floptle_core::Matter::MapMesh { id: i } if *i == id => Some(e),
            _ => None,
        })
    }

    /// Re-attach paint to the map meshes rebuilt this frame. Called straight
    /// after `sync_map_meshes`, which is the only place map geometry reaches
    /// the GPU.
    pub(crate) fn sync_map_paint(&mut self) {
        if self.maps.paint_stale.is_empty() {
            return;
        }
        for id in std::mem::take(&mut self.maps.paint_stale) {
            let key = crate::map_edit::map_key(id);
            // The CPU geometry the brush raycasts is stale whatever changed —
            // even a pure move — so it always goes. It rebuilds lazily on the
            // next dab.
            self.paint_meshes.remove(&key);
            let Some(e) = self.map_entity(id) else {
                self.maps.paint_ident.remove(&id);
                continue;
            };
            let painted = self.world.get::<VertexPaint>(e).is_some()
                || self.world.get::<TexturePaint>(e).is_some();
            if !painted {
                // Nothing to carry — and naming every corner of every blockout
                // in the level on every drag frame would be pure waste.
                self.maps.paint_ident.remove(&id);
                continue;
            }
            let Some(mesh) = self.maps.meshes.get(&id) else { continue };
            let new = ident_of(mesh, &floptle_map::triangulate(mesh));
            let Some(old) = self.maps.paint_ident.insert(id, new.clone()) else {
                continue; // first build of this mesh's paint — nothing to carry from
            };
            if old == new {
                continue; // the vertices moved, but they are the same vertices
            }
            // Rebuild the CPU geometry we just dropped: the texture atlas is
            // derived from it, and deriving it from nothing would leave the old
            // paint sitting on the new surface.
            self.ensure_paint_mesh_pub(e);
            self.carry_map_vertex_paint(e, &old, &new);
            self.carry_map_texture_paint(e, &key, &old, &new);
        }
    }

    /// Rebuild the node's vertex-paint blocks against the new triangulation,
    /// keeping every colour whose vertex survived.
    fn carry_map_vertex_paint(&mut self, e: Entity, old: &MapPaintIdent, new: &MapPaintIdent) {
        let Some(vp) = self.world.get::<VertexPaint>(e).copied() else { return };
        let Some(blocks) = self.paint_data.get(&vp.id).cloned() else { return };
        let mut by_key: HashMap<VertKey, [u8; 4]> = HashMap::new();
        {
            let Some(raster) = self.raster.as_ref() else { return };
            for (p, keys) in old.verts.iter().enumerate() {
                let Some(&(base, count)) = blocks.parts.get(p) else { continue };
                for (i, k) in keys.iter().enumerate() {
                    if (i as u32) < count {
                        by_key.entry(*k).or_insert_with(|| raster.paint_get(base, i as u32));
                    }
                }
            }
        }
        let mut out = PaintBlocks::default();
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else { return };
        for (p, keys) in new.verts.iter().enumerate() {
            let colors: Vec<[u8; 4]> =
                keys.iter().map(|k| by_key.get(k).copied().unwrap_or(NEUTRAL_PAINT)).collect();
            match blocks.parts.get(p) {
                // Same vertex count: rewrite in place. Blocks are bump-allocated
                // and never freed, so re-allocating one per edit of a mesh you
                // are actively dragging would grow the paint buffer forever.
                Some(&(base, count)) if count as usize == colors.len() => {
                    raster.paint_restore(gpu, base, &colors);
                    out.parts.push((base, count));
                }
                _ => {
                    let base = raster.paint_alloc_from(gpu, &colors);
                    if base == 0 {
                        return; // store full — alloc_paint already logged
                    }
                    out.parts.push((base, colors.len() as u32));
                }
            }
        }
        self.paint_data.insert(vp.id, out);
        self.vpaint_epoch += 1; // the texture-paint mirrors index into these
    }

    /// Rebuild the node's paint atlases against the new triangulation, copying
    /// each surviving triangle's patch of pixels into its new home.
    fn carry_map_texture_paint(
        &mut self,
        e: Entity,
        key: &str,
        old: &MapPaintIdent,
        new: &MapPaintIdent,
    ) {
        let Some(tp) = self.world.get::<TexturePaint>(e).copied() else { return };
        if !self.paint_tex.contains_key(&tp.id) {
            return;
        }
        // Where every old triangle's pixels are: (part, texel rect).
        let mut src: HashMap<TriKey, (usize, Rect)> = HashMap::new();
        {
            let Some(pt) = self.paint_tex.get(&tp.id) else { return };
            for (p, keys) in old.tris.iter().enumerate() {
                let Some(pp) = pt.parts.get(p) else { continue };
                for (t, k) in keys.iter().enumerate() {
                    let Some(cell) = pp.cells.get(t) else { continue };
                    src.entry(*k).or_insert((p, cell_rect(cell, pp.edge)));
                }
            }
        }
        let atlases: Vec<crate::paint_mesh::MeshAtlas> = (0..new.tris.len())
            .filter_map(|p| self.paint_meshes.atlas_mesh(key, p))
            .collect();
        if atlases.len() != new.tris.len() {
            return; // a part had no triangles — leave the old paint rather than misalign it
        }
        let Some(mut pt) = self.paint_tex.remove(&tp.id) else { return };
        let old_parts = std::mem::take(&mut pt.parts);
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            self.paint_tex.insert(tp.id, pt);
            return;
        };
        for (p, atlas) in atlases.into_iter().enumerate() {
            let edge = atlas.edge;
            let mut pixels = vec![0u8; (edge * edge * 4) as usize];
            for px in pixels.chunks_exact_mut(4) {
                px.copy_from_slice(&crate::paint_tex::CLEAR_TEXEL);
            }
            // Copy each surviving triangle's patch across. A triangle whose face
            // is unchanged has the same flattened size, so its patch has the
            // same texel size — only its place in the packing moved.
            for (t, k) in new.tris[p].iter().enumerate() {
                let (Some(&(sp, (sx, sy, sw, sh))), Some(cell)) = (src.get(k), atlas.cells.get(t))
                else {
                    continue;
                };
                let (Some(from), (dx, dy, dw, dh)) = (old_parts.get(sp), cell_rect(cell, edge))
                else {
                    continue;
                };
                if (sw, sh) != (dw, dh) {
                    continue; // the triangle changed shape — its paint no longer describes it
                }
                for row in 0..dh {
                    let s = (((sy + row) * from.edge + sx) * 4) as usize;
                    let d = (((dy + row) * edge + dx) * 4) as usize;
                    let n = (dw * 4) as usize;
                    if s + n <= from.pixels.len() && d + n <= pixels.len() {
                        pixels[d..d + n].copy_from_slice(&from.pixels[s..s + n]);
                    }
                }
            }
            // Reuse the old part's GPU handles wherever they still fit —
            // textures can't be freed at all, and a leaked atlas per edit of a
            // painted wall adds up fast.
            let data = floptle_render::TextureData { pixels: pixels.clone(), width: edge, height: edge };
            let tex = match old_parts.get(p).filter(|o| o.edge == edge) {
                Some(o) => {
                    raster.update_texture(gpu, o.tex, &data);
                    o.tex
                }
                None => raster.register_texture(gpu, &data, crate::paint_tex::PAINT_SAMPLING),
            };
            let mesh_id = match old_parts.get(p) {
                Some(o) if raster.replace_dynamic(gpu, o.atlas, &atlas.mesh) => o.atlas,
                _ => {
                    let id = raster.register_dynamic(
                        gpu,
                        atlas.mesh.vertices.len() as u32,
                        atlas.mesh.indices.len() as u32,
                        false,
                    );
                    raster.replace_dynamic(gpu, id, &atlas.mesh);
                    id
                }
            };
            pt.parts.push(crate::paint_tex::PaintPartTex {
                pixels,
                edge,
                tex,
                atlas: mesh_id,
                cells: atlas.cells,
                orig_vids: atlas.orig_vids,
                mesh_vp: 0, // blockout geometry carries no imported COLOR_0
                node_vp: 0, // rebuilt by the mirror sync, at the new vertex count
            });
        }
        // Parts that no longer exist hand their mesh slots back.
        for (p, o) in old_parts.iter().enumerate() {
            if p >= pt.parts.len() || pt.parts[p].atlas != o.atlas {
                raster.free_dynamic(o.atlas);
            }
        }
        pt.mirror_epoch = u64::MAX; // force the vertex-paint mirror to rebuild
        self.paint_tex.insert(tp.id, pt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_map::box_mesh;
    use floptle_core::math::Vec3;

    fn ident(m: &MapMesh) -> MapPaintIdent {
        ident_of(m, &floptle_map::triangulate(m))
    }

    /// Moving a vertex must NOT change any name — that is what stops a gizmo
    /// drag from rebuilding (and losing) the paint 60 times a second.
    #[test]
    fn moving_a_vertex_leaves_every_name_alone() {
        let m = box_mesh(Vec3::ONE);
        let before = ident(&m);
        let mut moved = m.clone();
        floptle_map::translate_verts(&mut moved, &[0, 1], Vec3::new(0.0, 3.0, 0.0));
        assert_eq!(ident(&moved), before);
    }

    /// Re-assigning faces to another material slot re-groups the parts, and the
    /// paint has to follow the FACE into its new part rather than staying at an
    /// index that now draws something else.
    #[test]
    fn a_face_keeps_its_name_when_it_moves_to_another_slot() {
        let mut m = box_mesh(Vec3::ONE);
        let before = ident(&m);
        m.slots.push("Trim".into());
        floptle_map::set_face_slot(&mut m, &[2], 1);
        let after = ident(&m);
        assert_eq!(after.verts.len(), 2, "two slots, two parts");
        // Every name in the new parts existed before; none was invented.
        let old: std::collections::HashSet<VertKey> =
            before.verts.iter().flatten().copied().collect();
        let new: std::collections::HashSet<VertKey> = after.verts.iter().flatten().copied().collect();
        assert_eq!(old, new);
    }

    /// Cutting one face renames only that face — the other five keep their
    /// paint.
    #[test]
    fn a_cut_only_renames_the_face_it_cut() {
        let mut m = box_mesh(Vec3::ONE);
        let before: std::collections::HashSet<VertKey> =
            ident(&m).verts.iter().flatten().copied().collect();
        let ring = m.faces[0].verts.clone();
        floptle_map::knife(
            &mut m,
            0,
            floptle_map::CutPoint::Vert(ring[0]),
            floptle_map::CutPoint::Vert(ring[2]),
        )
        .unwrap();
        let after: std::collections::HashSet<VertKey> =
            ident(&m).verts.iter().flatten().copied().collect();
        let kept = before.intersection(&after).count();
        // 6 quads = 24 corners; the cut face's 4 are renamed, the other 20 stay.
        assert_eq!(before.len(), 24);
        assert_eq!(kept, 20, "only the cut face should lose its paint");
    }

    /// Deleting a face reindexes every face after it. Names must survive that —
    /// an index-based identity would shift the whole level's paint by one.
    #[test]
    fn deleting_a_face_does_not_shift_the_others_names() {
        let mut m = box_mesh(Vec3::ONE);
        let before = ident(&m);
        let first: Vec<VertKey> = before.verts[0][..4].to_vec();
        floptle_map::delete_faces(&mut m, &[5]);
        let after: std::collections::HashSet<VertKey> =
            ident(&m).verts.iter().flatten().copied().collect();
        for k in first {
            assert!(after.contains(&k), "face 0's corners must keep their names");
        }
    }

    /// Every triangle of a face gets its own name, so a five-corner face's three
    /// texture patches don't collapse onto one.
    #[test]
    fn each_triangle_of_a_face_is_named_separately() {
        let m = floptle_map::cylinder(1.0, 1.0, 8);
        let id = ident(&m);
        let all: Vec<TriKey> = id.tris.iter().flatten().copied().collect();
        let unique: std::collections::HashSet<TriKey> = all.iter().copied().collect();
        assert_eq!(all.len(), unique.len(), "triangle names must be unique");
    }
}
