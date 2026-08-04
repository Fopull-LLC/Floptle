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
fn signature(cols: u32, rows: u32, tile: f32, data: &[u32], sheet: (u32, u32), texel: [f32; 2]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (cols, rows, sheet).hash(&mut h);
    tile.to_bits().hash(&mut h);
    texel[0].to_bits().hash(&mut h);
    texel[1].to_bits().hash(&mut h);
    data.hash(&mut h);
    h.finish()
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
        let live: Vec<(Entity, u32, u32, f32, Vec<u32>)> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Tilemap { cols, rows, tile, data } => {
                    Some((e, *cols, *rows, *tile, data.clone()))
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

        for (e, cols, rows, tile, data) in live {
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

            let sig = signature(cols, rows, tile, &data, (sc, sr), texel);
            if self.tilemaps.get(&e).is_some_and(|t| t.sig == sig) {
                continue;
            }
            let mesh_data =
                floptle_render::mesh::tilemap(cols, rows, tile, sc, sr, texel, &data);
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
/// `size` is the batch's quad edge; each sprite scales it, rolls about the
/// node's forward axis, and carries its own cell window and tint. The tint is
/// multiplied into the material's colour rather than replacing it, so a batch
/// can still be dimmed as a whole.
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

    out.reserve(sprites.0.len());
    for s in &sprites.0 {
        let local = Mat4::from_scale_rotation_translation(
            Vec3::splat((size * s.scale).max(1e-6)),
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
