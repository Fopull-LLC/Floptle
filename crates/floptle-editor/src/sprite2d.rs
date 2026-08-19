//! The 2D layer: tilemap meshes and sprite batches (`floptle/0058`).
//!
//! Two node kinds that exist so a 2D game does not have to build a renderer out
//! of scene nodes:
//!
//! * **[`Matter::Tilemap`]** — a grid of sheet cells as ONE mesh. Built here,
//!   uploaded as a dynamic mesh, and rebuilt only when the grid actually
//!   changes. The reason it is a mesh at all rather than one instance per tile
//!   is the seam: see [`floptle_render::mesh::tilemap`].
//! * **[`Matter::SpriteBatch`]** — N quads from one node, each with its own
//!   position, rotation, scale, cell **and tint**. Every one of those already
//!   has a lane in the per-instance stream, so this costs no new vertex
//!   attribute (the raster budget is full at 16/16) and no shader variant.
//!
//! A sprite batch takes its texture and sheet grid from the node's ordinary
//! Material, so a project does not learn a second way to say "this texture,
//! chopped this way". A **tilemap takes them from its tileset** — which is what
//! a tileset is — and falls back to the Material only when the tileset names no
//! sheet, which is every project written before it could. Both reach a custom
//! `.flsl` for free.

use std::collections::HashMap;

use floptle_core::math::DVec3;
use floptle_core::{Entity, Material, Matter, Sprites, World};
use floptle_render::{InstanceRaw, MaterialParams, MeshId, TexId, instance_of_mat};

use crate::Editor;

/// One page's uploaded geometry: the squares of a grid that come from ONE
/// sheet, welded into one mesh, plus the sheet they sample.
pub(crate) struct TilePageGpu {
    pub(crate) mesh: MeshId,
    /// Which page of the tileset this draws — 0 is the layer's own sheet.
    pub(crate) page: u32,
    /// The image, project-relative. Resolved to a `TexId` at gather time so a
    /// texture that finishes loading later is picked up without a rebuild.
    pub(crate) texture: Option<String>,
    /// Whether the mesh holds any triangles. An all-empty page uploads nothing
    /// and drawing it would be a wasted call.
    empty: bool,
}

/// A tilemap's uploaded geometry, and the signature of the grid it was built
/// from — so a map that hasn't changed isn't rebuilt sixty times a second.
///
/// One mesh **per page**. The seam argument that makes a tilemap one mesh is
/// about GEOMETRY — neighbouring quads sharing a bit-identical edge coordinate
/// — and splitting the draw by which sheet a square samples does not touch it:
/// the coordinates are still computed once, by the same expression, in the same
/// builder. What a split costs is one draw call per sheet the layer actually
/// uses, which is bounded by how many sheets a level has and not by how many
/// tiles (`floptle/0092`).
pub(crate) struct TileGpu {
    pub(crate) pages: Vec<TilePageGpu>,
    sig: u64,
}

/// A cheap change signature for a tilemap plus the sheet it is cut from.
///
/// Includes the sheet dimensions and the texel size because the UVs are baked
/// into the mesh: swap the material's sheet and the geometry is stale even
/// though every cell index is identical.
#[allow(clippy::too_many_arguments)]
fn signature(
    cols: u32,
    rows: u32,
    tile: f32,
    data: &[u32],
    sheet: (u32, u32),
    texel: [f32; 2],
    anim_step: u32,
    // Every page's image and cut. A page added, removed, re-pointed or re-cut
    // makes the built meshes stale even though every square is identical —
    // exactly the way the sheet dimensions already do.
    pages: &[(String, u32, u32)],
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (cols, rows, sheet).hash(&mut h);
    pages.hash(&mut h);
    tile.to_bits().hash(&mut h);
    texel[0].to_bits().hash(&mut h);
    texel[1].to_bits().hash(&mut h);
    data.hash(&mut h);
    // The animation STEP, not the clock: a map with animated tiles rebuilds when
    // the frame it shows changes, not sixty times a second. A tilemap with nothing
    // animated reports step 0 forever and is never rebuilt at all — which is what
    // keeps this feature free for the maps that do not use it.
    anim_step.hash(&mut h);
    h.finish()
}

/// Substitute each animated tile's current frame into a copy of the grid.
///
/// `None` when nothing in this map animates — the common case, and the one that
/// must not allocate a copy of the grid every frame.
///
/// The orientation is carried across, so a tile turned by hand keeps its angle
/// through every frame of its animation (what a rotated conveyor needs).
fn animate(data: &[u32], set: &floptle_tiles::TileSet, t: f32) -> Option<Vec<u32>> {
    if !set.animated() {
        return None;
    }
    let mut out: Option<Vec<u32>> = None;
    for (i, &packed) in data.iter().enumerate() {
        if set.is_empty_square(packed) {
            continue;
        }
        let cell = floptle_core::tile_index(packed);
        let Some(info) = set.info(cell) else { continue };
        let frame = info.frame_at(cell, t);
        if frame == cell {
            continue;
        }
        out.get_or_insert_with(|| data.to_vec())[i] =
            floptle_core::tile_pack(frame, floptle_core::tile_xform(packed));
    }
    out
}

/// Where every animated tile in a tileset is in its cycle at time `t`, folded
/// into one number for the rebuild signature.
///
/// One number rather than per-tile phases because the signature only has to
/// CHANGE when the picture does. Two tiles at different rates both advance it
/// whenever either ticks, which rebuilds a few times more than strictly needed
/// and never fewer — the safe direction.
fn anim_step(set: &floptle_tiles::TileSet, t: f32) -> u32 {
    if !t.is_finite() {
        return 0;
    }
    set.tiles
        .values()
        .filter(|i| !i.frames.is_empty() && i.anim_fps > 0.0)
        .map(|i| {
            let n = i.frames.len() as u32 + 1;
            ((t * i.anim_fps).max(0.0) as u32) % n
        })
        .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b))
}

