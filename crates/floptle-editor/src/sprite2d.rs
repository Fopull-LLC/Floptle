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
//! Both take their texture and sheet grid from the node's ordinary Material, so
//! a project does not learn a second way to say "this texture, chopped this
//! way", and both reach a custom `.flsl` for free.

use std::collections::HashMap;

use floptle_core::{Entity, Material, Matter, Sprites, World};
use floptle_render::{InstanceRaw, MaterialParams, MeshId, TexId, instance_of_mat};

use crate::Editor;

/// A tilemap's uploaded geometry, and the signature of the grid it was built
/// from — so a map that hasn't changed isn't rebuilt sixty times a second.
pub(crate) struct TileGpu {
    pub(crate) mesh: MeshId,
    sig: u64,
    /// Whether the mesh currently holds any triangles (an all-empty grid
    /// uploads nothing, and drawing it would be a wasted draw call).
    empty: bool,
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
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (cols, rows, sheet).hash(&mut h);
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
    let cells = set.cells();
    let mut out: Option<Vec<u32>> = None;
    for (i, &packed) in data.iter().enumerate() {
        if floptle_core::tile_is_empty(packed, cells) {
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
                raster.free_dynamic(t.mesh);
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
            let (sc, sr) = mat.sheet();
            // The texel size is what the half-texel inset is measured in. An
            // unloaded texture reports nothing, and the mesh is rebuilt when it
            // arrives because the signature covers it.
            let texel = mat
                .texture
                .as_deref()
                .and_then(|p| self.texture_registry.get(p).copied())
                .and_then(|id| raster.texture_size(id))
                .map(|[w, h]| [1.0 / w.max(1.0), 1.0 / h.max(1.0)])
                .unwrap_or([0.0, 0.0]);

            let set = (!tileset.is_empty()).then(|| self.tiles.get(&tileset)).flatten();
            let step = set.map(|s| anim_step(s, now)).unwrap_or(0);
            let sig = signature(cols, rows, tile, &data, (sc, sr), texel, step);
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
            let mesh_data =
                floptle_render::mesh::tilemap(cols, rows, tile, sc, sr, texel, draw);
            let empty = mesh_data.indices.is_empty();
            let (nv, ni) = (mesh_data.vertices.len() as u32, mesh_data.indices.len() as u32);

            // Reuse the slot when the new geometry still fits, else re-register
            // at the new size — the same pattern the map meshes and terrain use.
            let mesh = match self.tilemaps.get(&e) {
                Some(t) if raster.replace_dynamic(gpu, t.mesh, &mesh_data) => t.mesh,
                Some(t) => {
                    raster.free_dynamic(t.mesh);
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
            self.tilemaps.insert(e, TileGpu { mesh, sig, empty });
        }
    }
}

/// The draw call for a tilemap node, if its geometry is built and non-empty.
pub(crate) fn tilemap_draw(
    tilemaps: &HashMap<Entity, TileGpu>,
    e: Entity,
    model: floptle_core::math::Mat4,
    mat: Option<&Material>,
    tex: Option<TexId>,
) -> Option<(MeshId, Option<TexId>, InstanceRaw)> {
    let t = tilemaps.get(&e)?;
    if t.empty {
        return None;
    }
    let mut mp = mat.map(crate::shading::material_params).unwrap_or_else(|| {
        MaterialParams::flat([1.0, 1.0, 1.0])
    });
    // The cell UVs are baked into the mesh, so the instance must NOT also carry
    // the material's sheet window — applying the cell twice would show every
    // tile a sliver of cell 0.
    mp.tile_mode = 0;
    mp.tile = [0.0; 4];
    mp.tile_rotation = 0.0;
    Some((t.mesh, tex, instance_of_mat(model, &mp)))
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
}
