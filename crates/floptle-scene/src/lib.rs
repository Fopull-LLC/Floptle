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
use floptle_core::{
    AoMode, BodyKind, GravityMode, Light, Material, Matter, Name, RigidBody, ScriptInst, Scripts,
    Shape, World,
};
use serde::{Deserialize, Serialize};

pub mod anim;
pub use anim::{
    load_anim_clip, load_anim_controller, save_anim_clip, save_anim_controller, AnimChannelDoc,
    AnimClipDoc, AnimControllerDoc, AnimEventDoc, AnimLayerDoc, AnimStateDoc, AnimTrackDoc3,
    AnimTrackDoc4, AnimTransitionDoc, ANIM_CLIP_EXT, ANIM_CTL_EXT,
};
pub mod vfx;
pub use vfx::{
    load_vfx_effect, save_vfx_effect, VfxBlendDoc, VfxBurstDoc, VfxClipDoc, VfxCurveDoc,
    VfxEffectDoc, VfxEndDoc, VfxExtrapolateDoc, VfxFlipModeDoc, VfxFlipbookDoc, VfxForceDoc,
    VfxInterpDoc, VfxKeyDoc, VfxLaneDoc, VfxLaneTargetDoc, VfxOrientDoc, VfxPlaybackDoc,
    VfxPropDoc, VfxRenderDoc, VfxShapeDoc, VfxSpaceDoc, VfxTrackDoc, VfxValueDoc, VFX_EXT,
};

/// A whole scene: a name, its lighting (the mandatory Lighting node), and the
/// nodes in it. Project-wide render settings live separately in [`ProjectConfigDoc`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SceneDoc {
    pub name: String,
    #[serde(default)]
    pub lighting: LightDoc,
    pub nodes: Vec<NodeDoc>,
}

/// A bone/sub-object attachment of a node to its parent Mesh (see
/// [`floptle_core::BoneAttach`]). The target is the node's serialized `parent`; only
/// the bone name + bone-local offset are stored here.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AttachmentDoc {
    pub bone: String,
    #[serde(default)]
    pub offset: TransformDoc,
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
    /// A physics rigidbody on this node (`None` = not a physics body).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rigidbody: Option<RigidBodyDoc>,
    /// Marks a Mesh node as a static walkable collider (its triangles collide at Play).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mesh_collider: bool,
    /// The "collidable" switch: a static collider auto-shaped from this node's geometry
    /// (no dynamic rigidbody needed). See [`floptle_core::Collidable`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collidable: bool,
    /// Whether the node's geometry is drawn (default true). See [`floptle_core::Visible`].
    /// Only the rare hidden node serializes this.
    #[serde(default = "true_bool", skip_serializing_if = "is_true")]
    pub visible: bool,
    /// Whether the node's collider casts sun shadows as a proxy occluder (default
    /// true). See [`floptle_core::CastShadow`]; only an opted-out node serializes this.
    #[serde(default = "true_bool", skip_serializing_if = "is_true")]
    pub cast_shadow: bool,
    /// Animation controller asset key on this node (`None` = no controller).
    /// See [`floptle_core::AnimController`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anim_controller: Option<String>,
    /// Particle effect on this node (`None` = no particle system).
    /// See [`floptle_core::ParticleSystem`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub particles: Option<ParticleSystemDoc>,
    /// Index (into this scene's `nodes`) of this node's parent — its transform is
    /// local to it. `None` = a root node. The transform is local either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<usize>,
    /// Bone/sub-object of the parent Mesh this node rides (`None` = a plain child).
    /// The node's `transform` is serialized stable (identity) when attached, since
    /// its live transform is a derived pose value re-computed on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentDoc>,
}

/// Serializable particle-system component, mirroring [`floptle_core::ParticleSystem`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ParticleSystemDoc {
    /// Effect asset key: project-relative path without extension (`vfx/360Slash`).
    pub asset: String,
    #[serde(default = "true_bool")]
    pub play_on_start: bool,
}

