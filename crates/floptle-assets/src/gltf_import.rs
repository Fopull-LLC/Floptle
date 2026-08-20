//! glTF 2.0 import (Blender's native export format, ADR-0006).
//!
//! Loads a `.glb`/`.gltf`, walks the scene's node tree baking each node's world
//! transform into the vertices, and groups triangle primitives **per material**
//! into [`ImportedPart`]s — each carrying its geometry, base-color tint, and an
//! optional base-color texture (decoded to RGBA8). The whole model is recentered
//! to the origin so it frames/places predictably regardless of native scale.
//!
//! Deferred (per `docs/subsystems/asset-pipeline.md`): the metallic/roughness/
//! normal/emissive maps, per-material samplers and alpha blending, skins, and
//! animations. This returns what the lit textured pass consumes today.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use floptle_core::math::{Mat3, Mat4, Vec3};
use floptle_render::{MeshData, TextureData, Vertex};

/// A model imported from glTF: per-material parts (recentered to the origin), the
/// decoded base-color textures they reference, and overall bounds.
#[derive(Clone, Debug)]
pub struct ImportedModel {
    pub name: String,
    pub parts: Vec<ImportedPart>,
    /// Decoded base-color textures, indexed by `ImportedPart::texture`.
    pub textures: Vec<TextureData>,
    pub size: f32,
    /// Axis-aligned bounds after recentering (symmetric about the origin, so
    /// `min == -max`). `min[1]` is the floor offset for ground placement.
    pub min: [f32; 3],
    pub max: [f32; 3],
}

/// One material's worth of geometry within a model.
#[derive(Clone, Debug)]
pub struct ImportedPart {
    pub mesh: MeshData,
    /// The glTF material's name (`Material N` when unnamed) — surfaced in the
    /// editor's materials list and usable as an override key.
    pub material: String,
    /// Base-color factor (rgb) — a tint multiplied onto the texture.
    pub base_color: [f32; 3],
    /// Index into [`ImportedModel::textures`], if the material has a base-color map.
    pub texture: Option<usize>,
}

/// What can go wrong importing a model.
#[derive(Debug)]
pub enum ImportError {
    Gltf(gltf::Error),
    NoGeometry,
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportError::Gltf(e) => write!(f, "glTF parse error: {e}"),
            ImportError::NoGeometry => write!(f, "glTF contained no triangle geometry"),
        }
    }
}

impl std::error::Error for ImportError {}

/// Import a `.glb`/`.gltf` model into per-material parts + decoded textures.
pub fn import(path: &Path) -> Result<ImportedModel, ImportError> {
    import_inner(path, true)
}

/// The same model with **no images read and no textures decoded**.
///
/// For every caller that wants a model's *shape*: the navmesh bake, a mesh
/// collider, a measurement. Decoding a model's textures to answer a question
/// about its vertex positions is pure waste, and it is not small waste — a
/// streamed level of 313 props in `floptle/0140` spent ~43 MB of RGBA
/// allocation, per bake, on pixels it dropped on the next line.
///
/// Geometry is produced by the same walk as [`import`], deliberately: two
/// importers that were supposed to agree about vertex positions and did not
/// would move a level's navmesh out from under it, and the disagreement would
/// be invisible until somebody walked into a wall.
pub fn import_geometry(path: &Path) -> Result<ImportedModel, ImportError> {
    import_inner(path, false)
}

/// Path + when it was last written → the geometry read from it.
type GeometryCache = HashMap<(PathBuf, Option<SystemTime>), Arc<ImportedModel>>;

/// [`import_geometry`], memoised on path + modification time.
///
/// A bake reads every model in the level; a level that changes while you walk
/// through it bakes again and again, and re-reading an unchanged `.glb` off disk
/// each time is the largest single cost in it. Keyed on mtime rather than path
/// alone so re-exporting a model from Blender is picked up without anybody
/// having to know a cache exists.
pub fn geometry(path: &Path) -> Result<Arc<ImportedModel>, ImportError> {
    static CACHE: OnceLock<Mutex<GeometryCache>> = OnceLock::new();
    let stamp = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let key = (path.to_path_buf(), stamp);
    let cache = CACHE.get_or_init(Default::default);
    if let Ok(map) = cache.lock()
        && let Some(hit) = map.get(&key)
    {
        return Ok(hit.clone());
    }
    let model = Arc::new(import_geometry(path)?);
    if let Ok(mut map) = cache.lock() {
        // A model re-exported mid-session leaves its old entry behind. Drop the
        // stale stamps for this path rather than growing a set per save.
        map.retain(|(p, s), _| p != path || *s == stamp);
        map.insert(key, model.clone());
    }
    Ok(model)
}