impl Editor {
    /// Rebuild any tilemap whose grid (or sheet) changed, and drop the geometry
    /// of any that no longer exists.
    ///
    /// Called once a frame beside the map-mesh sync, before anything gathers
    /// draw calls, so the registry a gather reads is never a frame behind the
    /// component it came from.
    pub(crate) fn sync_tilemaps(&mut self) {
        // Collect first: building needs `&mut self.raster` and the walk holds
        // `&self.world`.
        let live: Vec<(Entity, u32, u32, f32, Vec<u32>, String)> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Tilemap { cols, rows, tile, data, tileset } => {
                    Some((e, *cols, *rows, *tile, data.clone(), tileset.clone()))
                }
                _ => None,
            })
            .collect();

        // Anything that stopped being a tilemap gives its mesh slot back.
        let alive: std::collections::HashSet<Entity> = live.iter().map(|(e, ..)| *e).collect();
        let gone: Vec<Entity> =
            self.tilemaps.keys().copied().filter(|e| !alive.contains(e)).collect();
        for e in gone {
            if let Some(t) = self.tilemaps.remove(&e)
                && let Some(raster) = self.raster.as_mut()
            {
                for p in t.pages {
                    raster.free_dynamic(p.mesh);
                }
            }
        }
        if live.is_empty() {
            return;
        }
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return; // no GPU yet — try again next frame
        };

        // Animated tiles advance on the EDIT clock as well as the play clock, so a
        // torch flickers while you are placing torches. That is the whole point of
        // authoring animation in the editor rather than discovering it in Play.
        let now = self.started.map(|s| s.elapsed().as_secs_f32()).unwrap_or(0.0);
        for (e, cols, rows, tile, data, tileset) in live {
            let mat = self.world.get::<Material>(e).cloned().unwrap_or_default();
            let set = (!tileset.is_empty()).then(|| self.tiles.get(&tileset)).flatten();
            let step = set.map(|s| anim_step(s, now)).unwrap_or(0);

            // Page 0 comes from the TILESET, and the node's Material is the
            // fallback for a layer whose tileset names no sheet of its own.
            //
            // It used to be the other way round — the material was the
            // authority and the tileset's own `texture` was informational, on
            // the reasoning that a tileset silently repainting a node's art
            // would be worse. The cost of that turned out to be the whole
            // feature: a tileset is *a sheet plus what its cells mean*, so
            // making it describe a sheet it does not draw means every tilemap
            // needs a Material carrying the same image and the same cols/rows,
            // kept in agreement by hand, and a tileset alone renders nothing at
            // all. Reported as "I still have to assign a texture to the
            // material on the tileset for it to register as something I can
            // use", which is precisely the bookkeeping.
            //
            // Nothing is silently repainted: a tileset that names no texture
            // still defers to the material, which is every project written
            // before this.
            let (p0_tex, sc, sr) = match set {
                Some(s) if !s.texture.trim().is_empty() => {
                    (s.texture.clone(), s.sheet_cols.max(1), s.sheet_rows.max(1))
                }
                _ => {
                    let (c, r) = mat.sheet();
                    (mat.texture.clone().unwrap_or_default(), c, r)
                }
            };
            // The texel size is what the half-texel inset is measured in, and it
            // has to be measured on the sheet page 0 ACTUALLY draws. An unloaded
            // texture reports nothing, and the mesh is rebuilt when it arrives
            // because the signature covers it.
            let texel = self
                .texture_registry
                .get(p0_tex.as_str())
                .copied()
                .and_then(|id| raster.texture_size(id))
                .map(|[w, h]| [1.0 / w.max(1.0), 1.0 / h.max(1.0)])
                .unwrap_or([0.0, 0.0]);

            let mut pages: Vec<(String, u32, u32)> = vec![(p0_tex, sc, sr)];
            if let Some(s) = set {
                for (p, tex, c, r) in s.pages_iter() {
                    if p > 0 {
                        pages.push((tex.to_string(), c, r));
                    }
                }
            }
            let sig = signature(cols, rows, tile, &data, (sc, sr), texel, step, &pages);
            if self.tilemaps.get(&e).is_some_and(|t| t.sig == sig) {
                continue;
            }
            // The grid AS DRAWN: the stored squares, with each animated tile's
            // current frame swapped in. `data` on the component is untouched —
            // animation is a VIEW of the map, not an edit to it, and writing the
            // frame back would make a saved scene record whichever moment the
            // artist happened to hit Ctrl-S on.
            let animated = set.and_then(|s| animate(&data, s, now));
            let draw = animated.as_deref().unwrap_or(&data);

            // The slots this entity already owns, to be reused page by page and
            // whatever is left over handed back.
            let mut spare: Vec<MeshId> =
                self.tilemaps.remove(&e).map(|t| t.pages.into_iter().map(|p| p.mesh).collect()).unwrap_or_default();
            let mut built: Vec<TilePageGpu> = Vec::with_capacity(pages.len());
            for (pi, (tex_path, pc, pr)) in pages.iter().enumerate() {
                let page = pi as u32;
                let Some(page_data) = page_squares(draw, page, pc * pr) else {
                    continue; // nothing on this page — no mesh, no draw call
                };
                // Each page's inset is measured in ITS OWN texels; page 0's was
                // measured above, on whichever sheet it resolved to.
                let ptexel = if page == 0 {
                    texel
                } else {
                    self.texture_registry
                        .get(tex_path.as_str())
                        .copied()
                        .and_then(|id| raster.texture_size(id))
                        .map(|[w, h]| [1.0 / w.max(1.0), 1.0 / h.max(1.0)])
                        .unwrap_or([0.0, 0.0])
                };
                let mesh_data =
                    floptle_render::mesh::tilemap(cols, rows, tile, *pc, *pr, ptexel, &page_data);
                let empty = mesh_data.indices.is_empty();
                let (nv, ni) = (mesh_data.vertices.len() as u32, mesh_data.indices.len() as u32);
                // Reuse a slot when the new geometry still fits, else
                // re-register at the new size — the pattern map meshes and
                // terrain use.
                let mesh = match spare.pop() {
                    Some(m) if raster.replace_dynamic(gpu, m, &mesh_data) => m,
                    Some(m) => {
                        raster.free_dynamic(m);
                        let fresh = raster.register_dynamic(gpu, nv.max(4), ni.max(6), false);
                        raster.replace_dynamic(gpu, fresh, &mesh_data);
                        fresh
                    }
                    None => {
                        let fresh = raster.register_dynamic(gpu, nv.max(4), ni.max(6), false);
                        raster.replace_dynamic(gpu, fresh, &mesh_data);
                        fresh
                    }
                };
                built.push(TilePageGpu {
                    mesh,
                    page,
                    texture: (!tex_path.is_empty()).then(|| tex_path.clone()),
                    empty,
                });
            }
            for m in spare {
                raster.free_dynamic(m);
            }
            self.tilemaps.insert(e, TileGpu { pages: built, sig });
        }
    }
}

/// The squares of `data` that live on `page`, remapped to that page's OWN cell
/// numbering so the ordinary mesh builder can be handed them unchanged. `None`
/// when the page has nothing on it.
///
/// Everything not on this page becomes a hole, which is exactly right: the page
/// draws its own squares and the pages beside it draw theirs, into the same
/// grid, at the same coordinates.
fn page_squares(data: &[u32], page: u32, page_cells: u32) -> Option<Vec<u32>> {
    let mut any = false;
    let out: Vec<u32> = data
        .iter()
        .map(|&packed| {
            if packed == floptle_core::EMPTY_TILE {
                return floptle_core::EMPTY_TILE;
            }
            let cell = floptle_core::tile_index(packed);
            if floptle_core::tile_page(cell) != page {
                return floptle_core::EMPTY_TILE;
            }
            let local = floptle_core::tile_in_page(cell);
            if local >= page_cells {
                return floptle_core::EMPTY_TILE; // a cell this page does not have
            }
            any = true;
            floptle_core::tile_pack(local, floptle_core::tile_xform(packed))
        })
        .collect();
    any.then_some(out)
}