impl ParticleSystemDoc {
    pub fn to_component(&self) -> floptle_core::ParticleSystem {
        floptle_core::ParticleSystem {
            asset: self.asset.clone(),
            play_on_start: self.play_on_start,
        }
    }

    pub fn from_component(p: &floptle_core::ParticleSystem) -> Self {
        Self { asset: p.asset.clone(), play_on_start: p.play_on_start }
    }
}

/// Serializable physics rigidbody, mirroring [`floptle_core::RigidBody`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct RigidBodyDoc {
    /// true = capsule (legacy field; ignored when `boxed` is set).
    #[serde(default)]
    pub capsule: bool,
    /// true = box (sized by `half_extents`). Takes priority over `capsule`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub boxed: bool,
    #[serde(default = "half_f32")]
    pub radius: f32,
    #[serde(default = "two_f32")]
    pub height: f32,
    #[serde(default = "half3_f32")]
    pub half_extents: [f32; 3],
    #[serde(default)]
    pub restitution: f32,
    #[serde(default = "frict_f32")]
    pub friction: f32,
    #[serde(default = "true_bool")]
    pub gravity: bool,
    #[serde(default)]
    pub lock_pos: [bool; 3],
    #[serde(default)]
    pub lock_rot: [bool; 3],
}

fn true_bool() -> bool {
    true
}
/// `skip_serializing_if` predicate: omit a bool that's at its `true` default.
fn is_true(b: &bool) -> bool {
    *b
}
fn half_f32() -> f32 {
    0.5
}
fn two_f32() -> f32 {
    2.0
}
fn half3_f32() -> [f32; 3] {
    [0.5, 0.5, 0.5]
}
fn frict_f32() -> f32 {
    0.3
}

impl RigidBodyDoc {
    pub fn to_rigidbody(&self) -> RigidBody {
        RigidBody {
            kind: if self.boxed {
                BodyKind::Box
            } else if self.capsule {
                BodyKind::Capsule
            } else {
                BodyKind::Sphere
            },
            radius: self.radius,
            height: self.height,
            half_extents: self.half_extents,
            restitution: self.restitution,
            friction: self.friction,
            gravity: self.gravity,
            lock_pos: self.lock_pos,
            lock_rot: self.lock_rot,
        }
    }
    pub fn from_rigidbody(rb: &RigidBody) -> Self {
        Self {
            capsule: rb.kind == BodyKind::Capsule,
            boxed: rb.kind == BodyKind::Box,
            radius: rb.radius,
            height: rb.height,
            half_extents: rb.half_extents,
            restitution: rb.restitution,
            friction: rb.friction,
            gravity: rb.gravity,
            lock_pos: rb.lock_pos,
            lock_rot: rb.lock_rot,
        }
    }
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
    /// A placeable point/omni light (position = node transform).
    PointLight {
        #[serde(default = "white3")]
        color: [f32; 3],
        #[serde(default = "one_f32")]
        intensity: f32,
        #[serde(default = "default_range")]
        range: f32,
    },
    /// A physics gravity source (Down = level gravity, Radial = planet).
    GravityVolume {
        #[serde(default)]
        radial: bool,
        #[serde(default = "default_gravity_strength")]
        strength: f32,
        #[serde(default = "default_range")]
        radius: f32,
    },
    /// The scene's environment background (solid color or equirect texture + tint).
    Skybox {
        #[serde(default = "sky_grey")]
        color: [f32; 3],
        #[serde(default = "default_sky_size")]
        size: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        texture: Option<String>,
        #[serde(default = "white3")]
        tint: [f32; 3],
    },
    /// The scene's post-processing chain (a mandatory node — self-healed on load).
    PostProcess {
        #[serde(default = "on")]
        enabled: bool,
        #[serde(default)]
        bloom: bool,
        #[serde(default = "default_bloom_threshold")]
        bloom_threshold: f32,
        #[serde(default = "default_bloom_intensity")]
        bloom_intensity: f32,
        #[serde(default)]
        vignette: bool,
        #[serde(default = "default_vignette_strength")]
        vignette_strength: f32,
        #[serde(default = "default_vignette_radius")]
        vignette_radius: f32,
        #[serde(default)]
        ao: AoModeDoc,
        #[serde(default = "default_ao_strength")]
        ao_strength: f32,
        #[serde(default = "default_ao_radius")]
        ao_radius: f32,
        #[serde(default)]
        posterize_bands: u32,
        #[serde(default)]
        posterize_dither: bool,
    },
}

