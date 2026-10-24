//! **Turn a model somebody downloaded into one this engine can open.**
//!
//! The engine imports glTF 2.0 and nothing else, which is the right call for a
//! runtime format — it is the one model format designed to be *loaded* rather
//! than authored. It is the wrong thing to ask of a person who has just bought
//! an asset pack, because asset packs are FBX and OBJ, scans are PLY, and
//! anything from CAD is STL.
//!
//! So this converts, and the output is always a **single self-contained
//! `.glb`**: geometry, materials and textures in one file, with no sidecar
//! `.bin`, no `textures/` folder, and no absolute paths from somebody else's
//! machine baked into it. A model that is one file cannot arrive half-copied.
//!
//! | Source | Read by | What comes across |
//! |---|---|---|
//! | `.fbx` | ufbx | meshes, every material map, skins, animation |
//! | `.obj` + `.mtl` | ufbx | meshes, every `.mtl` map |
//! | `.gltf` (loose) | packed verbatim | everything |
//! | `.stl`, `.ply` | in-crate | geometry and vertex colour |
//! | `.blend`, `.dae`, `.usd*` | the installed Blender | what Blender's glTF exporter writes |
//!
//! # What comes out is normalised, not just re-encoded
//!
//! Every format here disagrees with glTF about something, and a converter that
//! only re-encodes hands you a model that loads and is wrong:
//!
//! * **Axes.** FBX is Z-up out of 3ds Max and Y-up out of Maya; glTF is always
//!   right-handed Y-up. A model that arrives on its face is the single most
//!   common complaint about importing FBX anywhere.
//! * **Units.** FBX records its own unit and most exporters write centimetres.
//!   glTF is metres. A hundredfold scale error reads as "the importer is
//!   broken" rather than as a unit.
//! * **Winding.** Flipping an axis to correct handedness inverts triangle
//!   winding, and a model whose faces are all backwards is invisible from
//!   outside and solid from inside.
//! * **N-gons.** FBX and OBJ have quads and worse; glTF has triangles only.
//! * **Materials.** Separate roughness and metallic pictures, glossiness,
//!   height maps in the bump slot and opacity maps all become the slots glTF
//!   has, as the artist assigned them (see `ufbx_read::material`).
//!
//! # What is not attempted
//!
//! **Cameras, lights and blend shapes are dropped**, and the report says so
//! rather than letting somebody discover it later.
//!
//! # Which files it may write
//!
//! Every converted model is stamped with where it came from (see [`stamp`]),
//! and [`convert_file`] only ever writes over a stamped one. A `.glb` somebody
//! made themselves is never replaced, whatever sits beside it.

use std::path::{Path, PathBuf};

mod blender;
mod common;
mod gltf_pack;
mod ply;
pub mod stamp;
mod stl;
mod textures;
mod ufbx_read;

pub use common::{Scene, SubMesh};

/// Everything that can go wrong, in words a person can act on.
///
/// Deliberately not a wrapper around each library's error type: "unexpected end
/// of buffer at 0x2f1" is true and useless. What a person needs to know is
/// whether the file is broken, the wrong kind of thing, or fine but empty.
#[derive(Debug)]
pub enum ConvertError {
    /// The extension is not one we read.
    Unsupported(String),
    /// The file could not be read off disk at all.
    Io(String),
    /// It parsed, but it is not the format it claims to be.
    Malformed(String),
    /// It parsed and there is no geometry in it. A real case — an FBX of only
    /// bones, or a scene of only lights — and not an error in the file.
    NoGeometry,
    /// The format converts through Blender, and Blender is not installed.
    NeedsBlender(String),
    /// A `.glb` that this converter did not write is where the output would
    /// go. It is somebody's own model and is never replaced.
    WouldOverwrite(PathBuf),
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(e) => write!(
                f,
                "Floptle does not read `.{e}` models. It converts .fbx, .obj, .stl, .ply and \
                 loose .gltf files into .glb, and .blend, .dae and .usd through Blender."
            ),
            Self::Io(m) => write!(f, "That file could not be read: {m}"),
            Self::Malformed(m) => write!(f, "That file could not be understood: {m}"),
            Self::NoGeometry => write!(
                f,
                "There is no geometry in that file — no meshes, only things like bones, \
                 cameras or lights. There is nothing to convert."
            ),
            Self::NeedsBlender(m) => write!(f, "{m}"),
            Self::WouldOverwrite(p) => write!(
                f,
                "{} is already there and was not converted by Floptle, so it is left alone. \
                 Rename or move it to convert this model beside it.",
                p.file_name().unwrap_or_default().to_string_lossy()
            ),
        }
    }
}