/// The draw calls for a tilemap node — one per page that has squares on it.
///
/// `tex` is the node material's texture, which is page 0's; later pages name
/// their own and are resolved here, so a sheet that finishes loading after the
/// mesh was built is picked up without a rebuild.
pub(crate) fn tilemap_draws(
    tilemaps: &HashMap<Entity, TileGpu>,
    textures: &HashMap<String, TexId>,
    e: Entity,
    model: floptle_core::math::Mat4,
    mat: Option<&Material>,
    tex: Option<TexId>,
    out: &mut Vec<(MeshId, Option<TexId>, InstanceRaw)>,
) {
    let Some(t) = tilemaps.get(&e) else { return };
    let mut mp = mat.map(crate::shading::material_params).unwrap_or_else(|| {
        MaterialParams::flat([1.0, 1.0, 1.0])
    });
    // The cell UVs are baked into the mesh, so the instance must NOT also carry
    // the material's sheet window — applying the cell twice would show every
    // tile a sliver of cell 0.
    mp.tile_mode = 0;
    mp.tile = [0.0; 4];
    mp.tile_rotation = 0.0;
    let raw = instance_of_mat(model, &mp);
    for p in &t.pages {
        if p.empty {
            continue;
        }
        // A page draws the sheet it names. Page 0 falls back to the node's
        // Material when the tileset names none — a layer written before a
        // tileset could carry its own art, and the one case where the material
        // is still the authority.
        let page_tex = match p.texture.as_deref() {
            Some(path) => textures.get(path).copied().or(if p.page == 0 { tex } else { None }),
            None if p.page == 0 => tex,
            None => None,
        };
        out.push((p.mesh, page_tex, raw));
    }
}

/// One instance per sprite in a batch node.
///
/// `size` is the sprite's edge **in world units**; each sprite scales it, rolls
/// about the node's forward axis, and carries its own cell window and tint. The
/// tint is multiplied into the material's colour rather than replacing it, so a
/// batch can still be dimmed as a whole.
///
/// The quad these instance is [`crate::matter_catalog::PRIMITIVE_HALF`] across
/// — 1.4 units, not 1 — so `size` is divided by that rather than multiplied
/// straight onto the mesh. It used to be multiplied straight on, which made
/// `size = 1` draw a 1.4-unit sprite and the default the misleading case
/// (`floptle/0070`): a game that moved its bullets onto a batch saw them all
/// come out 40% too big, which reads as somebody's tuning change rather than a
/// unit mismatch.
pub(crate) fn sprite_draws(
    world: &World,
    e: Entity,
    size: f32,
    model: floptle_core::math::Mat4,
    mat: Option<&Material>,
    texel: [f32; 2],
    out: &mut Vec<InstanceRaw>,
) {
    use floptle_core::math::{Mat4, Quat, Vec3};
    let Some(sprites) = world.get::<Sprites>(e) else { return };
    if sprites.0.is_empty() {
        return;
    }
    let base = mat.map(crate::shading::material_params).unwrap_or_else(|| {
        MaterialParams::flat([1.0, 1.0, 1.0])
    });
    let (sc, sr) = mat.map(|m| m.sheet()).unwrap_or((1, 1));
    let cells = sc * sr;
    // Per-cell packing costs a Material clone, so only do it for a real sheet.
    let sheet_of = (cells > 1).then(|| mat.cloned().unwrap_or_default());

    // `size` is an edge length; the mesh is already 2 * PRIMITIVE_HALF across.
    let size = size / (2.0 * crate::matter_catalog::PRIMITIVE_HALF);

    out.reserve(sprites.0.len());
    for s in &sprites.0 {
        let local = Mat4::from_scale_rotation_translation(
            // Z takes the wider of the two: a sprite is flat, so its depth only
            // has to stay non-degenerate — and a zero would collapse the
            // matrix, not flatten the quad.
            Vec3::new(
                (size * s.scale[0]).max(1e-6),
                (size * s.scale[1]).max(1e-6),
                (size * s.scale[0].max(s.scale[1])).max(1e-6),
            ),
            Quat::from_rotation_z(s.rot),
            Vec3::from(s.pos),
        );
        let mut mp = base;
        // The per-instance tint — the whole reason a batch exists. Multiplied,
        // so a white sprite takes the tint and a material that is already
        // coloured is modulated rather than overwritten.
        mp.color = [
            base.color[0] * s.tint[0],
            base.color[1] * s.tint[1],
            base.color[2] * s.tint[2],
        ];
        mp.alpha = base.alpha * s.tint[3];
        // …and its own cell of the sheet, as a UV window. Same lane the
        // Material's own `cell` uses, so this needs no new instance attribute —
        // which matters, because the raster budget is full at 16/16.
        if let Some(m) = &sheet_of {
            let cell = Material { cell: s.cell.min(cells - 1), ..m.clone() };
            let packed = MaterialParams::from_material_inset(&cell, texel);
            mp.tile_mode = packed.tile_mode;
            mp.tile = packed.tile;
            mp.tile_rotation = packed.tile_rotation;
        }
        out.push(instance_of_mat(model * local, &mp));
    }
}

/// The one quad a [`Matter::Sprite`] draws.
///
/// `tex_px` is the texture's pixel size, needed only for `ppu` — the whole
/// point of pixels-per-unit is that the world size comes from the *image*, so
/// it cannot be answered without it. With no texture loaded yet it falls back
/// to the authored `size`, which draws something of about the right size rather
/// than nothing at all: a sprite that vanishes until its texture streams in
/// reads as a broken node.
#[allow(clippy::too_many_arguments)]
/// **How big one sprite is actually drawn**, in world units before its own scale.
///
/// One function because three things need the answer and they have to agree:
/// the draw, the click test, and the culling bounds. When only the draw knew, a
/// `ppu` sprite was drawn from its texture and picked and culled against its
/// `size` field — so a 16-unit sprite could be clicked only within half a unit
/// of its origin, and vanished at the screen edge while most of it was still on
/// screen.
///
/// The size of ONE CELL, not of the whole image — a sheet's cell is what a
/// sprite draws, and measuring the sheet makes every sprite in a 4×4 sheet come
/// out four times too big. With no texture yet there is nothing to measure, so
/// `size` is the answer, which is also the escape hatch for art that is not
/// pixel art.
pub(crate) fn sprite_world_size(
    ppu: f32,
    size: f32,
    mat: Option<&Material>,
    tex_px: Option<[f32; 2]>,
) -> (f32, f32) {
    let (sc, sr) = mat.map(|m| m.sheet()).unwrap_or((1, 1));
    match (ppu > 0.0, tex_px) {
        (true, Some([tw, th])) => (tw / sc.max(1) as f32 / ppu, th / sr.max(1) as f32 / ppu),
        _ => (size, size),
    }
}