/// Serializable [`AoMode`] (how the PostProcess node computes ambient occlusion).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AoModeDoc {
    Off,
    #[default]
    ScreenSpace,
    Sdf,
}

impl AoModeDoc {
    pub fn to_mode(self) -> AoMode {
        match self {
            AoModeDoc::Off => AoMode::Off,
            AoModeDoc::ScreenSpace => AoMode::ScreenSpace,
            AoModeDoc::Sdf => AoMode::Sdf,
        }
    }
}

impl From<AoMode> for AoModeDoc {
    fn from(m: AoMode) -> Self {
        match m {
            AoMode::Off => AoModeDoc::Off,
            AoMode::ScreenSpace => AoModeDoc::ScreenSpace,
            AoMode::Sdf => AoModeDoc::Sdf,
        }
    }
}

fn on() -> bool {
    true
}
fn default_ao_strength() -> f32 {
    0.7
}
fn default_ao_radius() -> f32 {
    0.5
}

fn sky_grey() -> [f32; 3] {
    [0.5, 0.5, 0.52]
}
fn default_sky_size() -> f32 {
    500.0
}

fn default_gravity_strength() -> f32 {
    9.81
}

fn default_fov() -> f32 {
    60f32.to_radians()
}

fn default_range() -> f32 {
    10.0
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum ShapeDoc {
    Cube,
    Sphere,
    Capsule,
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
            Matter::PointLight { color, intensity, range } => {
                MatterDoc::PointLight { color: *color, intensity: *intensity, range: *range }
            }
            Matter::GravityVolume { mode, strength, radius } => MatterDoc::GravityVolume {
                radial: *mode == GravityMode::Radial,
                strength: *strength,
                radius: *radius,
            },
            Matter::Skybox { color, size, texture, tint } => MatterDoc::Skybox {
                color: *color,
                size: *size,
                texture: texture.clone(),
                tint: *tint,
            },
            Matter::PostProcess {
                enabled,
                bloom,
                bloom_threshold,
                bloom_intensity,
                vignette,
                vignette_strength,
                vignette_radius,
                ao,
                ao_strength,
                ao_radius,
                posterize_bands,
                posterize_dither,
            } => MatterDoc::PostProcess {
                enabled: *enabled,
                bloom: *bloom,
                bloom_threshold: *bloom_threshold,
                bloom_intensity: *bloom_intensity,
                vignette: *vignette,
                vignette_strength: *vignette_strength,
                vignette_radius: *vignette_radius,
                ao: (*ao).into(),
                ao_strength: *ao_strength,
                ao_radius: *ao_radius,
                posterize_bands: *posterize_bands,
                posterize_dither: *posterize_dither,
            },
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
            MatterDoc::PointLight { color, intensity, range } => {
                Matter::PointLight { color: *color, intensity: *intensity, range: *range }
            }
            MatterDoc::GravityVolume { radial, strength, radius } => Matter::GravityVolume {
                mode: if *radial { GravityMode::Radial } else { GravityMode::Down },
                strength: *strength,
                radius: *radius,
            },
            MatterDoc::Skybox { color, size, texture, tint } => Matter::Skybox {
                color: *color,
                size: *size,
                texture: texture.clone(),
                tint: *tint,
            },
            MatterDoc::PostProcess {
                enabled,
                bloom,
                bloom_threshold,
                bloom_intensity,
                vignette,
                vignette_strength,
                vignette_radius,
                ao,
                ao_strength,
                ao_radius,
                posterize_bands,
                posterize_dither,
            } => Matter::PostProcess {
                enabled: *enabled,
                bloom: *bloom,
                bloom_threshold: *bloom_threshold,
                bloom_intensity: *bloom_intensity,
                vignette: *vignette,
                vignette_strength: *vignette_strength,
                vignette_radius: *vignette_radius,
                ao: ao.to_mode(),
                ao_strength: *ao_strength,
                ao_radius: *ao_radius,
                posterize_bands: *posterize_bands,
                posterize_dither: *posterize_dither,
            },
        }
    }
}

