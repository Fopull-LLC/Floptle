//! Data-driven scene + project model (RON) over the ECS (ADR-0005).
//!
//! A scene is a list of nodes (an entity = a `Transform` + a name + some `Matter`)
//! plus a render config, serialized to human-editable RON. `glam`/`Transform` have
//! no `serde` support and mix `f64`/`f32`, so the on-disk DTOs here use plain array
//! primitives and convert at the `World` boundary. `spawn_into` loads a doc into a
//! `World`; `to_doc` snapshots a `World` back out — the round-trip the editor's
//! Save/Open is built on.

use std::path::Path;

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{Light, Material, Matter, Name, ScriptInst, Scripts, Shape, World};
use serde::{Deserialize, Serialize};

/// A whole scene: a name, its lighting (the mandatory Lighting node), and the
/// nodes in it. Project-wide render settings live separately in [`ProjectConfigDoc`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SceneDoc {
    pub name: String,
    #[serde(default)]
    pub lighting: LightDoc,
    pub nodes: Vec<NodeDoc>,
}

/// One node = one entity's authored data.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NodeDoc {
    pub name: String,
    pub transform: TransformDoc,
    pub matter: MatterDoc,
    #[serde(default)]
    pub scripts: Vec<ScriptDoc>,
    /// The node's material (surface look). `None` = the engine's default look.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material: Option<MaterialDoc>,
    /// Index (into this scene's `nodes`) of this node's parent — its transform is
    /// local to it. `None` = a root node. The transform is local either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<usize>,
}

/// A serializable attached script, mirroring [`floptle_core::ScriptInst`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ScriptDoc {
    pub kind: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub params: Vec<(String, f32)>,
}

fn yes() -> bool {
    true
}

impl ScriptDoc {
    fn to_inst(&self) -> ScriptInst {
        ScriptInst { kind: self.kind.clone(), enabled: self.enabled, params: self.params.clone() }
    }
    fn from_inst(s: &ScriptInst) -> Self {
        Self { kind: s.kind.clone(), enabled: s.enabled, params: s.params.clone() }
    }
}

/// Serializable transform (translation `f64`, rotation `xyzw`, scale `f32`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct TransformDoc {
    pub translation: [f64; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

impl Default for TransformDoc {
    fn default() -> Self {
        Self { translation: [0.0; 3], rotation: [0.0, 0.0, 0.0, 1.0], scale: [1.0; 3] }
    }
}

impl From<&Transform> for TransformDoc {
    fn from(t: &Transform) -> Self {
        Self {
            translation: t.translation.to_array(),
            rotation: t.rotation.to_array(),
            scale: t.scale.to_array(),
        }
    }
}

impl TransformDoc {
    pub fn to_transform(self) -> Transform {
        Transform {
            translation: DVec3::from_array(self.translation),
            rotation: Quat::from_array(self.rotation),
            scale: Vec3::from_array(self.scale),
        }
    }
}

/// Serializable matter kind, mirroring [`floptle_core::Matter`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum MatterDoc {
    Primitive { shape: ShapeDoc, color: [f32; 3] },
    Blob { scale: f32 },
    Mesh { asset_path: String },
    Empty,
    Terrain {
        /// Stable per-terrain id (legacy single-terrain scenes default to 0).
        #[serde(default)]
        id: u32,
    },
    /// A camera viewpoint. `fov_y` is the vertical field of view (radians); `active`
    /// marks the camera that holds play-mode authority on load.
    Camera {
        #[serde(default = "default_fov")]
        fov_y: f32,
        #[serde(default)]
        active: bool,
    },
}

fn default_fov() -> f32 {
    60f32.to_radians()
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum ShapeDoc {
    Cube,
    Sphere,
}

impl From<&Matter> for MatterDoc {
    fn from(m: &Matter) -> Self {
        match m {
            Matter::Primitive { shape, color } => {
                MatterDoc::Primitive { shape: (*shape).into(), color: *color }
            }
            Matter::Blob { scale } => MatterDoc::Blob { scale: *scale },
            Matter::Mesh { asset_path } => MatterDoc::Mesh { asset_path: asset_path.clone() },
            Matter::Empty => MatterDoc::Empty,
            Matter::Terrain { id } => MatterDoc::Terrain { id: *id },
            Matter::Camera { fov_y, active } => MatterDoc::Camera { fov_y: *fov_y, active: *active },
        }
    }
}

impl MatterDoc {
    pub fn to_matter(&self) -> Matter {
        match self {
            MatterDoc::Primitive { shape, color } => {
                Matter::Primitive { shape: (*shape).into(), color: *color }
            }
            MatterDoc::Blob { scale } => Matter::Blob { scale: *scale },
            MatterDoc::Mesh { asset_path } => Matter::Mesh { asset_path: asset_path.clone() },
            MatterDoc::Empty => Matter::Empty,
            MatterDoc::Terrain { id } => Matter::Terrain { id: *id },
            MatterDoc::Camera { fov_y, active } => Matter::Camera { fov_y: *fov_y, active: *active },
        }
    }
}

impl From<Shape> for ShapeDoc {
    fn from(s: Shape) -> Self {
        match s {
            Shape::Cube => ShapeDoc::Cube,
            Shape::Sphere => ShapeDoc::Sphere,
        }
    }
}
impl From<ShapeDoc> for Shape {
    fn from(s: ShapeDoc) -> Self {
        match s {
            ShapeDoc::Cube => Shape::Cube,
            ShapeDoc::Sphere => Shape::Sphere,
        }
    }
}

/// Serializable lighting for the scene's mandatory Lighting node, mirroring
/// [`floptle_core::Light`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct LightDoc {
    pub direction: [f32; 3],
    pub color: [f32; 3],
    pub ambient: [f32; 3],
}

