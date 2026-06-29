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
use floptle_core::{Light, Matter, Name, Shape, World};
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
    ron::from_str(text).map_err(SceneError::Ron)
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
    for node in &doc.nodes {
        let e = world.spawn();
        world.insert(e, node.transform.to_transform());
        world.insert(e, Name(node.name.clone()));
        world.insert(e, node.matter.to_matter());
    }
    let light = world.spawn();
    world.insert(light, Name("Lighting".into()));
    world.insert(light, doc.lighting.to_light());
}

/// Snapshot every `Matter` entity (and the `Light` node) in `world` into a `SceneDoc`.
pub fn to_doc(name: impl Into<String>, world: &World) -> SceneDoc {
    let entities: Vec<_> = world.query::<Matter>().map(|(e, _)| e).collect();
    let mut nodes = Vec::with_capacity(entities.len());
    for e in entities {
        let Some(matter) = world.get::<Matter>(e) else { continue };
        let transform =
            world.get::<Transform>(e).map(TransformDoc::from).unwrap_or_default();
        let name = world.get::<Name>(e).map(|n| n.0.clone()).unwrap_or_default();
        nodes.push(NodeDoc { name, transform, matter: MatterDoc::from(matter) });
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
                },
                NodeDoc {
                    name: "blob".into(),
                    transform: TransformDoc::default(),
                    matter: MatterDoc::Blob { scale: 1.3 },
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
