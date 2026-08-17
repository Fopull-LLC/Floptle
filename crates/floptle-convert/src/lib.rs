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
//!
//! [`convert`] settles all four. The axis and unit conversion is ufbx's, asked
//! for explicitly rather than left at its default, and the winding fix rides
//! with it.
//!
//! # What is not attempted
//!
//! **Animation, cameras and lights are dropped**, and the report says so rather
//! than letting somebody discover it later. Geometry, materials and textures
//! are what "I want to use this model" means; a half-converted animation that
//! plays wrongly is worse than one that is honestly absent.

use std::path::{Path, PathBuf};

mod common;
mod gltf_pack;
mod ply;
mod stl;
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
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(e) => write!(
                f,
                "Floptle does not read `.{e}` models. It converts .fbx, .obj, .stl, .ply and \
                 loose .gltf files into .glb."
            ),
            Self::Io(m) => write!(f, "That file could not be read: {m}"),
            Self::Malformed(m) => write!(f, "That file could not be understood: {m}"),
            Self::NoGeometry => write!(
                f,
                "There is no geometry in that file — no meshes, only things like bones, \
                 cameras or lights. There is nothing to convert."
            ),
        }
    }
}

impl std::error::Error for ConvertError {}

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
    /// Things deliberately not carried across, in words. Shown, never hidden.
    pub dropped: Vec<String>,
    /// Trouble that did not stop the conversion — a texture that could not be
    /// found, a mesh with no normals. Worth saying; not worth failing over.
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
        if self.textures > 0 {
            s.push_str(&format!(", {} texture{} embedded", self.textures, plural(self.textures)));
        }
        if !self.dropped.is_empty() {
            s.push_str(&format!(" — dropped: {}", self.dropped.join(", ")));
        }
        s
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Every extension [`convert`] accepts, lowercase and without the dot.
pub const SUPPORTED: &[&str] = &["fbx", "obj", "stl", "ply", "gltf"];

/// Can this path be converted? Used by the editor to decide whether to offer.
///
/// `.glb` is deliberately **not** here. It is already the output format, and
/// offering to convert one to itself is an action whose best case is doing
/// nothing.
pub fn is_convertible(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => SUPPORTED.contains(&e.to_ascii_lowercase().as_str()),
        None => false,
    }
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

/// Read a model and return the bytes of an equivalent `.glb`.
///
/// The pure half — no writing, no editor — so the whole pipeline is testable
/// against a fixture file with nothing mocked.
pub fn convert(src: &Path) -> Result<(Vec<u8>, Report), ConvertError> {
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let scene = match ext.as_str() {
        // ufbx reads both, and sharing the path is deliberate: OBJ and FBX
        // otherwise end up with two implementations that disagree about winding
        // and about which way is up.
        "fbx" | "obj" => ufbx_read::read(src)?,
        "stl" => stl::read(src)?,
        "ply" => ply::read(src)?,
        "gltf" => gltf_pack::read(src)?,
        "glb" => {
            return Err(ConvertError::Unsupported(
                "glb — that is already the format this converts TO".into(),
            ));
        }
        other => return Err(ConvertError::Unsupported(other.to_string())),
    };

    scene.into_glb()
}

/// Convert `src` and write the `.glb` beside it. Returns where it went.
///
/// **Refuses to overwrite.** A conversion that silently replaces a file is one
/// that can destroy a model somebody spent an hour fixing up, and the caller is
/// better placed to ask than this is.
pub fn convert_file(src: &Path) -> Result<(PathBuf, Report), ConvertError> {
    let out = output_path(src);
    if out.exists() {
        return Err(ConvertError::Io(format!(
            "{} already exists. Rename or remove it first.",
            out.file_name().unwrap_or_default().to_string_lossy()
        )));
    }
    let (bytes, report) = convert(src)?;
    std::fs::write(&out, bytes).map_err(|e| ConvertError::Io(e.to_string()))?;
    Ok((out, report))
}