#[allow(clippy::too_many_arguments)] // one draw, one call shape — a param struct would just rename the args
pub(crate) fn sprite_one_draw(
    ppu: f32,
    size: f32,
    cell: u32,
    flip_x: bool,
    flip_y: bool,
    pivot: [f32; 2],
    model: floptle_core::math::Mat4,
    mat: Option<&Material>,
    tex_px: Option<[f32; 2]>,
    texel: [f32; 2],
) -> InstanceRaw {
    use floptle_core::math::{Mat4, Vec3};

    let (sc, sr) = mat.map(|m| m.sheet()).unwrap_or((1, 1));
    let cells = (sc * sr).max(1);
    let (w, h) = sprite_world_size(ppu, size, mat, tex_px);

    let base = mat.map(crate::shading::material_params).unwrap_or_else(|| {
        MaterialParams::flat([1.0, 1.0, 1.0])
    });
    let mut mp = base;
    if cells > 1 {
        let m = mat.cloned().unwrap_or_default();
        let c = Material { cell: cell.min(cells - 1), ..m };
        let packed = MaterialParams::from_material_inset(&c, texel);
        mp.tile_mode = packed.tile_mode;
        mp.tile = packed.tile;
        mp.tile_rotation = packed.tile_rotation;
    }

    // Flipping is a negative SCALE ON THE QUAD, not on the node: a negative node
    // scale would mirror the node's children and invert its normals too, and
    // "face the other way" must not do either.
    let sx = if flip_x { -w } else { w };
    let sy = if flip_y { -h } else { h };
    // The pivot moves the QUAD, not the node — the node's origin is where the
    // author put it and where a Y-sort reads from. `0.5, 0.5` is the centre and
    // shifts nothing.
    let off = Vec3::new((0.5 - pivot[0]) * w, (0.5 - pivot[1]) * h, 0.0);
    // The plane mesh is `2 * PRIMITIVE_HALF` across, not one unit — the trap
    // this node type exists to stop every project walking into.
    let unit = 2.0 * crate::matter_catalog::PRIMITIVE_HALF;
    let local = Mat4::from_translation(off)
        * Mat4::from_scale(Vec3::new(
            (sx / unit).clamp(-1e6, 1e6),
            (sy / unit).clamp(-1e6, 1e6),
            // Flat, so its depth only has to stay non-degenerate; a zero would
            // collapse the matrix rather than flatten the quad.
            (w.max(h) / unit).max(1e-6),
        ));
    instance_of_mat(model * local, &mp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::{Mat4, Vec3, Vec4};
    use floptle_core::{Sprite, World};

    fn sprite(scale: [f32; 2]) -> Sprite {
        Sprite { pos: [0.0; 3], rot: 0.0, scale, cell: 0, tint: [1.0; 4] }
    }

    /// The width the quad actually occupies, by pushing its own corners through
    /// the instance matrix — rather than reading a scale lane and trusting that
    /// the mesh behind it is a unit square, which is exactly the assumption
    /// that was wrong.
    fn drawn_extent(raw: &InstanceRaw) -> [f32; 2] {
        let m = Mat4::from_cols_array_2d(&raw.model);
        let corner = |x: f32, y: f32| m * Vec4::new(x, y, 0.0, 1.0);
        let h = crate::matter_catalog::PRIMITIVE_HALF;
        let (lo, hi) = (corner(-h, -h), corner(h, h));
        [(hi.x - lo.x).abs(), (hi.y - lo.y).abs()]
    }

    /// **`size` is the world edge length**, and that is the whole reason this
    /// node type exists rather than "use a Plane". The plane mesh is
    /// `2 × PRIMITIVE_HALF` = 1.4 units across, so a Plane at scale 1 is 1.4
    /// units and every project that built a sprite out of one got sprites 40%
    /// too big until somebody measured them.
    #[test]
    fn a_sprites_size_is_its_world_edge() {
        let raw = super::sprite_one_draw(
            0.0, 3.0, 0, false, false, [0.5, 0.5], Mat4::IDENTITY, None, None, [0.0; 2],
        );
        let [w, h] = drawn_extent(&raw);
        assert!((w - 3.0).abs() < 1e-4, "{w}");
        assert!((h - 3.0).abs() < 1e-4, "{h}");
    }

    /// **Pixels per unit measures ONE CELL, not the whole sheet.** A 128×128
    /// image cut 4×4 is a 32-pixel cell, so at 32 ppu it is one unit — and
    /// re-slicing the sheet finer must not resize every sprite on it.
    #[test]
    fn pixels_per_unit_measures_a_cell_not_the_sheet() {
        let mat = Material { sheet_cols: 4, sheet_rows: 4, ..Default::default() };
        let raw = super::sprite_one_draw(
            32.0, 1.0, 0, false, false, [0.5, 0.5], Mat4::IDENTITY, Some(&mat),
            Some([128.0, 128.0]), [1.0 / 128.0; 2],
        );
        let [w, _] = drawn_extent(&raw);
        assert!((w - 1.0).abs() < 1e-4, "a 32px cell at 32 ppu must be one unit, got {w}");
    }

    /// With no texture loaded yet it falls back to `size` rather than drawing
    /// nothing — a sprite that vanishes until its image streams in reads as a
    /// broken node, not as a slow load.
    #[test]
    fn a_pixel_sized_sprite_without_its_texture_still_draws() {
        let raw = super::sprite_one_draw(
            32.0, 2.0, 0, false, false, [0.5, 0.5], Mat4::IDENTITY, None, None, [0.0; 2],
        );
        let [w, _] = drawn_extent(&raw);
        assert!((w - 2.0).abs() < 1e-4, "{w}");
    }

    /// **The pivot moves the quad, not the node.** The node's origin is where
    /// the author put it and where a Y-sort reads from; putting the pivot at
    /// the feet must lift the picture, not drop the node.
    #[test]
    fn the_pivot_moves_the_picture_and_leaves_the_origin_alone() {
        let centre = super::sprite_one_draw(
            0.0, 2.0, 0, false, false, [0.5, 0.5], Mat4::IDENTITY, None, None, [0.0; 2],
        );
        let feet = super::sprite_one_draw(
            0.0, 2.0, 0, false, false, [0.5, 0.0], Mat4::IDENTITY, None, None, [0.0; 2],
        );
        let mid = |raw: &InstanceRaw| Mat4::from_cols_array_2d(&raw.model).w_axis.y;
        // Origin at the bottom of the sprite = the picture sits a half-height
        // ABOVE the node.
        assert!((mid(&feet) - mid(&centre) - 1.0).abs() < 1e-4, "{} {}", mid(&feet), mid(&centre));
        // …and it is still the same size.
        assert_eq!(drawn_extent(&centre), drawn_extent(&feet));
    }

    /// Flipping mirrors the picture. It is a negative scale on the QUAD and not
    /// on the node, because a negative node scale would mirror the node's
    /// children and invert its normals as well.
    #[test]
    fn flipping_mirrors_the_quad_without_resizing_it() {
        let plain = super::sprite_one_draw(
            0.0, 2.0, 0, false, false, [0.5, 0.5], Mat4::IDENTITY, None, None, [0.0; 2],
        );
        let flipped = super::sprite_one_draw(
            0.0, 2.0, 0, true, false, [0.5, 0.5], Mat4::IDENTITY, None, None, [0.0; 2],
        );
        assert_eq!(drawn_extent(&plain), drawn_extent(&flipped));
        let x_of = |raw: &InstanceRaw| Mat4::from_cols_array_2d(&raw.model).x_axis.x;
        assert!(x_of(&plain) > 0.0 && x_of(&flipped) < 0.0, "the quad's X axis must invert");
    }

    /// A tilemap's page draws the sheet the PAGE names, and page 0 falls back
    /// to the node's material only when the tileset names none.
    ///
    /// This is what "I have to assign a texture to the material for the tileset
    /// to register" was: page 0 ignored the tileset entirely and took the
    /// material's texture whatever the tileset said, so a tileset alone drew
    /// nothing and the tileset's own sheet was decoration.
    #[test]
    fn a_page_draws_the_sheet_it_names() {
        let mut textures = HashMap::new();
        textures.insert("tiles.png".to_string(), TexId(7));
        textures.insert("props.png".to_string(), TexId(9));
        let material_tex = Some(TexId(1));

        let page = |page: u32, texture: Option<&str>| TilePageGpu {
            mesh: MeshId(page),
            page,
            texture: texture.map(str::to_string),
            empty: false,
        };
        let draw = |pages: Vec<TilePageGpu>| {
            let mut world = World::default();
            let e = world.spawn();
            let mut tilemaps = HashMap::new();
            tilemaps.insert(e, TileGpu { pages, sig: 0 });
            let mut out = Vec::new();
            tilemap_draws(
                &tilemaps,
                &textures,
                e,
                floptle_core::math::Mat4::IDENTITY,
                None,
                material_tex,
                &mut out,
            );
            out.into_iter().map(|(_, t, _)| t).collect::<Vec<_>>()
        };

        // The tileset names its own sheet for page 0: that is what draws, and
        // the material is not consulted.
        assert_eq!(draw(vec![page(0, Some("tiles.png"))]), vec![Some(TexId(7))]);
        // A later page brings its own image.
        assert_eq!(
            draw(vec![page(0, Some("tiles.png")), page(1, Some("props.png"))]),
            vec![Some(TexId(7)), Some(TexId(9))]
        );
        // No sheet on page 0 — the material, which is every project written
        // before a tileset could carry art.
        assert_eq!(draw(vec![page(0, None)]), vec![material_tex]);
        // A page naming an image that has not finished uploading falls back on
        // page 0 and draws untextured on a later one, rather than borrowing
        // page 0's art and looking like the wrong tile.
        assert_eq!(draw(vec![page(0, Some("missing.png"))]), vec![material_tex]);
        assert_eq!(draw(vec![page(0, None), page(1, Some("missing.png"))]), vec![material_tex, None]);
    }

    fn draw_one(size: f32, s: Sprite) -> InstanceRaw {
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, floptle_core::Sprites(vec![s]));
        let mut out = Vec::new();
        sprite_draws(&world, e, size, Mat4::IDENTITY, None, [0.0, 0.0], &mut out);
        assert_eq!(out.len(), 1, "one sprite in, one instance out");
        out.remove(0)
    }

    /// `floptle/0070`: the doc comment says `size` is the sprite's edge in world
    /// units. It used to be multiplied onto a 1.4-unit quad, so it was 1.4x that.
    #[test]
    fn size_is_the_edge_in_world_units() {
        let [w, h] = drawn_extent(&draw_one(1.0, sprite([1.0, 1.0])));
        assert!((w - 1.0).abs() < 1e-4, "a size-1 sprite is 1 unit wide, got {w}");
        assert!((h - 1.0).abs() < 1e-4, "…and 1 unit tall, got {h}");

        let [w2, _] = drawn_extent(&draw_one(2.5, sprite([1.0, 1.0])));
        assert!((w2 - 2.5).abs() < 1e-4, "a size-2.5 sprite is 2.5 units wide, got {w2}");
    }

    /// A sprite's own scale still multiplies the batch's size, per axis — the
    /// squash-and-stretch the two-component form exists for.
    #[test]
    fn a_sprites_own_scale_still_multiplies_per_axis() {
        let [w, h] = drawn_extent(&draw_one(2.0, sprite([1.5, 0.5])));
        assert!((w - 3.0).abs() < 1e-4, "2 * 1.5 = 3 wide, got {w}");
        assert!((h - 1.0).abs() < 1e-4, "2 * 0.5 = 1 tall, got {h}");
    }

    /// The batch node's own transform still moves and sizes the whole thing —
    /// dividing out the quad's extent must not have eaten the node's scale.
    #[test]
    fn the_batch_nodes_transform_still_applies() {
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, floptle_core::Sprites(vec![sprite([1.0, 1.0])]));
        let mut out = Vec::new();
        let model = Mat4::from_scale_rotation_translation(
            Vec3::splat(3.0),
            floptle_core::math::Quat::IDENTITY,
            Vec3::new(10.0, 0.0, 0.0),
        );
        sprite_draws(&world, e, 1.0, model, None, [0.0, 0.0], &mut out);
        let [w, _] = drawn_extent(&out[0]);
        assert!((w - 3.0).abs() < 1e-4, "a 3x node makes its 1-unit sprites 3 units, got {w}");
        let centre = Mat4::from_cols_array_2d(&out[0].model) * Vec4::W;
        assert!((centre.x - 10.0).abs() < 1e-4, "and it is where the node is, got {}", centre.x);
    }

    // ---- floptle/0092: one grid, several sheets ----------------------------

    use floptle_core::{tile_cell_of, tile_pack, EMPTY_TILE};

    /// Each page draws its OWN squares and leaves the rest as holes, so the
    /// pages composite into one grid at one set of coordinates.
    #[test]
    fn a_page_takes_its_own_squares_and_holes_the_rest() {
        let data = vec![
            tile_cell_of(0, 3),
            tile_cell_of(1, 0),
            EMPTY_TILE,
            tile_cell_of(0, 1),
        ];
        let p0 = page_squares(&data, 0, 16).expect("page 0 has squares");
        assert_eq!(p0, vec![3, EMPTY_TILE, EMPTY_TILE, 1], "page 0's own numbering");
        let p1 = page_squares(&data, 1, 16).expect("page 1 has squares");
        assert_eq!(p1, vec![EMPTY_TILE, 0, EMPTY_TILE, EMPTY_TILE]);
        assert!(page_squares(&data, 2, 16).is_none(), "an unused page builds no mesh");
    }

    /// A square's orientation belongs to the square, not to the sheet it came
    /// from — it must survive being split onto its page.
    #[test]
    fn a_pages_squares_keep_their_orientation() {
        let xf = floptle_core::TileXform::new(3, true);
        let data = vec![tile_pack(tile_cell_of(2, 7), xf)];
        let out = page_squares(&data, 2, 16).expect("page 2 has a square");
        assert_eq!(floptle_core::tile_index(out[0]), 7, "renumbered into its page");
        assert_eq!(floptle_core::tile_xform(out[0]), xf, "and still turned the same way");
    }

    /// A cell past what its page actually holds is a hole, not a sliver of
    /// whatever the UV maths landed on.
    #[test]
    fn a_cell_the_page_does_not_have_is_a_hole() {
        let data = vec![tile_cell_of(1, 5), tile_cell_of(1, 99)];
        let out = page_squares(&data, 1, 6).expect("one of the two is real");
        assert_eq!(out, vec![5, EMPTY_TILE]);
    }

    /// A grid written before pages existed is entirely page 0 and splits to
    /// itself — the unpaged path must be byte-for-byte what it always was.
    #[test]
    fn a_grid_from_before_pages_is_unchanged_by_the_split() {
        let data: Vec<u32> = (0..16).collect();
        let out = page_squares(&data, 0, 16).expect("all page 0");
        assert_eq!(out, data);
    }
}

