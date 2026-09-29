//! The editor half of `decals.*`: one mesh per picture, rebuilt when the
//! script host says that picture's decals changed, and drawn through the
//! raster like any other surface, so a mark takes the sun, the shadows and
//! the fog of the ground it lies on.
//!
//! The laying happens in the script host (`floptle_script::decal_api`), which
//! has the colliders; this side only packs what it laid into GPU buffers.

use std::collections::{BTreeMap, HashSet};

use floptle_core::math::{DVec3, Mat4};
use floptle_render::{MaterialParams, MeshData, MeshId, TexId, Vertex};

/// Instance alpha for a decal batch: just under opaque, so the batch is drawn
/// blended, after the surfaces it lies on, and each mark's own alpha (its
/// vertex colour's) shows.
const BATCH_ALPHA: f32 = 0.99;

/// One picture's decals, as one mesh.
pub(crate) struct DecalBatch {
    mesh: MeshId,
    tex: TexId,
    /// The mesh's vertices are relative to this, so a mark far from the world
    /// origin keeps its precision.
    anchor: DVec3,
}

#[derive(Default)]
pub(crate) struct DecalBatches {
    epoch: u64,
    batches: BTreeMap<String, DecalBatch>,
    /// Pictures that would not load, said once.
    warned: HashSet<String>,
}

/// Pack every decal showing one picture into a mesh, relative to the first
/// one's centre. `None` when there are none.
pub(crate) fn pack(store: &floptle_script::decal_api::DecalStore, texture: &str) -> Option<(DVec3, MeshData)> {
    let mut decals = store.with_texture(texture).peekable();
    let anchor = decals.peek()?.pos;
    let mut data = MeshData { vertices: Vec::new(), indices: Vec::new(), colors: Some(Vec::new()) };
    let colors = data.colors.as_mut().expect("colors");
    for d in decals {
        let off = (d.pos - anchor).as_vec3();
        let base = data.vertices.len() as u32;
        let byte = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
        for v in &d.verts {
            data.vertices.push(Vertex { pos: (off + v.pos).to_array(), normal: v.normal.to_array(), uv: v.uv });
            colors.push([byte(d.color[0]), byte(d.color[1]), byte(d.color[2]), byte(d.alpha * v.fade)]);
        }
        data.indices.extend(d.indices.iter().map(|i| base + i));
    }
    Some((anchor, data))
}

impl crate::Editor {
    /// Rebuild the batch of every picture whose decals changed since the last
    /// frame. Once a frame, after the scripts; nothing to do without a GPU.
    pub(crate) fn sync_decals(&mut self) {
        let store = self.script_host.decals().clone();
        let mut store = store.borrow_mut();
        let mut dirty = store.take_dirty();
        if store.epoch() != self.decal_batches.epoch {
            // The store was emptied wholesale: every batch goes.
            self.decal_batches.epoch = store.epoch();
            dirty.extend(self.decal_batches.batches.keys().cloned());
            dirty.sort();
            dirty.dedup();
        }
        if dirty.is_empty() || self.gpu.is_none() || self.raster.is_none() {
            return;
        }
        for texture in dirty {
            let packed = pack(&store, &texture);
            let tex = packed.as_ref().and_then(|_| self.ensure_texture(&texture));
            if packed.is_some() && tex.is_none() && self.decal_batches.warned.insert(texture.clone()) {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("decals: the picture \"{texture}\" will not load, so its marks are not drawn"),
                    None,
                );
            }
            let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else { return };
            let (Some((anchor, data)), Some(tex)) = (packed, tex) else {
                if let Some(old) = self.decal_batches.batches.remove(&texture) {
                    raster.free_dynamic(old.mesh);
                }
                continue;
            };
            let mesh = match self.decal_batches.batches.get(&texture) {
                Some(b) if raster.replace_dynamic(gpu, b.mesh, &data) => b.mesh,
                _ => {
                    if let Some(old) = self.decal_batches.batches.remove(&texture) {
                        raster.free_dynamic(old.mesh);
                    }
                    let id = raster.register_dynamic(gpu, data.vertices.len() as u32, data.indices.len() as u32, true);
                    raster.replace_dynamic(gpu, id, &data);
                    id
                }
            };
            self.decal_batches.batches.insert(texture, DecalBatch { mesh, tex, anchor });
        }
    }
}

/// One instance per picture's batch, camera-relative. A free function for the
/// render loop's borrow split, like `push_terrain_instances`.
pub(crate) fn push_decal_instances(
    batches: &DecalBatches,
    raster: &floptle_render::Raster,
    cam_world: DVec3,
    instances: &mut Vec<(MeshId, Option<TexId>, floptle_render::InstanceRaw)>,
) {
    for b in batches.batches.values() {
        let model = Mat4::from_translation((b.anchor - cam_world).as_vec3());
        let mut mp = MaterialParams::flat([1.0; 3]);
        mp.alpha = BATCH_ALPHA;
        mp.terrain_paint_base = raster.dyn_paint_base(b.mesh);
        instances.push((b.mesh, Some(b.tex), floptle_render::instance_of_mat(model, &mp)));
    }
}