impl std::error::Error for ConvertError {}

/// A material slot a picture can be assigned to, in the words the report
/// uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapSlot {
    BaseColor,
    Normal,
    Roughness,
    Metallic,
    Occlusion,
    Emissive,
    Opacity,
}

impl MapSlot {
    pub fn label(self) -> &'static str {
        match self {
            MapSlot::BaseColor => "base colour",
            MapSlot::Normal => "normal",
            MapSlot::Roughness => "roughness",
            MapSlot::Metallic => "metallic",
            MapSlot::Occlusion => "occlusion",
            MapSlot::Emissive => "emissive",
            MapSlot::Opacity => "opacity",
        }
    }
}

/// One picture assigned to one slot of one material.
#[derive(Debug, Clone)]
pub struct MapUse {
    pub material: String,
    pub slot: MapSlot,
    /// The picture's file name, or `embedded` for one inside the model.
    pub texture: String,
    /// How it was turned into what glTF wants, when it was: "from a height
    /// map", "glossiness, inverted".
    pub note: Option<&'static str>,
}

/// A picture a material asked for that could not be found.
#[derive(Debug, Clone)]
pub struct MissingMap {
    pub material: String,
    pub slot: MapSlot,
    pub name: String,
}

/// What a conversion did, so the editor can say it rather than claim success.
///
/// The counts are the point. "Converted" tells somebody nothing; "4 meshes,
/// 12,536 triangles, 2 textures embedded, animation dropped" tells them whether
/// they got what they came for, and is the difference between noticing a
/// half-empty conversion now and noticing it in the level.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub meshes: usize,
    pub triangles: usize,
    pub textures: usize,
    pub materials: usize,
    /// Every picture that was assigned, and where.
    pub maps: Vec<MapUse>,
    /// Every picture that was asked for and not found.
    pub missing: Vec<MissingMap>,
    /// The animation clips carried, by name.
    pub animations: Vec<String>,
    /// Skinned meshes carried, each bending with its skeleton.
    pub skins: usize,
    /// Things not carried across, in words. Shown, never hidden.
    pub dropped: Vec<String>,
    /// Trouble that did not stop the conversion — a texture that could not be
    /// decoded, a mesh with no normals. Worth saying; not worth failing over.
    pub warnings: Vec<String>,
    /// What the source measured itself in, when it said. Reported because a
    /// hundredfold scale error is otherwise a mystery.
    pub source_unit_meters: Option<f32>,
    /// Whether the source had to be turned to stand up in glTF's axes.
    pub reoriented: bool,
}