impl Default for LightDoc {
    fn default() -> Self {
        Self::from(&Light::default())
    }
}

impl From<&Light> for LightDoc {
    fn from(l: &Light) -> Self {
        Self { direction: l.direction, color: l.color, ambient: l.ambient }
    }
}

impl LightDoc {
    pub fn to_light(self) -> Light {
        Light { direction: self.direction, color: self.color, ambient: self.ambient }
    }
}

/// Project-wide render settings — the PS1/PS2-style knobs that apply to every
/// scene. Saved to `project.ron`, edited in the editor's Project Settings.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct ProjectConfigDoc {
    pub retro: bool,
    pub retro_height: u32,
    pub matter: bool,
}

impl Default for ProjectConfigDoc {
    fn default() -> Self {
        Self::ps1()
    }
}

impl ProjectConfigDoc {
    /// The default PS1 look: 240p retro upscale, matter on.
    pub fn ps1() -> Self {
        Self { retro: true, retro_height: 240, matter: true }
    }

    /// A higher-resolution PS2-ish look.
    pub fn ps2() -> Self {
        Self { retro_height: 480, ..Self::ps1() }
    }
}

/// What can go wrong loading/saving a scene.
#[derive(Debug)]
pub enum SceneError {
    Io(std::io::Error),
    Ron(ron::error::SpannedError),
    Serialize(ron::Error),
}

impl std::fmt::Display for SceneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SceneError::Io(e) => write!(f, "scene io error: {e}"),
            SceneError::Ron(e) => write!(f, "scene parse error: {e}"),
            SceneError::Serialize(e) => write!(f, "scene write error: {e}"),
        }
    }
}
impl std::error::Error for SceneError {}

/// Parse a scene from a RON file.
pub fn load(path: &Path) -> Result<SceneDoc, SceneError> {
    let text = std::fs::read_to_string(path).map_err(SceneError::Io)?;
    from_ron(&text)
}

/// Parse a scene from RON text.
pub fn from_ron(text: &str) -> Result<SceneDoc, SceneError> {
    ron::from_str(&migrate_ron(text)).map_err(SceneError::Ron)
}