/// **Every node's draw-time offset for this frame — the one answer both gathers use.**
///
/// Two 2D features write into this and neither moves anything real: a sorting
/// layer is a nudge in **Z** (what draws in front) and parallax is a nudge in
/// **X and Y** (how much of the camera's movement a layer keeps). Both apply to
/// the *drawn* transform only, so a collider stays where it was authored and a
/// script reads back the position it set.
///
/// One map because the draw loops borrow `raster` mutably and cannot call an
/// `&self` helper once they are running, and because two maps would be two
/// things to remember to add.
///
/// **One function, called twice.** The Scene view and every offscreen camera
/// each gather the world separately, and those two gathers have drifted apart
/// three times in this file's history — see
/// `offscreen_draws_the_same_world.rs`, which exists because of it. Sorting is
/// exactly the kind of thing that drifts silently: a scene that sorts one way in
/// the Scene view and another in the Game view looks like a rendering bug in
/// whichever one you are not looking at.
///
/// The camera is a parameter, and only parallax reads it. **Y-sorting deliberately
/// does not**: it ranks a layer's nodes against each other rather than mapping
/// their coordinates onto a scale, so the answer does not depend on where the
/// camera is, how much of the world it can see, or where the level was built.
/// Two views of one scene therefore cannot disagree about what is in front of
/// what — a stronger guarantee than "we remembered to pass the same camera to
/// both". Parallax is the opposite by definition: it is a function of the
/// viewpoint, so the Scene view and the Game view *should* show it differently,
/// each correct for its own camera.
pub(crate) fn draw_offsets(
    world: &World,
    project: &floptle_scene::ProjectConfigDoc,
    cam: DVec3,
) -> HashMap<Entity, DVec3> {
    use floptle_core::{SORT_LAYER_STEP, SortMode, Sorting, rank_offset, sorting_offset};

    let mut out: HashMap<Entity, DVec3> = HashMap::new();
    // Parallax first: it moves a node ACROSS the screen and sorting moves it
    // through the stack, so the two never write the same axis and can share one
    // map without either having to know about the other.
    for (e, p) in world.query::<floptle_core::Parallax>() {
        if p.is_identity() {
            continue;
        }
        let [dx, dy] = p.offset(cam.x, cam.y);
        let v = out.entry(e).or_default();
        v.x += dx;
        v.y += dy;
    }

    // Everything that takes part in 2D sorting, gathered per layer so a layer
    // can be ranked as a whole.
    //
    // **Flat nodes with no `Sorting` component are in this too**, on the default
    // layer at order 0 — which is exactly what they are. Leaving them out was a
    // real bug: the ranked branch below re-spaces a layer by ordinal position,
    // so a node left at z = 0 sat in the MIDDLE of the span its neighbours were
    // spread across, and a sprite authored at `order = -1` could come out in
    // front of the ground tilemap it was meant to be behind. Nothing about the
    // tilemap had changed; somebody had switched one node in the layer to Y.
    //
    // Hidden and switched-off nodes are left out, because they spend the layer's
    // depth budget without drawing anything: two hundred pooled projectiles
    // waiting to be shown would push every visible character into the tie floor.
    /// One node's place in the sort: `(node, order, Y, X, does it Y-sort)`.
    type Ranked = (Entity, i32, f64, f64, bool);
    let mut layers: HashMap<u32, Vec<Ranked>> = HashMap::new();
    let mut seen: std::collections::HashSet<Entity> = std::collections::HashSet::new();
    for (e, s) in world.query::<Sorting>() {
        if !floptle_core::is_drawn(world, e) {
            continue;
        }
        let rank = project.sorting_rank(&s.layer);
        let at = floptle_core::world_transform(world, e).translation;
        layers.entry(rank).or_default().push((e, s.order, at.y, at.x, s.mode == SortMode::Y));
        seen.insert(e);
    }
    let default_rank = project.sorting_rank(floptle_core::DEFAULT_SORTING_LAYER);
    for (e, m) in world.query::<floptle_core::Matter>() {
        if seen.contains(&e) || !is_flat_2d(m) || !floptle_core::is_drawn(world, e) {
            continue;
        }
        let at = floptle_core::world_transform(world, e).translation;
        layers.entry(default_rank).or_default().push((e, 0, at.y, at.x, false));
    }

    for (rank, mut nodes) in layers {
        // A layer nobody Y-sorts is left exactly as it was: the fixed order
        // step, the same Z it has had since sorting layers shipped. The whole
        // ranking below exists for Y-sorting, and a scene that does not use it
        // must not be re-spaced by its arrival.
        if !nodes.iter().any(|n| n.4) {
            for (e, order, _, _, _) in nodes {
                out.entry(e).or_default().z += sorting_offset(rank, order) as f64;
            }
            continue;
        }
        // One node cannot be ranked against itself, and the arithmetic below
        // divides by `n - 1`.
        if nodes.len() == 1 {
            let (e, order, _, _, _) = nodes[0];
            out.entry(e).or_default().z += sorting_offset(rank, order) as f64;
            continue;
        }
        // **Sorting layer, then order, then Y.** Order is not replaced by Y and
        // never was meant to be: it is what lets a character Y-sort against the
        // props around it while its shadow stays pinned below the lot. Y only
        // decides between nodes that would otherwise be level, which is exactly
        // the case that used to be settled by whatever the ECS yielded first.
        //
        // **X, then the entity index, break a remaining exact tie.** The
        // answer has to be the same every frame — two nodes swapping places
        // because the world was walked in a different order reads as flicker and
        // cannot be reproduced on purpose — and X is the tiebreak that is also
        // the same next *session*. The index alone is stable within a run and
        // not across one: destroy a node and spawn it again and it can come back
        // with a lower index than the sibling it used to sit behind, so a scene
        // reloaded, or a pooled enemy respawned, quietly restacked. X is
        // ascending, so the node further right draws in front. Two nodes at the
        // same X and the same Y are in the same place, where nothing is
        // observable either way, and the index settles those.
        nodes.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then(b.2.total_cmp(&a.2))
                .then(a.3.total_cmp(&b.3))
                .then(a.0.index().cmp(&b.0.index()))
        });
        // Spread across the whole layer, rather than into each order's own
        // sub-step. The layer holds about `SORT_Y_BANDS` distinguishable depths
        // in total (measured — see `sort_precision_probe`), and an order's share
        // of that would be one or two. Ranking the layer as a whole spends the
        // budget on the nodes that are actually in it, so the ordinary case —
        // one or two orders and a crowd of characters — gets nearly all of it.
        // Relative order is what is observable, and this preserves it exactly.
        let n = nodes.len();
        for (i, (e, _, _, _, _)) in nodes.into_iter().enumerate() {
            out.entry(e).or_default().z +=
                (rank as f32 * SORT_LAYER_STEP + rank_offset(i, n)) as f64;
        }
    }
    out
}

