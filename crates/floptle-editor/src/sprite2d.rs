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