impl From<Shape> for ShapeDoc {
    fn from(s: Shape) -> Self {
        match s {
            Shape::Cube => ShapeDoc::Cube,
            Shape::Sphere => ShapeDoc::Sphere,
            Shape::Capsule => ShapeDoc::Capsule,
        }
    }
}
impl From<ShapeDoc> for Shape {
    fn from(s: ShapeDoc) -> Self {
        match s {
            ShapeDoc::Cube => Shape::Cube,
            ShapeDoc::Sphere => Shape::Sphere,
            ShapeDoc::Capsule => Shape::Capsule,
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
    #[serde(default = "one_f32")]
    pub intensity: f32,
    // Sun shadows (SDF field march). Pre-shadow scenes deserialize to the defaults.
    #[serde(default = "true_bool")]
    pub shadows: bool,
    #[serde(default = "default_shadow_softness")]
    pub shadow_softness: f32,
    #[serde(default = "one_f32")]
    pub shadow_strength: f32,
    #[serde(default)]
    pub shadow_tint: [f32; 3],
    #[serde(default)]
    pub shadow_quantize: u32,
    #[serde(default)]
    pub shadow_dither: bool,
    #[serde(default = "default_shadow_distance")]
    pub shadow_distance: f32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fog: bool,
    #[serde(default = "default_fog_color")]
    pub fog_color: [f32; 3],
    #[serde(default = "default_fog_start")]
    pub fog_start: f32,
    #[serde(default = "default_fog_end")]
    pub fog_end: f32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fog_dither: bool,
    #[serde(default = "default_fog_dither_strength")]
    pub fog_dither_strength: f32,
}

fn default_shadow_softness() -> f32 {
    0.35
}
fn default_shadow_distance() -> f32 {
    150.0
}
fn default_fog_color() -> [f32; 3] {
    [0.6, 0.65, 0.72]
}
fn default_fog_start() -> f32 {
    40.0
}
fn default_fog_end() -> f32 {
    200.0
}
fn default_fog_dither_strength() -> f32 {
    0.5
}

impl Default for LightDoc {
    fn default() -> Self {
        Self::from(&Light::default())
    }
}

impl From<&Light> for LightDoc {
    fn from(l: &Light) -> Self {
        Self {
            direction: l.direction,
            color: l.color,
            ambient: l.ambient,
            intensity: l.intensity,
            shadows: l.shadows,
            shadow_softness: l.shadow_softness,
            shadow_strength: l.shadow_strength,
            shadow_tint: l.shadow_tint,
            shadow_quantize: l.shadow_quantize,
            shadow_dither: l.shadow_dither,
            shadow_distance: l.shadow_distance,
            fog: l.fog,
            fog_color: l.fog_color,
            fog_start: l.fog_start,
            fog_end: l.fog_end,
            fog_dither: l.fog_dither,
            fog_dither_strength: l.fog_dither_strength,
        }
    }
}

impl LightDoc {
    pub fn to_light(self) -> Light {
        Light {
            direction: self.direction,
            color: self.color,
            ambient: self.ambient,
            intensity: self.intensity,
            shadows: self.shadows,
            shadow_softness: self.shadow_softness,
            shadow_strength: self.shadow_strength,
            shadow_tint: self.shadow_tint,
            shadow_quantize: self.shadow_quantize,
            shadow_dither: self.shadow_dither,
            shadow_distance: self.shadow_distance,
            fog: self.fog,
            fog_color: self.fog_color,
            fog_start: self.fog_start,
            fog_end: self.fog_end,
            fog_dither: self.fog_dither,
            fog_dither_strength: self.fog_dither_strength,
        }
    }
}

/// Project-wide render settings — the PS1/PS2-style knobs that apply to every
/// scene. Saved to `project.ron`, edited in the editor's Project Settings.
///
/// Post-processing moved to the per-scene `PostProcess` node ([`MatterDoc::PostProcess`]);
/// the `bloom`/`vignette` fields below are **legacy** — still read so an old
/// `project.ron`'s look can be migrated onto a scene's node, but never written back.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct ProjectConfigDoc {
    pub retro: bool,
    pub retro_height: u32,
    pub matter: bool,
    // Legacy post-processing (pre-PostProcess-node projects) — deserialize only.
    #[serde(default, skip_serializing)]
    pub bloom: bool,
    #[serde(default = "default_bloom_threshold", skip_serializing)]
    pub bloom_threshold: f32,
    #[serde(default = "default_bloom_intensity", skip_serializing)]
    pub bloom_intensity: f32,
    #[serde(default, skip_serializing)]
    pub vignette: bool,
    #[serde(default = "default_vignette_strength", skip_serializing)]
    pub vignette_strength: f32,
    #[serde(default = "default_vignette_radius", skip_serializing)]
    pub vignette_radius: f32,
}

fn default_bloom_threshold() -> f32 {
    1.0
}
fn default_bloom_intensity() -> f32 {
    0.7
}
fn default_vignette_strength() -> f32 {
    0.5
}
fn default_vignette_radius() -> f32 {
    0.7
}

impl Default for ProjectConfigDoc {
    fn default() -> Self {
        Self::ps1()
    }
}

impl ProjectConfigDoc {
    /// The default PS1 look: 240p retro upscale, matter on. Post effects start off.
    pub fn ps1() -> Self {
        Self {
            retro: true,
            retro_height: 240,
            matter: true,
            bloom: false,
            bloom_threshold: default_bloom_threshold(),
            bloom_intensity: default_bloom_intensity(),
            vignette: false,
            vignette_strength: default_vignette_strength(),
            vignette_radius: default_vignette_radius(),
        }
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
        if let Some(after) = rest.strip_prefix("Terrain")
            && !after.starts_with('(') {
                out.push_str("Terrain(id: 0)");
                rest = after;
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
    #[serde(default = "one_f32")]
    pub alpha: f32,
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
            alpha: self.alpha,
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
            alpha: m.alpha,
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
        if let Ok(mat) = std::fs::read_to_string(&p).ok().map(|t| ron::from_str(&t)).transpose()
            && let Some(mat) = mat {
                out.push((name, mat));
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
        if let Some(rb) = &node.rigidbody {
            world.insert(e, rb.to_rigidbody());
        }
        if node.mesh_collider {
            world.insert(e, floptle_core::MeshCollider);
        }
        if node.collidable {
            world.insert(e, floptle_core::Collidable);
        }
        // Visible is the default; only attach the component for an explicitly hidden node.
        if !node.visible {
            world.insert(e, floptle_core::Visible(false));
        }
        // Casting is the default; only an opted-out node carries the component.
        if !node.cast_shadow {
            world.insert(e, floptle_core::CastShadow(false));
        }
        if let Some(ctl) = &node.anim_controller {
            world.insert(e, floptle_core::AnimController { asset: ctl.clone() });
        }
        if let Some(p) = &node.particles {
            world.insert(e, p.to_component());
        }
        ents.push(e);
    }
    // Second pass: link parents (skip out-of-range / self references).
    for (i, node) in doc.nodes.iter().enumerate() {
        if let Some(p) = node.parent
            && p < ents.len() && p != i {
                world.insert(ents[i], floptle_core::Parent(ents[p]));
            }
    }
    // Third pass: bone attachments (target = the parent linked above; resolved by the
    // editor's resolve_attachments each frame, which fixes the identity transform).
    for (i, node) in doc.nodes.iter().enumerate() {
        if let (Some(att), Some(p)) = (&node.attachment, node.parent)
            && p < ents.len()
            && p != i
        {
            world.insert(
                ents[i],
                floptle_core::BoneAttach {
                    target: ents[p],
                    bone: att.bone.clone(),
                    offset: att.offset.to_transform(),
                },
            );
        }
    }
    let light = world.spawn();
    world.insert(light, Name("Lighting".into()));
    world.insert(light, doc.lighting.to_light());

    // Every scene carries a Skybox node (the environment background). If the doc didn't
    // include one (e.g. an older scene), spawn a default grey skybox so a scene always
    // has an editable environment.
    if !doc.nodes.iter().any(|n| matches!(n.matter, MatterDoc::Skybox { .. })) {
        let sky = world.spawn();
        world.insert(sky, Name("Skybox".into()));
        world.insert(sky, Transform::IDENTITY);
        world.insert(sky, Matter::default_skybox());
    }

    // Every scene has gravity out of the box: if the doc has no GravityVolume node at
    // all, spawn a default normal-game "Down" volume (strength 10) so bodies fall
    // without any setup. A scene that already defines its own gravity (a planet's
    // Radial well, or a custom-tuned Down volume) is left alone.
    if !doc.nodes.iter().any(|n| matches!(n.matter, MatterDoc::GravityVolume { .. })) {
        let gravity = world.spawn();
        world.insert(gravity, Name("Gravity".into()));
        world.insert(gravity, Transform::IDENTITY);
        world.insert(gravity, Matter::GravityVolume { mode: GravityMode::Down, strength: 10.0, radius: 20.0 });
    }

    // Every scene carries a PostProcess node — post-processing is tuned per scene,
    // not per project. If the doc predates the node, spawn the default chain (AO on,
    // bloom/vignette off); the editor migrates legacy project-wide bloom/vignette
    // settings onto it right after load.
    if !doc.nodes.iter().any(|n| matches!(n.matter, MatterDoc::PostProcess { .. })) {
        let post = world.spawn();
        world.insert(post, Name("Post Processing".into()));
        world.insert(post, Transform::IDENTITY);
        world.insert(post, Matter::default_post_process());
    }
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
        let attachment = world.get::<floptle_core::BoneAttach>(e).map(|a| AttachmentDoc {
            bone: a.bone.clone(),
            offset: TransformDoc::from(&a.offset),
        });
        // An attached node's live Transform is a derived (pose-baked) value — serialize
        // a STABLE identity instead; `resolve_attachments` re-derives it on load.
        let transform = if attachment.is_some() {
            TransformDoc::from(&Transform::IDENTITY)
        } else {
            world.get::<Transform>(e).map(TransformDoc::from).unwrap_or_default()
        };
        let name = world.get::<Name>(e).map(|n| n.0.clone()).unwrap_or_default();
        let scripts = world
            .get::<Scripts>(e)
            .map(|s| s.0.iter().map(ScriptDoc::from_inst).collect())
            .unwrap_or_default();
        let material = world.get::<Material>(e).map(MaterialDoc::from_material);
        let rigidbody = world.get::<RigidBody>(e).map(RigidBodyDoc::from_rigidbody);
        let mesh_collider = world.get::<floptle_core::MeshCollider>(e).is_some();
        let collidable = world.get::<floptle_core::Collidable>(e).is_some();
        let visible = world.get::<floptle_core::Visible>(e).map(|v| v.0).unwrap_or(true);
        let cast_shadow = world.get::<floptle_core::CastShadow>(e).map(|c| c.0).unwrap_or(true);
        let anim_controller =
            world.get::<floptle_core::AnimController>(e).map(|c| c.asset.clone());
        let particles = world
            .get::<floptle_core::ParticleSystem>(e)
            .map(ParticleSystemDoc::from_component);
        let parent = world.get::<floptle_core::Parent>(e).and_then(|p| index.get(&p.0).copied());
        nodes.push(NodeDoc {
            name,
            transform,
            matter: MatterDoc::from(matter),
            scripts,
            material,
            rigidbody,
            mesh_collider,
            collidable,
            visible,
            cast_shadow,
            anim_controller,
            particles,
            parent,
            attachment,
        });
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
            lighting: LightDoc {
                intensity: 2.5,
                // exercise the shadow-knob round-trips
                shadow_softness: 0.8,
                shadow_tint: [0.3, 0.1, 0.4],
                shadow_quantize: 3,
                shadow_dither: true,
                ..LightDoc::default()
            },
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
                    rigidbody: Some(RigidBodyDoc {
                        capsule: true,
                        boxed: false,
                        radius: 0.6,
                        height: 2.4,
                        half_extents: [0.5, 0.5, 0.5],
                        restitution: 0.2,
                        friction: 0.5,
                        gravity: false, // exercise the gravity-flag round-trip
                        lock_pos: [false, false, true],
                        lock_rot: [true, false, true],
                    }),
                    mesh_collider: true, // exercise the mesh-collider round-trip
                    collidable: true,    // exercise the collidable round-trip
                    visible: false,      // exercise the visible round-trip
                    cast_shadow: false,  // exercise the cast-shadow opt-out round-trip
                    anim_controller: Some("animation_controllers/Test".into()),
                    particles: Some(ParticleSystemDoc {
                        asset: "vfx/Test".into(),
                        play_on_start: false, // exercise the non-default round-trip
                    }),
                    parent: None,
                    attachment: None,
                },
                NodeDoc {
                    name: "blob".into(),
                    transform: TransformDoc::default(),
                    matter: MatterDoc::Blob { scale: 1.3 },
                    scripts: Vec::new(),
                    material: None,
                    rigidbody: None,
                    mesh_collider: false,
                    collidable: false,
                    visible: true,
                    cast_shadow: true,
                    anim_controller: None,
                    particles: None,
                    parent: Some(0), // child of the cube — exercises parent round-trip
                    attachment: Some(AttachmentDoc {
                        bone: "Root".into(),
                        offset: TransformDoc::default(),
                    }), // exercise the bone-attachment round-trip
                },
                NodeDoc {
                    name: "lamp".into(),
                    transform: TransformDoc::default(),
                    matter: MatterDoc::PointLight { color: [0.1, 0.2, 0.9], intensity: 3.5, range: 7.5 },
                    scripts: Vec::new(),
                    material: None,
                    rigidbody: None,
                    mesh_collider: false,
                    collidable: false,
                    visible: true,
                    cast_shadow: true,
                    anim_controller: None,
                    particles: None,
                    parent: None,
                    attachment: None,
                },
                NodeDoc {
                    name: "eye".into(),
                    transform: TransformDoc::default(),
                    matter: MatterDoc::Camera { fov_y: 1.0, active: true },
                    scripts: Vec::new(),
                    material: None,
                    rigidbody: None,
                    mesh_collider: false,
                    collidable: false,
                    visible: true,
                    cast_shadow: true,
                    anim_controller: None,
                    particles: None,
                    parent: None,
                    attachment: None,
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
        // 4 matter nodes (cube, blob, lamp, eye) + an auto-spawned Skybox + an
        // auto-spawned GravityVolume + an auto-spawned PostProcess node + the
        // mandatory Lighting node.
        assert_eq!(world.len(), 8);
        let snap = to_doc("demo", &world);
        // The 4 authored matter nodes plus the auto-added Skybox + GravityVolume +
        // PostProcess nodes.
        assert_eq!(snap.nodes.len(), 7);
        assert!(
            snap.nodes.iter().any(|n| matches!(n.matter, MatterDoc::Skybox { .. })),
            "a default Skybox node should be present"
        );
        assert!(
            snap.nodes.iter().any(|n| matches!(n.matter, MatterDoc::GravityVolume { .. })),
            "a default GravityVolume node should be present"
        );
        assert!(
            snap.nodes.iter().any(|n| matches!(n.matter, MatterDoc::PostProcess { .. })),
            "a default PostProcess node should be present"
        );
        // non-default directional intensity + shadow knobs survive
        assert_eq!(snap.lighting.intensity, 2.5);
        assert_eq!(snap.lighting.shadow_softness, 0.8);
        assert_eq!(snap.lighting.shadow_tint, [0.3, 0.1, 0.4]);
        assert_eq!(snap.lighting.shadow_quantize, 3);
        assert!(snap.lighting.shadow_dither);
        // the cube's authored translation survives the World round-trip
        let cube = snap.nodes.iter().find(|n| n.name == "cube").unwrap();
        assert_eq!(cube.transform.translation, [1.0, 2.0, 3.0]);
        assert!(matches!(cube.matter, MatterDoc::Primitive { shape: ShapeDoc::Cube, .. }));
        // the cube's rigidbody (shape + constraints) round-trips through the World
        let rb = cube.rigidbody.expect("cube rigidbody lost");
        assert!(rb.capsule && rb.radius == 0.6 && rb.height == 2.4);
        assert_eq!(rb.lock_pos, [false, false, true]);
        assert_eq!(rb.lock_rot, [true, false, true]);
        assert!(cube.mesh_collider, "mesh_collider flag lost in round-trip");
        assert!(cube.collidable, "collidable flag lost in round-trip");
        assert!(!cube.visible, "visible flag lost in round-trip");
        assert!(!cube.cast_shadow, "cast_shadow opt-out lost in round-trip");
        assert!(!rb.gravity, "rigidbody gravity flag lost in round-trip");
        // the point light's color/intensity/range round-trip
        let lamp = snap.nodes.iter().find(|n| n.name == "lamp").unwrap();
        assert_eq!(
            lamp.matter,
            MatterDoc::PointLight { color: [0.1, 0.2, 0.9], intensity: 3.5, range: 7.5 }
        );
        // the camera's fov/active round-trip
        let eye = snap.nodes.iter().find(|n| n.name == "eye").unwrap();
        assert_eq!(eye.matter, MatterDoc::Camera { fov_y: 1.0, active: true });
    }

    #[test]
    fn post_process_settings_round_trip() {
        // An authored PostProcess node survives World → RON → World unchanged,
        // and the self-heal does NOT add a second one.
        let mut world = World::new();
        let e = world.spawn();
        world.insert(e, Name("Post Processing".into()));
        world.insert(e, Transform::IDENTITY);
        let authored = Matter::PostProcess {
            enabled: false,
            bloom: true,
            bloom_threshold: 0.15,
            bloom_intensity: 1.1,
            vignette: true,
            vignette_strength: 0.56,
            vignette_radius: 0.45,
            ao: AoMode::Sdf,
            ao_strength: 0.9,
            ao_radius: 1.25,
            posterize_bands: 6,
            posterize_dither: true,
        };
        world.insert(e, authored.clone());

        let text = to_ron(&to_doc("post", &world)).unwrap();
        let mut world2 = World::new();
        spawn_into(&from_ron(&text).unwrap(), &mut world2);

        let posts: Vec<_> =
            world2.query::<Matter>().filter(|(_, m)| matches!(m, Matter::PostProcess { .. })).collect();
        assert_eq!(posts.len(), 1, "self-heal must not duplicate an authored PostProcess node");
        assert_eq!(*posts[0].1, authored);
    }
}