/// Rewrite legacy serialized forms so old scenes still load. Currently: the
/// `Terrain` matter became a struct variant `Terrain(id: u32)`, so the old unit
/// form (`matter: Terrain`, any whitespace) needs an explicit id. A bare `matter:
/// Terrain` not already followed by `(` is rewritten to `Terrain(id: 0)`.
fn migrate_ron(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut rest = text;
    while let Some(i) = rest.find("matter:") {
        out.push_str(&rest[..i + "matter:".len()]);
        rest = &rest[i + "matter:".len()..];
        let ws_end = rest.find(|c: char| !c.is_whitespace()).unwrap_or(rest.len());
        out.push_str(&rest[..ws_end]); // preserve the whitespace as-is
        rest = &rest[ws_end..];
        if let Some(after) = rest.strip_prefix("Terrain") {
            if !after.starts_with('(') {
                out.push_str("Terrain(id: 0)");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod migrate_tests {
    use super::*;
    #[test]
    fn legacy_terrain_forms_migrate() {
        for legacy in [
            r#"(name:"s",nodes:[(name:"T",transform:(translation:(0.0,0.0,0.0),rotation:(0.0,0.0,0.0,1.0),scale:(1.0,1.0,1.0)),matter:Terrain)])"#,
            "(name:\"s\",nodes:[(name:\"T\",transform:(translation:(0.0,0.0,0.0),rotation:(0.0,0.0,0.0,1.0),scale:(1.0,1.0,1.0)),matter: Terrain,)])",
        ] {
            let doc = from_ron(legacy).expect("legacy scene parses");
            assert!(matches!(doc.nodes[0].matter, MatterDoc::Terrain { id: 0 }));
        }
        // a new-form scene with an id is untouched.
        let newform = r#"(name:"s",nodes:[(name:"T",transform:(translation:(0.0,0.0,0.0),rotation:(0.0,0.0,0.0,1.0),scale:(1.0,1.0,1.0)),matter:Terrain(id:5))])"#;
        let doc = from_ron(newform).expect("new scene parses");
        assert!(matches!(doc.nodes[0].matter, MatterDoc::Terrain { id: 5 }));
    }
}

/// Serialize a scene to a pretty RON file.
pub fn save(doc: &SceneDoc, path: &Path) -> Result<(), SceneError> {
    let text = to_ron(doc)?;
    std::fs::write(path, text).map_err(SceneError::Io)
}

/// Serialize a scene to pretty RON text.
pub fn to_ron(doc: &SceneDoc) -> Result<String, SceneError> {
    ron::ser::to_string_pretty(doc, ron::ser::PrettyConfig::default()).map_err(SceneError::Serialize)
}

/// A material — the artist-facing surface look, mirroring [`floptle_core::Material`]
/// (color, emissive, specular, rim, unlit, ambient). Used both as a named preset
/// (one-per-file under `assets/materials/`) and as a node's own material. Every
/// field past `color` has a serde default, so old color-only files still load.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MaterialDoc {
    pub color: [f32; 3],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub texture: Option<String>,
    #[serde(default)]
    pub emissive: [f32; 3],
    #[serde(default)]
    pub emissive_strength: f32,
    #[serde(default = "white3")]
    pub specular: [f32; 3],
    #[serde(default = "default_shininess")]
    pub shininess: f32,
    #[serde(default)]
    pub specular_strength: f32,
    #[serde(default)]
    pub rim: [f32; 3],
    #[serde(default)]
    pub rim_strength: f32,
    #[serde(default)]
    pub unlit: bool,
    #[serde(default = "one_f32")]
    pub ambient: f32,
}

fn white3() -> [f32; 3] {
    [1.0, 1.0, 1.0]
}
fn one_f32() -> f32 {
    1.0
}
fn default_shininess() -> f32 {
    16.0
}

impl Default for MaterialDoc {
    fn default() -> Self {
        Self::from_material(&Material::default())
    }
}

impl MaterialDoc {
    pub fn to_material(&self) -> Material {
        Material {
            texture: self.texture.clone(),
            color: self.color,
            emissive: self.emissive,
            emissive_strength: self.emissive_strength,
            specular: self.specular,
            shininess: self.shininess,
            specular_strength: self.specular_strength,
            rim: self.rim,
            rim_strength: self.rim_strength,
            unlit: self.unlit,
            ambient: self.ambient,
        }
    }
    pub fn from_material(m: &Material) -> Self {
        Self {
            texture: m.texture.clone(),
            color: m.color,
            emissive: m.emissive,
            emissive_strength: m.emissive_strength,
            specular: m.specular,
            shininess: m.shininess,
            specular_strength: m.specular_strength,
            rim: m.rim,
            rim_strength: m.rim_strength,
            unlit: m.unlit,
            ambient: m.ambient,
        }
    }
}

/// Scan `dir` for `*.ron` materials, returning (name, material) sorted by name.
pub fn load_materials(dir: &Path) -> Vec<(String, MaterialDoc)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("ron") {
            continue;
        }
        let Some(name) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
        if let Ok(mat) = std::fs::read_to_string(&p).ok().map(|t| ron::from_str(&t)).transpose() {
            if let Some(mat) = mat {
                out.push((name, mat));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Write a material to `dir/<name>.ron`.
pub fn save_material(name: &str, mat: &MaterialDoc, dir: &Path) -> Result<(), SceneError> {
    let _ = std::fs::create_dir_all(dir);
    let text = ron::ser::to_string_pretty(mat, ron::ser::PrettyConfig::default())
        .map_err(SceneError::Serialize)?;
    std::fs::write(dir.join(format!("{name}.ron")), text).map_err(SceneError::Io)
}

/// Load the project-wide render config, or the default if the file is missing.
pub fn load_project(path: &Path) -> ProjectConfigDoc {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| ron::from_str(&t).ok())
        .unwrap_or_default()
}

/// Save the project-wide render config to a pretty RON file.
pub fn save_project(cfg: &ProjectConfigDoc, path: &Path) -> Result<(), SceneError> {
    let text = ron::ser::to_string_pretty(cfg, ron::ser::PrettyConfig::default())
        .map_err(SceneError::Serialize)?;
    std::fs::write(path, text).map_err(SceneError::Io)
}

/// Spawn every node into `world` as an entity with `Transform` + `Name` + `Matter`,
/// then spawn the one mandatory Lighting node (`Name` + [`Light`]).
pub fn spawn_into(doc: &SceneDoc, world: &mut World) {
    // First pass: spawn each node (keeping the index→entity map for parent links).
    let mut ents = Vec::with_capacity(doc.nodes.len());
    for node in &doc.nodes {
        let e = world.spawn();
        world.insert(e, node.transform.to_transform());
        world.insert(e, Name(node.name.clone()));
        world.insert(e, node.matter.to_matter());
        if !node.scripts.is_empty() {
            world.insert(e, Scripts(node.scripts.iter().map(ScriptDoc::to_inst).collect()));
        }
        if let Some(m) = &node.material {
            world.insert(e, m.to_material());
        }
        ents.push(e);
    }
    // Second pass: link parents (skip out-of-range / self references).
    for (i, node) in doc.nodes.iter().enumerate() {
        if let Some(p) = node.parent {
            if p < ents.len() && p != i {
                world.insert(ents[i], floptle_core::Parent(ents[p]));
            }
        }
    }
    let light = world.spawn();
    world.insert(light, Name("Lighting".into()));
    world.insert(light, doc.lighting.to_light());
}

/// Snapshot every `Matter` entity (and the `Light` node) in `world` into a `SceneDoc`.
pub fn to_doc(name: impl Into<String>, world: &World) -> SceneDoc {
    let entities: Vec<_> = world.query::<Matter>().map(|(e, _)| e).collect();
    // Entity → node index, so parent links serialize as indices into `nodes`.
    let index: std::collections::HashMap<_, usize> =
        entities.iter().enumerate().map(|(i, e)| (*e, i)).collect();
    let mut nodes = Vec::with_capacity(entities.len());
    for &e in &entities {
        let Some(matter) = world.get::<Matter>(e) else { continue };
        let transform =
            world.get::<Transform>(e).map(TransformDoc::from).unwrap_or_default();
        let name = world.get::<Name>(e).map(|n| n.0.clone()).unwrap_or_default();
        let scripts = world
            .get::<Scripts>(e)
            .map(|s| s.0.iter().map(ScriptDoc::from_inst).collect())
            .unwrap_or_default();
        let material = world.get::<Material>(e).map(MaterialDoc::from_material);
        let parent = world.get::<floptle_core::Parent>(e).and_then(|p| index.get(&p.0).copied());
        nodes.push(NodeDoc { name, transform, matter: MatterDoc::from(matter), scripts, material, parent });
    }
    let lighting =
        world.query::<Light>().next().map(|(_, l)| LightDoc::from(l)).unwrap_or_default();
    SceneDoc { name: name.into(), lighting, nodes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> SceneDoc {
        SceneDoc {
            name: "demo".into(),
            lighting: LightDoc::default(),
            nodes: vec![
                NodeDoc {
                    name: "cube".into(),
                    transform: TransformDoc { translation: [1.0, 2.0, 3.0], ..Default::default() },
                    matter: MatterDoc::Primitive { shape: ShapeDoc::Cube, color: [0.9, 0.4, 0.3] },
                    scripts: vec![ScriptDoc {
                        kind: "pulsate".into(),
                        enabled: true,
                        params: vec![("speed".into(), 2.0)],
                    }],
                    material: Some(MaterialDoc {
                        color: [0.8, 0.3, 0.2],
                        emissive: [0.4, 0.0, 0.6],
                        emissive_strength: 1.2,
                        unlit: false,
                        ..Default::default()
                    }),
                    parent: None,
                },
                NodeDoc {
                    name: "blob".into(),
                    transform: TransformDoc::default(),
                    matter: MatterDoc::Blob { scale: 1.3 },
                    scripts: Vec::new(),
                    material: None,
                    parent: Some(0), // child of the cube — exercises parent round-trip
                },
            ],
        }
    }

    #[test]
    fn ron_round_trips() {
        let doc = demo();
        let text = to_ron(&doc).unwrap();
        let back = from_ron(&text).unwrap();
        assert_eq!(doc, back);
    }

    #[test]
    fn world_round_trips() {
        let doc = demo();
        let mut world = World::new();
        spawn_into(&doc, &mut world);
        // 2 matter nodes + the mandatory Lighting node
        assert_eq!(world.len(), 3);
        let snap = to_doc("demo", &world);
        assert_eq!(snap.nodes.len(), 2);
        assert_eq!(snap.lighting, LightDoc::default());
        // the cube's authored translation survives the World round-trip
        let cube = snap.nodes.iter().find(|n| n.name == "cube").unwrap();
        assert_eq!(cube.transform.translation, [1.0, 2.0, 3.0]);
        assert!(matches!(cube.matter, MatterDoc::Primitive { shape: ShapeDoc::Cube, .. }));
    }
}


