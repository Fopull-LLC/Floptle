//! The editor half of `decals.*`: one mesh per picture, rebuilt when the
//! script host says that picture's decals changed, and drawn through the
//! raster like any other surface, so a mark takes the sun, the shadows and
//! the fog of the ground it lies on.
//!
//! The laying happens in the script host (`floptle_script::decal_api`), which
//! has the colliders; this side only packs what it laid into GPU buffers.

use std::collections::{BTreeMap, HashSet};

use floptle_core::math::{DVec3, Mat4, Quat};
use floptle_core::transform::Transform;
use floptle_script::decal_api::{Decal, DecalFrame, DecalStore};
use floptle_render::{MaterialParams, MeshData, MeshId, TexId, Vertex};

/// Instance alpha for a decal batch: just under opaque, so the batch is drawn
/// blended, after the surfaces it lies on, and each mark's own alpha (its
/// vertex colour's) shows.
const BATCH_ALPHA: f32 = 0.99;

/// One picture's decals on one frame, as one mesh.
pub(crate) struct DecalBatch {
    mesh: MeshId,
    tex: TexId,
    /// The mesh's vertices are relative to this, so a mark far from the world
    /// origin keeps its precision: in the world, or in the frame's own space
    /// when the batch rides one.
    anchor: DVec3,
}

/// A batch's key: the picture, and the node its marks ride (`None` for marks
/// that stay where they were laid).
type BatchKey = (String, Option<u32>);

#[derive(Default)]
pub(crate) struct DecalBatches {
    epoch: u64,
    batches: BTreeMap<BatchKey, DecalBatch>,
    /// Pictures that would not load, said once.
    warned: HashSet<String>,
}

/// The frame a placed decal rides, and the frame's pose when it was laid.
fn ridden(d: &Decal) -> Option<(u32, Transform)> {
    match d.frame {
        DecalFrame::On(f, at) => Some((f, at)),
        _ => None,
    }
}

/// Pack every decal showing one picture on one frame into a mesh, relative to
/// the first one's centre, in the frame's own space when it has one. `None`
/// when there are none.
pub(crate) fn pack(store: &DecalStore, texture: &str, frame: Option<u32>) -> Option<(DVec3, MeshData)> {
    // Each mark's centre and turn in the space the batch is kept in.
    let placed = |d: &Decal| -> (DVec3, Quat) {
        match ridden(d) {
            Some((_, at)) => {
                let local = at.inv_mul(&Transform::from_translation(d.pos));
                (local.translation, at.rotation.inverse())
            }
            None => (d.pos, Quat::IDENTITY),
        }
    };
    let mut decals = store.with_texture(texture).filter(|d| ridden(d).map(|(f, _)| f) == frame).peekable();
    let anchor = placed(decals.peek()?).0;
    let mut data = MeshData { vertices: Vec::new(), indices: Vec::new(), colors: Some(Vec::new()) };
    let colors = data.colors.as_mut().expect("colors");
    for d in decals {
        let (at, turn) = placed(d);
        let off = (at - anchor).as_vec3();
        let base = data.vertices.len() as u32;
        let byte = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
        for v in &d.verts {
            data.vertices.push(Vertex {
                pos: (off + turn * v.pos).to_array(),
                normal: (turn * v.normal).to_array(),
                uv: v.uv,
            });
            colors.push([byte(d.color[0]), byte(d.color[1]), byte(d.color[2]), byte(d.alpha * v.fade)]);
        }
        data.indices.extend(d.indices.iter().map(|i| base + i));
    }
    Some((anchor, data))
}

impl crate::Editor {
    /// Settle what each decal laid in the script pass just run rides: the node
    /// `decals.add` named, or the moving world its centre is in, as that node
    /// stands now, which is how it stood when the mark was laid.
    pub(crate) fn place_script_decals(&mut self) {
        let store = self.script_host.decals().clone();
        let unplaced = store.borrow().unplaced();
        if unplaced.is_empty() {
            return;
        }
        let spheres = self.moving_world_spheres();
        let mut store = store.borrow_mut();
        for (id, asked, pos) in unplaced {
            let entity = asked
                .and_then(|f| self.world.entity_with::<Transform>(f))
                .or_else(|| crate::play_step::moving_world_at(&spheres, pos));
            let frame = match entity {
                Some(e) => DecalFrame::On(e.index(), floptle_core::world_transform(&self.world, e)),
                None => DecalFrame::World,
            };
            store.place(id, frame);
        }
    }