impl Report {
    /// One line for the Console.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} mesh{}, {} triangle{}",
            self.meshes,
            if self.meshes == 1 { "" } else { "es" },
            self.triangles,
            if self.triangles == 1 { "" } else { "s" },
        );
        if self.materials > 0 {
            s.push_str(&format!(", {} material{}", self.materials, plural(self.materials)));
        }
        if self.textures > 0 {
            s.push_str(&format!(", {} texture{} embedded", self.textures, plural(self.textures)));
        }
        if self.skins > 0 {
            s.push_str(&format!(", {} skinned mesh{}", self.skins, if self.skins == 1 { "" } else { "es" }));
        }
        if !self.animations.is_empty() {
            s.push_str(&format!(", {} animation{}", self.animations.len(), plural(self.animations.len())));
        }
        if !self.missing.is_empty() {
            s.push_str(&format!(", {} texture{} missing", self.missing.len(), plural(self.missing.len())));
        }
        s
    }

    /// The rest of the import report, a line each, in the order somebody
    /// checks them: which pictures went where, which clips came across, and
    /// what was not found. Warnings and drops are [`Self::warnings`] and
    /// [`Self::dropped`], said separately because they are the lines that ask
    /// for something to be done.
    pub fn detail_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut materials: Vec<&str> = Vec::new();
        for m in &self.maps {
            if !materials.contains(&m.material.as_str()) {
                materials.push(&m.material);
            }
        }
        for mat in materials {
            let parts: Vec<String> = self
                .maps
                .iter()
                .filter(|m| m.material == mat)
                .map(|m| match m.note {
                    Some(n) => format!("{} ← {} ({n})", m.slot.label(), m.texture),
                    None => format!("{} ← {}", m.slot.label(), m.texture),
                })
                .collect();
            out.push(format!("{}: {}", if mat.is_empty() { "material" } else { mat }, parts.join(", ")));
        }
        if !self.animations.is_empty() {
            out.push(format!("animations: {}", self.animations.join(", ")));
        }
        out
    }

    /// One line per picture that was asked for and not found, naming the
    /// material that wanted it and the slot it was for.
    pub fn missing_lines(&self) -> Vec<String> {
        self.missing
            .iter()
            .map(|m| {
                format!(
                    "{}: the {} map `{}` was not found beside the model or in a textures folder near it",
                    if m.material.is_empty() { "material" } else { &m.material },
                    m.slot.label(),
                    m.name
                )
            })
            .collect()
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Every extension [`convert`] accepts, lowercase and without the dot.
pub const SUPPORTED: &[&str] = &["fbx", "obj", "stl", "ply", "gltf", "blend", "dae", "usd", "usda", "usdc", "usdz"];

/// The extensions that need Blender installed to convert.
pub use blender::FORMATS as NEEDS_BLENDER;

/// Where Blender is, when it is installed.
pub fn blender() -> Option<PathBuf> {
    blender::find()
}

fn ext_of(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default()
}

/// Can this path be converted? Used by the editor to decide whether to offer.
///
/// `.glb` is **not** here. It is already the output format, and
/// offering to convert one to itself is an action whose best case is doing
/// nothing.
pub fn is_convertible(path: &Path) -> bool {
    SUPPORTED.contains(&ext_of(path).as_str())
}

/// Should a file of this kind be converted as soon as it is in a project?
/// Every convertible format except a loose `.gltf`: the engine already opens
/// those as they are, so packing one is a choice, not a step that has to
/// happen before it can be used.
pub fn converts_automatically(path: &Path) -> bool {
    is_convertible(path) && ext_of(path) != "gltf"
}

/// Where [`convert_file`] would write, given a source path: the same folder,
/// the same stem, `.glb`.
///
/// Beside the source rather than in some models folder, because the person who
/// put the FBX there chose where it goes, and a converter that files the result
/// somewhere else has an opinion it was not asked for.
pub fn output_path(src: &Path) -> PathBuf {
    src.with_extension("glb")
}

/// What is at a source's output path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Nothing yet.
    Missing,
    /// A model this converter wrote from this source. `current` is false when
    /// the source has changed since, or the converter has learned something.
    Converted { current: bool },
    /// A `.glb` the converter wrote before it stamped its output: signed
    /// with the old generator, beside this source. Converted again, and
    /// stamped, so it stops being a file nothing will ever update.
    Unstamped,
    /// A `.glb` somebody made. Never written over.
    TheirOwn,
}

/// What is at `src`'s output path, and whether it is up to date.
///
/// A source newer than its output is hashed and compared with the stamp
/// before being called changed: a fresh clone or a copied folder gives every
/// file a new time, and converting a whole project again because of it would
/// be minutes of work that changes nothing.
pub fn output_state(src: &Path) -> Output {
    let out = output_path(src);
    if !out.is_file() {
        return Output::Missing;
    }
    let Some(stamp) = stamp::read_stamp(&out) else {
        return if is_old_conversion_of(src, &out) { Output::Unstamped } else { Output::TheirOwn };
    };
    if stamp.from != ext_of(src) {
        // Converted from a different source with the same name (`crate.obj`
        // beside `crate.fbx`): that one owns it.
        return Output::TheirOwn;
    }
    if stamp.converter != stamp::CONVERTER {
        return Output::Converted { current: false };
    }
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut newer = matches!((mtime(src), mtime(&out)), (Some(s), Some(o)) if s > o);
    if ext_of(src) == "obj" {
        let mtl = src.with_extension("mtl");
        newer |= matches!((mtime(&mtl), mtime(&out)), (Some(s), Some(o)) if s > o);
    }
    if !newer {
        return Output::Converted { current: true };
    }
    let same = stamp::hash_source(src).is_ok_and(|h| h == stamp.source_hash);
    Output::Converted { current: same }
}

