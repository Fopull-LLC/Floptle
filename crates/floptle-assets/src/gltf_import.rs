//! glTF 2.0 import (Blender's native export format).
//!
//! Loads a `.glb`/`.gltf`, walks the scene's node tree baking each node's world
//! transform into the vertices, and groups triangle primitives **per material**
//! into [`ImportedPart`]s — each carrying its geometry, base-color tint, and an
//! optional base-color texture (decoded to RGBA8). The whole model is recentered
//! to the origin so it frames/places predictably regardless of native scale.
//!
//! Each part also carries its material's other maps — metallic-roughness,
//! normal, occlusion and emissive — with their factors ([`PartMaps`]), which the
//! editor turns into the same surface a hand-set material with those maps
//! draws. Deferred (per `docs/subsystems/asset-pipeline.md`): per-material
//! samplers, alpha blending and a second UV set; skins and animations come
//! through `gltf_rig`.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use floptle_core::time::SystemTime;

use floptle_core::math::{Mat3, Mat4, Vec3};
use floptle_render::{MeshData, TexFilter, TextureData, Vertex};

/// A model imported from glTF: per-material parts (recentered to the origin), the
/// decoded base-color textures they reference, and overall bounds.
#[derive(Clone, Debug)]
pub struct ImportedModel {
    pub name: String,
    pub parts: Vec<ImportedPart>,
    /// Decoded base-color textures, indexed by `ImportedPart::texture`.
    pub textures: Vec<TextureData>,
    /// How the file says each of `textures` is filtered, in the same order:
    /// `None` where it does not say. See [`image_filters`].
    pub texture_filters: Vec<Option<TexFilter>>,
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
    /// The material's other maps and factors.
    pub maps: PartMaps,
}

/// A glTF material's maps beyond base colour, as indices into the model's
/// decoded images, and the factors that scale them. glTF's packing is the
/// engine's: roughness in green and metallic in blue of one image, occlusion
/// in red.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PartMaps {
    pub metallic_roughness: Option<usize>,
    pub metallic: f32,
    pub roughness: f32,
    pub normal: Option<usize>,
    pub normal_scale: f32,
    pub occlusion: Option<usize>,
    pub occlusion_strength: f32,
    pub emissive: Option<usize>,
    pub emissive_factor: [f32; 3],
}

impl PartMaps {
    /// Does the material ask for anything a plain base colour does not
    /// draw — a map, or a glow?
    pub fn any(&self) -> bool {
        self.metallic_roughness.is_some()
            || self.normal.is_some()
            || self.occlusion.is_some()
            || self.emissive.is_some()
            || self.emissive_factor.iter().any(|&c| c > 0.0)
    }
}

/// The maps of `m`. A map on a second UV set is left out: every mesh here
/// carries one.
pub(crate) fn part_maps(m: &gltf::Material) -> PartMaps {
    let pbr = m.pbr_metallic_roughness();
    let first_uv = |set: u32, idx: usize| (set == 0).then_some(idx);
    PartMaps {
        metallic_roughness: pbr
            .metallic_roughness_texture()
            .and_then(|t| first_uv(t.tex_coord(), t.texture().source().index())),
        metallic: pbr.metallic_factor(),
        roughness: pbr.roughness_factor(),
        normal: m.normal_texture().and_then(|t| first_uv(t.tex_coord(), t.texture().source().index())),
        normal_scale: m.normal_texture().map_or(1.0, |t| t.scale()),
        occlusion: m.occlusion_texture().and_then(|t| first_uv(t.tex_coord(), t.texture().source().index())),
        occlusion_strength: m.occlusion_texture().map_or(1.0, |t| t.strength()),
        emissive: m.emissive_texture().and_then(|t| first_uv(t.tex_coord(), t.texture().source().index())),
        emissive_factor: m.emissive_factor(),
    }
}