/// Is this node's matter one of the flat 2D kinds — the ones that take part in
/// sorting whether or not anybody gave them a layer?
fn is_flat_2d(m: &floptle_core::Matter) -> bool {
    matches!(
        m,
        floptle_core::Matter::Tilemap { .. }
            | floptle_core::Matter::SpriteBatch { .. }
            | floptle_core::Matter::Sprite { .. }
    )
}

#[cfg(test)]
mod sort_tests {
    use floptle_core::math::DVec3;
    use floptle_core::{Entity, SortMode, Sorting, World};

    /// Just the Z half, with the camera at the origin — every test below is
    /// about sorting, and parallax is exercised on its own further down.
    fn zs(
        world: &World,
        project: &floptle_scene::ProjectConfigDoc,
    ) -> std::collections::HashMap<Entity, f64> {
        super::draw_offsets(world, project, DVec3::ZERO)
            .into_iter()
            .map(|(e, v)| (e, v.z))
            .collect()
    }

    /// One node per Y, each on its own ascending `order` — for the tests about
    /// order rather than about Y.
    fn scene(ys: &[f32], mode: SortMode) -> (World, Vec<Entity>, floptle_scene::ProjectConfigDoc) {
        build(ys, |i| i as i32, mode)
    }

    /// One node per Y, all sharing `order` — for the tests about Y, where
    /// anything else deciding would hide what is being asserted.
    fn scene_at(
        ys: &[f32],
        order: i32,
        mode: SortMode,
    ) -> (World, Vec<Entity>, floptle_scene::ProjectConfigDoc) {
        build(ys, |_| order, mode)
    }