fn import_inner(path: &Path, want_textures: bool) -> Result<ImportedModel, ImportError> {
    let (doc, buffers, textures) = if want_textures {
        let (doc, buffers, images) = gltf::import(path).map_err(ImportError::Gltf)?;
        // Decode every image to RGBA8 once; parts reference them by index.
        (doc, buffers, images.iter().map(to_rgba8).collect())
    } else {
        // `Gltf::open` parses the document and keeps the binary blob; it does
        // not touch the images at all.
        let gltf = gltf::Gltf::open(path).map_err(ImportError::Gltf)?;
        let buffers = gltf::import_buffers(&gltf.document, path.parent(), gltf.blob.clone())
            .map_err(ImportError::Gltf)?;
        (gltf.document, buffers, Vec::new())
    };

    let mut parts = Parts::default();
    if let Some(scene) = doc.default_scene().or_else(|| doc.scenes().next()) {
        for node in scene.nodes() {
            add_node(&node, Mat4::IDENTITY, &buffers, &mut parts);
        }
    } else {
        for m in doc.meshes() {
            add_primitives(&m, Mat4::IDENTITY, &buffers, &mut parts);
        }
    }

    let mut parts = parts.list;
    parts.retain(|p| !p.mesh.vertices.is_empty() && !p.mesh.indices.is_empty());
    if parts.is_empty() {
        return Err(ImportError::NoGeometry);
    }

    let (size, min, max) = recenter_and_measure(&mut parts);
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("model").to_string();
    Ok(ImportedModel { name, parts, textures, size, min, max })
}

/// Per-material accumulator: maps a material key (its index, or -1 for the default
/// material) to a part being built.
#[derive(Default)]
struct Parts {
    by_key: HashMap<i64, usize>,
    list: Vec<ImportedPart>,
}

impl Parts {
    /// The part for `material`, creating it (with tint + texture) on first use.
    fn part_for(&mut self, material: &gltf::Material) -> usize {
        let key = material.index().map(|i| i as i64).unwrap_or(-1);
        if let Some(&idx) = self.by_key.get(&key) {
            return idx;
        }
        let pbr = material.pbr_metallic_roughness();
        let bcf = pbr.base_color_factor();
        let texture = pbr.base_color_texture().map(|info| info.texture().source().index());
        let name = material
            .name()
            .map(str::to_string)
            .unwrap_or_else(|| format!("Material {}", material.index().map_or(0, |i| i + 1)));
        self.list.push(ImportedPart {
            mesh: MeshData::default(),
            material: name,
            base_color: [bcf[0], bcf[1], bcf[2]],
            texture,
        });
        let idx = self.list.len() - 1;
        self.by_key.insert(key, idx);
        idx
    }
}

fn add_node(node: &gltf::Node, parent: Mat4, buffers: &[gltf::buffer::Data], parts: &mut Parts) {
    let world = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
    if let Some(m) = node.mesh() {
        add_primitives(&m, world, buffers, parts);
    }
    for child in node.children() {
        add_node(&child, world, buffers, parts);
    }
}