/// Does `src` need converting — no output yet, a stale one of ours, or one
/// from before outputs were stamped?
pub fn needs_conversion(src: &Path) -> bool {
    matches!(output_state(src), Output::Missing | Output::Converted { current: false } | Output::Unstamped)
}

/// Did the old Convert button write `out` from `src`? It signed with
/// [`stamp::OLD_GENERATOR`] and wrote beside the source under its stem, and
/// nothing else did both: the engine's other bakes with that generator are
/// named `.mirrored.glb` and `.rigged.glb`, which no source shares a stem
/// with. When two sources share the stem (`crate.fbx` and `crate.obj`), the
/// one first in [`SUPPORTED`] is taken for the one that was converted.
fn is_old_conversion_of(src: &Path, out: &Path) -> bool {
    if stamp::read_generator(out).as_deref() != Some(stamp::OLD_GENERATOR) || !src.is_file() {
        return false;
    }
    let Some(dir) = out.parent() else { return false };
    let stem = out.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let first = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.file_stem().is_some_and(|s| s.to_string_lossy() == stem))
        .filter(|p| is_convertible(p))
        .min_by_key(|p| SUPPORTED.iter().position(|e| *e == ext_of(p)).unwrap_or(usize::MAX));
    first.as_deref() == Some(src)
}

/// The source a `.glb` was converted from, when it is one of ours and that
/// source is still beside it.
pub fn source_of(glb: &Path) -> Option<PathBuf> {
    let stamp = stamp::read_stamp(glb)?;
    let dir = glb.parent()?;
    let stem = glb.file_stem()?.to_string_lossy().to_string();
    // The extension as the stamp has it, then in any case: `Crate.FBX`.
    let rd = std::fs::read_dir(dir).ok()?;
    rd.flatten().map(|e| e.path()).find(|p| {
        p.is_file()
            && p.file_stem().is_some_and(|s| s.to_string_lossy() == stem)
            && ext_of(p) == stamp.from
    })
}

/// Read a model and return the bytes of an equivalent `.glb`.
///
/// The pure half — no writing, no editor — so the whole pipeline is testable
/// against a fixture file with nothing mocked.
pub fn convert(src: &Path) -> Result<(Vec<u8>, Report), ConvertError> {
    let ext = ext_of(src);
    let scene = match ext.as_str() {
        // ufbx reads both, and sharing the path is deliberate: OBJ and FBX
        // otherwise end up with two implementations that disagree about winding
        // and about which way is up.
        "fbx" | "obj" => ufbx_read::read(src)?,
        "stl" => stl::read(src)?,
        "ply" => ply::read(src)?,
        "gltf" => return gltf_pack::pack(src),
        e if blender::FORMATS.contains(&e) => return blender::convert(src),
        "glb" => {
            return Err(ConvertError::Unsupported(
                "glb — that is already the format this converts TO".into(),
            ));
        }
        other => return Err(ConvertError::Unsupported(other.to_string())),
    };

    scene.into_glb()
}

/// Convert `src` and write the `.glb` beside it, stamped as converted from
/// it. Returns where it went.
///
/// **Writes over only its own output.** A `.glb` there that this converter
/// did not write is somebody's model, and the conversion is refused with
/// [`ConvertError::WouldOverwrite`] rather than replacing it.
pub fn convert_file(src: &Path) -> Result<(PathBuf, Report), ConvertError> {
    let out = output_path(src);
    if output_state(src) == Output::TheirOwn {
        return Err(ConvertError::WouldOverwrite(out));
    }
    let source_hash = stamp::hash_source(src).map_err(|e| ConvertError::Io(e.to_string()))?;
    let (bytes, report) = convert(src)?;
    let bytes = stamp::stamp_glb(
        &bytes,
        &stamp::Stamp { from: ext_of(src), converter: stamp::CONVERTER, source_hash },
    );
    // Written beside and moved into place, so a model open in the editor is
    // never read half-written, and a failed write leaves the old one.
    let tmp = out.with_extension("glb.part");
    std::fs::write(&tmp, bytes).map_err(|e| ConvertError::Io(e.to_string()))?;
    std::fs::rename(&tmp, &out).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ConvertError::Io(e.to_string())
    })?;
    Ok((out, report))
}