    fn build(
        ys: &[f32],
        order: impl Fn(usize) -> i32,
        mode: SortMode,
    ) -> (World, Vec<Entity>, floptle_scene::ProjectConfigDoc) {
        let mut world = World::default();
        let mut es = Vec::new();
        for (i, y) in ys.iter().enumerate() {
            let e = world.spawn();
            let mut t = floptle_core::Transform::default();
            t.translation.y = *y as f64;
            world.insert(e, t);
            world.insert(e, Sorting { layer: String::new(), order: order(i), mode });
            es.push(e);
        }
        (world, es, floptle_scene::ProjectConfigDoc::default())
    }

    /// Lower on the screen draws in FRONT — the whole claim of the feature,
    /// at one order so nothing else is deciding.
    #[test]
    fn a_lower_node_gets_a_nearer_z() {
        let (world, es, project) = scene_at(&[3.0, -1.0, 7.0], 0, SortMode::Y);
        let z = zs(&world, &project);
        let (top, middle, bottom) = (z[&es[2]], z[&es[0]], z[&es[1]]);
        assert!(bottom > middle, "y=-1 must be in front of y=3");
        assert!(middle > top, "y=3 must be in front of y=7");
    }

    /// **Two nodes level in Y stack the same way next session.** The last
    /// tiebreak used to be the entity index alone, which is stable within a run
    /// and not across one: a pooled enemy destroyed and respawned comes back
    /// with whatever index was free, so a scene reloaded — or a bullet reused —
    /// could quietly restack against a sibling standing at the same height.
    ///
    /// Driven by building the same scene twice with the spawn order reversed,
    /// because that is what a respawn actually does to the indices, and by
    /// asserting the two runs agree rather than asserting either answer.
    #[test]
    fn nodes_level_in_y_stack_the_same_way_whatever_order_they_were_spawned_in() {
        let put = |world: &mut World, x: f64, y: f64| {
            let e = world.spawn();
            let mut t = floptle_core::Transform::default();
            t.translation.x = x;
            t.translation.y = y;
            world.insert(e, t);
            world.insert(e, Sorting { layer: String::new(), order: 0, mode: SortMode::Y });
            e
        };
        let project = floptle_scene::ProjectConfigDoc::default();
        // Same three places, spawned in opposite orders.
        let places = [(-4.0, 2.0), (0.0, 2.0), (6.0, 2.0)];
        let mut first = World::default();
        let a: Vec<_> = places.iter().map(|&(x, y)| put(&mut first, x, y)).collect();
        let mut second = World::default();
        let b: Vec<_> = places.iter().rev().map(|&(x, y)| put(&mut second, x, y)).collect();
        let (za, zb) = (zs(&first, &project), zs(&second, &project));
        // Compare by PLACE, not by handle: `b` is in reverse place order.
        for (i, _) in places.iter().enumerate() {
            let (ea, eb) = (a[i], b[places.len() - 1 - i]);
            assert_eq!(
                za[&ea], zb[&eb],
                "the node at {:?} landed at a different depth once respawned",
                places[i]
            );
        }
        // And the tiebreak reads left to right — the node further right draws in
        // front — so it is a decision somebody can see in the scene rather than
        // an internal number they cannot.
        assert!(za[&a[2]] > za[&a[1]] && za[&a[1]] > za[&a[0]], "not ordered by x: {za:?}");
    }

    /// **Order wins over Y.** The sort is sorting layer → order → Y, so a node
    /// on a higher order is in front however far up the screen it is. Y decides
    /// between nodes that would otherwise be level, and nothing else.
    ///
    /// This is the case that makes a character's shadow work: the shadow is on
    /// `order = -1` under a Y-sorted crowd and stays under all of them, rather
    /// than surfacing through whoever happens to be standing above it.
    #[test]
    fn order_wins_over_y() {
        let mut world = World::default();
        let project = floptle_scene::ProjectConfigDoc::default();
        let put = |world: &mut World, y: f64, order: i32| {
            let e = world.spawn();
            let mut t = floptle_core::Transform::default();
            t.translation.y = y;
            world.insert(e, t);
            world.insert(e, Sorting { layer: String::new(), order, mode: SortMode::Y });
            e
        };
        // Right at the bottom of the screen (so Y wants it in FRONT of
        // everything) but on the order below.
        let shadow = put(&mut world, -100.0, -1);
        // Right at the top (so Y wants it at the BACK) but on the order above.
        let hat = put(&mut world, 100.0, 1);
        let body = put(&mut world, 0.0, 0);
        let z = zs(&world, &project);
        assert!(z[&hat] > z[&body], "order 1 must beat order 0 whatever Y says");
        assert!(z[&body] > z[&shadow], "order -1 must lose to order 0 whatever Y says");
    }

    /// …and *within* one order, Y is what decides.
    #[test]
    fn y_breaks_ties_inside_an_order() {
        let (world, es, project) = scene_at(&[3.0, -1.0, 7.0], 4, SortMode::Y);
        let z = zs(&world, &project);
        assert!(z[&es[1]] > z[&es[0]], "y=-1 in front of y=3 at the same order");
        assert!(z[&es[0]] > z[&es[2]], "y=3 in front of y=7 at the same order");
    }

    /// The default mode is the old behaviour, exactly.
    #[test]
    fn order_mode_is_unchanged() {
        let (world, es, project) = scene(&[3.0, -1.0, 7.0], SortMode::Order);
        let z = zs(&world, &project);
        for (i, e) in es.iter().enumerate() {
            assert_eq!(z[e], floptle_core::sorting_offset(0, i as i32) as f64);
        }
    }