fn add_primitives(m: &gltf::Mesh, world: Mat4, buffers: &[gltf::buffer::Data], parts: &mut Parts) {
    let m3 = Mat3::from_mat4(world);
    let nmat = if m3.determinant().abs() > 1e-12 { m3.inverse().transpose() } else { m3 };

    for prim in m.primitives() {
        if prim.mode() != gltf::mesh::Mode::Triangles {
            continue;
        }
        let reader = prim.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
        let Some(pos_iter) = reader.read_positions() else { continue };
        let positions: Vec<[f32; 3]> = pos_iter.collect();
        if positions.is_empty() {
            continue;
        }
        let indices: Vec<u32> = match reader.read_indices() {
            Some(ri) => ri.into_u32().collect(),
            None => (0..positions.len() as u32).collect(),
        };
        let normals: Vec<[f32; 3]> = match reader.read_normals() {
            Some(it) => it.collect(),
            None => compute_normals(&positions, &indices),
        };
        let uvs: Option<Vec<[f32; 2]>> = reader.read_tex_coords(0).map(|tc| tc.into_f32().collect());
        // COLOR_0 — vertex paint authored in Blender. glTF allows RGB or RGBA, u8/u16/f32;
        // `into_rgba_u8` normalizes all six spellings to the RGBA8 the `vpaint` store wants.
        let colors: Option<Vec<[u8; 4]>> = reader.read_colors(0).map(|c| c.into_rgba_u8().collect());

        let part_idx = parts.part_for(&prim.material());
        let part = &mut parts.list[part_idx];
        let base = part.mesh.vertices.len() as u32;
        for i in 0..positions.len() {
            let p = world.transform_point3(Vec3::from(positions[i]));
            let n = (nmat * Vec3::from(normals[i])).normalize_or_zero();
            let uv = uvs.as_ref().map(|u| u[i]).unwrap_or([0.0, 0.0]);
            part.mesh.vertices.push(Vertex { pos: p.to_array(), normal: n.to_array(), uv });
        }
        for idx in indices {
            part.mesh.indices.push(base + idx);
        }

        // Parts accumulate MANY primitives, and COLOR_0 is per-primitive — so a part can
        // mix painted and unpainted prims. The colors stream must stay exactly parallel to
        // `vertices` (the renderer drops a mismatched one), so back-fill earlier unpainted
        // prims with white and pad this one if it came up short. White is the identity for
        // the albedo multiply, so back-filled vertices render exactly as before.
        const WHITE: [u8; 4] = [255; 4];
        if colors.is_some() || part.mesh.colors.is_some() {
            let c = part.mesh.colors.get_or_insert_with(Vec::new);
            c.resize(base as usize, WHITE);
            match &colors {
                Some(src) => {
                    c.extend(src.iter().take(positions.len()).copied());
                    c.resize(base as usize + positions.len(), WHITE);
                }
                None => c.resize(base as usize + positions.len(), WHITE),
            }
        }
    }
}

/// Recenter every part by the model's combined AABB center; return (size, min, max).
fn recenter_and_measure(parts: &mut [ImportedPart]) -> (f32, [f32; 3], [f32; 3]) {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for part in parts.iter() {
        for v in &part.mesh.vertices {
            let p = Vec3::from(v.pos);
            min = min.min(p);
            max = max.max(p);
        }
    }
    let center = (min + max) * 0.5;
    for part in parts.iter_mut() {
        for v in &mut part.mesh.vertices {
            v.pos = [v.pos[0] - center.x, v.pos[1] - center.y, v.pos[2] - center.z];
        }
    }
    let half = (max - min) * 0.5;
    let size = (max - min).max_element().max(1e-6);
    (size, (-half).to_array(), half.to_array())
}

/// Area-weighted vertex normals for a primitive that ships without them.
pub(crate) fn compute_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut acc = vec![Vec3::ZERO; positions.len()];
    for tri in indices.as_chunks::<3>().0 {
        let (a, b, c) = (
            Vec3::from(positions[tri[0] as usize]),
            Vec3::from(positions[tri[1] as usize]),
            Vec3::from(positions[tri[2] as usize]),
        );
        let face = (b - a).cross(c - a);
        acc[tri[0] as usize] += face;
        acc[tri[1] as usize] += face;
        acc[tri[2] as usize] += face;
    }
    acc.iter().map(|v| v.normalize_or_zero().to_array()).collect()
}