    /// Rebuild the batch of every picture whose decals changed since the last
    /// frame. Once a frame, after the scripts; nothing to do without a GPU.
    pub(crate) fn sync_decals(&mut self) {
        let store = self.script_host.decals().clone();
        let mut store = store.borrow_mut();
        let mut dirty = store.take_dirty();
        if store.epoch() != self.decal_batches.epoch {
            // The store was emptied wholesale: every batch goes.
            self.decal_batches.epoch = store.epoch();
            dirty.extend(self.decal_batches.batches.keys().map(|(t, _)| t.clone()));
            dirty.sort();
            dirty.dedup();
        }
        if dirty.is_empty() || self.gpu.is_none() || self.raster.is_none() {
            return;
        }
        for texture in dirty {
            // Every frame this picture is on now, and every one it had a batch on.
            let mut frames: Vec<Option<u32>> =
                store.with_texture(&texture).map(|d| ridden(d).map(|(f, _)| f)).collect();
            frames.extend(self.decal_batches.batches.keys().filter(|(t, _)| *t == texture).map(|(_, f)| *f));
            frames.sort();
            frames.dedup();
            for frame in frames {
                self.rebuild_decal_batch(&store, &texture, frame);
            }
        }
    }

    fn rebuild_decal_batch(&mut self, store: &DecalStore, texture: &str, frame: Option<u32>) {
        let key = (texture.to_string(), frame);
        let packed = pack(store, texture, frame);
        let tex = packed.as_ref().and_then(|_| self.ensure_texture(texture));
        if packed.is_some() && tex.is_none() && self.decal_batches.warned.insert(texture.to_string()) {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!("decals: the picture \"{texture}\" will not load, so its marks are not drawn"),
                None,
            );
        }
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else { return };
        let (Some((anchor, data)), Some(tex)) = (packed, tex) else {
            if let Some(old) = self.decal_batches.batches.remove(&key) {
                raster.free_dynamic(old.mesh);
            }
            return;
        };
        let mesh = match self.decal_batches.batches.get(&key) {
            Some(b) if raster.replace_dynamic(gpu, b.mesh, &data) => b.mesh,
            _ => {
                if let Some(old) = self.decal_batches.batches.remove(&key) {
                    raster.free_dynamic(old.mesh);
                }
                let id = raster.register_dynamic(gpu, data.vertices.len() as u32, data.indices.len() as u32, true);
                raster.replace_dynamic(gpu, id, &data);
                id
            }
        };
        self.decal_batches.batches.insert(key, DecalBatch { mesh, tex, anchor });
    }
}

/// One instance per batch, camera-relative: a batch that rides a frame is
/// drawn where the frame is this frame. A free function for the render
/// loop's borrow split, like `push_terrain_instances`.
pub(crate) fn push_decal_instances(
    batches: &DecalBatches,
    world: &floptle_core::World,
    raster: &floptle_render::Raster,
    cam_world: DVec3,
    instances: &mut Vec<(MeshId, Option<TexId>, floptle_render::InstanceRaw)>,
) {
    for ((_, frame), b) in &batches.batches {
        let model = match frame.and_then(|f| world.entity_with::<Transform>(f)) {
            Some(e) => floptle_core::world_transform(world, e)
                .mul_transform(&Transform::from_translation(b.anchor))
                .render_matrix(cam_world),
            None => Mat4::from_translation((b.anchor - cam_world).as_vec3()),
        };
        let mut mp = MaterialParams::flat([1.0; 3]);
        mp.alpha = BATCH_ALPHA;
        mp.terrain_paint_base = raster.dyn_paint_base(b.mesh);
        instances.push((b.mesh, Some(b.tex), floptle_render::instance_of_mat(model, &mp)));
    }
}