    /// Every Y-sorted node in a layer gets its own depth, and they all stay
    /// inside the layer — a sorting layer that leaks is worse than none.
    #[test]
    fn a_full_layer_stays_inside_itself_and_never_ties() {
        let ys: Vec<f32> = (0..floptle_core::SORT_Y_BANDS).map(|i| i as f32 * 0.5).collect();
        let (world, es, project) = scene_at(&ys, 0, SortMode::Y);
        let z = zs(&world, &project);
        let half = (floptle_core::SORT_LAYER_STEP * 0.5) as f64;
        let mut seen: Vec<f64> = es.iter().map(|e| z[e]).collect();
        seen.sort_by(f64::total_cmp);
        // **Separated by the MEASURED floor, not merely distinct as f64.** The
        // first version of this asserted `w[1] > w[0]`, which is true for ten
        // thousand nodes in a layer and says nothing at all about whether the
        // depth buffer can tell them apart. `sort_precision_probe` measured the
        // floor at one `SORT_ORDER_STEP`; anything closer than that is a tie on
        // screen however different the numbers are.
        let floor = floptle_core::SORT_ORDER_STEP as f64;
        for w in seen.windows(2) {
            assert!(
                w[1] - w[0] >= floor * 0.999,
                "two nodes are {} apart, under the {floor} the depth buffer resolves: {w:?}",
                w[1] - w[0]
            );
        }
        assert!(seen[0] >= -half && seen[seen.len() - 1] <= half, "a node left its layer");
    }

    /// The ground a 2D scene is built on carries no `Sorting` component, and it
    /// still has to stay behind the things authored behind it.
    ///
    /// The bug: the ranked branch re-spaces a layer by ordinal position, so a
    /// node left out of the ranking sat in the MIDDLE of the span everything
    /// else was spread across. Switching one node in the layer to Y-sorting
    /// therefore pushed a sprite at `order = -1` in FRONT of the tilemap it was
    /// authored behind — with nothing about either of them having changed.
    #[test]
    fn un_layered_ground_stays_behind_what_was_put_behind_it() {
        let mut world = World::new();
        let project = floptle_scene::ProjectConfigDoc::default();
        // A tilemap with no Sorting at all — the floor of the level.
        let ground = world.spawn();
        world.insert(ground, floptle_core::Transform::default());
        world.insert(
            ground,
            floptle_core::Matter::Tilemap {
                cols: 4,
                rows: 4,
                tile: 1.0,
                data: vec![0; 16],
                tileset: String::new(),
            },
        );
        // Three sprites authored behind it, and one Y-sorted character.
        let mut behind = Vec::new();
        for (order, y, mode) in
            [(-5, 0.0, SortMode::Order), (-3, 1.0, SortMode::Order), (-2, 2.0, SortMode::Y)]
        {
            let e = world.spawn();
            let mut t = floptle_core::Transform::default();
            t.translation.y = y;
            world.insert(e, t);
            world.insert(
                e,
                Sorting { layer: "Default".into(), order, mode },
            );
            behind.push(e);
        }
        let z = zs(&world, &project);
        let gz = z.get(&ground).copied().unwrap_or(0.0);
        for e in &behind {
            assert!(
                z[e] < gz,
                "a node authored behind the ground came out in front of it: {} vs {gz}",
                z[e]
            );
        }
    }

    /// Two nodes at the same Y still get an answer, and the same one every
    /// frame — the alternative is a pair that swaps every time the ECS is
    /// walked, which reads as flicker.
    #[test]
    fn an_exact_tie_is_broken_the_same_way_twice() {
        let (world, es, project) = scene_at(&[2.0, 2.0, 2.0], 0, SortMode::Y);
        let a = zs(&world, &project);
        let b = zs(&world, &project);
        for e in &es {
            assert_eq!(a[e], b[e]);
        }
        assert!(a[&es[0]] != a[&es[1]], "tied nodes must still get distinct depths");
    }

    /// A parallax layer keeps `1 - factor` of the camera's movement, so a
    /// factor of 1 keeps none of it and moves with the world.
    #[test]
    fn parallax_moves_a_layer_less_than_the_world() {
        let mut world = World::default();
        let project = floptle_scene::ProjectConfigDoc::default();
        let make = |world: &mut World, fx: f32| {
            let e = world.spawn();
            world.insert(e, floptle_core::Transform::default());
            world.insert(e, floptle_core::Parallax { factor: [fx, 1.0] });
            e
        };
        let far = make(&mut world, 0.25);
        let near = make(&mut world, 1.0);
        let pinned = make(&mut world, 0.0);

        let cam = DVec3::new(100.0, 0.0, 0.0);
        let off = super::draw_offsets(&world, &project, cam);
        // Moves with the world: no offset at all, and no entry either.
        assert!(off.get(&near).is_none_or(|v| v.x == 0.0));
        // A quarter-speed layer keeps three quarters of the camera's move.
        assert_eq!(off[&far].x, 75.0);
        // Pinned to the camera: it is exactly as far along as the camera is, so
        // on screen it has not moved at all.
        assert_eq!(off[&pinned].x, 100.0);
        // …and the axis nobody asked about is untouched.
        assert_eq!(off[&far].y, 0.0);
    }

    /// Parallax and sorting write different axes of one offset, so a node can
    /// have both without either being lost.
    #[test]
    fn a_node_can_parallax_and_sort_at_once() {
        let mut world = World::default();
        let project = floptle_scene::ProjectConfigDoc::default();
        let e = world.spawn();
        world.insert(e, floptle_core::Transform::default());
        world.insert(e, floptle_core::Parallax { factor: [0.5, 1.0] });
        world.insert(e, Sorting { layer: String::new(), order: 4, mode: SortMode::Order });
        let off = super::draw_offsets(&world, &project, DVec3::new(10.0, 0.0, 0.0));
        assert_eq!(off[&e].x, 5.0);
        assert_eq!(off[&e].z, floptle_core::sorting_offset(0, 4) as f64);
    }

    /// A node on a layer further forward is in front of a Y-sorted node on a
    /// layer behind it, whatever their Ys are. Y-sorting orders WITHIN a layer;
    /// it does not let anything climb out of one.
    #[test]
    fn y_sorting_cannot_climb_out_of_its_layer() {
        let mut world = World::default();
        let project = floptle_scene::ProjectConfigDoc {
            sorting_layers: vec!["Characters".into()],
            ..Default::default()
        };
        // Far up the screen (so it sorts to the BACK of its own layer), but on
        // the front layer.
        let front = world.spawn();
        let mut t = floptle_core::Transform::default();
        t.translation.y = 1000.0;
        world.insert(front, t);
        world.insert(
            front,
            Sorting { layer: "Characters".into(), order: 0, mode: SortMode::Y },
        );
        // Right at the bottom of the screen, but on the default layer behind it.
        let back = world.spawn();
        let mut t = floptle_core::Transform::default();
        t.translation.y = -1000.0;
        world.insert(back, t);
        world.insert(back, Sorting { layer: String::new(), order: 0, mode: SortMode::Y });

        let z = zs(&world, &project);
        assert!(z[&front] > z[&back], "the front layer must win however low the other node is");
    }
}