/// Convert a decoded glTF image to tightly-packed RGBA8.
pub(crate) fn to_rgba8(img: &gltf::image::Data) -> TextureData {
    use gltf::image::Format::*;
    let (w, h) = (img.width, img.height);
    let n = (w as usize) * (h as usize);
    let s = &img.pixels;
    let mut out = vec![0u8; n * 4];
    // Read a little-endian 16-bit channel's high byte; a 32-bit float channel → u8.
    let hi16 = |i: usize| s[i * 2 + 1];
    let f32at = |i: usize| {
        f32::from_le_bytes([s[i * 4], s[i * 4 + 1], s[i * 4 + 2], s[i * 4 + 3]]).clamp(0.0, 1.0)
    };
    for i in 0..n {
        let (r, g, b, a) = match img.format {
            R8 => (s[i], s[i], s[i], 255),
            R8G8 => (s[i * 2], s[i * 2], s[i * 2], s[i * 2 + 1]),
            R8G8B8 => (s[i * 3], s[i * 3 + 1], s[i * 3 + 2], 255),
            R8G8B8A8 => (s[i * 4], s[i * 4 + 1], s[i * 4 + 2], s[i * 4 + 3]),
            R16 => (hi16(i), hi16(i), hi16(i), 255),
            R16G16 => (hi16(i * 2), hi16(i * 2), hi16(i * 2), hi16(i * 2 + 1)),
            R16G16B16 => (hi16(i * 3), hi16(i * 3 + 1), hi16(i * 3 + 2), 255),
            R16G16B16A16 => (hi16(i * 4), hi16(i * 4 + 1), hi16(i * 4 + 2), hi16(i * 4 + 3)),
            R32G32B32FLOAT => {
                let p = |c| (f32at(i * 3 + c) * 255.0) as u8;
                (p(0), p(1), p(2), 255)
            }
            R32G32B32A32FLOAT => {
                let p = |c| (f32at(i * 4 + c) * 255.0) as u8;
                (p(0), p(1), p(2), p(3))
            }
        };
        out[i * 4] = r;
        out[i * 4 + 1] = g;
        out[i * 4 + 2] = b;
        out[i * 4 + 3] = a;
    }
    TextureData { pixels: out, width: w, height: h }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_normals_point_along_face() {
        let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let n = compute_normals(&positions, &[0, 1, 2]);
        assert_eq!(n.len(), 3);
        for v in n {
            assert!((v[0]).abs() < 1e-6 && (v[1]).abs() < 1e-6);
            assert!((v[2] - 1.0).abs() < 1e-6, "expected +Z normal, got {v:?}");
        }
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    /// A model in the repo to measure against. `None` skips rather than fails —
    /// this file is checked in, but a test that hard-fails on a missing asset
    /// turns an asset move into a red build for no engineering reason.
    fn sample() -> Option<PathBuf> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/fofighter-kit/assets/models/Elvira.glb");
        p.exists().then_some(p)
    }

    /// The two importers must agree about vertex positions, exactly.
    ///
    /// **This is the one that matters.** They are two entry points into one walk
    /// precisely so they cannot drift — but "cannot" is a claim, and a
    /// disagreement here would move a level's navmesh out from under the
    /// characters walking on it, invisibly, until somebody walked into a wall.
    #[test]
    fn geometry_is_the_same_shape_as_a_full_import() {
        let Some(p) = sample() else { return };
        let full = import(&p).expect("the sample imports");
        let geo = import_geometry(&p).expect("and imports without its textures");

        assert_eq!(geo.parts.len(), full.parts.len(), "a different number of parts");
        assert_eq!(geo.size, full.size);
        assert_eq!(geo.min, full.min);
        assert_eq!(geo.max, full.max);
        for (a, b) in geo.parts.iter().zip(&full.parts) {
            assert_eq!(a.mesh.indices, b.mesh.indices);
            let pa: Vec<[f32; 3]> = a.mesh.vertices.iter().map(|v| v.pos).collect();
            let pb: Vec<[f32; 3]> = b.mesh.vertices.iter().map(|v| v.pos).collect();
            assert_eq!(pa, pb, "vertex positions differ between the two importers");
        }
        assert!(geo.textures.is_empty(), "the geometry path decoded textures anyway");
        assert!(!full.textures.is_empty(), "the sample has no textures, so this proves nothing");
    }

    /// Two reads of the same unchanged file are one read.
    #[test]
    fn geometry_is_cached_on_the_file_it_read() {
        let Some(p) = sample() else { return };
        let a = geometry(&p).expect("first read");
        let b = geometry(&p).expect("second read");
        assert!(Arc::ptr_eq(&a, &b), "the second read went back to disk");
    }

    /// The numbers `floptle/0140` asks for, printed rather than asserted —
    /// a threshold in wall-clock on a shared runner is a coin flip.
    ///
    /// `cargo test -p floptle-assets --release geometry_cost -- --nocapture`
    #[test]
    fn geometry_cost_against_a_full_import() {
        let Some(p) = sample() else { return };
        let time = |mut f: Box<dyn FnMut()>| {
            let t = std::time::Instant::now();
            for _ in 0..8 {
                f();
            }
            t.elapsed().as_secs_f64() * 1000.0 / 8.0
        };
        let full = time(Box::new(|| {
            let _ = std::hint::black_box(import(&p));
        }));
        let geo = time(Box::new(|| {
            let _ = std::hint::black_box(import_geometry(&p));
        }));
        let cached = time(Box::new(|| {
            let _ = std::hint::black_box(geometry(&p));
        }));
        let bytes: usize =
            import(&p).map(|m| m.textures.iter().map(|t| t.pixels.len()).sum()).unwrap_or(0);
        println!("\n  {}", p.file_name().unwrap().to_string_lossy());
        println!("    full import      {full:7.2} ms   ({} KB of texture decoded)", bytes / 1024);
        println!("    geometry only    {geo:7.2} ms");
        println!("    geometry cached  {cached:7.3} ms");
        println!(
            "\n  A navmesh bake reads every model in the level, and a level that\n  \
             streams bakes again every few seconds. The third row is what a\n  \
             second bake of an unchanged level now costs.\n"
        );
    }
}