/// What can go wrong importing a model.
#[derive(Debug)]
pub enum ImportError {
    Gltf(gltf::Error),
    NoGeometry,
    /// The file requires an extension this importer cannot decode, so it has no
    /// geometry it could read. Named, so the fix is in the message.
    Unsupported(&'static str),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportError::Gltf(e) => write!(f, "glTF parse error: {e}"),
            ImportError::NoGeometry => write!(f, "glTF contained no triangle geometry"),
            ImportError::Unsupported(ext) => write!(
                f,
                "the file is compressed with {ext}, which Floptle cannot decode; \
                 re-export it without mesh compression (Blender: File ▸ Export ▸ glTF ▸ Data ▸ Compression off)"
            ),
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
/// streamed level of 313 props spends ~43 MB of RGBA allocation, per bake,
/// on pixels it drops on the next line.
///
/// Geometry is produced by the same walk as [`import`]: two
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
    let stamp = floptle_vfs::modified(path);
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

/// Mesh compression this importer cannot decode. A file that requires one keeps
/// its vertices only in the compressed stream; a file that merely uses one
/// carries an uncompressed fallback and loads.
const REFUSED_EXTENSIONS: [&str; 2] = ["KHR_draco_mesh_compression", "EXT_meshopt_compression"];

fn refused_extension(doc: &gltf::Document) -> Option<&'static str> {
    doc.extensions_required().find_map(|e| REFUSED_EXTENSIONS.iter().copied().find(|r| *r == e))
}

/// Parse a glTF, naming the compression when that is why it was refused. The
/// validator already turns such a file away, as "Unsupported extension" among
/// its other findings; the name and the remedy are what a person needs.
fn parse(bytes: &[u8]) -> Result<gltf::Gltf, ImportError> {
    gltf::Gltf::from_slice(bytes).map_err(|e| {
        match gltf::Gltf::from_slice_without_validation(bytes).ok().and_then(|g| refused_extension(&g.document)) {
            Some(ext) => ImportError::Unsupported(ext),
            None => ImportError::Gltf(e),
        }
    })
}

/// Can this importer read the model at `path`? Parses the document only, not
/// its images, so `floptle check` can ask it of every model a project names.
pub fn check_readable(path: &Path) -> Result<(), ImportError> {
    let bytes = floptle_vfs::read(path).map_err(|e| ImportError::Gltf(gltf::Error::Io(e)))?;
    parse(&bytes).map(|_| ())
}

/// Read a glTF file and everything it references — the document, its
/// buffers, and (when asked) its images decoded to pixels — through the
/// engine's filesystem.
///
/// This is `gltf::import` re-done over `floptle_vfs`, because `gltf::import`
/// opens files itself: fine on a desktop, and a browser build has no disk to
/// open. The three sources a glTF buffer or image can name are all here — the
/// `.glb` binary chunk, a sibling file by relative URI, and a base64 data URI.
/// `want_images: false` skips the images entirely: nothing that asks for a
/// model's *shape* should pay to decode its textures.
pub(crate) fn read_gltf(
    path: &Path,
    want_images: bool,
) -> Result<(gltf::Document, Vec<gltf::buffer::Data>, Vec<gltf::image::Data>), ImportError> {
    let bytes = floptle_vfs::read(path).map_err(|e| ImportError::Gltf(gltf::Error::Io(e)))?;
    let gltf::Gltf { document, mut blob } = parse(&bytes)?;
    let base = path.parent();
    let mut buffers = Vec::with_capacity(document.buffers().len());
    for buffer in document.buffers() {
        let mut data = match buffer.source() {
            gltf::buffer::Source::Bin => blob.take().ok_or(ImportError::Gltf(gltf::Error::MissingBlob))?,
            gltf::buffer::Source::Uri(uri) => read_uri(base, uri)?,
        };
        if data.len() < buffer.length() {
            return Err(ImportError::Gltf(gltf::Error::BufferLength {
                buffer: buffer.index(),
                expected: buffer.length(),
                actual: data.len(),
            }));
        }
        // glTF wants every buffer 4-byte aligned; `gltf::import` pads the same way.
        while data.len() % 4 != 0 {
            data.push(0);
        }
        buffers.push(gltf::buffer::Data(data));
    }
    let mut images = Vec::new();
    if want_images {
        for image in document.images() {
            // An image in a file beside the model is decoded once per session
            // however many models name it: a kit of parts pointing at one
            // texture library reads and decodes the library once.
            if let gltf::image::Source::Uri { uri, .. } = image.source()
                && let Some(file) = uri_file(base, uri)
            {
                let decoded = shared_image(&file)?;
                images.push(gltf::image::Data {
                    pixels: decoded.pixels.clone(),
                    format: gltf::image::Format::R8G8B8A8,
                    width: decoded.width,
                    height: decoded.height,
                });
                continue;
            }
            let encoded: std::borrow::Cow<[u8]> = match image.source() {
                gltf::image::Source::View { view, .. } => {
                    let parent = &buffers[view.buffer().index()].0;
                    let (start, end) = (view.offset(), view.offset() + view.length());
                    std::borrow::Cow::Borrowed(parent.get(start..end).ok_or(ImportError::NoGeometry)?)
                }
                gltf::image::Source::Uri { uri, .. } => std::borrow::Cow::Owned(read_uri(base, uri)?),
            };
            images.push(decode_image(&encoded)?);
        }
    }
    Ok((document, buffers, images))
}

/// Encoded image bytes to RGBA8.
fn decode_image(encoded: &[u8]) -> Result<gltf::image::Data, ImportError> {
    let decoded = image::load_from_memory(encoded)
        .map_err(|e| ImportError::Gltf(gltf::Error::Io(std::io::Error::other(e.to_string()))))?
        .to_rgba8();
    let (width, height) = decoded.dimensions();
    Ok(gltf::image::Data { pixels: decoded.into_raw(), format: gltf::image::Format::R8G8B8A8, width, height })
}

/// The file a relative glTF URI names, or `None` for a data URI (whose bytes
/// are the model's own).
fn uri_file(base: Option<&Path>, uri: &str) -> Option<PathBuf> {
    if uri.starts_with("data:") {
        return None;
    }
    // A relative URI is percent-encoded; the common case is a plain name.
    let name = uri.replace("%20", " ");
    Some(base.map(|b| b.join(&name)).unwrap_or_else(|| PathBuf::from(&name)))
}

/// One decoded image file, as [`shared_image`] keeps it.
#[derive(Debug)]
pub(crate) struct SharedImage {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Decoded pixels kept for this many bytes at most. A texture library shared
/// by a kit of models fits many times over; past it the least recently used
/// image is dropped and decoded again if asked for.
const IMAGE_CACHE_BYTES: usize = 256 << 20;

#[derive(Default)]
struct ImageCache {
    /// Path + when it was last written → the decoded image and when it was
    /// last asked for.
    map: HashMap<(PathBuf, Option<SystemTime>), (Arc<SharedImage>, u64)>,
    bytes: usize,
    clock: u64,
}

/// The decoded image in `file`, decoded once and shared by every model that
/// names it. Keyed on modification time as well as path, so an image saved
/// over mid-session is read afresh.
pub(crate) fn shared_image(file: &Path) -> Result<Arc<SharedImage>, ImportError> {
    static CACHE: OnceLock<Mutex<ImageCache>> = OnceLock::new();
    let file = floptle_vfs::normalize(file);
    let stamp = floptle_vfs::modified(&file);
    let key = (file.clone(), stamp);
    let cache = CACHE.get_or_init(Default::default);
    if let Ok(mut c) = cache.lock() {
        c.clock += 1;
        let now = c.clock;
        if let Some((hit, used)) = c.map.get_mut(&key) {
            *used = now;
            return Ok(hit.clone());
        }
    }
    let encoded = floptle_vfs::read(&file).map_err(|e| ImportError::Gltf(gltf::Error::Io(e)))?;
    let d = decode_image(&encoded)?;
    let image = Arc::new(SharedImage { pixels: d.pixels, width: d.width, height: d.height });
    if let Ok(mut c) = cache.lock() {
        let size = image.pixels.len();
        // The same file's older stamps are stale now.
        let stale: Vec<_> = c.map.keys().filter(|(p, s)| *p == file && *s != stamp).cloned().collect();
        for k in stale {
            if let Some((old, _)) = c.map.remove(&k) {
                c.bytes -= old.pixels.len();
            }
        }
        while c.bytes + size > IMAGE_CACHE_BYTES && !c.map.is_empty() {
            let Some(oldest) = c.map.iter().min_by_key(|(_, (_, used))| *used).map(|(k, _)| k.clone()) else { break };
            if let Some((old, _)) = c.map.remove(&oldest) {
                c.bytes -= old.pixels.len();
            }
        }
        if size <= IMAGE_CACHE_BYTES {
            c.clock += 1;
            let now = c.clock;
            c.bytes += size;
            c.map.insert(key, (image.clone(), now));
        }
    }
    Ok(image)
}

/// The bytes a glTF URI names: a base64 data URI decoded, or a file beside
/// the model read through the engine's filesystem.
fn read_uri(base: Option<&Path>, uri: &str) -> Result<Vec<u8>, ImportError> {
    if let Some(rest) = uri.strip_prefix("data:") {
        let payload = rest.split_once(";base64,").map(|(_, b)| b).unwrap_or(rest);
        use base64::Engine as _;
        return base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|e| ImportError::Gltf(gltf::Error::Io(std::io::Error::other(format!("data URI: {e}")))));
    }
    let file = uri_file(base, uri).unwrap_or_default();
    floptle_vfs::read(&file).map_err(|e| ImportError::Gltf(gltf::Error::Io(e)))
}

fn import_inner(path: &Path, want_textures: bool) -> Result<ImportedModel, ImportError> {
    let (doc, buffers, images) = read_gltf(path, want_textures)?;
    build_static(path, &doc, &buffers, &images)
}

/// The static bake from a file [`read_gltf`] has already read.
pub(crate) fn build_static(
    path: &Path,
    doc: &gltf::Document,
    buffers: &[gltf::buffer::Data],
    images: &[gltf::image::Data],
) -> Result<ImportedModel, ImportError> {
    // Decode every image to RGBA8 once; parts reference them by index.
    let textures: Vec<TextureData> = images.iter().map(to_rgba8).collect();
    let texture_filters = image_filters(doc, textures.len());

    let mut parts = Parts::default();
    if let Some(scene) = doc.default_scene().or_else(|| doc.scenes().next()) {
        for node in scene.nodes() {
            add_node(&node, Mat4::IDENTITY, buffers, &mut parts);
        }
    } else {
        for m in doc.meshes() {
            add_primitives(&m, Mat4::IDENTITY, buffers, &mut parts);
        }
    }

    let mut parts = parts.list;
    parts.retain(|p| !p.mesh.vertices.is_empty() && !p.mesh.indices.is_empty());
    if parts.is_empty() {
        return Err(ImportError::NoGeometry);
    }

    let (size, min, max) = recenter_and_measure(&mut parts);
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("model").to_string();
    Ok(ImportedModel { name, parts, textures, texture_filters, size, min, max })
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
            maps: part_maps(material),
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

        // Parts accumulate many primitives, and COLOR_0 is per-primitive — so a part can
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

/// How a glTF sampler filters, as the engine's [`TexFilter`]: point
/// magnification is crisp pixels, linear is smooth, and smooth gets a mip
/// chain unless the sampler asks for a minification without one. `None` when
/// the sampler names neither filter, which glTF leaves to the engine.
pub fn sampler_filter(
    mag: Option<gltf::texture::MagFilter>,
    min: Option<gltf::texture::MinFilter>,
) -> Option<TexFilter> {
    use gltf::texture::{MagFilter, MinFilter};
    match (mag, min) {
        (Some(MagFilter::Nearest), _) => Some(TexFilter::Pixelated),
        (Some(MagFilter::Linear), Some(MinFilter::Nearest | MinFilter::Linear)) => Some(TexFilter::Smooth),
        (Some(MagFilter::Linear), _) => Some(TexFilter::SmoothMipmaps),
        (None, Some(MinFilter::Nearest | MinFilter::NearestMipmapNearest)) => Some(TexFilter::Pixelated),
        (None, Some(MinFilter::Linear)) => Some(TexFilter::Smooth),
        (None, Some(_)) => Some(TexFilter::SmoothMipmaps),
        (None, None) => None,
    }
}

/// What the engine's own `.glb` writer signs its files with. Files signed
/// [`OLD_GENERATOR`] came from a writer that gave every texture the same
/// smooth, mipmapped sampler whatever its source said, so their samplers say
/// nothing about the texture.
pub const GENERATOR: &str = "Floptle glb_write 2";
const OLD_GENERATOR: &str = "Floptle glb_write";

/// Each image's filter as the file states it, one per image: the sampler of
/// the first texture that shows that image. A texture's sampler belongs to the
/// texture, not the image, so two textures could show one image two ways; the
/// engine uploads an image once, and the first one wins.
pub(crate) fn image_filters(doc: &gltf::Document, images: usize) -> Vec<Option<TexFilter>> {
    let mut out = vec![None; images];
    if doc.as_json().asset.generator.as_deref() == Some(OLD_GENERATOR) {
        return out;
    }
    let mut seen = vec![false; images];
    for t in doc.textures() {
        let i = t.source().index();
        if i < images && !seen[i] {
            seen[i] = true;
            let s = t.sampler();
            out[i] = sampler_filter(s.mag_filter(), s.min_filter());
        }
    }
    out
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

    /// **An image file named by many models is decoded once.** A kit of
    /// parts pointing at one texture library decoded the library once per
    /// part, and carried a copy per part on the GPU. The decode is now shared
    /// by the file's resolved path, through `../` and all, and an image saved
    /// over mid-session is read afresh.
    #[test]
    fn an_image_file_many_models_name_is_decoded_once() {
        use base64::Engine as _;
        let dir = std::env::temp_dir().join(format!("floptle-shared-image-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("parts")).unwrap();
        let png = |rgba: [u8; 4]| {
            let img = image::RgbaImage::from_pixel(2, 2, image::Rgba(rgba));
            let mut out = std::io::Cursor::new(Vec::new());
            img.write_to(&mut out, image::ImageFormat::Png).unwrap();
            out.into_inner()
        };
        std::fs::write(dir.join("library.png"), png([10, 20, 30, 255])).unwrap();
        // One triangle and an index list, with the image by relative URI.
        let mut bin = Vec::new();
        for v in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
            bin.extend_from_slice(&v.to_le_bytes());
        }
        bin.extend_from_slice(&[0, 0, 1, 0, 2, 0, 0, 0]);
        let data = base64::engine::general_purpose::STANDARD.encode(&bin);
        let gltf = |uri: &str| {
            format!(
                r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0}}],
                "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1,"material":0}}]}}],
                "materials":[{{"pbrMetallicRoughness":{{"baseColorTexture":{{"index":0}}}}}}],
                "textures":[{{"source":0}}],"images":[{{"uri":"{uri}"}}],
                "buffers":[{{"byteLength":44,"uri":"data:application/octet-stream;base64,{data}"}}],
                "bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":6}}],
                "accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
                {{"bufferView":1,"componentType":5123,"count":3,"type":"SCALAR"}}]}}"#
            )
        };
        std::fs::write(dir.join("a.gltf"), gltf("library.png")).unwrap();
        std::fs::write(dir.join("parts/b.gltf"), gltf("../library.png")).unwrap();
        let a = import(&dir.join("a.gltf")).unwrap();
        let b = import(&dir.join("parts/b.gltf")).unwrap();
        assert_eq!(a.textures[0].pixels[..4], [10, 20, 30, 255]);
        assert_eq!(a.textures[0].pixels, b.textures[0].pixels);
        // Both models' decodes are one decode.
        let lib = dir.join("library.png");
        let once = shared_image(&lib).unwrap();
        assert!(Arc::ptr_eq(&once, &shared_image(&dir.join("parts/../library.png")).unwrap()), "decoded again");
        // Saved over: the new pixels, not the remembered ones.
        std::fs::write(&lib, png([200, 100, 50, 255])).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(&lib).unwrap().set_modified(later).unwrap();
        assert_eq!(import(&dir.join("a.gltf")).unwrap().textures[0].pixels[..4], [200, 100, 50, 255]);
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// What each sampler a modelling tool writes means here. A pixel-art export
    /// says nearest; Blender's default says linear with mipmaps; a sampler
    /// that names nothing leaves it to the engine.
    #[test]
    fn a_sampler_says_point_or_smooth() {
        use gltf::texture::{MagFilter as Mag, MinFilter as Min};
        assert_eq!(sampler_filter(Some(Mag::Nearest), Some(Min::Nearest)), Some(TexFilter::Pixelated));
        assert_eq!(sampler_filter(Some(Mag::Nearest), Some(Min::LinearMipmapLinear)), Some(TexFilter::Pixelated));
        assert_eq!(sampler_filter(Some(Mag::Linear), Some(Min::LinearMipmapLinear)), Some(TexFilter::SmoothMipmaps));
        assert_eq!(sampler_filter(Some(Mag::Linear), None), Some(TexFilter::SmoothMipmaps));
        assert_eq!(sampler_filter(Some(Mag::Linear), Some(Min::Linear)), Some(TexFilter::Smooth));
        assert_eq!(sampler_filter(None, Some(Min::NearestMipmapNearest)), Some(TexFilter::Pixelated));
        assert_eq!(sampler_filter(None, Some(Min::Linear)), Some(TexFilter::Smooth));
        assert_eq!(sampler_filter(None, None), None);
    }

    /// **Each texture keeps the filter its file states**, through the
    /// engine's own writer and back, so a mirrored or rigged bake of a model
    /// draws the way its source did. One that states none comes back as none.
    #[test]
    fn each_texture_keeps_the_filter_its_file_states() {
        use crate::glb_write::{write_glb, WriteMesh, WriteNode};
        let tex = TextureData { pixels: vec![255; 16], width: 2, height: 2 };
        let tri = |t: usize| WriteMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: Some(vec![[0.0, 0.0]; 3]),
            colors: None,
            joints: None,
            weights: None,
            indices: vec![0, 1, 2],
            base_color: [1.0; 4],
            texture: Some(t),
        };
        let filters = [Some(TexFilter::Pixelated), Some(TexFilter::SmoothMipmaps), None, Some(TexFilter::Smooth)];
        let nodes: Vec<WriteNode> = (0..filters.len()).map(|i| WriteNode::mesh_node(format!("n{i}"), tri(i))).collect();
        let bytes = write_glb(&nodes, &[], &vec![tex; filters.len()], &filters);
        let dir = std::env::temp_dir().join(format!("floptle-filters-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("filters.glb");
        std::fs::write(&path, bytes).unwrap();
        let model = import(&path).unwrap();
        let rigged = crate::import_rigged(&path).unwrap().expect("four mesh objects keep their tree");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(model.texture_filters, filters);
        assert_eq!(rigged.texture_filters, filters);
    }

    /// **A file from the engine's old writer says nothing about its textures.**
    /// That writer gave every texture one smooth, mipmapped sampler whatever
    /// the source was, so believing it would turn every model converted from
    /// FBX smooth. The same sampler from any other tool is believed.
    #[test]
    fn the_old_writers_one_sampler_is_not_believed() {
        let doc = |generator: &str| {
            let json = format!(
                "{{\"asset\":{{\"version\":\"2.0\",\"generator\":\"{generator}\"}},\"images\":[{{\"uri\":\"a.png\"}}],\
                 \"samplers\":[{{\"magFilter\":9729,\"minFilter\":9987}}],\"textures\":[{{\"source\":0,\"sampler\":0}}]}}"
            );
            gltf::Gltf::from_slice(json.as_bytes()).unwrap().document
        };
        assert_eq!(image_filters(&doc("Floptle glb_write"), 1), [None]);
        assert_eq!(image_filters(&doc("Khronos glTF Blender I/O"), 1), [Some(TexFilter::SmoothMipmaps)]);
        assert_eq!(image_filters(&doc(GENERATOR), 1), [Some(TexFilter::SmoothMipmaps)]);
    }

    /// **A Draco-compressed model is refused by name, not imported as nothing.**
    /// Its vertices live only in the compressed stream, so reading it the
    /// ordinary way found no geometry and the scene had a hole with no reason
    /// given. A file that only uses the extension has an uncompressed fallback
    /// and still loads.
    #[test]
    fn a_model_that_requires_mesh_compression_is_refused_by_name() {
        let dir = std::env::temp_dir().join(format!("floptle-draco-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let doc = |required: &str| {
            format!(
                "{{\"asset\":{{\"version\":\"2.0\"}},\"extensionsUsed\":[\"KHR_draco_mesh_compression\"],\
                 \"extensionsRequired\":[{required}]}}"
            )
        };
        let draco = dir.join("draco.gltf");
        std::fs::write(&draco, doc("\"KHR_draco_mesh_compression\"")).unwrap();
        let used = dir.join("used.gltf");
        std::fs::write(&used, doc("")).unwrap();

        for err in [check_readable(&draco).unwrap_err(), import(&draco).err().unwrap()] {
            let msg = err.to_string();
            assert!(msg.contains("KHR_draco_mesh_compression") && msg.contains("re-export"), "{msg}");
        }
        check_readable(&used).expect("a file that only uses the extension is readable");
        let _ = std::fs::remove_dir_all(&dir);
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

    /// The import costs, printed rather than asserted —
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
