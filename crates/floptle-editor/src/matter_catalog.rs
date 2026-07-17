//! The editor's catalog of node "types" (`Matter` kinds): default constructors
//! for the spawn menus, human labels + Inspector glyphs, and the Add Component
//! type-switch list.

use floptle_core::{Matter, Shape};
use floptle_render::MeshData;
use floptle_scene::{MatterDoc, ShapeDoc};

/// The CPU geometry behind each built-in primitive — the ONE definition.
///
/// The renderer registers these at startup (`Editor::init`, mapping `Shape as usize`
/// → `MeshId`) and the vertex-paint brush caches them for raycasting. Both MUST get
/// byte-identical geometry: paint is indexed by `vertex_index`, so if these two ever
/// disagreed on vertex count or order, the brush would paint the wrong vertices.
/// Hence one function, called twice — never two copies of the parameters.
pub(crate) fn primitive_mesh(shape: Shape) -> MeshData {
    match shape {
        Shape::Cube => floptle_render::cube(0.7),
        Shape::Sphere => floptle_render::uv_sphere(0.85, 24, 36),
        Shape::Capsule => floptle_render::capsule(0.5, 0.5, 16, 24),
        Shape::Plane => floptle_render::plane(0.7),
    }
}

pub(crate) fn new_cube() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Cube, color: [0.8, 0.5, 0.4] }
}
pub(crate) fn new_plane() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Plane, color: [0.8, 0.8, 0.8] }
}

/// The default node name for a matter kind.
pub(crate) fn matter_doc_name(m: &MatterDoc) -> &'static str {
    match m {
        MatterDoc::Primitive { shape: ShapeDoc::Cube, .. } => "Cube",
        MatterDoc::Primitive { shape: ShapeDoc::Sphere, .. } => "Sphere",
        MatterDoc::Primitive { shape: ShapeDoc::Capsule, .. } => "Capsule",
        MatterDoc::Primitive { shape: ShapeDoc::Plane, .. } => "Plane",
        MatterDoc::Blob { .. } => "Blob",
        MatterDoc::Mesh { .. } => "Mesh",
        MatterDoc::Empty => "Empty",
        MatterDoc::Terrain { .. } => "Terrain",
        MatterDoc::Camera { .. } => "Camera",
        MatterDoc::PointLight { .. } => "Point Light",
        MatterDoc::GravityVolume { .. } => "Gravity Volume",
        MatterDoc::FieldShape { .. } => "Field Shape",
        MatterDoc::Skybox { .. } => "Skybox",
        MatterDoc::PostProcess { .. } => "Post Processing",
    }
}
pub(crate) fn new_sphere() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Sphere, color: [0.4, 0.6, 0.9] }
}
pub(crate) fn new_capsule() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Capsule, color: [0.5, 0.85, 0.6] }
}

/// A short human label for a node's runtime `Matter` "type".
pub(crate) fn matter_kind_label(m: &Matter) -> &'static str {
    match m {
        Matter::Primitive { shape: Shape::Cube, .. } => "Cube",
        Matter::Primitive { shape: Shape::Sphere, .. } => "Sphere",
        Matter::Primitive { shape: Shape::Capsule, .. } => "Capsule",
        Matter::Primitive { shape: Shape::Plane, .. } => "Plane",
        Matter::Blob { .. } => "Blob",
        Matter::Mesh { .. } => "Mesh",
        Matter::Empty => "Empty",
        Matter::Terrain { .. } => "Terrain",
        Matter::Camera { .. } => "Camera",
        Matter::PointLight { .. } => "Point Light",
        Matter::GravityVolume { .. } => "Gravity Volume",
        Matter::FieldShape { .. } => "Field Shape",
        Matter::Skybox { .. } => "Skybox",
        Matter::PostProcess { .. } => "Post Processing",
    }
}

/// The little glyph shown beside a node's type in the Inspector header.
pub(crate) fn matter_icon(m: &Matter) -> &'static str {
    match m {
        Matter::Primitive { shape: Shape::Cube, .. } => "■",
        Matter::Primitive { shape: Shape::Sphere, .. } => "○",
        Matter::Primitive { shape: Shape::Capsule, .. } => "▪",
        Matter::Primitive { shape: Shape::Plane, .. } => "▭",
        Matter::Blob { .. } => "◑",
        Matter::Mesh { .. } => "✳",
        Matter::Empty => "🗀",
        Matter::Terrain { .. } => "Δ",
        Matter::Camera { .. } => "⌖",
        Matter::PointLight { .. } => "●",
        Matter::GravityVolume { .. } => "⬇",
        Matter::FieldShape { .. } => "◈",
        Matter::Skybox { .. } => "◎",
        Matter::PostProcess { .. } => "✨",
    }
}

/// The set of node "types" the Inspector's Add Component menu can switch a node to
/// (icon-labeled). Mutually exclusive — picking one replaces the node's current
/// `Matter`. Terrain (a special SDF field) and Mesh (needs an asset) are omitted,
/// as is PostProcess (the mandatory per-scene node — every scene already has one).
pub(crate) fn type_catalog() -> Vec<(&'static str, Matter)> {
    use floptle_core::GravityMode;
    vec![
        ("■  Cube", Matter::Primitive { shape: Shape::Cube, color: [0.8, 0.5, 0.4] }),
        ("○  Sphere", Matter::Primitive { shape: Shape::Sphere, color: [0.4, 0.6, 0.9] }),
        ("▪  Capsule", Matter::Primitive { shape: Shape::Capsule, color: [0.5, 0.85, 0.6] }),
        ("▭  Plane", Matter::Primitive { shape: Shape::Plane, color: [0.8, 0.8, 0.8] }),
        ("◑  Blob", Matter::Blob { scale: 1.0 }),
        ("🗀  Empty", Matter::Empty),
        ("⌖  Camera", Matter::Camera { fov_y: 60f32.to_radians(), active: false }),
        ("●  Point Light", Matter::PointLight { color: [1.0, 0.95, 0.85], intensity: 1.0, range: 10.0 }),
        ("⬇  Gravity Volume", Matter::GravityVolume { mode: GravityMode::Down, strength: 9.81, radius: 20.0 }),
        ("◈  Field Shape", Matter::FieldShape { radius: 1.5 }),
        ("◎  Skybox", Matter::default_skybox()),
    ]
}
