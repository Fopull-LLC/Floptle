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
    AoMode, BodyKind, GravityMode, Light, Material, Matter, Name, RigidBody, ScreenShader,
    ScriptInst, Scripts,
    Shape, World,
};
use serde::{Deserialize, Serialize};

pub mod anim;
pub use anim::{
    load_anim_clip, load_anim_controller, save_anim_clip, save_anim_controller, AnimChannelDoc,
    AnimClipDoc, AnimControllerDoc, AnimEventDoc, AnimLayerDoc, AnimPropTrackDoc, AnimPropValueDoc,
    load_sprite_anim, save_sprite_anim, AnimStateDoc, AnimTrackDoc3, AnimTrackDoc4,
    AnimTransitionDoc, SpriteAnimDoc, SpriteAnimFrameDoc, SpriteFrameDoc, ANIM_CLIP_EXT,
    ANIM_CTL_EXT, SPRITE_ANIM_EXT, SPRITE_COMPONENT, SPRITE_FIELD,
};
pub mod vfx;
pub use vfx::{
    load_vfx_effect, save_vfx_effect, VfxBlendDoc, VfxBurstDoc, VfxClipDoc, VfxCurveDoc,
    VfxEffectDoc, VfxEmitDoc, VfxEndDoc, VfxExtrapolateDoc, VfxFlipModeDoc, VfxFlipbookDoc,
    VfxForceDoc, VfxGravityDoc, VfxInterpDoc, VfxKeyDoc, VfxLaneDoc, VfxLaneTargetDoc,
    VfxLifetimeScaleDoc, VfxOrientDoc, VfxPlaybackDoc, VfxPropDoc, VfxRenderDoc, VfxShapeDoc,
    VfxSpaceDoc, VfxTrackDoc, VfxTrailDoc, VfxValueDoc,
    VFX_EXT,
};

/// A prefab asset (`*.prefab.ron`): a reusable node subtree in the same flat
/// `Vec<NodeDoc>` format the editor's node clipboard uses — `parent` is an
/// index into the list (`None` = a root), children carry local transforms.
pub const PREFAB_EXT: &str = ".prefab.ron";

/// A whole scene: a name, its lighting (the mandatory Lighting node), and the
/// nodes in it. Project-wide render settings live separately in [`ProjectConfigDoc`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SceneDoc {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub lighting: LightDoc,
    #[serde(default)]
    pub nodes: Vec<NodeDoc>,
}

/// A bone/sub-object attachment of a node to its parent Mesh (see
/// [`floptle_core::BoneAttach`]). The target is the node's serialized `parent`; only
/// the bone name + bone-local offset are stored here.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AttachmentDoc {
    #[serde(default)]
    pub bone: String,
    #[serde(default)]
    pub offset: TransformDoc,
}

/// One node = one entity's authored data.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NodeDoc {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub transform: TransformDoc,
    #[serde(default)]
    pub matter: MatterDoc,
    #[serde(default)]
    pub scripts: Vec<ScriptDoc>,
    /// The node's material (surface look). `None` = the engine's default look.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material: Option<MaterialDoc>,
    /// Per-SUB-OBJECT material overrides on a Mesh node: object name (or, for a
    /// flattened model, material name) ⏵ that part's material. See
    /// [`floptle_core::ObjectMaterials`]. Empty = none (old scenes untouched).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub object_materials: std::collections::BTreeMap<String, MaterialDoc>,
    /// A physics rigidbody on this node (`None` = not a physics body).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rigidbody: Option<RigidBodyDoc>,
    /// Puts the node on Kepler rails + makes it an inverse-square gravity
    /// source (solar demo S2). See [`floptle_core::CelestialBody`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub celestial: Option<CelestialBodyDoc>,
    /// Marks a Mesh node as a static walkable collider (its triangles collide at Play).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mesh_collider: bool,
    /// Switched off: no draw, no collision, no scripts — for this node and everything
    /// under it. Stored INVERTED (`disabled`, skipped when false) so the overwhelmingly
    /// common case adds nothing to a scene file and every scene ever written still
    /// loads meaning "on".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Stable key for this node's vertex paint, if it has any. The colors themselves
    /// live in `<project>/paint/<scene>.vpaint` — per-vertex arrays don't belong in a
    /// scene `.ron`, the same call terrain fields make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paint: Option<u32>,
    /// Texture-paint id (the node carries a hand-painted texture) — its images live in the
    /// editor's store keyed by this stable id, exactly like `paint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tex_paint: Option<u32>,
    /// On-demand terrain genspec (RON `PlanetFill`) — this Terrain node's field
    /// generates from it when first approached instead of loading a `.cfield`.
    /// See [`floptle_core::TerrainGen`] (G2 galaxy streaming).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terrain_gen: Option<String>,
    /// The "collidable" switch: a static collider auto-shaped from this node's geometry
    /// (no dynamic rigidbody needed). See [`floptle_core::Collidable`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collidable: bool,
    /// Makes the collidable a TRIGGER: bodies pass through, overlap fires the
    /// `onTriggerEnter/Stay/Exit` hooks. See [`floptle_core::Trigger`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trigger: bool,
    /// Keeps this node out of every navmesh bake, whatever else it is. See
    /// [`floptle_core::NavMeshExclude`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nav_exclude: bool,
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
    /// A stable identity for this node **within this scene**, allocated on save
    /// and never reused. What [`NodeDoc::parent_id`] points at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u32>,
    /// This node's parent, by [`NodeDoc::id`] — the authoritative link.
    ///
    /// Preferred over [`NodeDoc::parent`] whenever present, because a positional
    /// index is not a reference to a node, it is a reference to a *position*.
    /// Inserting or removing any node ahead of it silently re-points it at a
    /// different node, the file still loads, and nothing warns: the scene is
    /// simply wired to something else. In the field this moved a whole match HUD
    /// onto a line of help text inside another panel, and since an invisible
    /// parent hides its subtree, the round clock and score pips were never drawn
    /// in any mode. It reached players as three unrelated UI bugs. floptle/0046.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<u32>,
    /// Index (into this scene's `nodes`) of this node's parent — its transform is
    /// local to it. `None` = a root node. The transform is local either way.
    ///
    /// **Legacy.** Still written so older engines can read new scenes, and still
    /// honoured when `parent_id` is absent, but [`NodeDoc::parent_id`] wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<usize>,
    /// Bone/sub-object of the parent Mesh this node rides (`None` = a plain child).
    /// The node's `transform` is serialized stable (identity) when attached, since
    /// its live transform is a derived pose value re-computed on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentDoc>,
    /// The "Networked" component: how this node replicates in a multiplayer
    /// session (`None` = local-only). See [`floptle_core::Replicated`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<ReplicatedDoc>,
    /// A game-UI layer root on this node (docs/ui-system-proposal.md §3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_layer: Option<floptle_ui::UiLayer>,
    /// A game-UI element on this node (place/size/shape/text/image/stack).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<floptle_ui::ElementSpec>,
    /// A sound emitter on this node (`None` = silent). The component type is
    /// its own serialized form — see [`floptle_audio::AudioSource`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<floptle_audio::AudioSource>,
    /// The node's collision/query layer, BY NAME (`None` = "Default"). Stored
    /// by name so reordering the project's layer list never re-layers a scene;
    /// unknown names fall back to Default at Play (the editor warns).
    /// See [`floptle_core::Layer`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    /// Free-form string tags on this node (`node:hasTag` / `findTagged`).
    /// See [`floptle_core::Tags`]; only tagged nodes serialize this.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// What draws in front of what: the sorting layer's NAME and the order
    /// within it. `None` = the Default layer at order 0, which is every node
    /// that has not opted in — so this writes nothing to a scene that does not
    /// use it. See [`floptle_core::Sorting`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sorting: Option<(String, i32)>,
    /// How the node's place *within* its sorting layer is decided: `"y"` for
    /// Y-sorting, absent for the ordinary `order`. See [`floptle_core::SortMode`].
    ///
    /// **Its own field rather than a third element of `sorting`.** A tuple that
    /// grew a slot would stop every scene written before this from loading, and
    /// a mode is meaningful on a node that has said nothing else about sorting
    /// (Y-sorting on the Default layer is the ordinary top-down case), so it
    /// could not be folded in as an option on the tuple either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_mode: Option<String>,
    /// Per-axis parallax scroll factor. `None` = `(1, 1)`, which is no parallax
    /// and is every node that has not opted in — so this writes nothing to a
    /// scene that does not use it. See [`floptle_core::Parallax`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallax: Option<(f32, f32)>,
    /// How this (orthographic) Camera follows. `None` = it does not, which is
    /// every camera that has not opted in. See [`floptle_core::camera2d::Camera2D`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_2d: Option<Camera2DDoc>,
    /// Whether this node is on the 2D lighting path: `"auto"`, `"2d"` or
    /// `"3d"`. `None` = `auto`, which is every node that has not opted in, so
    /// this writes nothing to a scene that does not use 2D lighting.
    /// See [`floptle_core::Lit2D`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lit_2d: Option<String>,
    /// **Lights only.** The sorting layers this light reaches, by name. Empty —
    /// the default — means every layer. See [`floptle_core::Lighting2D`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub light_layers: Vec<String>,
    /// Whether this node blocks 2D light: `"auto"`, `"on"` or `"off"`.
    /// `None` = `auto`. See [`floptle_core::Cast2D`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_2d: Option<String>,
    /// **Lights only.** Full brightness out to this radius before the ramp
    /// starts. `None` = 0, which is every light written before `floptle/0126`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light_inner: Option<f32>,
    /// **Lights only.** The exponent of that ramp. `None` = 2, the curve every
    /// light has always had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light_falloff: Option<f32>,
    /// **Lights only.** Whether casters stop this light. `None` = yes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light_shadows: Option<bool>,
}

/// Serializable replication settings, mirroring [`floptle_core::Replicated`].
/// The runtime `owner`/`NetId` are session state, not authored — they are
/// deliberately NOT serialized.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct ReplicatedDoc {
    /// true = the owner-client predicts this node (its own avatar);
    /// false = plain server-authoritative replication (the default).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub predicted: bool,
    /// true = EVERY peer simulates this node every tick from the session input
    /// set, rolling back on a mispredict (`ReplicationMode::Rollback`).
    ///
    /// A separate flag rather than turning `predicted` into an enum, so every
    /// scene written before rollback existed still loads unchanged. Rollback
    /// wins if both are somehow set — it is the stronger claim, and silently
    /// picking the weaker one would be a desync nobody could see in the file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rollback: bool,
    /// Sync position/rotation (default true).
    #[serde(default = "true_bool")]
    pub transform: bool,
    /// Sync velocity too (default false).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub physics: bool,
    /// Sync the Animation Controller's playback (default true; off =
    /// client-sided animator).
    #[serde(default = "true_bool")]
    pub animator: bool,
    /// Smooth remote entities between snapshots (default true).
    #[serde(default = "true_bool")]
    pub interp: bool,
    /// Remote-render delay in gameplay ticks (default 6 ≈ 100 ms @ 60 Hz).
    #[serde(default = "default_interp_delay", skip_serializing_if = "is_default_interp_delay")]
    pub interp_delay: u8,
    /// Never interest-culled — replicated to every client wherever they are
    /// (default false).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub always_relevant: bool,
}

fn default_interp_delay() -> u8 {
    floptle_core::Replicated::DEFAULT_INTERP_DELAY
}
fn is_default_interp_delay(d: &u8) -> bool {
    *d == floptle_core::Replicated::DEFAULT_INTERP_DELAY
}

impl ReplicatedDoc {
    pub fn to_component(&self) -> floptle_core::Replicated {
        floptle_core::Replicated {
            mode: if self.rollback {
                floptle_core::ReplicationMode::Rollback
            } else if self.predicted {
                floptle_core::ReplicationMode::Predicted
            } else {
                floptle_core::ReplicationMode::Authority
            },
            owner: None, // session state, assigned at runtime
            transform: self.transform,
            physics: self.physics,
            animator: self.animator,
            interp: self.interp,
            interp_delay: self.interp_delay,
            always_relevant: self.always_relevant,
        }
    }

    pub fn from_component(r: &floptle_core::Replicated) -> Self {
        Self {
            predicted: r.mode == floptle_core::ReplicationMode::Predicted,
            rollback: r.mode == floptle_core::ReplicationMode::Rollback,
            transform: r.transform,
            physics: r.physics,
            animator: r.animator,
            interp: r.interp,
            interp_delay: r.interp_delay,
            always_relevant: r.always_relevant,
        }
    }
}

/// Serializable particle-system component, mirroring [`floptle_core::ParticleSystem`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ParticleSystemDoc {
    /// Effect asset key: project-relative path without extension (`vfx/360Slash`).
    #[serde(default)]
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
    /// How the body simulates: `Dynamic` (default, omitted), `Kinematic`
    /// (transform-driven, pushes dynamic bodies), or `Static` (a baked
    /// immovable collider — no body at all). See [`floptle_core::BodyMode`].
    #[serde(default, skip_serializing_if = "is_dynamic")]
    pub mode: BodyModeDoc,
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
    /// Steepest standable surface, degrees. Omitted at its 60° default, so
    /// every scene written before it existed loads meaning exactly what it did.
    #[serde(default = "slope_limit_f32", skip_serializing_if = "is_default_slope")]
    pub slope_limit: f32,
    #[serde(default = "true_bool")]
    pub gravity: bool,
    #[serde(default)]
    pub lock_pos: [bool; 3],
    /// 2D: keep the body in the XY plane (`RigidBody::two_d`).
    #[serde(default)]
    pub two_d: bool,
    #[serde(default)]
    pub lock_rot: [bool; 3],
    /// Tilt the node so local +Y tracks −gravity (radial-planet characters).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub align_up: bool,
    /// Mass (a shape's share inside an assembly compound).
    #[serde(default = "one_f32", skip_serializing_if = "is_one")]
    pub mass: f32,
    /// Root of a compound assembly built from descendant RigidBody shapes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub assembly: bool,
    /// Pushbox-only: the solver never resolves this body's contacts — it
    /// integrates its velocity and nothing else. The rollback profile
    /// (`docs/rollback-netcode-design.md` §3). Omitted when off, so every scene
    /// that predates it loads unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pushbox_only: bool,
}

fn is_one(v: &f32) -> bool {
    *v == 1.0
}

fn true_bool() -> bool {
    true
}
/// `skip_serializing_if` predicate: omit a bool that's at its `true` default.
fn is_true(b: &bool) -> bool {
    *b
}

/// Serializable [`floptle_core::BodyMode`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BodyModeDoc {
    #[default]
    Dynamic,
    Kinematic,
    Static,
}

fn is_dynamic(m: &BodyModeDoc) -> bool {
    *m == BodyModeDoc::Dynamic
}

impl BodyModeDoc {
    fn to_mode(self) -> floptle_core::BodyMode {
        match self {
            BodyModeDoc::Dynamic => floptle_core::BodyMode::Dynamic,
            BodyModeDoc::Kinematic => floptle_core::BodyMode::Kinematic,
            BodyModeDoc::Static => floptle_core::BodyMode::Static,
        }
    }
    fn from_mode(m: floptle_core::BodyMode) -> Self {
        match m {
            floptle_core::BodyMode::Dynamic => BodyModeDoc::Dynamic,
            floptle_core::BodyMode::Kinematic => BodyModeDoc::Kinematic,
            floptle_core::BodyMode::Static => BodyModeDoc::Static,
        }
    }
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
fn slope_limit_f32() -> f32 {
    60.0
}
fn is_default_slope(v: &f32) -> bool {
    *v == 60.0
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
            mode: self.mode.to_mode(),
            radius: self.radius,
            height: self.height,
            half_extents: self.half_extents,
            restitution: self.restitution,
            friction: self.friction,
            slope_limit: self.slope_limit.clamp(0.0, 90.0),
            gravity: self.gravity,
            lock_pos: self.lock_pos,
            lock_rot: self.lock_rot,
            two_d: self.two_d,
            align_up: self.align_up,
            mass: self.mass,
            assembly: self.assembly,
            pushbox_only: self.pushbox_only,
        }
    }
    pub fn from_rigidbody(rb: &RigidBody) -> Self {
        Self {
            capsule: rb.kind == BodyKind::Capsule,
            boxed: rb.kind == BodyKind::Box,
            mode: BodyModeDoc::from_mode(rb.mode),
            radius: rb.radius,
            height: rb.height,
            half_extents: rb.half_extents,
            restitution: rb.restitution,
            friction: rb.friction,
            slope_limit: rb.slope_limit,
            gravity: rb.gravity,
            lock_pos: rb.lock_pos,
            lock_rot: rb.lock_rot,
            two_d: rb.two_d,
            align_up: rb.align_up,
            mass: rb.mass,
            assembly: rb.assembly,
            pushbox_only: rb.pushbox_only,
        }
    }
}

/// Serializable 2D camera behaviour, mirroring
/// [`floptle_core::camera2d::Camera2D`] — the saved half of it.
///
/// The live half (where the follow has got to, and any shake in progress) is
/// deliberately absent: a scene records the *rule*, and a camera that reloaded
/// mid-shake would be recording a moment.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Camera2DDoc {
    /// Name of the node to follow. Empty = none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub follow: String,
    /// Seconds to close the gap; `0` snaps.
    #[serde(default = "default_smoothing")]
    pub smoothing: f32,
    /// Half-size of the box the target moves in before the camera does.
    #[serde(default, skip_serializing_if = "is_zero2")]
    pub dead_zone: (f32, f32),
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub limits_on: bool,
    #[serde(default, skip_serializing_if = "is_zero2")]
    pub limit_min: (f32, f32),
    #[serde(default, skip_serializing_if = "is_zero2")]
    pub limit_max: (f32, f32),
    /// Land the drawn camera on a whole pixel of this many per world unit;
    /// `0` = off. See `Camera2D::pixel_snap`.
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub pixel_snap: f32,
}

fn default_smoothing() -> f32 {
    0.15
}

fn is_zero2(v: &(f32, f32)) -> bool {
    v.0 == 0.0 && v.1 == 0.0
}

impl Default for Camera2DDoc {
    fn default() -> Self {
        Self {
            follow: String::new(),
            smoothing: default_smoothing(),
            dead_zone: (0.0, 0.0),
            limits_on: false,
            limit_min: (0.0, 0.0),
            limit_max: (0.0, 0.0),
            pixel_snap: 0.0,
        }
    }
}

impl From<&floptle_core::camera2d::Camera2D> for Camera2DDoc {
    fn from(c: &floptle_core::camera2d::Camera2D) -> Self {
        Self {
            follow: c.follow.clone(),
            smoothing: c.smoothing,
            dead_zone: (c.dead_zone[0], c.dead_zone[1]),
            limits_on: c.limits_on,
            limit_min: (c.limit_min[0], c.limit_min[1]),
            limit_max: (c.limit_max[0], c.limit_max[1]),
            pixel_snap: c.pixel_snap,
        }
    }
}

impl From<&Camera2DDoc> for floptle_core::camera2d::Camera2D {
    fn from(d: &Camera2DDoc) -> Self {
        floptle_core::camera2d::Camera2D {
            follow: d.follow.clone(),
            smoothing: d.smoothing,
            dead_zone: [d.dead_zone.0, d.dead_zone.1],
            limits_on: d.limits_on,
            limit_min: [d.limit_min.0, d.limit_min.1],
            limit_max: [d.limit_max.0, d.limit_max.1],
            pixel_snap: d.pixel_snap,
            ..Default::default()
        }
    }
}

/// Serializable on-rails celestial body, mirroring [`floptle_core::CelestialBody`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CelestialBodyDoc {
    #[serde(default = "mu_default")]
    pub mu: f64,
    #[serde(default = "body_radius_default")]
    pub body_radius: f64,
    #[serde(default)]
    pub soi: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    #[serde(default)]
    pub a: f64,
    #[serde(default)]
    pub e: f64,
    #[serde(default)]
    pub i: f64,
    #[serde(default)]
    pub lan: f64,
    #[serde(default)]
    pub arg_pe: f64,
    #[serde(default)]
    pub m0: f64,
    /// S8 atmosphere (black + height 0 = airless).
    #[serde(default, skip_serializing_if = "is_zero3")]
    pub atmo_color: [f32; 3],
    #[serde(default)]
    pub atmo_height: f64,
    #[serde(default = "one_f32")]
    pub atmo_density: f32,
    #[serde(default)]
    pub clouds: f32,
    /// Star: irradiance at distance d = luminosity × 1e6 / d². 0 = not a star.
    #[serde(default)]
    pub luminosity: f32,
    #[serde(default = "default_star_color")]
    pub star_color: [f32; 3],
    /// Occlusion culling: solid-core radius geometry never pierces (0 = off).
    #[serde(default)]
    pub occluder_radius: f64,
}

fn default_star_color() -> [f32; 3] {
    [1.0, 0.97, 0.9]
}

fn is_zero3(v: &[f32; 3]) -> bool {
    *v == [0.0, 0.0, 0.0]
}

fn mu_default() -> f64 {
    1.0e6
}
fn body_radius_default() -> f64 {
    30.0
}

impl CelestialBodyDoc {
    pub fn to_body(&self) -> floptle_core::CelestialBody {
        floptle_core::CelestialBody {
            mu: self.mu,
            body_radius: self.body_radius,
            soi: self.soi,
            parent: self.parent.clone(),
            a: self.a,
            e: self.e,
            i: self.i,
            lan: self.lan,
            arg_pe: self.arg_pe,
            m0: self.m0,
            atmo_color: self.atmo_color,
            atmo_height: self.atmo_height,
            atmo_density: self.atmo_density,
            clouds: self.clouds,
            luminosity: self.luminosity,
            star_color: self.star_color,
            occluder_radius: self.occluder_radius,
        }
    }
    pub fn from_body(b: &floptle_core::CelestialBody) -> Self {
        Self {
            mu: b.mu,
            body_radius: b.body_radius,
            soi: b.soi,
            parent: b.parent.clone(),
            a: b.a,
            e: b.e,
            i: b.i,
            lan: b.lan,
            arg_pe: b.arg_pe,
            m0: b.m0,
            atmo_color: b.atmo_color,
            atmo_height: b.atmo_height,
            atmo_density: b.atmo_density,
            clouds: b.clouds,
            luminosity: b.luminosity,
            star_color: b.star_color,
            occluder_radius: b.occluder_radius,
        }
    }
}

/// A serializable attached script, mirroring [`floptle_core::ScriptInst`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ScriptDoc {
    #[serde(default)]
    pub kind: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub params: Vec<(String, f32)>,
    /// Node-reference params: param name → target node NAME (Inspector-wired).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<(String, String)>,
    /// String params: per-instance text tunables (`name = "value"` defaults).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strs: Vec<(String, String)>,
}

fn yes() -> bool {
    true
}

impl ScriptDoc {
    fn to_inst(&self) -> ScriptInst {
        ScriptInst {
            kind: self.kind.clone(),
            enabled: self.enabled,
            params: self.params.clone(),
            refs: self.refs.clone(),
            strs: self.strs.clone(),
        }
    }
    fn from_inst(s: &ScriptInst) -> Self {
        Self {
            kind: s.kind.clone(),
            enabled: s.enabled,
            params: s.params.clone(),
            refs: s.refs.clone(),
            strs: s.strs.clone(),
        }
    }
}

/// Serializable transform (translation `f64`, rotation `xyzw`, scale `f32`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct TransformDoc {
    #[serde(default)]
    pub translation: [f64; 3],
    #[serde(default = "identity_quat")]
    pub rotation: [f32; 4],
    #[serde(default = "one3")]
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
///
/// `Empty` is the `Default` so a `NodeDoc` missing its `matter:` line still
/// loads — a node with nothing in it, which is a thing you can see and fix,
/// rather than a whole scene that refuses to open.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub enum MatterDoc {
    Primitive { shape: ShapeDoc, color: [f32; 3] },
    Blob { scale: f32 },
    Mesh { asset_path: String },
    #[default]
    Empty,
    Terrain {
        /// Stable per-terrain id (legacy single-terrain scenes default to 0).
        #[serde(default)]
        id: u32,
    },
    /// An editable map-building polygon mesh. Geometry lives in the per-scene
    /// `maps/<scene>.map.ron` sidecar keyed by this stable id (the terrain
    /// pattern — big data never rides the scene doc).
    ///
    /// `geo` is the ESCAPE HATCH for documents that leave the scene: a prefab or
    /// a clipboard payload has no sidecar to key into, so those writers stamp the
    /// geometry in and the spawner mints a fresh id from it. Scene saves and the
    /// per-frame undo baseline leave it `None` (`to_doc` never fills it), so the
    /// hot path stays exactly as small as it was.
    MapMesh {
        id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        geo: Option<floptle_map::MapMesh>,
    },
    /// A camera viewpoint. `fov_y` is the vertical field of view (radians); `active`
    /// marks the camera that holds play-mode authority on load. A non-empty
    /// `target` renders the camera into the live `rt:<target>` texture; the
    /// layer `cull_mask` defaults to everything.
    Camera {
        #[serde(default = "default_fov")]
        fov_y: f32,
        #[serde(default)]
        active: bool,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        target: String,
        #[serde(default = "all_layers", skip_serializing_if = "is_all_layers")]
        cull_mask: u32,
        /// Render-target size in pixels and refresh rate in Hz (0 = every
        /// frame). Defaulted so a scene written before `floptle/0078` loads
        /// with the size it used to get.
        #[serde(default = "default_target_w", skip_serializing_if = "is_default_target_w")]
        target_w: u32,
        #[serde(default = "default_target_h", skip_serializing_if = "is_default_target_h")]
        target_h: u32,
        #[serde(default, skip_serializing_if = "is_zero_f32")]
        target_hz: f32,
        /// Orthographic rather than perspective — the 2D / strategy projection.
        #[serde(default, skip_serializing_if = "is_false")]
        ortho: bool,
        /// World-space height the orthographic view covers. Only meaningful with
        /// `ortho`, and skipped at its default so a perspective camera's `.ron`
        /// does not carry a number that does nothing.
        #[serde(default = "default_ortho_height", skip_serializing_if = "is_default_ortho_height")]
        ortho_height: f32,
    },
    /// A placeable point/omni/area light (position = node transform).
    PointLight {
        #[serde(default = "white3")]
        color: [f32; 3],
        #[serde(default = "one_f32")]
        intensity: f32,
        #[serde(default = "default_range")]
        range: f32,
        /// The surface it emits from. Skipped at `Point`, so every light written
        /// before area lights existed round-trips byte-identically.
        #[serde(default, skip_serializing_if = "is_point_shape")]
        shape: LightShapeDoc,
        /// Local shadows, off by default and skipped when off — so a lamp placed
        /// before this existed round-trips byte-identically AND costs nothing.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        shadows: bool,
        /// Aimed down the node's local −Z, or `None` for an ordinary
        /// omnidirectional lamp.
        ///
        /// **One optional field rather than two defaulted numbers**, for a
        /// reason worth stating: `skip_serializing_if` cannot see a sibling, so
        /// two fields could not be omitted *together* when there is no cone.
        /// Either every light ever written would grow two lines it does not
        /// need, or a spot with a deliberately hard edge (`softness: 0.0`)
        /// would have that zero skipped as "the default" and come back soft.
        /// Grouping them makes both problems go away: absent means omni,
        /// present means both numbers were meant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spot: Option<SpotDoc>,
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
    /// A body of water — a planet's sea (`pool: false`) or a lake/tank.
    /// Every knob defaults, so a hand-written `WaterVolume()` is a plain sea.
    WaterVolume {
        #[serde(default)]
        pool: bool,
        #[serde(default = "default_range")]
        radius: f32,
        #[serde(default = "default_pool_half")]
        half_extents: [f32; 3],
        #[serde(default = "default_water_density")]
        density: f32,
        #[serde(default = "one_f32")]
        drag: f32,
        #[serde(default = "one_f32")]
        angular_drag: f32,
        #[serde(default)]
        frozen: bool,
        #[serde(default = "default_water_tint")]
        tint: [f32; 3],
        #[serde(default = "default_water_visibility")]
        visibility: f32,
    },
    /// An authored SDF shape — its Material's sdf-stage `.flsl` is the geometry.
    FieldShape {
        #[serde(default = "one_f32")]
        radius: f32,
    },
    /// A grid of spritesheet cells drawn as one mesh (`floptle/0058`). The sheet
    /// is the node's Material; this is only the grid.
    Tilemap {
        #[serde(default)]
        cols: u32,
        #[serde(default)]
        rows: u32,
        #[serde(default = "one_f32")]
        tile: f32,
        /// Row-major packed squares (cell index + orientation) from the top-left,
        /// `cols * rows` long. A grid written before orientations existed is a
        /// list of bare indices and loads unchanged.
        #[serde(default)]
        data: Vec<u32>,
        /// Project-relative `.tileset.ron` giving each cell its collision, tags,
        /// autotile group and animation. Skipped when empty, so an art-only
        /// tilemap's `.ron` does not carry the field at all.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tileset: String,
    },
    /// N sprites from one node, each with its own transform, cell and tint. The
    /// sprites themselves are runtime-only and deliberately not saved.
    SpriteBatch {
        #[serde(default = "one_f32")]
        size: f32,
    },
    /// One sprite — see [`floptle_core::Matter::Sprite`]. Every field defaults,
    /// so a hand-written `Sprite()` is a one-unit centred quad on cell 0.
    Sprite {
        /// Pixels per world unit; `0` = use `size`.
        #[serde(default)]
        ppu: f32,
        #[serde(default = "one_f32")]
        size: f32,
        #[serde(default)]
        cell: u32,
        #[serde(default, skip_serializing_if = "is_false")]
        flip_x: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        flip_y: bool,
        /// Origin within the sprite, `0..1` from the bottom-left. Skipped at the
        /// centre so a sprite that never moved its pivot writes nothing.
        #[serde(default = "centre_pivot", skip_serializing_if = "is_centre_pivot")]
        pivot: [f32; 2],
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shader: Option<String>,
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        shader_params: std::collections::BTreeMap<String, [f32; 4]>,
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
        #[serde(default)]
        posterize_chroma: bool,
        /// 0 clip (default) / 1 Reinhard / 2 ACES / 3 AgX. Skips at 0, so a
        /// scene that never chose one writes the RON it always did.
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        tonemap: u32,
        // ---- the look chain -------------------------------------------------
        // Every one has a default AND a `skip_serializing_if` at that default,
        // so a scene that touches none of it writes not one extra line — and an
        // older scene, which has none of them, loads unchanged.
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        exposure: f32,
        #[serde(default = "one_f32", skip_serializing_if = "is_one_f32")]
        contrast: f32,
        #[serde(default = "one_f32", skip_serializing_if = "is_one_f32")]
        saturation: f32,
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        temperature: f32,
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        tint: f32,
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        lift: f32,
        #[serde(default = "one_f32", skip_serializing_if = "is_one_f32")]
        grade_gamma: f32,
        #[serde(default = "one_f32", skip_serializing_if = "is_one_f32")]
        gain: f32,
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        aberration: f32,
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        distortion: f32,
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        sharpen: f32,
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        denoise: f32,
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        grain: f32,
        #[serde(default = "one_f32", skip_serializing_if = "is_one_f32")]
        grain_size: f32,
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        dof_focus: f32,
        #[serde(default = "default_dof_range", skip_serializing_if = "is_default_dof_range")]
        dof_range: f32,
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        dof_max_blur: f32,
        /// 0 = half of `dof_range` (what the effect did before there were two).
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        dof_near_range: f32,
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        dof_blades: u32,
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        dof_blade_rotation: f32,
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        dof_highlight: f32,
        /// 0 = the default 16 taps.
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        dof_quality: u32,
        /// Motion-blur shutter (0 = off), and taps along the streak (0 = 12).
        /// Both omitted at their defaults, so no existing scene grows a line.
        #[serde(default = "zero_f32", skip_serializing_if = "is_zero_f32")]
        motion_blur: f32,
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        motion_samples: u32,
        /// A tuning view, so it is deliberately NOT saved when off — and it is
        /// saved when on, because leaving it on and closing the project is a
        /// thing that happens and finding it still on is better than a frame
        /// that mysteriously fixed itself.
        #[serde(default, skip_serializing_if = "is_false")]
        dof_show_focus: bool,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        dof_focus_node: String,
        /// Authored `stage post` passes, in order. Empty on every scene that
        /// has never used one, and skipped when empty, so nothing is written.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        screen_shaders: Vec<ScreenShaderDoc>,
    },
    /// The baked-GI volume ([`Matter::LightProbes`]). Every knob defaults, so a
    /// hand-written `LightProbes()` is a room-sized box at one probe per two
    /// metres — which is a sensible thing to type and then bake.
    ///
    /// The bake itself is NOT here: it lives in a `.fgi` beside the scene. A
    /// scene file is a thing people read and merge, and a few hundred kilobytes
    /// of spherical harmonics is neither.
    LightProbes {
        #[serde(default = "default_probe_half")]
        half_extents: [f32; 3],
        #[serde(default = "default_probe_spacing")]
        spacing: f32,
        #[serde(default = "on")]
        enabled: bool,
        #[serde(default = "one_f32")]
        intensity: f32,
        #[serde(default = "one_u32")]
        bounces: u32,
        #[serde(default = "default_probe_quality")]
        quality: u32,
        #[serde(default = "one_f32")]
        leak: f32,
        #[serde(default = "default_probe_normal_bias")]
        normal_bias: f32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude_layers: Vec<String>,
    },
    /// A navmesh ([`Matter::NavMesh`]). Every knob defaults, so a hand-written
    /// `NavMesh()` bakes every layer, works its own bounds out, and describes a
    /// human-sized character — which is a sensible thing to type and then bake.
    ///
    /// The bake itself is NOT here: it lives in a `.fnav` beside the scene, for
    /// the same reason the light bake is a `.fgi`.
    NavMesh {
        #[serde(default)]
        id: u32,
        #[serde(default = "default_nav_half")]
        half_extents: [f32; 3],
        #[serde(default = "on")]
        auto_bounds: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        layers: Vec<String>,
        #[serde(default = "default_agent_radius")]
        agent_radius: f32,
        #[serde(default = "default_agent_height")]
        agent_height: f32,
        #[serde(default = "default_max_slope")]
        max_slope: f32,
        #[serde(default = "default_step_height")]
        step_height: f32,
        #[serde(default = "default_nav_cell")]
        cell_size: f32,
        #[serde(default = "on")]
        enabled: bool,
        #[serde(default)]
        auto_rebake: bool,
    },
    /// A nav link ([`Matter::NavLink`]) — a ladder, a jump, a door. A
    /// hand-written `NavLink()` is a one-way step two metres forward.
    NavLink {
        #[serde(default)]
        id: u32,
        #[serde(default = "default_link_to")]
        to: [f32; 3],
        #[serde(default)]
        bidirectional: bool,
        #[serde(default = "default_link_cost")]
        cost: f32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        area: String,
        #[serde(default)]
        duration: f32,
        #[serde(default = "on")]
        enabled: bool,
    },
    /// A nav area ([`Matter::NavArea`]) — ground that costs more, or ground that
    /// is not ground.
    NavArea {
        #[serde(default = "default_nav_area_half")]
        half_extents: [f32; 3],
        #[serde(default = "default_nav_area_name")]
        area: String,
        #[serde(default = "default_nav_area_cost")]
        cost: f32,
        #[serde(default)]
        blocks: bool,
        #[serde(default = "on")]
        enabled: bool,
    },
    /// A reflection probe ([`Matter::ReflectionProbe`]). Every knob defaults, so
    /// a hand-written `ReflectionProbe()` is a room-sized box that captures on
    /// load — which is a sensible thing to type and then stop thinking about.
    ///
    /// The capture is NOT here and is not anywhere: it is taken at runtime, so
    /// there is no artefact to go stale and nothing added to a file people read.
    ReflectionProbe {
        #[serde(default = "default_probe_half")]
        half_extents: [f32; 3],
        #[serde(default = "on")]
        enabled: bool,
        #[serde(default = "one_f32")]
        intensity: f32,
        #[serde(default = "default_probe_fade")]
        fade: f32,
    },
}

/// Serializable reflection-probe detail — mirrors `floptle_render::ProbeDetail`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ProbeDetailDoc {
    Low,
    Medium,
    #[default]
    High,
    Ultra,
}

/// Serializable frame pacing — mirrors `floptle_render::Vsync`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VsyncDoc {
    /// Every frame shown, in order, at the display's cadence.
    #[default]
    On,
    /// Render freely; the display takes the newest frame each refresh.
    Adaptive,
    /// Present the instant a frame is ready, tearing and all.
    Off,
}

/// Serializable [`ScreenShader`] — one authored full-screen pass.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ScreenShaderDoc {
    pub shader: String,
    /// Defaults to ON: a pass that arrives without the field was written before
    /// there was one, and it was running.
    #[serde(default = "on")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub params: std::collections::BTreeMap<String, [f32; 4]>,
}

impl From<&ScreenShader> for ScreenShaderDoc {
    fn from(s: &ScreenShader) -> Self {
        Self { shader: s.shader.clone(), enabled: s.enabled, params: s.params.clone() }
    }
}

impl ScreenShaderDoc {
    pub fn to_screen_shader(&self) -> ScreenShader {
        ScreenShader {
            shader: self.shader.clone(),
            enabled: self.enabled,
            params: self.params.clone(),
        }
    }
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

fn all_layers() -> u32 {
    u32::MAX
}
fn is_all_layers(m: &u32) -> bool {
    *m == u32::MAX
}
fn default_fov() -> f32 {
    60f32.to_radians()
}
fn default_target_w() -> u32 {
    Matter::TARGET_W
}
fn default_target_h() -> u32 {
    Matter::TARGET_H
}
fn is_default_target_w(v: &u32) -> bool {
    *v == Matter::TARGET_W
}
fn is_default_target_h(v: &u32) -> bool {
    *v == Matter::TARGET_H
}
fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}
fn is_false(v: &bool) -> bool {
    !*v
}
fn default_ortho_height() -> f32 {
    Matter::ORTHO_HEIGHT
}
fn is_default_ortho_height(v: &f32) -> bool {
    *v == Matter::ORTHO_HEIGHT
}

/// A light's emitting shape, as it appears in a scene file.
///
/// Sizes are clamped on the way IN rather than trusted: a hand-typed
/// `Rect(width: 0, height: 0)` is a degenerate emitter whose polygon integral is
/// a divide by zero, and "the scene file said so" is not a reason to hand that
/// to the shader.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
pub enum LightShapeDoc {
    #[default]
    Point,
    Sphere {
        radius: f32,
    },
    Rect {
        width: f32,
        height: f32,
        #[serde(default)]
        two_sided: bool,
    },
    Disk {
        radius: f32,
        #[serde(default)]
        two_sided: bool,
    },
    Tube {
        length: f32,
        radius: f32,
    },
}

/// The floor every authored emitter dimension is held to (1 mm). Small enough to
/// be indistinguishable from a point, large enough that nothing downstream
/// divides by it.
const MIN_EMITTER: f32 = 0.001;

fn is_point_shape(s: &LightShapeDoc) -> bool {
    matches!(s, LightShapeDoc::Point)
}

/// A lamp's cone, when it has one.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpotDoc {
    /// The FULL cone angle in degrees, down the node's local −Z.
    pub angle: f32,
    /// How much of the cone's edge is falloff, 0–1. Defaulted so a hand-written
    /// scene may say only the angle, which is the number somebody actually has
    /// in mind.
    #[serde(default = "default_spot_softness")]
    pub softness: f32,
}

fn default_spot_softness() -> f32 {
    0.25
}

impl LightShapeDoc {
    pub fn to_shape(self) -> floptle_core::LightShape {
        use floptle_core::LightShape as S;
        match self {
            LightShapeDoc::Point => S::Point,
            LightShapeDoc::Sphere { radius } => S::Sphere { radius: radius.max(MIN_EMITTER) },
            LightShapeDoc::Rect { width, height, two_sided } => S::Rect {
                width: width.max(MIN_EMITTER),
                height: height.max(MIN_EMITTER),
                two_sided,
            },
            LightShapeDoc::Disk { radius, two_sided } => {
                S::Disk { radius: radius.max(MIN_EMITTER), two_sided }
            }
            LightShapeDoc::Tube { length, radius } => S::Tube {
                length: length.max(MIN_EMITTER),
                radius: radius.max(MIN_EMITTER),
            },
        }
    }
}

impl From<floptle_core::LightShape> for LightShapeDoc {
    fn from(s: floptle_core::LightShape) -> Self {
        use floptle_core::LightShape as S;
        match s {
            S::Point => LightShapeDoc::Point,
            S::Sphere { radius } => LightShapeDoc::Sphere { radius },
            S::Rect { width, height, two_sided } => {
                LightShapeDoc::Rect { width, height, two_sided }
            }
            S::Disk { radius, two_sided } => LightShapeDoc::Disk { radius, two_sided },
            S::Tube { length, radius } => LightShapeDoc::Tube { length, radius },
        }
    }
}

fn default_range() -> f32 {
    10.0
}

/// A room, not a level: 16 × 8 × 16 metres. Small enough that the first bake
/// finishes while you are still looking at it.
fn default_probe_half() -> [f32; 3] {
    [8.0, 4.0, 8.0]
}
fn default_probe_spacing() -> f32 {
    2.0
}
fn default_probe_quality() -> u32 {
    16
}
/// Twice a light probe volume's box — a navmesh covers a level rather than a
/// room, and this is only what it starts at before `auto_bounds` measures the
/// real thing.
fn default_nav_half() -> [f32; 3] {
    [16.0, 8.0, 16.0]
}
/// Unity's default character, in Unity's four numbers. A level baked there and
/// baked here should come out the same shape, and a designer arriving from
/// there should not have to learn a new vocabulary to get it.
fn default_agent_radius() -> f32 {
    0.5
}
fn default_agent_height() -> f32 {
    2.0
}
fn default_max_slope() -> f32 {
    45.0
}
fn default_step_height() -> f32 {
    0.75
}
/// A third of the radius, which is Unity's rule and comfortably inside what the
/// baker asks for — erosion works in whole cells, so a cell that is coarse next
/// to the radius closes narrow gaps without saying so.
fn default_nav_cell() -> f32 {
    0.15
}
/// Two metres forward: far enough that a link written by hand is visible where
/// it was put rather than a dot at the origin.
fn default_link_to() -> [f32; 3] {
    [0.0, 0.0, 2.0]
}
fn default_link_cost() -> f32 {
    2.0
}
fn default_nav_area_half() -> [f32; 3] {
    [4.0, 2.0, 4.0]
}
fn default_nav_area_name() -> String {
    "rough".into()
}
fn default_nav_area_cost() -> f32 {
    4.0
}
/// Two metres of crossover at a doorway: enough that walking out of a room does
/// not switch environments in a single step, small enough that a probe does not
/// quietly speak for the corridor outside it.
fn default_probe_fade() -> f32 {
    2.0
}
fn default_probe_normal_bias() -> f32 {
    0.5
}
fn one_u32() -> u32 {
    1
}

/// A tank you could stand in — 10 m across, 4 m deep.
fn default_pool_half() -> [f32; 3] {
    [5.0, 2.0, 5.0]
}

/// Fresh water, kg/m³. The one number that decides whether a given hull floats,
/// so the default is the one everybody's intuition is calibrated to.
fn default_water_density() -> f32 {
    1000.0
}

/// A green-blue you can see through — deliberately not "ocean blue", which
/// reads as a filter rather than as water.
fn default_water_tint() -> [f32; 3] {
    [0.10, 0.32, 0.38]
}

/// Metres of underwater visibility. Short enough to feel like water, long
/// enough to swim in.
fn default_water_visibility() -> f32 {
    28.0
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum ShapeDoc {
    Cube,
    Sphere,
    Capsule,
    Plane,
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
            Matter::MapMesh { id } => MatterDoc::MapMesh { id: *id, geo: None },
            Matter::Camera {
                fov_y,
                active,
                target,
                cull_mask,
                target_w,
                target_h,
                target_hz,
                ortho,
                ortho_height,
            } => MatterDoc::Camera {
                fov_y: *fov_y,
                active: *active,
                target: target.clone(),
                cull_mask: *cull_mask,
                target_w: *target_w,
                target_h: *target_h,
                target_hz: *target_hz,
                ortho: *ortho,
                ortho_height: *ortho_height,
            },
            Matter::PointLight { color, intensity, range, shape, shadows, spot_angle, spot_softness } => {
                MatterDoc::PointLight {
                    color: *color,
                    intensity: *intensity,
                    range: *range,
                    shape: LightShapeDoc::from(*shape),
                    shadows: *shadows,
                    // Written only when the lamp is actually aimed, which is
                    // what keeps every scene authored before spots existed
                    // byte-identical through a load and a save.
                    spot: floptle_core::is_spot(*spot_angle)
                        .then_some(SpotDoc { angle: *spot_angle, softness: *spot_softness }),
                }
            }
            Matter::GravityVolume { mode, strength, radius } => MatterDoc::GravityVolume {
                radial: *mode == GravityMode::Radial,
                strength: *strength,
                radius: *radius,
            },
            Matter::WaterVolume {
                kind,
                radius,
                half_extents,
                density,
                drag,
                angular_drag,
                frozen,
                tint,
                visibility,
            } => MatterDoc::WaterVolume {
                pool: *kind == floptle_core::WaterKind::Pool,
                radius: *radius,
                half_extents: *half_extents,
                density: *density,
                drag: *drag,
                angular_drag: *angular_drag,
                frozen: *frozen,
                tint: *tint,
                visibility: *visibility,
            },
            Matter::LightProbes {
                half_extents,
                spacing,
                enabled,
                intensity,
                bounces,
                quality,
                leak,
                normal_bias,
                exclude_layers,
            } => MatterDoc::LightProbes {
                half_extents: *half_extents,
                spacing: *spacing,
                enabled: *enabled,
                intensity: *intensity,
                bounces: *bounces,
                quality: *quality,
                leak: *leak,
                normal_bias: *normal_bias,
                exclude_layers: exclude_layers.clone(),
            },
            Matter::NavMesh {
                id,
                half_extents,
                auto_bounds,
                layers,
                agent_radius,
                agent_height,
                max_slope,
                step_height,
                cell_size,
                enabled,
                auto_rebake,
            } => MatterDoc::NavMesh {
                id: *id,
                half_extents: *half_extents,
                auto_bounds: *auto_bounds,
                layers: layers.clone(),
                agent_radius: *agent_radius,
                agent_height: *agent_height,
                max_slope: *max_slope,
                step_height: *step_height,
                cell_size: *cell_size,
                enabled: *enabled,
                auto_rebake: *auto_rebake,
            },
            Matter::NavLink { id, to, bidirectional, cost, area, duration, enabled } => {
                MatterDoc::NavLink {
                    id: *id,
                    to: *to,
                    bidirectional: *bidirectional,
                    cost: *cost,
                    area: area.clone(),
                    duration: *duration,
                    enabled: *enabled,
                }
            }
            Matter::NavArea { half_extents, area, cost, blocks, enabled } => MatterDoc::NavArea {
                half_extents: *half_extents,
                area: area.clone(),
                cost: *cost,
                blocks: *blocks,
                enabled: *enabled,
            },
            Matter::ReflectionProbe { half_extents, enabled, intensity, fade } => {
                MatterDoc::ReflectionProbe {
                    half_extents: *half_extents,
                    enabled: *enabled,
                    intensity: *intensity,
                    fade: *fade,
                }
            }
            Matter::FieldShape { radius } => MatterDoc::FieldShape { radius: *radius },
            Matter::Tilemap { cols, rows, tile, data, tileset } => MatterDoc::Tilemap {
                cols: *cols,
                rows: *rows,
                tile: *tile,
                data: data.clone(),
                tileset: tileset.clone(),
            },
            Matter::SpriteBatch { size } => MatterDoc::SpriteBatch { size: *size },
            Matter::Sprite { ppu, size, cell, flip_x, flip_y, pivot } => MatterDoc::Sprite {
                ppu: *ppu,
                size: *size,
                cell: *cell,
                flip_x: *flip_x,
                flip_y: *flip_y,
                pivot: *pivot,
            },
            Matter::Skybox { color, size, texture, tint, shader, shader_params } => {
                MatterDoc::Skybox {
                    color: *color,
                    size: *size,
                    texture: texture.clone(),
                    tint: *tint,
                    shader: shader.clone(),
                    shader_params: shader_params.clone(),
                }
            }
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
                posterize_chroma,
                tonemap,
                exposure,
                contrast,
                saturation,
                temperature,
                tint,
                lift,
                grade_gamma,
                gain,
                aberration,
                distortion,
                sharpen,
                denoise,
                grain,
                grain_size,
                dof_focus,
                dof_range,
                dof_near_range,
                dof_max_blur,
                dof_blades,
                dof_blade_rotation,
                dof_highlight,
                dof_quality,
                motion_blur,
                motion_samples,
                dof_show_focus,
                dof_focus_node,
                screen_shaders,
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
                posterize_chroma: *posterize_chroma,
                tonemap: *tonemap,
                exposure: *exposure,
                contrast: *contrast,
                saturation: *saturation,
                temperature: *temperature,
                tint: *tint,
                lift: *lift,
                grade_gamma: *grade_gamma,
                gain: *gain,
                aberration: *aberration,
                distortion: *distortion,
                sharpen: *sharpen,
                denoise: *denoise,
                grain: *grain,
                grain_size: *grain_size,
                dof_focus: *dof_focus,
                dof_range: *dof_range,
                dof_max_blur: *dof_max_blur,
                dof_near_range: *dof_near_range,
                dof_blades: *dof_blades,
                dof_blade_rotation: *dof_blade_rotation,
                dof_highlight: *dof_highlight,
                dof_quality: *dof_quality,
                motion_blur: *motion_blur,
                motion_samples: *motion_samples,
                dof_show_focus: *dof_show_focus,
                dof_focus_node: dof_focus_node.clone(),
                screen_shaders: screen_shaders.iter().map(ScreenShaderDoc::from).collect(),
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
            MatterDoc::MapMesh { id, .. } => Matter::MapMesh { id: *id },
            MatterDoc::Camera {
                fov_y,
                active,
                target,
                cull_mask,
                target_w,
                target_h,
                target_hz,
                ortho,
                ortho_height,
            } => {
                let (w, h) = Matter::clamp_target_size(*target_w, *target_h);
                Matter::Camera {
                    fov_y: *fov_y,
                    active: *active,
                    target: target.clone(),
                    cull_mask: *cull_mask,
                    target_w: w,
                    target_h: h,
                    target_hz: target_hz.max(0.0),
                    ortho: *ortho,
                    // Clamped on the way IN, so a hand-edited `.ron` with a zero
                    // height cannot hand a singular projection matrix to the
                    // renderer — every ray through its inverse would be NaN.
                    ortho_height: Matter::clamp_ortho_height(*ortho_height),
                }
            }
            MatterDoc::PointLight { color, intensity, range, shape, shadows, spot } => Matter::PointLight {
                spot_angle: spot.map(|s| s.angle).unwrap_or(floptle_core::OMNI_ANGLE),
                spot_softness: spot.map(|s| s.softness).unwrap_or_else(default_spot_softness),
                color: *color,
                intensity: *intensity,
                range: *range,
                shape: shape.to_shape(),
                shadows: *shadows,
            },
            MatterDoc::GravityVolume { radial, strength, radius } => Matter::GravityVolume {
                mode: if *radial { GravityMode::Radial } else { GravityMode::Down },
                strength: *strength,
                radius: *radius,
            },
            MatterDoc::WaterVolume {
                pool,
                radius,
                half_extents,
                density,
                drag,
                angular_drag,
                frozen,
                tint,
                visibility,
            } => Matter::WaterVolume {
                kind: if *pool {
                    floptle_core::WaterKind::Pool
                } else {
                    floptle_core::WaterKind::Sea
                },
                radius: *radius,
                half_extents: *half_extents,
                density: *density,
                drag: *drag,
                angular_drag: *angular_drag,
                frozen: *frozen,
                tint: *tint,
                visibility: *visibility,
            },
            MatterDoc::LightProbes {
                half_extents,
                spacing,
                enabled,
                intensity,
                bounces,
                quality,
                leak,
                normal_bias,
                exclude_layers,
            } => Matter::LightProbes {
                half_extents: *half_extents,
                spacing: *spacing,
                enabled: *enabled,
                intensity: *intensity,
                bounces: *bounces,
                quality: *quality,
                leak: *leak,
                normal_bias: *normal_bias,
                exclude_layers: exclude_layers.clone(),
            },
            MatterDoc::NavMesh {
                id,
                half_extents,
                auto_bounds,
                layers,
                agent_radius,
                agent_height,
                max_slope,
                step_height,
                cell_size,
                enabled,
                auto_rebake,
            } => Matter::NavMesh {
                id: *id,
                half_extents: *half_extents,
                auto_bounds: *auto_bounds,
                layers: layers.clone(),
                // Clamped here rather than trusted, because these are hand-editable
                // and every one of them has a value that makes the bake do nothing
                // quietly. A zero cell size is an infinite grid; a zero height means
                // no surface ever has headroom; a negative radius erodes inward.
                agent_radius: agent_radius.max(0.0),
                agent_height: agent_height.max(0.01),
                max_slope: max_slope.clamp(0.0, 89.9),
                step_height: step_height.max(0.0),
                cell_size: cell_size.max(0.01),
                enabled: *enabled,
                auto_rebake: *auto_rebake,
            },
            MatterDoc::NavLink { id, to, bidirectional, cost, area, duration, enabled } => {
                Matter::NavLink {
                    id: *id,
                    to: *to,
                    bidirectional: *bidirectional,
                    // A negative cost would make the router prefer going round in
                    // circles, which is a level that hangs rather than one that
                    // looks wrong.
                    cost: cost.max(0.0),
                    area: area.clone(),
                    duration: duration.max(0.0),
                    enabled: *enabled,
                }
            }
            MatterDoc::NavArea { half_extents, area, cost, blocks, enabled } => Matter::NavArea {
                half_extents: *half_extents,
                area: area.clone(),
                cost: cost.max(0.0),
                blocks: *blocks,
                enabled: *enabled,
            },
            MatterDoc::ReflectionProbe { half_extents, enabled, intensity, fade } => {
                Matter::ReflectionProbe {
                    half_extents: *half_extents,
                    enabled: *enabled,
                    intensity: *intensity,
                    // A zero box would cover nothing and read as a probe that
                    // does not work; a negative one would invert the slab test.
                    fade: fade.clamp(0.0, 1e4),
                }
            }
            MatterDoc::FieldShape { radius } => Matter::FieldShape { radius: *radius },
            MatterDoc::Tilemap { cols, rows, tile, data, tileset } => Matter::Tilemap {
                cols: *cols,
                rows: *rows,
                tile: *tile,
                data: data.clone(),
                tileset: tileset.clone(),
            },
            MatterDoc::SpriteBatch { size } => Matter::SpriteBatch { size: *size },
            MatterDoc::Sprite { ppu, size, cell, flip_x, flip_y, pivot } => Matter::Sprite {
                ppu: *ppu,
                size: *size,
                cell: *cell,
                flip_x: *flip_x,
                flip_y: *flip_y,
                pivot: *pivot,
            },
            MatterDoc::Skybox { color, size, texture, tint, shader, shader_params } => {
                Matter::Skybox {
                    color: *color,
                    size: *size,
                    texture: texture.clone(),
                    tint: *tint,
                    shader: shader.clone(),
                    shader_params: shader_params.clone(),
                }
            }
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
                posterize_chroma,
                tonemap,
                exposure,
                contrast,
                saturation,
                temperature,
                tint,
                lift,
                grade_gamma,
                gain,
                aberration,
                distortion,
                sharpen,
                denoise,
                grain,
                grain_size,
                dof_focus,
                dof_range,
                dof_near_range,
                dof_max_blur,
                dof_blades,
                dof_blade_rotation,
                dof_highlight,
                dof_quality,
                motion_blur,
                motion_samples,
                dof_show_focus,
                dof_focus_node,
                screen_shaders,
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
                posterize_chroma: *posterize_chroma,
                tonemap: *tonemap,
                exposure: *exposure,
                contrast: *contrast,
                saturation: *saturation,
                temperature: *temperature,
                tint: *tint,
                lift: *lift,
                grade_gamma: *grade_gamma,
                gain: *gain,
                aberration: *aberration,
                distortion: *distortion,
                sharpen: *sharpen,
                denoise: *denoise,
                grain: *grain,
                grain_size: *grain_size,
                dof_focus: *dof_focus,
                dof_range: *dof_range,
                dof_max_blur: *dof_max_blur,
                dof_near_range: *dof_near_range,
                dof_blades: *dof_blades,
                dof_blade_rotation: *dof_blade_rotation,
                dof_highlight: *dof_highlight,
                dof_quality: *dof_quality,
                motion_blur: *motion_blur,
                motion_samples: *motion_samples,
                dof_show_focus: *dof_show_focus,
                dof_focus_node: dof_focus_node.clone(),
                screen_shaders: screen_shaders
                    .iter()
                    .map(ScreenShaderDoc::to_screen_shader)
                    .collect(),
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
            Shape::Plane => ShapeDoc::Plane,
        }
    }
}
impl From<ShapeDoc> for Shape {
    fn from(s: ShapeDoc) -> Self {
        match s {
            ShapeDoc::Cube => Shape::Cube,
            ShapeDoc::Sphere => Shape::Sphere,
            ShapeDoc::Capsule => Shape::Capsule,
            ShapeDoc::Plane => Shape::Plane,
        }
    }
}

/// Serializable lighting for the scene's mandatory Lighting node, mirroring
/// [`floptle_core::Light`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct LightDoc {
    #[serde(default = "default_light_direction")]
    pub direction: [f32; 3],
    /// Stars mode: the directional light turns off and celestial bodies with
    /// `luminosity > 0` become the key lights (radial terminators + shadows,
    /// genuinely dark far sides, multiple stars). Pre-star scenes → off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stars: bool,
    #[serde(default = "white3")]
    pub color: [f32; 3],
    #[serde(default = "default_light_ambient")]
    pub ambient: [f32; 3],
    /// The base light every 2D surface gets. WHITE by default, so a scene
    /// written before 2D lighting existed — and a scene that never turns it
    /// down — looks exactly as it always did.
    #[serde(default = "white3", skip_serializing_if = "is_white3")]
    pub ambient_2d: [f32; 3],
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
    /// Contact shadows default OFF: they cost a screen-space trace per lit
    /// fragment, and a scene that never asked for them should not start paying.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub contact_shadows: bool,
    #[serde(default = "default_contact_length")]
    pub contact_length: f32,
    #[serde(default = "default_contact_steps")]
    pub contact_steps: u32,
    #[serde(default = "default_contact_strength")]
    pub contact_strength: f32,
    /// Screen-space reflections default OFF, on the same principle contact
    /// shadows do: they cost a march per reflective pixel and a copy of the
    /// frame, so an existing scene must load doing exactly what it did.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reflections: bool,
    #[serde(default = "default_reflection_distance")]
    pub reflection_distance: f32,
    #[serde(default = "default_reflection_steps")]
    pub reflection_steps: u32,
    #[serde(default = "default_reflection_thickness")]
    pub reflection_thickness: f32,
    #[serde(default = "default_reflection_clamp")]
    pub reflection_clamp: f32,
    #[serde(default = "default_refraction_layers")]
    pub refraction_layers: u32,
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
    /// Volumetric mode: a height-bounded, noise-broken fog layer marched per
    /// pixel instead of the flat distance ramp. Pre-volumetric scenes → off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fog_volumetric: bool,
    #[serde(default = "default_fog_density")]
    pub fog_density: f32,
    #[serde(default = "default_fog_height")]
    pub fog_height: f32,
    #[serde(default = "default_fog_falloff")]
    pub fog_falloff: f32,
    #[serde(default = "default_fog_noise")]
    pub fog_noise: f32,
    #[serde(default = "default_fog_noise_scale")]
    pub fog_noise_scale: f32,
    /// Volumetric light injection. These default to lit rather than to the old
    /// flat look on purpose: a fog layer that ignores the sun standing behind it
    /// is the thing that made volumetric mode read as a grey wash, and a scene
    /// saved before this existed wants the fix, not a preserved bug. Set
    /// `fog_light: 0` to pin the previous appearance exactly.
    #[serde(default = "default_fog_light")]
    pub fog_light: f32,
    #[serde(default = "default_fog_anisotropy")]
    pub fog_anisotropy: f32,
    #[serde(default = "default_fog_steps")]
    pub fog_steps: u32,
    #[serde(default = "true_bool")]
    pub fog_shafts: bool,
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
fn default_fog_density() -> f32 {
    0.02
}
fn default_fog_height() -> f32 {
    6.0
}
fn default_fog_falloff() -> f32 {
    8.0
}
fn default_fog_noise() -> f32 {
    0.5
}
fn default_fog_noise_scale() -> f32 {
    24.0
}
fn default_contact_length() -> f32 {
    0.35
}
fn default_contact_steps() -> u32 {
    12
}
fn default_contact_strength() -> f32 {
    0.9
}
fn default_reflection_distance() -> f32 {
    30.0
}
fn default_reflection_steps() -> u32 {
    32
}
fn default_reflection_thickness() -> f32 {
    0.5
}
fn default_reflection_clamp() -> f32 {
    8.0
}
fn default_refraction_layers() -> u32 {
    2
}
fn default_fog_light() -> f32 {
    1.0
}
fn default_fog_anisotropy() -> f32 {
    0.6
}
fn default_fog_steps() -> u32 {
    16
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
            stars: l.stars,
            color: l.color,
            ambient: l.ambient,
            ambient_2d: l.ambient_2d,
            intensity: l.intensity,
            shadows: l.shadows,
            shadow_softness: l.shadow_softness,
            shadow_strength: l.shadow_strength,
            shadow_tint: l.shadow_tint,
            shadow_quantize: l.shadow_quantize,
            shadow_dither: l.shadow_dither,
            shadow_distance: l.shadow_distance,
            contact_shadows: l.contact_shadows,
            contact_length: l.contact_length,
            contact_steps: l.contact_steps,
            contact_strength: l.contact_strength,
            reflections: l.reflections,
            reflection_distance: l.reflection_distance,
            reflection_steps: l.reflection_steps,
            reflection_thickness: l.reflection_thickness,
            reflection_clamp: l.reflection_clamp,
            refraction_layers: l.refraction_layers,
            fog: l.fog,
            fog_color: l.fog_color,
            fog_start: l.fog_start,
            fog_end: l.fog_end,
            fog_dither: l.fog_dither,
            fog_dither_strength: l.fog_dither_strength,
            fog_volumetric: l.fog_volumetric,
            fog_density: l.fog_density,
            fog_height: l.fog_height,
            fog_falloff: l.fog_falloff,
            fog_noise: l.fog_noise,
            fog_noise_scale: l.fog_noise_scale,
            fog_light: l.fog_light,
            fog_anisotropy: l.fog_anisotropy,
            fog_steps: l.fog_steps,
            fog_shafts: l.fog_shafts,
        }
    }
}

impl LightDoc {
    pub fn to_light(self) -> Light {
        Light {
            direction: self.direction,
            stars: self.stars,
            color: self.color,
            ambient: self.ambient,
            ambient_2d: self.ambient_2d,
            intensity: self.intensity,
            shadows: self.shadows,
            shadow_softness: self.shadow_softness,
            shadow_strength: self.shadow_strength,
            shadow_tint: self.shadow_tint,
            shadow_quantize: self.shadow_quantize,
            shadow_dither: self.shadow_dither,
            shadow_distance: self.shadow_distance,
            contact_shadows: self.contact_shadows,
            contact_length: self.contact_length.clamp(0.01, 20.0),
            contact_steps: self.contact_steps.clamp(2, 32),
            contact_strength: self.contact_strength.clamp(0.0, 1.0),
            reflections: self.reflections,
            // Fenced for the same reason the contact trace is: these come off
            // disk, and a hand-edited or hand-migrated scene must not be able to
            // ask for a zero-step march or a reach that walks the whole level.
            reflection_distance: self.reflection_distance.clamp(0.1, 500.0),
            reflection_steps: self.reflection_steps.clamp(8, 64),
            reflection_thickness: self.reflection_thickness.clamp(0.01, 20.0),
            // 0 is meaningful — it means no ceiling — so the floor is 0 and not
            // some small positive number that would quietly darken every mirror.
            reflection_clamp: self.reflection_clamp.clamp(0.0, 10_000.0),
            refraction_layers: self
                .refraction_layers
                .clamp(1, floptle_core::Light::MAX_REFRACTION_LAYERS),
            fog: self.fog,
            fog_color: self.fog_color,
            fog_start: self.fog_start,
            fog_end: self.fog_end,
            fog_dither: self.fog_dither,
            fog_dither_strength: self.fog_dither_strength,
            fog_volumetric: self.fog_volumetric,
            fog_density: self.fog_density,
            fog_height: self.fog_height,
            fog_falloff: self.fog_falloff,
            fog_noise: self.fog_noise,
            fog_noise_scale: self.fog_noise_scale,
            fog_light: self.fog_light,
            fog_anisotropy: self.fog_anisotropy,
            fog_steps: self.fog_steps.clamp(2, 64),
            fog_shafts: self.fog_shafts,
        }
    }
}

/// Project-wide render settings — the PS1/PS2-style knobs that apply to every
/// scene. Saved to `project.ron`, edited in the editor's Project Settings.
///
/// Post-processing moved to the per-scene `PostProcess` node ([`MatterDoc::PostProcess`]);
/// the `bloom`/`vignette` fields below are **legacy** — still read so an old
/// `project.ron`'s look can be migrated onto a scene's node, but never written back.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProjectConfigDoc {
    #[serde(default = "true_bool")]
    pub retro: bool,
    #[serde(default = "default_retro_height")]
    pub retro_height: u32,
    /// Fixed internal WIDTH for the retro target, in pixels. `0` = derive it
    /// from the window's aspect, which is the original behaviour.
    ///
    /// Deriving the width means the amount of world on screen changes with the
    /// window: a 2.0-aspect panel shows 12% more than 16:9, which for a game
    /// that has been balanced is a difficulty setting nobody chose. Pinning it
    /// makes the framing the same everywhere, and the leftover becomes bars
    /// rather than extra world.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub retro_width: u32,
    /// Upscale the retro composite by a WHOLE number and centre it, letterboxing
    /// the remainder, instead of stretching it to fill.
    ///
    /// A fractional upscale puts some source rows on two screen pixels and some
    /// on three; on pixel art with a small font that is the difference between
    /// crisp and mush, and it changes with every window size. Off by default —
    /// stretching is what every existing project is drawn against.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retro_integer_scale: bool,

    // --- The era's ARTEFACTS, project-wide (v0.51). --------------------------
    // The four knobs that were only ever per-material. A game whose whole look
    // is of that era had to set them on every material it owned and on every
    // material it imported next week; these say it once. All default to off, so
    // an existing project.ron loads to exactly the look it has now, and each
    // one folds into a material through `floptle_core::Retro::under` — which is
    // where the precedence rule lives.
    //
    // They reach RASTER MESHES: primitives, models, tilemaps, map geometry,
    // skinned characters. SDF matter and terrain are raymarched and have no
    // vertices to snap.
    /// Snap every surface's vertices to a screen grid of this many steps across
    /// the viewport — the PS1's integer vertex coordinates. `0` = off. A
    /// material with its own jitter keeps it; one marked exempt takes none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub retro_jitter: f32,
    /// Interpolate every surface's UVs without the perspective divide.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retro_affine_uv: bool,
    /// Light every surface per vertex instead of per pixel (Gouraud).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retro_vertex_lit: bool,
    /// Draw every partial opacity as screen-door dither instead of blending.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retro_dither_alpha: bool,

    /// How finished frames reach the display.
    ///
    /// `On` is classic vsync and the default. It is a setting because on some
    /// compositors vsync presents at a FRACTION of the refresh rate — a window
    /// doing nothing but clearing itself can sit at a flat 20 fps on a 60 Hz
    /// display — and with the mode hardcoded a project had no way to tell that
    /// apart from the engine being slow, let alone escape it.
    #[serde(default)]
    pub vsync: VsyncDoc,

    /// How much detail a reflection probe's capture keeps.
    ///
    /// A probe's picture spans a full turn across its width, so its width IS the
    /// finest thing a mirror in that room can show. Too little and no roughness
    /// setting recovers it — the surface reads as frosted however it is
    /// authored. The cost is paid at capture, not per frame.
    #[serde(default)]
    pub probe_detail: ProbeDetailDoc,

    #[serde(default = "true_bool")]
    pub matter: bool,
    /// The game's title: names exported builds (their binary + window title).
    /// `None` = untitled (exports fall back to the project folder's name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The scene a BUILD boots into, as a project-root-relative path
    /// (e.g. `scenes/first.ron`). The editor opens it on project load too, so
    /// what you see is what ships. `None` = the `scenes/first.ron` convention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_scene: Option<String>,
    /// The engine version this project targets — written by the Hub / `--new`, read by
    /// the Hub to launch the matching install. Advisory (the editor doesn't enforce it);
    /// `None` on projects created before the Hub existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    /// The project's collision/query **layers**, by name (up to 32; "Default"
    /// is implicit and always index 0 — it need not be listed). Nodes reference
    /// these by name ([`NodeDoc::layer`]); Project Settings edits them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<String>,
    /// Collision-matrix EXCEPTIONS: pairs of layer names that DON'T collide
    /// (everything collides by default, so this stays tiny and readable).
    /// Pairs naming a since-removed layer are ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub no_collide: Vec<(String, String)>,
    /// The project's **sorting layers**, back to front, by name. "Default" is
    /// implicit and always first; it need not be listed.
    ///
    /// Separate from `layers` on purpose: collision layers answer "does this hit
    /// that" and sorting layers answer "which draws in front". A scene routinely
    /// wants a Background that collides with nothing and a Player that does,
    /// while both sort independently of either fact — folding them together
    /// would mean every new sort order invents a physics layer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sorting_layers: Vec<String>,
    /// The project's **UI font**: a project-relative `.ttf`/`.otf` path used
    /// wherever no font is named — `draw.text`, a `ui.make` label, an element
    /// whose style sets none. Empty (the default) means the embedded Roboto.
    ///
    /// It exists because project fonts *append* to the renderer's font stack,
    /// so slot 0 could never be the project's, so **every** string that did not
    /// spell out a path came out in Roboto. For a game whose UI is a pixel font
    /// that is all of them, and the symptom is not "wrong typeface" — it is
    /// text that reads as badly spaced, because a layout built on a monospace
    /// grid is being drawn with a proportional font: wide letters overlap their
    /// neighbours and narrow ones leave holes (`floptle/0124`).
    ///
    /// Naming it here rather than at each call site is the point: it fixes the
    /// code nobody is going to edit.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ui_font: String,
    /// The project-wide audio mixer graph (tracks, effects, routing). Edited
    /// in the Mixer tab; every scene plays through it.
    #[serde(default)]
    pub mixer: floptle_audio::MixerDesc,
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
            retro_width: 0,
            retro_integer_scale: false,
            retro_jitter: 0.0,
            retro_affine_uv: false,
            retro_vertex_lit: false,
            retro_dither_alpha: false,
            vsync: VsyncDoc::default(),
            probe_detail: ProbeDetailDoc::default(),
            matter: true,
            title: None,
            entry_scene: None,
            engine_version: None,
            layers: Vec::new(),
            no_collide: Vec::new(),
            sorting_layers: Vec::new(),
            ui_font: String::new(),
            mixer: floptle_audio::MixerDesc::default(),
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

    /// The jitter grid that lands one cell on one **pixel row** of this
    /// project's own retro target — the era's actual behaviour, and the
    /// subtlest setting that is still visible.
    ///
    /// Derived rather than a fixed number, because the right value is not a
    /// matter of taste: hardware with no fractional vertex coordinates snapped
    /// to ITS pixels, so the grid that reads as authentic depends entirely on
    /// how many pixels this project renders. A 240-row game and a 480-row game
    /// want different numbers for the same look, and asking somebody to work
    /// that out from a slider labelled 0–512 is asking them to guess.
    ///
    /// Halved because the shader's steps are counted across normalised device
    /// coordinates, which span 2 — so `height / 2` steps is `height` cells.
    ///
    /// Keyed on the HEIGHT and not the width: the width often follows the
    /// window ([`retro_width`](Self::retro_width) = 0), and a look that changed
    /// when somebody resized the window would be the same complaint in a
    /// different place. The cells are then square in NDC, so at a wide aspect
    /// they are a little wider than one pixel — which is the correct trade for
    /// a number that holds still.
    pub fn retro_jitter_pixels(&self) -> f32 {
        (self.retro_height.max(80) as f32 * 0.5).round()
    }

    /// The named strengths offered in Project Settings, coarsest cell last.
    /// Each is a whole multiple of the pixel grid, so they stay in step with
    /// each other and with the project's resolution.
    ///
    /// There is nothing FINER than pixel-exact on offer, because there is
    /// nothing to see there: a grid finer than the pixels it is drawn on snaps
    /// vertices to positions the frame cannot tell apart.
    pub fn retro_jitter_presets(&self) -> [(&'static str, f32, &'static str); 4] {
        let px = self.retro_jitter_pixels();
        [
            ("off", 0.0, "no snapping at all"),
            ("pixels", px, "one cell per pixel row — what the hardware actually did, and the \
                            subtlest setting that still shows"),
            ("chunky", (px * 0.5).round(), "cells twice the size of a pixel — the look, turned up"),
            ("heavy", (px * 0.25).round(), "four pixels to a cell. Geometry visibly swims; a \
                                            whole game at this is a lot"),
        ]
    }

    /// The project-wide artefacts as a [`Retro`](floptle_core::Retro), ready to
    /// fold into a material with [`Retro::under`](floptle_core::Retro::under).
    ///
    /// `exempt` is meaningless at this level — the project has nothing to be
    /// exempt from — so it is always `false` here.
    pub fn retro_artefacts(&self) -> floptle_core::Retro {
        floptle_core::Retro {
            jitter: self.retro_jitter.max(0.0),
            affine_uv: self.retro_affine_uv,
            vertex_lit: self.retro_vertex_lit,
            dither_alpha: self.retro_dither_alpha,
            exempt: false,
        }
    }

    /// The retro target's internal size for a view of `aspect` (width / height).
    ///
    /// One answer for every place that sizes one — the window, a docked Game
    /// tab, an exported build — so a project cannot look one way in the editor
    /// and another in a build. With `retro_width` set the size is FIXED and the
    /// aspect is ignored: that is the whole point of setting it.
    pub fn retro_size(&self, aspect: f32) -> (u32, u32) {
        let h = self.retro_height.max(80);
        let w = match self.retro_width {
            0 => ((h as f32 * aspect.max(0.05)).round() as u32).max(1),
            fixed => fixed,
        };
        (w, h)
    }

    /// The aspect the CAMERA must project at, given the aspect of the panel or
    /// window the frame ends up on.
    ///
    /// **The projection follows the target the scene composites into, not the
    /// surface it is eventually shown on.** With a pinned `retro_width` those are
    /// two different shapes: the scene is rendered into a fixed
    /// `retro_width × retro_height` target and then blitted, letterboxed, into
    /// whatever the panel is. Projecting at the panel's aspect and rendering into
    /// the target's squashes the picture horizontally — and it defeats the entire
    /// reason for pinning a width, which is that the framing stops depending on
    /// the window.
    ///
    /// Unpinned, the target is derived FROM the panel aspect, so this is the
    /// panel aspect and deliberately not a re-derivation of it: rounding the
    /// width to whole pixels and dividing back would move the projection by a
    /// fraction of a percent for no reason.
    pub fn render_aspect(&self, panel_aspect: f32) -> f32 {
        if self.retro && self.retro_width > 0 {
            let (w, h) = self.retro_size(panel_aspect);
            w.max(1) as f32 / h.max(1) as f32
        } else {
            panel_aspect
        }
    }

    /// Resolve this project's named layers + no-collide exceptions into the
    /// runtime table physics and scripts filter with (Default pinned at bit 0).
    pub fn build_layers(&self) -> floptle_core::Layers {
        floptle_core::Layers::resolve(self.layers.clone(), &self.no_collide)
    }

    /// The sorting layers back to front, with "Default" pinned first and blanks
    /// and duplicates dropped — the same normalisation collision layers get, for
    /// the same reason: the list is user-edited text.
    pub fn sorting_order(&self) -> Vec<String> {
        let mut names = self.sorting_layers.clone();
        names.retain(|n| !n.trim().is_empty() && n != floptle_core::DEFAULT_SORTING_LAYER);
        names.insert(0, floptle_core::DEFAULT_SORTING_LAYER.to_string());
        let mut seen = std::collections::HashSet::new();
        names.retain(|n| seen.insert(n.clone()));
        names
    }

    /// Where a sorting layer NAME sits, back to front.
    ///
    /// An unknown name ranks last rather than at 0. A layer deleted from the
    /// project while scenes still name it should leave those nodes drawing in
    /// FRONT, where they are visible and obviously wrong, rather than silently
    /// sinking them behind the background where the bug is a mystery.
    pub fn sorting_rank(&self, layer: &str) -> u32 {
        let order = self.sorting_order();
        let name = if layer.trim().is_empty() { floptle_core::DEFAULT_SORTING_LAYER } else { layer };
        order.iter().position(|n| n == name).unwrap_or(order.len()) as u32
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
    /// Rollback is a THIRD replication mode, added as its own flag so every
    /// scene written before it existed still loads with the mode it had. A file
    /// with neither flag is Authority; one with only `predicted` is unchanged.
    #[test]
    fn replication_modes_round_trip_and_old_scenes_are_unaffected() {
        use floptle_core::ReplicationMode;
        for mode in [ReplicationMode::Authority, ReplicationMode::Predicted, ReplicationMode::Rollback] {
            let c = floptle_core::Replicated { mode, ..floptle_core::Replicated::default() };
            let doc = ReplicatedDoc::from_component(&c);
            let text = ron::to_string(&doc).unwrap();
            let back: ReplicatedDoc = ron::from_str(&text).unwrap();
            assert_eq!(back.to_component().mode, mode, "{mode:?} did not survive {text}");
        }
        // A doc written before `rollback` existed: no such field at all.
        let legacy: ReplicatedDoc = ron::from_str("(predicted:true,transform:true)").unwrap();
        assert_eq!(legacy.to_component().mode, ReplicationMode::Predicted);
        let plain: ReplicatedDoc = ron::from_str("(transform:true)").unwrap();
        assert_eq!(plain.to_component().mode, ReplicationMode::Authority);
        // Neither flag is written when it doesn't apply, so files stay clean.
        let auth = ReplicatedDoc::from_component(&floptle_core::Replicated::default());
        let text = ron::to_string(&auth).unwrap();
        assert!(!text.contains("rollback"), "{text}");
        assert!(!text.contains("predicted"), "{text}");
    }

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
    #[serde(default = "white3")]
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
    /// Does the scene's fog reach this surface? Skips at `true`, so a material
    /// that never turned it off writes byte-identical RON to a pre-v0.51 one.
    #[serde(default = "true_bool", skip_serializing_if = "is_true")]
    pub fog: bool,
    #[serde(default = "one_f32")]
    pub ambient: f32,
    #[serde(default = "one_f32")]
    pub alpha: f32,
    /// Custom `.flsl` shader path (ADR-0007) + its uniform overrides and
    /// texture-slot bindings. All default-empty so pre-shader files still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shader: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub shader_params: std::collections::BTreeMap<String, [f32; 4]>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub shader_textures: std::collections::BTreeMap<String, String>,
    /// The base texture's tiling block + per-shader-slot tiling (proposal §8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiling: Option<TilingDoc>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub shader_tiling: std::collections::BTreeMap<String, TilingDoc>,
    /// Spritesheet: the base texture is a `sheet_cols`×`sheet_rows` grid and
    /// `cell` (row-major) is what draws. All three skip-serialize at 0, so a
    /// non-sheet material's RON is byte-identical to a pre-sheet one.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub sheet_cols: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub sheet_rows: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub cell: u32,

    // --- The surface maps (v0.43). ------------------------------------------
    // Each skips at its neutral value, so a material that uses none of them
    // writes byte-identical RON to a pre-v0.43 one. That is not tidiness: it is
    // what lets an artist diff a scene file and see only what they changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normal_map: Option<String>,
    #[serde(default = "one_f32", skip_serializing_if = "is_one_f32")]
    pub normal_strength: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roughness_map: Option<String>,
    #[serde(default = "default_roughness", skip_serializing_if = "is_default_roughness")]
    pub roughness: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metallic_map: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub metallic: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ao_map: Option<String>,
    #[serde(default = "one_f32", skip_serializing_if = "is_one_f32")]
    pub occlusion_strength: f32,
    /// How much of the environment (the sky) this surface reflects. `1` is the
    /// physically honest amount and the default, so a file written before
    /// reflections existed loads as a surface that reflects its sky properly.
    #[serde(default = "one_f32", skip_serializing_if = "is_one_f32")]
    pub reflectivity: f32,
    /// Glass: how much light passes THROUGH. `0` is a solid surface, and a file
    /// written before glass existed says nothing and loads as one.
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub transmission: f32,
    /// Index of refraction. Only meaningful with `transmission`, so it is
    /// skipped at its default rather than written beside every material.
    #[serde(default = "default_ior", skip_serializing_if = "is_default_ior")]
    pub ior: f32,
    #[serde(default = "default_thickness", skip_serializing_if = "is_default_thickness")]
    pub thickness: f32,
    #[serde(default, skip_serializing_if = "ShadingDoc::is_classic")]
    pub shading: ShadingDoc,
    #[serde(default, skip_serializing_if = "RetroDoc::is_off")]
    pub retro: RetroDoc,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
fn is_zero(v: &f32) -> bool {
    *v == 0.0
}
fn default_roughness() -> f32 {
    Material::default().roughness
}
fn is_default_roughness(v: &f32) -> bool {
    *v == Material::default().roughness
}

/// RON mirror of [`floptle_core::Shading`]. Its own type rather than a bare
/// string so a typo in a hand-edited scene is a parse error naming the field,
/// not a silent fall back to the wrong lighting model.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShadingDoc {
    #[default]
    Classic,
    Physical,
}

impl ShadingDoc {
    fn is_classic(&self) -> bool {
        matches!(self, ShadingDoc::Classic)
    }
    pub fn to_shading(self) -> floptle_core::Shading {
        match self {
            ShadingDoc::Classic => floptle_core::Shading::Classic,
            ShadingDoc::Physical => floptle_core::Shading::Physical,
        }
    }
    pub fn from_shading(s: floptle_core::Shading) -> Self {
        match s {
            floptle_core::Shading::Classic => ShadingDoc::Classic,
            floptle_core::Shading::Physical => ShadingDoc::Physical,
        }
    }
}

/// RON mirror of [`floptle_core::Retro`] — the deliberate PS1/N64 artefacts.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct RetroDoc {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub jitter: f32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub affine_uv: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vertex_lit: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dither_alpha: bool,
    /// Take none of the project-wide artefacts (`ProjectConfigDoc::retro_*`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exempt: bool,
}

impl RetroDoc {
    fn is_off(&self) -> bool {
        *self == RetroDoc::default()
    }
    pub fn to_retro(self) -> floptle_core::Retro {
        floptle_core::Retro {
            jitter: self.jitter,
            affine_uv: self.affine_uv,
            vertex_lit: self.vertex_lit,
            dither_alpha: self.dither_alpha,
            exempt: self.exempt,
        }
    }
    pub fn from_retro(r: floptle_core::Retro) -> Self {
        Self {
            jitter: r.jitter,
            affine_uv: r.affine_uv,
            vertex_lit: r.vertex_lit,
            dither_alpha: r.dither_alpha,
            exempt: r.exempt,
        }
    }
}

/// RON mirror of [`floptle_core::Tiling`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum TilingDoc {
    Uv { count: [f32; 2], offset: [f32; 2], rotation: f32 },
    Triplanar { scale: f32, blend: f32 },
}

impl TilingDoc {
    pub fn to_tiling(self) -> floptle_core::Tiling {
        match self {
            TilingDoc::Uv { count, offset, rotation } => {
                floptle_core::Tiling::Uv { count, offset, rotation }
            }
            TilingDoc::Triplanar { scale, blend } => {
                floptle_core::Tiling::Triplanar { scale, blend }
            }
        }
    }
    pub fn from_tiling(t: floptle_core::Tiling) -> Self {
        match t {
            floptle_core::Tiling::Uv { count, offset, rotation } => {
                TilingDoc::Uv { count, offset, rotation }
            }
            floptle_core::Tiling::Triplanar { scale, blend } => {
                TilingDoc::Triplanar { scale, blend }
            }
        }
    }
}

/// Also the default 2D base light: adding a light to a 2D scene must only ever
/// make it brighter, never black it out.
fn white3() -> [f32; 3] {
    [1.0, 1.0, 1.0]
}

/// Zero, one, and "is this still the default" — the pair a `skip_serializing_if`
/// needs so a scene only writes the knobs it actually turned.
fn zero_f32() -> f32 {
    0.0
}
fn is_one_f32(v: &f32) -> bool {
    *v == 1.0
}
fn default_dof_range() -> f32 {
    5.0
}
fn is_default_dof_range(v: &f32) -> bool {
    *v == 5.0
}

fn is_white3(c: &[f32; 3]) -> bool {
    *c == [1.0, 1.0, 1.0]
}

// ---- defaults for fields that used to be MANDATORY --------------------------
//
// A scene file is authored data that outlives the code reading it: hand-edited,
// generated by a script, written by an older engine, merged by git. Every one of
// those produces a file with a field missing, and until now a handful of fields
// answered that by refusing to parse the WHOLE scene — `Unexpected missing field
// 'direction' in 'LightDoc'` for a `lighting: ()` line, and the double-click that
// should have opened the level did nothing at all, because the only report was an
// `eprintln!` to a terminal nobody has.
//
// These are the values `Default` already gives; naming them per field just means
// a missing line costs that line and nothing else.
fn identity_quat() -> [f32; 4] {
    [0.0, 0.0, 0.0, 1.0]
}

fn one3() -> [f32; 3] {
    [1.0, 1.0, 1.0]
}

fn default_light_direction() -> [f32; 3] {
    Light::default().direction
}

fn default_light_ambient() -> [f32; 3] {
    Light::default().ambient
}

fn default_retro_height() -> u32 {
    240
}

fn default_ior() -> f32 {
    1.5
}
fn is_default_ior(v: &f32) -> bool {
    (*v - 1.5).abs() < f32::EPSILON
}
fn default_thickness() -> f32 {
    0.5
}
fn is_default_thickness(v: &f32) -> bool {
    (*v - 0.5).abs() < f32::EPSILON
}
/// A sprite's default pivot: its centre.
fn centre_pivot() -> [f32; 2] {
    [0.5, 0.5]
}
fn is_centre_pivot(p: &[f32; 2]) -> bool {
    *p == centre_pivot()
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
            fog: self.fog,
            ambient: self.ambient,
            alpha: self.alpha,
            shader: self.shader.clone(),
            shader_params: self.shader_params.clone(),
            shader_textures: self.shader_textures.clone(),
            tiling: self.tiling.map(TilingDoc::to_tiling),
            shader_tiling: self
                .shader_tiling
                .iter()
                .map(|(k, v)| (k.clone(), v.to_tiling()))
                .collect(),
            sheet_cols: self.sheet_cols,
            sheet_rows: self.sheet_rows,
            cell: self.cell,
            normal_map: self.normal_map.clone(),
            normal_strength: self.normal_strength,
            roughness_map: self.roughness_map.clone(),
            roughness: self.roughness,
            metallic_map: self.metallic_map.clone(),
            metallic: self.metallic,
            ao_map: self.ao_map.clone(),
            occlusion_strength: self.occlusion_strength,
            reflectivity: self.reflectivity,
            transmission: self.transmission.clamp(0.0, 1.0),
            // Fenced: an ior below 1 inverts the bend into something no material
            // does, and the maths behind it (`1/ior`) divides.
            ior: self.ior.clamp(1.0, 3.0),
            thickness: self.thickness.clamp(0.0, 100.0),
            shading: self.shading.to_shading(),
            retro: self.retro.to_retro(),
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
            fog: m.fog,
            ambient: m.ambient,
            alpha: m.alpha,
            shader: m.shader.clone(),
            shader_params: m.shader_params.clone(),
            shader_textures: m.shader_textures.clone(),
            tiling: m.tiling.map(TilingDoc::from_tiling),
            shader_tiling: m
                .shader_tiling
                .iter()
                .map(|(k, v)| (k.clone(), TilingDoc::from_tiling(*v)))
                .collect(),
            sheet_cols: m.sheet_cols,
            sheet_rows: m.sheet_rows,
            cell: m.cell,
            normal_map: m.normal_map.clone(),
            normal_strength: m.normal_strength,
            roughness_map: m.roughness_map.clone(),
            roughness: m.roughness,
            metallic_map: m.metallic_map.clone(),
            metallic: m.metallic,
            ao_map: m.ao_map.clone(),
            occlusion_strength: m.occlusion_strength,
            reflectivity: m.reflectivity,
            transmission: m.transmission,
            ior: m.ior,
            thickness: m.thickness,
            shading: ShadingDoc::from_shading(m.shading),
            retro: RetroDoc::from_retro(m.retro),
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

/// Load the project config distinguishing the three cases: `Ok(None)` = the file is
/// absent, `Ok(Some(cfg))` = present + parsed, `Err` = present but won't parse. Lets a
/// migrate/upgrade step avoid clobbering a broken config or fabricating a missing one.
pub fn try_load_project(path: &Path) -> Result<Option<ProjectConfigDoc>, SceneError> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SceneError::Io(e)),
        Ok(text) => ron::from_str(&text).map(Some).map_err(SceneError::Ron),
    }
}

/// Save the project-wide render config to a pretty RON file.
pub fn save_project(cfg: &ProjectConfigDoc, path: &Path) -> Result<(), SceneError> {
    let text = ron::ser::to_string_pretty(cfg, ron::ser::PrettyConfig::default())
        .map_err(SceneError::Serialize)?;
    std::fs::write(path, text).map_err(SceneError::Io)
}

/// Spawn every node into `world` as an entity with `Transform` + `Name` + `Matter`,
/// then spawn the one mandatory Lighting node (`Name` + [`Light`]).
/// `NodeDoc::id` → position in `nodes`, for resolving stable parent links.
pub fn node_id_positions(nodes: &[NodeDoc]) -> std::collections::HashMap<u32, usize> {
    nodes.iter().enumerate().filter_map(|(i, n)| n.id.map(|id| (id, i))).collect()
}

/// This node's parent as a position in `nodes`.
///
/// `parent_id` wins whenever it is present: it names a NODE. The positional
/// `parent` names a *position*, which any insertion or removal ahead of it
/// silently re-points at something else — see [`NodeDoc::parent_id`]. A
/// `parent_id` that resolves to nothing falls through to the index rather than
/// orphaning the node, so a hand-edited file degrades instead of breaking.
pub fn resolve_parent(
    node: &NodeDoc,
    by_id: &std::collections::HashMap<u32, usize>,
) -> Option<usize> {
    node.parent_id.and_then(|id| by_id.get(&id).copied()).or(node.parent)
}

/// Everything wrong with a scene's parent wiring that can be seen from the file
/// alone, each as a line naming the node.
///
/// A dangling index would at least be *caught*; the fault this exists for never
/// was, because a stale positional link is always **valid** — node 156 exists,
/// it is simply not the node the author meant, and nothing in the file records
/// which one that was. So this reports the cases that ARE decidable, and the
/// stable ids ([`NodeDoc::parent_id`]) prevent the case that is not.
/// floptle/0046.
pub fn validate_parents(nodes: &[NodeDoc]) -> Vec<String> {
    let mut out = Vec::new();
    let by_id = node_id_positions(nodes);
    // Two nodes claiming one id makes every link to it ambiguous — the shape a
    // generator script hits when it copies a block without renumbering.
    let mut seen: std::collections::HashMap<u32, &str> = std::collections::HashMap::new();
    for n in nodes {
        if let Some(id) = n.id
            && let Some(first) = seen.insert(id, n.name.as_str())
        {
            out.push(format!(
                "\"{}\" and \"{first}\" both claim node id {id} — links to it are ambiguous",
                n.name
            ));
        }
    }
    let name = |i: usize| -> String {
        nodes.get(i).map(|n| n.name.clone()).unwrap_or_else(|| format!("#{i}"))
    };
    for (i, n) in nodes.iter().enumerate() {
        if let Some(id) = n.parent_id
            && !by_id.contains_key(&id)
        {
            out.push(format!(
                "\"{}\" names parent id {id}, which no node in this scene carries{}",
                n.name,
                match n.parent {
                    Some(p) => format!(" — falling back to the positional link ({p})"),
                    None => " — it will load as a root".into(),
                }
            ));
        }
        if let Some(p) = n.parent
            && n.parent_id.is_none()
            && p >= nodes.len()
        {
            out.push(format!(
                "\"{}\" has parent index {p}, but this scene has {} node(s) — it will load \
                 as a root",
                n.name,
                nodes.len()
            ));
        }
        if resolve_parent(n, &by_id) == Some(i) {
            out.push(format!("\"{}\" is its own parent — the link is ignored", n.name));
        }
    }
    // Cycles: walk each node's chain with a step budget rather than tracking a
    // visited set per node, which is the same answer for far less work.
    for (i, _) in nodes.iter().enumerate() {
        let mut at = i;
        for step in 0..=nodes.len() {
            let Some(p) = nodes.get(at).and_then(|n| resolve_parent(n, &by_id)) else { break };
            if p == i && step > 0 {
                out.push(format!("\"{}\" is in a parent CYCLE — the scene cannot lay out", name(i)));
                break;
            }
            at = p;
        }
    }
    out
}

/// UI elements whose parent chain can never be visible, one line each.
///
/// An invisible parent hides its whole subtree, so this is the difference
/// between "obviously broken" and "you cannot see any of this and nothing will
/// tell you why" — which is how a match HUD went missing for two play sessions
/// and was reported as three separate bugs. floptle/0046.
pub fn validate_ui_visibility(nodes: &[NodeDoc]) -> Vec<String> {
    let by_id = node_id_positions(nodes);
    let mut out = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        if n.ui.is_none() {
            continue;
        }
        let mut at = i;
        for _ in 0..=nodes.len() {
            let Some(p) = nodes.get(at).and_then(|x| resolve_parent(x, &by_id)) else { break };
            let Some(parent) = nodes.get(p) else { break };
            let hidden_spec = parent.ui.as_ref().is_some_and(|u| !u.visible);
            if !parent.visible || hidden_spec {
                out.push(format!(
                    "UI element \"{}\" sits under \"{}\", which is not visible — nothing in \
                     this subtree will ever be drawn",
                    n.name, parent.name
                ));
                break;
            }
            at = p;
        }
    }
    out
}

/// Spawn ONE node's components into the world (no parent/attachment linking —
/// those need the whole doc's index space; the caller links if it wants to).
/// Used by the editor's node clipboard/spawners and by `floptle-net` to
/// materialize a replicated runtime spawn.
pub fn spawn_node(node: &NodeDoc, world: &mut World) -> floptle_core::Entity {
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
    if !node.object_materials.is_empty() {
        world.insert(
            e,
            floptle_core::ObjectMaterials(
                node.object_materials
                    .iter()
                    .map(|(k, m)| (k.clone(), m.to_material()))
                    .collect(),
            ),
        );
    }
    if let Some(rb) = &node.rigidbody {
        world.insert(e, rb.to_rigidbody());
    }
    if let Some(cb) = &node.celestial {
        world.insert(e, cb.to_body());
    }
    if node.mesh_collider {
        world.insert(e, floptle_core::MeshCollider);
    }
    if node.disabled {
        world.insert(e, floptle_core::Disabled);
    }
    if node.collidable {
        world.insert(e, floptle_core::Collidable);
    }
    if node.nav_exclude {
        world.insert(e, floptle_core::NavMeshExclude);
    }
    if let Some(id) = node.paint {
        world.insert(e, floptle_core::VertexPaint { id });
    }
    if let Some(id) = node.tex_paint {
        world.insert(e, floptle_core::TexturePaint { id });
    }
    if let Some(spec) = &node.terrain_gen {
        world.insert(e, floptle_core::TerrainGen(spec.clone()));
    }
    if node.trigger {
        world.insert(e, floptle_core::Trigger);
    }
    if !node.visible {
        world.insert(e, floptle_core::Visible(false));
    }
    if !node.cast_shadow {
        world.insert(e, floptle_core::CastShadow(false));
    }
    if let Some(ctl) = &node.anim_controller {
        world.insert(e, floptle_core::AnimController { asset: ctl.clone() });
    }
    if let Some(p) = &node.particles {
        world.insert(e, p.to_component());
    }
    if let Some(n) = &node.net {
        world.insert(e, n.to_component());
    }
    if let Some(l) = &node.ui_layer {
        world.insert(e, *l);
    }
    if let Some(u) = &node.ui {
        world.insert(e, u.clone());
    }
    if let Some(a) = &node.audio {
        world.insert(e, a.clone());
    }
    if let Some(l) = &node.layer {
        world.insert(e, floptle_core::Layer(l.clone()));
    }
    if !node.tags.is_empty() {
        world.insert(e, floptle_core::Tags(node.tags.clone()));
    }
    // An unknown spelling falls back to `Auto` rather than refusing the node:
    // a scene from a newer engine should still open, with the light behaving as
    // an unconfigured one rather than the whole file failing to load.
    if node.lit_2d.is_some()
        || !node.light_layers.is_empty()
        || node.light_inner.is_some()
        || node.light_falloff.is_some()
        || node.light_shadows.is_some()
    {
        let d = floptle_core::Lighting2D::default();
        world.insert(
            e,
            floptle_core::Lighting2D {
                mode: node
                    .lit_2d
                    .as_deref()
                    .and_then(floptle_core::Lit2D::parse)
                    .unwrap_or_default(),
                layers: node.light_layers.clone(),
                inner: node.light_inner.unwrap_or(d.inner),
                falloff: node.light_falloff.unwrap_or(d.falloff),
                shadows: node.light_shadows.unwrap_or(d.shadows),
            },
        );
    }
    if let Some(c) = node.shadow_2d.as_deref().and_then(floptle_core::Cast2D::parse) {
        world.insert(e, floptle_core::Shadow2D(c));
    }
    // The mode stands alone: a node can Y-sort on the Default layer at order 0,
    // which writes no `sorting` tuple at all.
    if let Some((x, y)) = node.parallax {
        let p = floptle_core::Parallax { factor: [x, y] };
        if !p.is_identity() {
            world.insert(e, p);
        }
    }
    if let Some(c) = node.camera_2d.as_ref() {
        world.insert(e, floptle_core::camera2d::Camera2D::from(c));
    }
    let sort_mode = node.sort_mode.as_deref().map(floptle_core::SortMode::parse).unwrap_or_default();
    if node.sorting.is_some() || sort_mode != floptle_core::SortMode::default() {
        let (layer, order) = node.sorting.clone().unwrap_or_default();
        world.insert(e, floptle_core::Sorting { layer, order, mode: sort_mode });
    }
    e
}

/// Spawn a doc's nodes and wire their hierarchy — the part that is the same
/// whether a scene is being opened or layered on top of another one. Returns
/// the entities in `doc.nodes` order.
///
/// Deliberately does NOT create the per-scene singletons (lighting, skybox,
/// post-processing): those belong to whichever scene is the base.
pub fn spawn_nodes(nodes: &[NodeDoc], world: &mut World) -> Vec<floptle_core::Entity> {
    // First pass: spawn each node (keeping the index→entity map for parent links).
    let mut ents = Vec::with_capacity(nodes.len());
    for node in nodes {
        ents.push(spawn_node(node, world));
    }
    // Second pass: link parents (skip out-of-range / self references).
    let by_id = node_id_positions(nodes);
    for (i, node) in nodes.iter().enumerate() {
        if let Some(p) = resolve_parent(node, &by_id)
            && p < ents.len() && p != i {
                world.insert(ents[i], floptle_core::Parent(ents[p]));
            }
    }
    // Third pass: bone attachments (target = the parent linked above; resolved by the
    // editor's resolve_attachments each frame, which fixes the identity transform).
    for (i, node) in nodes.iter().enumerate() {
        if let (Some(att), Some(p)) = (&node.attachment, resolve_parent(node, &by_id))
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
    ents
}

/// Layer a scene's nodes on top of a world that is already running — the
/// `scene.load(name, { additive = true })` path.
///
/// Two differences from [`spawn_into`], both of them the whole point:
///
/// * **No singletons.** An additive scene brings no second sun, skybox or
///   post-processing chain. A world has one environment; a second Lighting node
///   would silently win or lose depending on query order, which is the kind of
///   bug that reads as "the additive scene broke my lighting".
/// * **Everything is tagged** with `tag`, so [`despawn_tagged`] can take exactly
///   these nodes away again and nothing else.
pub fn spawn_additive(
    doc: &SceneDoc,
    world: &mut World,
    tag: &str,
) -> Vec<floptle_core::Entity> {
    let ents = spawn_nodes(&doc.nodes, world);
    for &e in &ents {
        world.insert(e, floptle_core::SceneTag(tag.to_string()));
    }
    ents
}

/// Remove every node an additive load tagged with `tag`. Returns how many went.
///
/// Children first is not required — the ECS has no ordering constraint — but a
/// node whose PARENT is being removed goes too even if it was spawned later by
/// something else (a projectile parented to an additive room leaves with the
/// room, rather than becoming a child of nothing).
pub fn despawn_tagged(world: &mut World, tag: &str) -> usize {
    let direct: Vec<floptle_core::Entity> = world
        .query::<floptle_core::SceneTag>()
        .filter(|(_, t)| t.0 == tag)
        .map(|(e, _)| e)
        .collect();
    if direct.is_empty() {
        return 0;
    }
    let doomed: std::collections::HashSet<floptle_core::Entity> = direct.iter().copied().collect();
    // Anything parented (at any depth) under a doomed node goes with it.
    let mut all = doomed.clone();
    for (e, _) in world.query::<floptle_core::Matter>() {
        let mut cur = e;
        for _ in 0..64 {
            let Some(floptle_core::Parent(p)) = world.get::<floptle_core::Parent>(cur).copied()
            else {
                break;
            };
            if doomed.contains(&p) {
                all.insert(e);
                break;
            }
            cur = p;
        }
    }
    let n = all.len();
    for e in all {
        world.despawn(e);
    }
    n
}

pub fn spawn_into(doc: &SceneDoc, world: &mut World) {
    spawn_nodes(&doc.nodes, world);
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

    // Gravity volumes are OPTIONAL — deleting one STICKS. (Load used to
    // self-heal a strength-10 uniform-Down volume into any scene without one,
    // which silently injected a world −Y pull into celestial scenes — uniform
    // −Y pumps orbit energy and flings things off planets. New scenes get
    // their starter Gravity node from the editor's new-scene template
    // instead; a space scene simply has none.)

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
        let object_materials = world
            .get::<floptle_core::ObjectMaterials>(e)
            .map(|om| {
                om.0.iter().map(|(k, m)| (k.clone(), MaterialDoc::from_material(m))).collect()
            })
            .unwrap_or_default();
        let rigidbody = world.get::<RigidBody>(e).map(RigidBodyDoc::from_rigidbody);
        let celestial =
            world.get::<floptle_core::CelestialBody>(e).map(CelestialBodyDoc::from_body);
        let mesh_collider = world.get::<floptle_core::MeshCollider>(e).is_some();
        let disabled = world.get::<floptle_core::Disabled>(e).is_some();
        let collidable = world.get::<floptle_core::Collidable>(e).is_some();
        let paint = world.get::<floptle_core::VertexPaint>(e).map(|p| p.id);
        let tex_paint = world.get::<floptle_core::TexturePaint>(e).map(|p| p.id);
        let terrain_gen = world.get::<floptle_core::TerrainGen>(e).map(|g| g.0.clone());
        let trigger = world.get::<floptle_core::Trigger>(e).is_some();
        let nav_exclude = world.get::<floptle_core::NavMeshExclude>(e).is_some();
        let visible = world.get::<floptle_core::Visible>(e).map(|v| v.0).unwrap_or(true);
        let cast_shadow = world.get::<floptle_core::CastShadow>(e).map(|c| c.0).unwrap_or(true);
        let anim_controller =
            world.get::<floptle_core::AnimController>(e).map(|c| c.asset.clone());
        let particles = world
            .get::<floptle_core::ParticleSystem>(e)
            .map(ParticleSystemDoc::from_component);
        let net = world.get::<floptle_core::Replicated>(e).map(ReplicatedDoc::from_component);
        let ui_layer = world.get::<floptle_ui::UiLayer>(e).copied();
        let ui = world.get::<floptle_ui::ElementSpec>(e).cloned();
        let audio = world.get::<floptle_audio::AudioSource>(e).cloned();
        let parent = world.get::<floptle_core::Parent>(e).and_then(|p| index.get(&p.0).copied());
        // Both links are written: `parent_id` is what any current engine reads,
        // and the positional `parent` keeps an older one able to open the file.
        // Ids are the node's position at save time + 1 — unique within the
        // scene, which is all the reference needs to be, and stable across the
        // reorder or insertion that would move an index. floptle/0046.
        let id = index.get(&e).map(|i| *i as u32 + 1);
        let parent_id = parent.map(|p| p as u32 + 1);
        // "Default" never serializes — a node's absence of a layer IS Default.
        let layer = world
            .get::<floptle_core::Layer>(e)
            .map(|l| l.0.clone())
            .filter(|l| l != floptle_core::layers::DEFAULT_LAYER);
        let tags = world.get::<floptle_core::Tags>(e).map(|t| t.0.clone()).unwrap_or_default();
        // Default-at-0 never serializes: a node's absence of sorting IS the
        // default, exactly as with `layer` above, so a scene that does not use
        // sorting layers is written byte for byte as it always was.
        let sorting = world.get::<floptle_core::Sorting>(e).and_then(|s| {
            let name = if s.layer.trim().is_empty() {
                floptle_core::DEFAULT_SORTING_LAYER
            } else {
                s.layer.as_str()
            };
            (name != floptle_core::DEFAULT_SORTING_LAYER || s.order != 0)
                .then(|| (name.to_string(), s.order))
        });
        // Same rule again: identity IS the default, so it writes nothing.
        let parallax = world
            .get::<floptle_core::Parallax>(e)
            .filter(|p| !p.is_identity())
            .map(|p| (p.factor[0], p.factor[1]));
        // Same rule again: `Order` IS the default, so it writes nothing.
        let sort_mode = world
            .get::<floptle_core::Sorting>(e)
            .and_then(|s| s.mode.as_str())
            .map(str::to_string);
        // Same rule as `sorting` above: `Auto` with no layer list IS the
        // default, so a scene that has never touched 2D lighting is written
        // byte for byte as it always was.
        let lit = world.get::<floptle_core::Lighting2D>(e);
        let lit_2d = lit
            .map(|l| l.mode)
            .filter(|m| *m != floptle_core::Lit2D::Auto)
            .map(|m| m.name().to_string());
        let light_layers = lit.map(|l| l.layers.clone()).unwrap_or_default();
        // …and the same rule again for the shaping knobs: only a value that
        // differs from what every light has always done is written.
        let d2 = floptle_core::Lighting2D::default();
        let light_inner = lit.map(|l| l.inner).filter(|v| *v != d2.inner);
        let light_falloff = lit.map(|l| l.falloff).filter(|v| *v != d2.falloff);
        let light_shadows = lit.map(|l| l.shadows).filter(|v| *v != d2.shadows);
        let shadow_2d = world
            .get::<floptle_core::Shadow2D>(e)
            .map(|s| s.0)
            .filter(|c| *c != floptle_core::Cast2D::Auto)
            .map(|c| c.name().to_string());
        nodes.push(NodeDoc {
            id,
            parent_id,
            name,
            transform,
            sort_mode,
            parallax,
            camera_2d: world.get::<floptle_core::camera2d::Camera2D>(e).map(Camera2DDoc::from),
            matter: MatterDoc::from(matter),
            scripts,
            material,
            object_materials,
            rigidbody,
            celestial,
            mesh_collider,
            disabled,
            paint,
            tex_paint,
            terrain_gen,
            collidable,
            trigger,
            nav_exclude,
            visible,
            cast_shadow,
            anim_controller,
            particles,
            parent,
            attachment,
            net,
            ui_layer,
            ui,
            audio,
            layer,
            tags,
            sorting,
            lit_2d,
            light_layers,
            shadow_2d,
            light_inner,
            light_falloff,
            light_shadows,
        });
    }
    let lighting =
        world.query::<Light>().next().map(|(_, l)| LightDoc::from(l)).unwrap_or_default();
    SceneDoc { name: name.into(), lighting, nodes }
}

#[cfg(test)]
mod tests {

    /// **Pinning a retro width has to change the FRAMING, not just the target.**
    ///
    /// The scene is composited into a fixed `retro_width × retro_height` target
    /// and then blitted, letterboxed, into the panel. Projecting at the panel's
    /// aspect while rendering into that target squashes the picture
    /// horizontally — and it defeats the whole reason for pinning a width, which
    /// is that the framing stops depending on the window. Reported from a real
    /// project: with `retro_integer_scale` on, both settings had to be left off.
    #[test]
    fn a_pinned_retro_width_decides_the_projection_not_the_window() {
        let pinned = ProjectConfigDoc {
            retro: true,
            retro_width: 320,
            retro_height: 180,
            ..Default::default()
        };
        // Whatever shape the window is, the camera frames the target.
        for panel in [4.0 / 3.0, 16.0 / 9.0, 21.0 / 9.0, 1.0] {
            let a = pinned.render_aspect(panel);
            assert!(
                (a - 320.0 / 180.0).abs() < 1e-6,
                "at a {panel} panel the camera projected at {a}, not the target's aspect"
            );
        }
        // Unpinned, the target is derived FROM the panel, so the panel's own
        // number is the answer — not a re-derivation of it through a rounded
        // pixel width.
        let derived = ProjectConfigDoc { retro: true, retro_height: 180, ..Default::default() };
        assert_eq!(derived.render_aspect(4.0 / 3.0), 4.0 / 3.0);
        // And with retro off it is the panel, pinned width or not — nothing is
        // being composited into a target of another shape.
        let off = ProjectConfigDoc {
            retro: false,
            retro_width: 320,
            retro_height: 180,
            ..Default::default()
        };
        assert_eq!(off.render_aspect(4.0 / 3.0), 4.0 / 3.0);
    }
    use super::*;

    /// A bare node with a name, for wiring tests — cloned off the demo scene so
    /// it stays valid as `NodeDoc` grows.
    fn plain(name: &str, id: u32, parent_id: Option<u32>) -> NodeDoc {
        let mut n = demo().nodes[0].clone();
        n.id = Some(id);
        n.parent_id = parent_id;
        n.parent = None;
        n.name = name.into();
        n.scripts.clear();
        n.ui = None;
        n.visible = true;
        n
    }

    /// A 2D camera survives a save/load, and a camera without one writes nothing
    /// — so adding this cannot churn every scene file in a project's diff.
    ///
    /// The live half is asserted absent on purpose: a scene records the *rule*,
    /// and a camera that came back mid-shake, or already halfway to a target it
    /// has not been given yet, would be recording a moment.
    #[test]
    fn a_2d_camera_round_trips_and_stays_out_of_scenes_without_one() {
        use floptle_core::camera2d::Camera2D;
        let mut world = floptle_core::World::new();

        let plain = world.spawn();
        world.insert(plain, floptle_core::Transform::default());
        world.insert(plain, floptle_core::Name("Plain".into()));
        world.insert(
            plain,
            floptle_core::Matter::Camera {
                fov_y: 1.0,
                active: true,
                target: String::new(),
                cull_mask: !0,
                target_w: 0,
                target_h: 0,
                target_hz: 0.0,
                ortho: true,
                ortho_height: 10.0,
            },
        );

        let follower = world.spawn();
        world.insert(follower, floptle_core::Transform::default());
        world.insert(follower, floptle_core::Name("Follower".into()));
        world.insert(
            follower,
            floptle_core::Matter::Camera {
                fov_y: 1.0,
                active: false,
                target: String::new(),
                cull_mask: !0,
                target_w: 0,
                target_h: 0,
                target_hz: 0.0,
                ortho: true,
                ortho_height: 10.0,
            },
        );
        let mut c = Camera2D {
            follow: "Player".into(),
            smoothing: 0.2,
            dead_zone: [1.5, 0.75],
            limits_on: true,
            limit_min: [0.0, -5.0],
            limit_max: [200.0, 40.0],
            ..Default::default()
        };
        // Live state that must NOT be written.
        c.pos = floptle_core::math::DVec2::new(123.0, 456.0);
        c.started = true;
        c.shake(9.0, 9.0);
        world.insert(follower, c);

        let doc = to_doc("test", &world);
        let text = ron::ser::to_string_pretty(&doc, Default::default()).unwrap();
        assert_eq!(
            text.matches("camera_2d").count(),
            1,
            "only the camera that has one should write one:\n{text}"
        );
        assert!(!text.contains("123"), "the live follow position was saved:\n{text}");
        assert!(!text.contains("shake"), "a shake in progress was saved:\n{text}");

        let back: SceneDoc = ron::from_str(&text).unwrap();
        let mut world2 = floptle_core::World::new();
        spawn_into(&back, &mut world2);
        let got: Vec<Camera2D> =
            world2.query::<Camera2D>().map(|(_, c)| c.clone()).collect();
        assert_eq!(got.len(), 1);
        let g = &got[0];
        assert_eq!(g.follow, "Player");
        assert_eq!(g.smoothing, 0.2);
        assert_eq!(g.dead_zone, [1.5, 0.75]);
        assert!(g.limits_on);
        assert_eq!(g.limit_min, [0.0, -5.0]);
        assert_eq!(g.limit_max, [200.0, 40.0]);
        assert!(!g.started, "a loaded camera has not started following yet");
        assert!(!g.shaking(), "a loaded camera is not mid-shake");
    }

    /// A material's spritesheet survives a save/load, and a material that isn't a
    /// sheet writes no sheet keys at all — so adding this feature can't churn
    /// every existing material file in a project's diff.
    #[test]
    fn a_materials_spritesheet_round_trips_and_stays_out_of_plain_files() {
        let m = Material { sheet_cols: 4, sheet_rows: 2, cell: 5, ..Material::default() };
        let ron = ron::ser::to_string(&MaterialDoc::from_material(&m)).expect("serialize");
        assert!(ron.contains("sheet_cols:4") || ron.contains("sheet_cols: 4"), "{ron}");
        let back: MaterialDoc = ron::from_str(&ron).expect("deserialize");
        let back = back.to_material();
        assert_eq!((back.sheet_cols, back.sheet_rows, back.cell), (4, 2, 5));

        let plain = ron::ser::to_string(&MaterialDoc::from_material(&Material::default()))
            .expect("serialize");
        assert!(!plain.contains("sheet"), "a non-sheet material must not write sheet keys: {plain}");
        assert!(!plain.contains("cell"), "a non-sheet material must not write a cell: {plain}");
    }

    /// A pre-spritesheet material file still loads — the fields default to "not a
    /// sheet", which is the whole-texture behaviour those files were authored with.
    #[test]
    fn a_material_file_from_before_spritesheets_still_loads() {
        let doc: MaterialDoc =
            ron::from_str("(color:(1,1,1),texture:Some(\"t.png\"),unlit:true)").expect("load");
        let m = doc.to_material();
        assert_eq!((m.sheet_cols, m.sheet_rows, m.cell), (0, 0, 0));
        assert!(!m.is_sheet());
        assert_eq!(m.cell_uv(), [0.0, 0.0, 1.0, 1.0]);
    }

    /// The fog opt-out survives a save/load, and a material that never touched
    /// it writes nothing — so adding the flag cannot churn every existing
    /// material file in a project's diff.
    #[test]
    fn a_fog_opt_out_round_trips_and_stays_out_of_every_other_file() {
        let m = Material { fog: false, ..Material::default() };
        let ron = ron::ser::to_string(&MaterialDoc::from_material(&m)).expect("serialize");
        let back: MaterialDoc = ron::from_str(&ron).expect("deserialize");
        assert!(!back.to_material().fog, "the opt-out did not survive the round trip: {ron}");

        let plain =
            ron::ser::to_string(&MaterialDoc::from_material(&Material::default())).expect("ser");
        assert!(!plain.contains("fog"), "a fogged material must write no fog key: {plain}");
    }

    /// A material file written before the flag existed loads as FOGGED, which
    /// is what those files were authored against. Defaulting a bool the other
    /// way would silently lift the fog off every surface in every old project.
    #[test]
    fn a_material_file_from_before_the_fog_flag_still_fogs() {
        let doc: MaterialDoc = ron::from_str("(color:(1,1,1),unlit:true)").expect("load");
        assert!(doc.to_material().fog);
    }

    /// The project's era artefacts survive a save/load, default to off, and
    /// write nothing while they are off — an existing `project.ron` must load
    /// to exactly the look it already has.
    #[test]
    fn the_projects_era_artefacts_default_to_off_and_write_nothing() {
        let off = ProjectConfigDoc::ps1();
        assert_eq!(off.retro_artefacts(), floptle_core::Retro::default());
        let ron = ron::ser::to_string(&off).expect("serialize");
        assert!(!ron.contains("retro_jitter"), "an untouched project wrote a jitter: {ron}");
        assert!(!ron.contains("retro_vertex_lit"), "…or a lighting switch: {ron}");

        let on = ProjectConfigDoc { retro_jitter: 160.0, retro_vertex_lit: true, ..off };
        let back: ProjectConfigDoc =
            ron::from_str(&ron::ser::to_string(&on).expect("ser")).expect("load");
        let r = back.retro_artefacts();
        assert_eq!(r.jitter, 160.0);
        assert!(r.vertex_lit && !r.affine_uv);
        assert!(!r.exempt, "the project itself has nothing to be exempt from");
    }

    /// The named jitter strengths are measured against the project's OWN pixel
    /// resolution, so the same preset means the same look at any resolution —
    /// which is the whole reason they are derived rather than fixed numbers.
    /// They must also be ordered coarsest-last, because that is the order they
    /// are offered in.
    #[test]
    fn the_jitter_presets_follow_the_projects_own_resolution() {
        let p240 = ProjectConfigDoc { retro_height: 240, ..ProjectConfigDoc::ps1() };
        let p480 = ProjectConfigDoc { retro_height: 480, ..ProjectConfigDoc::ps1() };
        assert_eq!(p240.retro_jitter_pixels(), 120.0, "240 rows is 120 steps across NDC's span of 2");
        assert_eq!(p480.retro_jitter_pixels(), 240.0, "a taller target needs a finer grid");

        let steps: Vec<f32> = p240.retro_jitter_presets().iter().map(|(_, s, _)| *s).collect();
        assert_eq!(steps, vec![0.0, 120.0, 60.0, 30.0]);
        assert!(
            steps[1..].windows(2).all(|w| w[0] > w[1]),
            "presets must run finest to coarsest — fewer steps is a BIGGER cell, and a list \
             that climbed would read as getting subtler while getting harsher"
        );
        // Nothing finer than pixel-exact is offered: a grid finer than the
        // pixels it is drawn on snaps vertices to positions no frame can show.
        assert_eq!(steps[1], p240.retro_jitter_pixels());
    }

    /// floptle/0046: the whole point of a stable link. Inserting a node ahead of
    /// a subtree must not re-point it — which is exactly what positional
    /// indices did, silently, moving a match HUD onto a line of help text in
    /// another panel and hiding it for two play sessions.
    #[test]
    fn inserting_a_node_cannot_re_parent_an_existing_subtree() {
        // "Match" (id 1) with a child "Timer" (id 2) that names it by ID.
        let mut nodes = vec![plain("Match", 1, None), plain("Timer", 2, Some(1))];
        // The legacy positional link agrees, for now.
        nodes[1].parent = Some(0);

        let by_id = node_id_positions(&nodes);
        assert_eq!(resolve_parent(&nodes[1], &by_id), Some(0), "Timer starts under Match");

        // Someone inserts two nodes AHEAD of the block — a script generating a
        // UI layer, a hand edit, anything. Every positional index now names a
        // different node; the stale ones are left exactly as they were.
        nodes.insert(0, plain("Legend", 90, None));
        nodes.insert(0, plain("Legend2", 91, None));
        let by_id = node_id_positions(&nodes);

        let timer = nodes.iter().find(|n| n.name == "Timer").unwrap();
        let resolved = resolve_parent(timer, &by_id).map(|i| nodes[i].name.clone());
        assert_eq!(resolved.as_deref(), Some("Match"), "the ID link survived the insertion");
        // And the stale positional link is what would have been followed before:
        // it now names something else entirely.
        assert_eq!(timer.parent, Some(0));
        assert_eq!(nodes[0].name, "Legend2", "index 0 is a DIFFERENT node now — the bug");
    }

    #[test]
    fn a_scene_round_trips_its_parent_ids() {
        let mut doc = demo();
        doc.nodes = vec![plain("Root", 1, None), plain("Child", 2, Some(1))];
        let text = to_ron(&doc).expect("serializes");
        assert!(text.contains("parent_id"), "the stable link is written:\n{text}");
        let back: SceneDoc = ron::from_str(&text).expect("re-parses");
        let by_id = node_id_positions(&back.nodes);
        assert_eq!(resolve_parent(&back.nodes[1], &by_id), Some(0));
    }

    /// A file with no ids at all — every scene written before this existed —
    /// must load exactly as it always did.
    #[test]
    fn a_legacy_scene_still_links_by_index() {
        let mut nodes = vec![plain("Root", 1, None), plain("Child", 2, None)];
        for n in &mut nodes {
            n.id = None;
        }
        nodes[1].parent = Some(0);
        let by_id = node_id_positions(&nodes);
        assert!(by_id.is_empty());
        assert_eq!(resolve_parent(&nodes[1], &by_id), Some(0), "the index still governs");
    }

    #[test]
    fn broken_wiring_is_reported_by_name() {
        // A parent_id nothing carries.
        let mut nodes = vec![plain("Root", 1, None), plain("Orphan", 2, Some(404))];
        let out = validate_parents(&nodes);
        assert!(out.iter().any(|l| l.contains("Orphan") && l.contains("404")), "{out:?}");

        // An out-of-range positional index, on a legacy node.
        nodes[1].parent_id = None;
        nodes[1].parent = Some(99);
        let out = validate_parents(&nodes);
        assert!(out.iter().any(|l| l.contains("Orphan") && l.contains("99")), "{out:?}");

        // A cycle.
        let cyc = vec![plain("A", 1, Some(2)), plain("B", 2, Some(1))];
        let out = validate_parents(&cyc);
        assert!(out.iter().any(|l| l.contains("CYCLE")), "{out:?}");

        // Two nodes claiming one id.
        let dup = vec![plain("A", 7, None), plain("B", 7, None)];
        let out = validate_parents(&dup);
        assert!(out.iter().any(|l| l.contains("both claim node id 7")), "{out:?}");

        // A healthy scene says nothing.
        assert!(validate_parents(&[plain("Root", 1, None), plain("Kid", 2, Some(1))]).is_empty());
    }

    /// The lint for the shape that made this invisible rather than obviously
    /// wrong: a UI element under a permanently invisible ancestor is never
    /// drawn, in any mode, and nothing anywhere says why.
    #[test]
    fn a_ui_element_under_an_invisible_parent_is_reported() {
        let mut panel = plain("Hidden Panel", 1, None);
        panel.visible = false;
        panel.ui = Some(floptle_ui::ElementSpec::default());
        let mut label = plain("Score", 2, Some(1));
        label.ui = Some(floptle_ui::ElementSpec::default());
        let nodes = vec![panel, label];
        let out = validate_ui_visibility(&nodes);
        assert!(
            out.iter().any(|l| l.contains("Score") && l.contains("Hidden Panel")),
            "names both the element and what is hiding it: {out:?}"
        );

        // Visible parent: silent.
        let mut ok_parent = plain("Panel", 1, None);
        ok_parent.ui = Some(floptle_ui::ElementSpec::default());
        let mut kid = plain("Score", 2, Some(1));
        kid.ui = Some(floptle_ui::ElementSpec::default());
        assert!(validate_ui_visibility(&[ok_parent, kid]).is_empty());
    }

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
                    camera_2d: None,
                    sort_mode: None,
                    parallax: None,
                    id: None,
                    parent_id: None,
                    name: "cube".into(),
                    transform: TransformDoc { translation: [1.0, 2.0, 3.0], ..Default::default() },
                    matter: MatterDoc::Primitive { shape: ShapeDoc::Cube, color: [0.9, 0.4, 0.3] },
                    object_materials: Default::default(),
                    scripts: vec![ScriptDoc {
                        kind: "pulsate".into(),
                        enabled: true,
                        params: vec![("speed".into(), 2.0)],
                        refs: vec![("target".into(), "blob".into())], // exercise the round-trip
                        strs: vec![("scene".into(), "arena".into())], // exercise the round-trip
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
                        mode: BodyModeDoc::Kinematic, // exercise the mode round-trip
                        radius: 0.6,
                        height: 2.4,
                        half_extents: [0.5, 0.5, 0.5],
                        restitution: 0.2,
                        friction: 0.5,
                        slope_limit: 42.0, // exercise the slope-limit round-trip
                        gravity: false,    // exercise the gravity-flag round-trip
                        lock_pos: [false, false, true],
                        lock_rot: [true, false, true],
                        two_d: true,
                        align_up: true, // exercise the align-to-gravity round-trip
                        mass: 3.5,      // exercise the assembly-field round-trips
                        assembly: true,
                        pushbox_only: true, // exercise the rollback-profile round-trip
                    }),
                    celestial: Some(CelestialBodyDoc {
                        mu: 25000.0,
                        body_radius: 60.0,
                        soi: 0.0,
                        parent: "Sun".into(),
                        a: 220.0,
                        e: 0.1,
                        i: 0.15,
                        lan: 0.3,
                        arg_pe: 1.2,
                        m0: 0.8,
                        atmo_color: [0.4, 0.55, 0.8], // exercise the atmosphere round-trip
                        atmo_height: 42.0,
                        atmo_density: 0.7,
                        clouds: 0.35,
                        luminosity: 12.0, // exercise the star round-trip
                        star_color: [1.0, 0.9, 0.8],
                        occluder_radius: 48.0, // exercise the occluder round-trip
                    }),
                    mesh_collider: true, // exercise the mesh-collider round-trip
                    disabled: true,      // …and the disabled one
                    paint: None,
                    tex_paint: None,
                    // exercise the genspec round-trip (G2 on-demand terrain)
                    terrain_gen: Some("(seed:99,radius:42.0)".into()),
                    collidable: true,    // exercise the collidable round-trip
                    nav_exclude: false,
                    trigger: true,       // exercise the trigger round-trip
                    visible: false,      // exercise the visible round-trip
                    cast_shadow: false,  // exercise the cast-shadow opt-out round-trip
                    anim_controller: Some("animation_controllers/Test".into()),
                    particles: Some(ParticleSystemDoc {
                        asset: "vfx/Test".into(),
                        play_on_start: false, // exercise the non-default round-trip
                    }),
                    parent: None,
                    attachment: None,
                    net: Some(ReplicatedDoc {
                        predicted: true, // exercise the non-default round-trip
                        rollback: false,
                        transform: true,
                        physics: true,
                        animator: false, // exercise the non-default round-trip
                        interp: false,
                        interp_delay: 12, // exercise the non-default round-trip
                        always_relevant: true, // exercise the non-default round-trip
                    }),
                    ui_layer: Some(floptle_ui::UiLayer { design_height: 1080.0, z: 2, enabled: true, space: floptle_ui::UiSpace::World, canvas_scale: 0.02, ..Default::default() }),
                    ui: Some(floptle_ui::ElementSpec {
                        place: floptle_ui::Place::Pin {
                            anchor: floptle_ui::Anchor::BottomRight,
                            offset: [-12.0, -12.0],
                        },
                        size: [floptle_ui::Size::Fixed(220.0), floptle_ui::Size::Fixed(40.0)],
                        shape: Some(floptle_ui::ShapeSpec {
                            fill: [0.1, 0.1, 0.1, 0.7],
                            radius: 8.0.into(),
                            ..Default::default()
                        }),
                        text: Some(floptle_ui::TextSpec {
                            text: "HP".into(),
                            valign: floptle_ui::Align::End, // exercise the round-trip
                            fit: true,
                            ..Default::default()
                        }),
                        // Exercise the slider/part/mask round-trips.
                        slider: Some(floptle_ui::SliderSpec {
                            min: 0.0,
                            max: 200.0,
                            value: 150.0,
                            dir: floptle_ui::Dir::Row,
                            flip: true,
                            interact: true,
                        }),
                        part: Some(floptle_ui::SliderPart::Fill),
                        mask: Some(floptle_ui::MaskSpec { targets: vec!["Minimap".into()] }),
                        ..Default::default()
                    }),
                    audio: Some(floptle_audio::AudioSource {
                        clip: "audio/hum.ogg".into(),
                        params: floptle_audio::PlayParams {
                            volume: 0.7,
                            max_distance: 35.0, // exercise the non-default round-trip
                            falloff: floptle_audio::Falloff::Linear,
                            track: "SFX".into(),
                            end: floptle_audio::EndBehavior::Loop,
                            ..Default::default()
                        },
                        play_on_start: false, // exercise the non-default round-trip
                    }),
                    layer: Some("Enemies".into()), // exercise the layer round-trip
                    tags: vec!["enemy".into(), "boss".into()],
                    sorting: None, // exercise the tags round-trip
                    lit_2d: Some("2d".into()), // exercise the 2D-lighting round-trips
                    light_layers: vec!["Terrain".into(), "Characters".into()],
                    shadow_2d: Some("on".into()),
                    light_inner: None,
                    light_falloff: None,
                    light_shadows: None,
                },
                NodeDoc {
                    camera_2d: None,
                    sort_mode: None,
                    parallax: None,
                    id: None,
                    parent_id: None,
                    terrain_gen: None,
                    name: "blob".into(),
                    transform: TransformDoc::default(),
                    matter: MatterDoc::Blob { scale: 1.3 },
                    object_materials: Default::default(),
                    scripts: Vec::new(),
                    material: None,
                    rigidbody: None,
                    celestial: None,
                    mesh_collider: false,
                    disabled: false,
                    paint: None,
                    tex_paint: None,
                    collidable: false,
                    nav_exclude: false,
                    trigger: false,
                    visible: true,
                    cast_shadow: true,
                    anim_controller: None,
                    particles: None,
                    parent: Some(0), // child of the cube — exercises parent round-trip
                    attachment: Some(AttachmentDoc {
                        bone: "Root".into(),
                        offset: TransformDoc::default(),
                    }), // exercise the bone-attachment round-trip
                    net: None,
                    ui_layer: None,
                    ui: None,
                    audio: None,
                    layer: None,
                    tags: Vec::new(),
                    sorting: None,
                    lit_2d: None,
                    light_layers: Vec::new(),
                    shadow_2d: None,
                    light_inner: None,
                    light_falloff: None,
                    light_shadows: None,
                },
                NodeDoc {
                    camera_2d: None,
                    sort_mode: None,
                    parallax: None,
                    id: None,
                    parent_id: None,
                    terrain_gen: None,
                    name: "lamp".into(),
                    transform: TransformDoc::default(),
                    matter: MatterDoc::PointLight { color: [0.1, 0.2, 0.9], intensity: 3.5, range: 7.5, shape: Default::default(), shadows: false, spot: None },
                    object_materials: Default::default(),
                    scripts: Vec::new(),
                    material: None,
                    rigidbody: None,
                    celestial: None,
                    mesh_collider: false,
                    disabled: false,
                    paint: None,
                    tex_paint: None,
                    collidable: false,
                    nav_exclude: false,
                    trigger: false,
                    visible: true,
                    cast_shadow: true,
                    anim_controller: None,
                    particles: None,
                    parent: None,
                    attachment: None,
                    net: None,
                    ui_layer: None,
                    ui: None,
                    audio: None,
                    layer: None,
                    tags: Vec::new(),
                    sorting: None,
                    lit_2d: None,
                    light_layers: Vec::new(),
                    shadow_2d: None,
                    light_inner: None,
                    light_falloff: None,
                    light_shadows: None,
                },
                NodeDoc {
                    camera_2d: None,
                    sort_mode: None,
                    parallax: None,
                    id: None,
                    parent_id: None,
                    terrain_gen: None,
                    name: "eye".into(),
                    transform: TransformDoc::default(),
                    matter: MatterDoc::Camera { fov_y: 1.0, active: true, target: String::new(), cull_mask: u32::MAX, target_w: Matter::TARGET_W, target_h: Matter::TARGET_H, target_hz: 0.0, ortho: false, ortho_height: Matter::ORTHO_HEIGHT },
                    object_materials: Default::default(),
                    scripts: Vec::new(),
                    material: None,
                    rigidbody: None,
                    celestial: None,
                    mesh_collider: false,
                    disabled: false,
                    paint: None,
                    tex_paint: None,
                    collidable: false,
                    nav_exclude: false,
                    trigger: false,
                    visible: true,
                    cast_shadow: true,
                    anim_controller: None,
                    particles: None,
                    parent: None,
                    attachment: None,
                    net: None,
                    ui_layer: None,
                    ui: None,
                    audio: None,
                    layer: None,
                    tags: Vec::new(),
                    sorting: None,
                    lit_2d: None,
                    light_layers: Vec::new(),
                    shadow_2d: None,
                    light_inner: None,
                    light_falloff: None,
                    light_shadows: None,
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

    /// A light's shaping survives a save and a load, and — the half that is
    /// easier to get wrong — a light that has *not* been shaped writes nothing.
    /// A default that serializes is a diff in every 2D scene anybody opens.
    #[test]
    fn a_shaped_light_round_trips_and_an_unshaped_one_writes_nothing() {
        let mut world = World::new();
        let e = world.spawn();
        world.insert(e, floptle_core::transform::Transform::IDENTITY);
        world.insert(e, Matter::PointLight { color: [1.0, 0.86, 0.62], intensity: 3.0, range: 8.0, shape: Default::default(), shadows: false, spot_angle: floptle_core::OMNI_ANGLE, spot_softness: 0.25 });
        world.insert(
            e,
            floptle_core::Lighting2D {
                mode: floptle_core::Lit2D::Yes,
                inner: 6.4,
                falloff: 3.5,
                shadows: false,
                ..Default::default()
            },
        );
        let text = to_ron(&to_doc("lit", &world)).unwrap();
        assert!(text.contains("light_inner"), "the inner radius did not reach the file: {text}");
        let back = from_ron(&text).unwrap();
        let mut w2 = World::new();
        spawn_into(&back, &mut w2);
        let got = w2
            .query::<floptle_core::Lighting2D>()
            .map(|(_, l)| l.clone())
            .find(|l| l.mode == floptle_core::Lit2D::Yes)
            .expect("the light's 2D component");
        assert_eq!((got.inner, got.falloff, got.shadows), (6.4, 3.5, false));

        // …and the light nobody shaped.
        let mut plain = World::new();
        let p = plain.spawn();
        plain.insert(p, floptle_core::transform::Transform::IDENTITY);
        plain.insert(p, Matter::PointLight { color: [1.0; 3], intensity: 1.0, range: 10.0, shape: Default::default(), shadows: false, spot_angle: floptle_core::OMNI_ANGLE, spot_softness: 0.25 });
        plain.insert(p, floptle_core::Lighting2D { mode: floptle_core::Lit2D::Yes, ..Default::default() });
        let text = to_ron(&to_doc("plain", &plain)).unwrap();
        for key in ["light_inner", "light_falloff", "light_shadows"] {
            assert!(!text.contains(key), "an unshaped light wrote `{key}`: {text}");
        }
    }

    #[test]
    fn world_round_trips() {
        let doc = demo();
        let mut world = World::new();
        spawn_into(&doc, &mut world);
        // 4 matter nodes (cube, blob, lamp, eye) + an auto-spawned Skybox + an
        // auto-spawned PostProcess node + the mandatory Lighting node. NO
        // auto gravity: gravity volumes are optional (deleting one sticks).
        assert_eq!(world.len(), 7);
        let snap = to_doc("demo", &world);
        // The 4 authored matter nodes plus the auto-added Skybox + PostProcess.
        assert_eq!(snap.nodes.len(), 6);
        assert!(
            snap.nodes.iter().any(|n| matches!(n.matter, MatterDoc::Skybox { .. })),
            "a default Skybox node should be present"
        );
        assert!(
            !snap.nodes.iter().any(|n| matches!(n.matter, MatterDoc::GravityVolume { .. })),
            "gravity must NOT be auto-injected (optional; deletion sticks)"
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
        assert!(rb.two_d, "the 2D switch has to survive a save and a load like every other field");
        assert!(rb.align_up, "align-to-gravity flag lost in the round-trip");
        let cb = cube.celestial.as_ref().expect("celestial body lost in round-trip");
        assert_eq!(cb.parent, "Sun");
        assert!(
            cb.mu == 25000.0 && cb.a == 220.0 && cb.e == 0.1 && cb.m0 == 0.8,
            "celestial elements lost in round-trip: {cb:?}"
        );
        assert!(cube.mesh_collider, "mesh_collider flag lost in round-trip");
        // A switched-off node has to come back switched off, or opening a scene turns
        // everything the author disabled back on — silently, and all at once.
        assert!(cube.disabled, "disabled flag lost in round-trip");
        assert!(cube.collidable, "collidable flag lost in round-trip");
        assert!(!cube.visible, "visible flag lost in round-trip");
        assert!(!cube.cast_shadow, "cast_shadow opt-out lost in round-trip");
        assert!(!rb.gravity, "rigidbody gravity flag lost in round-trip");
        // the point light's color/intensity/range round-trip
        let lamp = snap.nodes.iter().find(|n| n.name == "lamp").unwrap();
        assert_eq!(
            lamp.matter,
            MatterDoc::PointLight { color: [0.1, 0.2, 0.9], intensity: 3.5, range: 7.5, shape: Default::default(), shadows: false, spot: None }
        );
        // the camera's fov/active round-trip
        let eye = snap.nodes.iter().find(|n| n.name == "eye").unwrap();
        assert_eq!(
            eye.matter,
            MatterDoc::Camera { fov_y: 1.0, active: true, target: String::new(), cull_mask: u32::MAX, target_w: Matter::TARGET_W, target_h: Matter::TARGET_H, target_hz: 0.0, ortho: false, ortho_height: Matter::ORTHO_HEIGHT }
        );
    }

    /// A HAND-WRITTEN screen shader list loads, including the fields a person
    /// leaves out.
    ///
    /// The round trip below proves the code can read what it wrote; this proves
    /// it can read what somebody TYPED. It matters because every field here has
    /// a serde default, so a name that does not line up does not fail the
    /// load — it silently yields an empty list, and the scene renders exactly as
    /// it did before with no message anywhere.
    #[test]
    fn a_hand_written_screen_shader_list_loads() {
        const SRC: &str = r#"(
    name: "post",
    nodes: [
        (
            name: "Post Processing",
            transform: (translation: (0, 0, 0), rotation: (0, 0, 0, 1), scale: (1, 1, 1)),
            matter: PostProcess(
                screen_shaders: [
                    (
                        shader: "shaders/examples/inkOutline.flsl",
                        params: {"thickness": (2, 0, 0, 0)},
                    ),
                    (
                        shader: "shaders/examples/crtScanlines.flsl",
                        enabled: false,
                    ),
                ],
            ),
        ),
    ],
)"#;
        let doc = from_ron(SRC).expect("a hand-written scene loads");
        let MatterDoc::PostProcess { screen_shaders, .. } = &doc.nodes[0].matter else {
            panic!("the node is a PostProcess");
        };
        assert_eq!(screen_shaders.len(), 2, "both passes survive: {screen_shaders:?}");
        // Omitted `enabled` means ON — a pass written before the field existed
        // was running, and so is one somebody typed without it.
        assert!(screen_shaders[0].enabled, "an omitted `enabled` is on");
        assert_eq!(screen_shaders[0].params["thickness"], [2.0, 0.0, 0.0, 0.0]);
        assert!(!screen_shaders[1].enabled);
        assert!(screen_shaders[1].params.is_empty());
        // …and the ORDER is the list's meaning, so it must not be a set.
        assert!(screen_shaders[0].shader.ends_with("inkOutline.flsl"));
    }

    /// A light probe volume survives World → RON → World, and a hand-written one
    /// with nothing but a size loads as a usable volume rather than as a black
    /// box of zeroes. The second half is the one that matters: a `spacing` of 0
    /// or a `quality` of 0 is not a smaller bake, it is a divide by zero.
    #[test]
    fn light_probe_volumes_round_trip_and_default_sanely() {
        let mut world = World::new();
        let e = world.spawn();
        world.insert(e, Name("GI".into()));
        world.insert(e, Transform::IDENTITY);
        let authored = Matter::LightProbes {
            half_extents: [20.0, 6.0, 12.0],
            spacing: 1.5,
            enabled: false,
            intensity: 1.4,
            bounces: 3,
            quality: 32,
            leak: 0.5,
            normal_bias: 0.25,
            exclude_layers: vec!["Characters".into(), "FX".into()],
        };
        world.insert(e, authored.clone());
        let ron = to_ron(&to_doc("gi", &world)).expect("serializes");
        let mut round = World::new();
        spawn_into(&from_ron(&ron).expect("parses"), &mut round);
        let got = round
            .query::<Matter>()
            .find(|(_, m)| matches!(m, Matter::LightProbes { .. }))
            .map(|(_, m)| m.clone())
            .expect("the volume survives");
        assert_eq!(got, authored, "every knob round trips");

        // The bake is a build artefact, not scene text: nothing that looks like
        // probe data may appear in the `.ron`.
        assert!(!ron.contains("probes:"), "the bake stays out of the scene file");

        const BARE: &str = r#"(
    name: "gi",
    nodes: [
        (
            name: "GI",
            transform: (translation: (0, 2, 0), rotation: (0, 0, 0, 1), scale: (1, 1, 1)),
            matter: LightProbes(half_extents: (30, 5, 30)),
        ),
    ],
)"#;
        let doc = from_ron(BARE).expect("a hand-written volume loads");
        let MatterDoc::LightProbes { spacing, quality, bounces, enabled, intensity, .. } =
            &doc.nodes[0].matter
        else {
            panic!("the node is a LightProbes");
        };
        assert!(*spacing > 0.0 && *quality >= 4 && *bounces >= 1, "usable, not zeroed");
        assert!(*enabled, "a volume you typed out is on");
        assert_eq!(*intensity, 1.0);
    }

    #[test]
    fn a_light_emitter_round_trips_and_an_old_light_stays_a_point() {
        use floptle_core::LightShape as LS;
        let mut world = World::new();
        for (name, shape) in [
            ("Window", LS::Rect { width: 2.5, height: 4.0, two_sided: false }),
            ("Panel", LS::Rect { width: 1.0, height: 1.0, two_sided: true }),
            ("Bulb", LS::Sphere { radius: 0.35 }),
            ("Downlight", LS::Disk { radius: 0.6, two_sided: false }),
            ("Strip", LS::Tube { length: 3.0, radius: 0.04 }),
            ("Bare", LS::Point),
        ] {
            let e = world.spawn();
            world.insert(e, Name(name.into()));
            world.insert(e, Transform::IDENTITY);
            world.insert(
                e,
                Matter::PointLight { color: [1.0; 3], intensity: 1.0, range: 10.0, shape, shadows: false, spot_angle: floptle_core::OMNI_ANGLE, spot_softness: 0.25 },
            );
        }
        let ron = to_ron(&to_doc("lights", &world)).expect("serializes");
        let mut round = World::new();
        spawn_into(&from_ron(&ron).expect("parses"), &mut round);
        let mut seen = 0;
        for (e, m) in round.query::<Matter>() {
            let Matter::PointLight { shape, .. } = m else { continue };
            let name = round.get::<Name>(e).map(|n| n.0.clone()).unwrap_or_default();
            seen += 1;
            match (name.as_str(), shape) {
                ("Window", LS::Rect { width, height, two_sided }) => {
                    assert_eq!((*width, *height, *two_sided), (2.5, 4.0, false));
                }
                ("Panel", LS::Rect { two_sided, .. }) => assert!(*two_sided),
                ("Bulb", LS::Sphere { radius }) => assert_eq!(*radius, 0.35),
                ("Downlight", LS::Disk { radius, .. }) => assert_eq!(*radius, 0.6),
                ("Strip", LS::Tube { length, radius }) => assert_eq!((*length, *radius), (3.0, 0.04)),
                ("Bare", LS::Point) => {}
                other => panic!("{other:?} came back as a different emitter"),
            }
        }
        assert_eq!(seen, 6, "every light survived");

        // A bare point writes NOTHING extra. Every light in every scene that
        // exists is one, and a release that rewrites all of them on first save
        // is a release that shows up as a diff nobody asked for.
        let just_a_point = to_ron(&{
            let mut w = World::new();
            let e = w.spawn();
            w.insert(e, Name("Bare".into()));
            w.insert(e, Transform::IDENTITY);
            w.insert(e, Matter::PointLight {
                color: [1.0; 3],
                intensity: 1.0,
                range: 10.0,
                shape: LS::Point,
                shadows: false, spot_angle: floptle_core::OMNI_ANGLE, spot_softness: 0.25,
            });
            to_doc("one", &w)
        })
        .expect("serializes");
        assert!(!just_a_point.contains("shape"), "a point light writes no emitter: {just_a_point}");
        // …and no shadow flag either, so a lamp placed before local shadows
        // existed round-trips byte-identically. Matched against `shadows: false`
        // rather than `shadows`, because the LIGHTING node in every scene writes
        // a `shadows: true` of its own and a bare substring test passes on that
        // whatever the lamp does.
        assert!(
            !just_a_point.contains("shadows: false"),
            "a lamp that casts nothing must write nothing: {just_a_point}"
        );

        // A hand-typed emitter with a zero dimension is a degenerate polygon
        // whose integral divides by zero. The scene file does not get to hand
        // that to the shader.
        const FLAT: &str = r#"(
    name: "lights",
    nodes: [
        (
            name: "Window",
            transform: (translation: (0, 2, 0), rotation: (0, 0, 0, 1), scale: (1, 1, 1)),
            matter: PointLight(shape: Rect(width: 0, height: 0)),
        ),
    ],
)"#;
        let doc = from_ron(FLAT).expect("a hand-written emitter loads");
        let MatterDoc::PointLight { shape, .. } = &doc.nodes[0].matter else {
            panic!("the node is a light");
        };
        let LS::Rect { width, height, .. } = shape.to_shape() else { panic!("still a rect") };
        assert!(width > 0.0 && height > 0.0, "a zero-size emitter is clamped, not passed on");
    }

    #[test]
    fn contact_shadow_settings_round_trip_and_default_off() {
        let authored = Light {
            contact_shadows: true,
            contact_length: 0.8,
            contact_steps: 24,
            contact_strength: 0.55,
            ..Light::default()
        };
        let doc = LightDoc::from(&authored);
        let ron = ron::ser::to_string(&doc).expect("serializes");
        let back: LightDoc = ron::from_str(&ron).expect("parses");
        assert_eq!(back.to_light(), authored, "every contact knob round trips");

        // A scene written before this existed must arrive with contact shadows
        // OFF. They cost a trace per lit fragment, and a release that silently
        // switches on a per-fragment cost in every existing project is a release
        // that reads as "the update made my game slower".
        let old: LightDoc = ron::from_str(
            "(direction: (0.4, 0.9, 0.45), color: (1, 1, 1), ambient: (0.1, 0.1, 0.1), intensity: 1)",
        )
        .expect("a pre-contact Lighting block still loads");
        assert!(!old.to_light().contact_shadows, "an old scene does not start paying");

        // …and the loader clamps, because a hand-typed 0-step trace divides by
        // its own step count.
        let wild: LightDoc = ron::from_str(
            "(direction: (0, 1, 0), color: (1, 1, 1), ambient: (0, 0, 0), intensity: 1, \
             contact_shadows: true, contact_steps: 0, contact_length: 900)",
        )
        .expect("parses");
        let l = wild.to_light();
        assert_eq!(l.contact_steps, 2, "a zero step count is clamped, not honoured");
        assert_eq!(l.contact_length, 20.0, "and a wild reach is fenced");
    }

    /// A body's slope limit round-trips, and a scene written before it existed
    /// arrives at the 60° this used to be fixed at — because the limit now
    /// decides what counts as ground, so a different default would silently
    /// change where every existing character can stand.
    #[test]
    fn a_slope_limit_round_trips_and_an_old_body_keeps_the_old_angle() {
        let authored = floptle_core::RigidBody { slope_limit: 38.5, ..Default::default() };
        let doc = RigidBodyDoc::from_rigidbody(&authored);
        let ron = ron::ser::to_string(&doc).expect("serializes");
        let back: RigidBodyDoc = ron::from_str(&ron).expect("parses");
        assert_eq!(back.to_rigidbody().slope_limit, 38.5);

        let old: RigidBodyDoc =
            ron::from_str("(capsule: true, radius: 0.4, height: 1.8)").expect("an old body loads");
        assert_eq!(old.to_rigidbody().slope_limit, 60.0, "the angle it always had");

        // A default body does not write the field at all, so scenes do not all
        // grow a line the day this shipped.
        let plain = RigidBodyDoc::from_rigidbody(&floptle_core::RigidBody::default());
        assert!(
            !ron::ser::to_string(&plain).expect("serializes").contains("slope_limit"),
            "the default is omitted"
        );

        // Hand-typed nonsense is fenced: a limit past vertical has no cosine
        // that means anything.
        let wild: RigidBodyDoc = ron::from_str("(slope_limit: 400)").expect("parses");
        assert_eq!(wild.to_rigidbody().slope_limit, 90.0);
    }

    #[test]
    fn lit_fog_round_trips_and_an_old_scene_arrives_lit() {
        let authored = Light {
            fog: true,
            fog_volumetric: true,
            fog_light: 2.25,
            fog_anisotropy: -0.4,
            fog_steps: 40,
            fog_shafts: false,
            ..Light::default()
        };
        let doc = LightDoc::from(&authored);
        let ron = ron::ser::to_string(&doc).expect("serializes");
        let back: LightDoc = ron::from_str(&ron).expect("parses");
        assert_eq!(back.to_light(), authored, "every injection knob round trips");

        // A `Lighting` block written before light injection existed. The defaults
        // land it on LIT rather than on the flat colour, deliberately: fog that
        // ignores the sun standing behind it is what made volumetric mode read as
        // a grey wash, and a scene saved before the fix wants the fix.
        let old: LightDoc = ron::from_str(
            "(direction: (0.4, 0.9, 0.45), color: (1, 1, 1), ambient: (0.1, 0.1, 0.1), \
             intensity: 1, fog: true, fog_volumetric: true, fog_density: 0.03)",
        )
        .expect("a pre-injection Lighting block still loads");
        let l = old.to_light();
        assert!(l.fog_light > 0.0, "an old volumetric scene arrives lit, not flat");
        assert!(l.fog_shafts, "and with its beams on");
        assert!(l.fog_anisotropy > 0.0, "scattering forward, which is what makes them read");
        assert_eq!(l.fog_density, 0.03, "without disturbing what the scene did say");

        // The march is bounded on BOTH ends: 0 steps is a divide, and an
        // unbounded one is a hang no scene should be able to author.
        let wild: LightDoc =
            ron::from_str("(direction: (0, 1, 0), color: (1, 1, 1), ambient: (0, 0, 0), \
                           intensity: 1, fog_steps: 100000)")
                .expect("parses");
        assert_eq!(wild.to_light().fog_steps, 64, "a wild step count is clamped, not honoured");
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
            posterize_chroma: true,
            tonemap: 2,
            // The look chain, all non-default, so the round trip is proving the
            // NEW knobs survive it and not just that the old ones still do.
            exposure: -0.75,
            contrast: 1.4,
            saturation: 0.6,
            temperature: 0.3,
            tint: -0.2,
            lift: 0.05,
            grade_gamma: 0.85,
            gain: 1.2,
            aberration: 0.4,
            distortion: -0.15,
            sharpen: 0.7,
            denoise: 0.35,
            grain: 0.25,
            grain_size: 3.0,
            dof_focus: 12.5,
            dof_range: 2.5,
            dof_near_range: 0.8,
            dof_max_blur: 6.0,
            dof_blades: 6,
            dof_blade_rotation: 0.4,
            dof_highlight: 2.0,
            dof_quality: 32,
            motion_blur: 0.5,
            motion_samples: 20,
            dof_show_focus: true,
            dof_focus_node: "Hero".into(),
            // Two passes, one of them switched off and one with a knob override:
            // the ORDER, the switch and the params all have to survive, and a
            // Vec of structs is exactly where a round trip quietly loses one.
            screen_shaders: vec![
                floptle_core::ScreenShader {
                    shader: "shaders/examples/inkOutline.flsl".into(),
                    enabled: true,
                    params: [("thickness".to_string(), [2.0, 0.0, 0.0, 0.0])]
                        .into_iter()
                        .collect(),
                },
                floptle_core::ScreenShader {
                    shader: "shaders/examples/crtScanlines.flsl".into(),
                    enabled: false,
                    params: Default::default(),
                },
            ],
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

    #[test]
    fn skybox_shader_params_round_trip() {
        // A sky shader plus its Inspector knob overrides survive World → RON → World.
        // These `shader_params` are what make the built-in skies customizable templates.
        let mut world = World::new();
        let e = world.spawn();
        world.insert(e, Name("Skybox".into()));
        world.insert(e, Transform::IDENTITY);
        let mut params = std::collections::BTreeMap::new();
        params.insert("cover".into(), [0.85, 0.0, 0.0, 0.0]);
        params.insert("zenith".into(), [0.1, 0.2, 0.6, 1.0]);
        let authored = Matter::Skybox {
            color: [0.5, 0.5, 0.52],
            size: 500.0,
            texture: None,
            tint: [1.0, 1.0, 1.0],
            shader: Some("assets/shaders/examples/dayBreeze.flsl".into()),
            shader_params: params,
        };
        world.insert(e, authored.clone());

        let text = to_ron(&to_doc("sky", &world)).unwrap();
        let mut world2 = World::new();
        spawn_into(&from_ron(&text).unwrap(), &mut world2);

        let skies: Vec<_> =
            world2.query::<Matter>().filter(|(_, m)| matches!(m, Matter::Skybox { .. })).collect();
        assert_eq!(skies.len(), 1, "self-heal must not duplicate an authored Skybox");
        assert_eq!(*skies[0].1, authored, "sky shader + knob overrides lost in round-trip");
    }

    /// A scene doc for additive tests: two roots, one with a child.
    fn layer_doc(name: &str) -> SceneDoc {
        let mut d = demo();
        d.name = name.into();
        d.nodes = vec![plain("root", 1, None), plain("child", 2, Some(1)), plain("other", 3, None)];
        d
    }

    /// An additive load brings NODES and nothing else. A second sun, a second
    /// skybox or a second post-processing chain would leave the world's
    /// environment decided by query order — the failure reads as "the additive
    /// scene broke my lighting", which is nobody's first guess.
    #[test]
    fn an_additive_load_brings_no_second_environment() {
        let mut world = World::new();
        spawn_into(&layer_doc("base"), &mut world);
        let lights_before = world.query::<floptle_core::Light>().count();
        let skies_before =
            world.query::<Matter>().filter(|(_, m)| matches!(m, Matter::Skybox { .. })).count();
        let posts_before = world
            .query::<Matter>()
            .filter(|(_, m)| matches!(m, Matter::PostProcess { .. }))
            .count();
        assert_eq!((lights_before, skies_before, posts_before), (1, 1, 1), "base scene singletons");

        spawn_additive(&layer_doc("props"), &mut world, "props");

        assert_eq!(world.query::<floptle_core::Light>().count(), 1, "a second Lighting node");
        assert_eq!(
            world.query::<Matter>().filter(|(_, m)| matches!(m, Matter::Skybox { .. })).count(),
            1,
            "a second Skybox"
        );
        assert_eq!(
            world.query::<Matter>().filter(|(_, m)| matches!(m, Matter::PostProcess { .. })).count(),
            1,
            "a second PostProcess"
        );
    }

    /// `unload` takes back exactly what the matching `load` brought, and the
    /// base scene is never a candidate — you cannot unload the world you opened
    /// out from under yourself.
    #[test]
    fn unload_removes_its_own_layer_and_only_that() {
        let mut world = World::new();
        spawn_into(&layer_doc("base"), &mut world);
        let base = world.len();

        spawn_additive(&layer_doc("props"), &mut world, "props");
        spawn_additive(&layer_doc("enemies"), &mut world, "enemies");
        assert_eq!(world.len(), base + 6, "two three-node layers");

        assert_eq!(despawn_tagged(&mut world, "props"), 3);
        assert_eq!(world.len(), base + 3, "only the props layer went");

        // An unknown tag is a no-op, not a world-clearing wildcard.
        assert_eq!(despawn_tagged(&mut world, "nothing-by-that-name"), 0);
        assert_eq!(world.len(), base + 3);

        assert_eq!(despawn_tagged(&mut world, "enemies"), 3);
        assert_eq!(world.len(), base, "back to the base scene exactly");
    }

    /// A node parented under an additive layer leaves WITH the layer, even
    /// though nothing tagged it — otherwise unloading a room leaves the
    /// projectile fired inside it orphaned, drawing at a pose derived from a
    /// parent that no longer exists.
    #[test]
    fn unload_takes_children_spawned_into_the_layer() {
        let mut world = World::new();
        spawn_into(&layer_doc("base"), &mut world);
        let base = world.len();
        let ents = spawn_additive(&layer_doc("room"), &mut world, "room");

        // Something the GAME spawned later, parented into the room.
        let bullet = world.spawn();
        world.insert(bullet, Transform::IDENTITY);
        world.insert(bullet, Matter::Empty);
        world.insert(bullet, floptle_core::Name("bullet".into()));
        world.insert(bullet, floptle_core::Parent(ents[0]));
        assert!(world.get::<floptle_core::SceneTag>(bullet).is_none(), "untagged on purpose");

        assert_eq!(despawn_tagged(&mut world, "room"), 4, "3 tagged + the child");
        assert_eq!(world.len(), base);
        assert!(!world.is_alive(bullet), "an orphan is worse than a removal");
    }

    /// Persistence is a SUBTREE rule: marking a folder carries everything under
    /// it. A child left behind when its parent survived would be the same trap
    /// as a visible child under a disabled parent.
    #[test]
    fn persistence_is_inherited_down_the_subtree() {
        let mut world = World::new();
        let ents = spawn_nodes(&layer_doc("base").nodes, &mut world);
        let (root, child, other) = (ents[0], ents[1], ents[2]);
        assert!(!floptle_core::is_persistent(&world, root));

        world.insert(root, floptle_core::Persistent);
        assert!(floptle_core::is_persistent(&world, root));
        assert!(floptle_core::is_persistent(&world, child), "a child of a keeper is a keeper");
        assert!(!floptle_core::is_persistent(&world, other), "and a sibling is not");
    }

    /// Spot lights: the cone round-trips, a lamp nobody aimed writes nothing at
    /// all, and a spot with a deliberately HARD edge keeps its zero.
    ///
    /// That last one is why the two numbers live in one optional field rather
    /// than as two defaulted ones. `skip_serializing_if` cannot see a sibling,
    /// so `softness: 0.0` would have been skipped as "the default" and come back
    /// as the default softness — a hard-edged spot that goes soft on reload, in
    /// a scene file that looks correct.
    #[test]
    fn a_spot_round_trips_its_cone_and_an_unaimed_lamp_writes_nothing() {
        let mut world = World::new();
        let e = world.spawn();
        world.insert(e, floptle_core::transform::Transform::IDENTITY);
        world.insert(e, floptle_core::Name("Spot".into()));
        world.insert(
            e,
            Matter::PointLight {
                color: [1.0, 0.95, 0.85],
                intensity: 4.0,
                range: 14.0,
                shape: Default::default(),
                shadows: true,
                spot_angle: 37.5,
                // Hard edge, on purpose.
                spot_softness: 0.0,
            },
        );
        let text = to_ron(&to_doc("spot", &world)).unwrap();
        assert!(text.contains("spot"), "the cone did not reach the file: {text}");

        let back = from_ron(&text).unwrap();
        let mut w2 = World::new();
        spawn_into(&back, &mut w2);
        let (angle, softness) = w2
            .query::<Matter>()
            .find_map(|(_, m)| match m {
                Matter::PointLight { spot_angle, spot_softness, .. } => {
                    Some((*spot_angle, *spot_softness))
                }
                _ => None,
            })
            .expect("the spot came back");
        assert_eq!(angle, 37.5);
        assert_eq!(softness, 0.0, "a hard edge must survive a save and a load");

        // …and a lamp nobody aimed writes no cone at all, so every scene
        // authored before spots existed is byte-identical through a round trip.
        let mut plain = World::new();
        let p = plain.spawn();
        plain.insert(p, floptle_core::transform::Transform::IDENTITY);
        plain.insert(p, floptle_core::Name("Lamp".into()));
        plain.insert(
            p,
            Matter::PointLight {
                color: [1.0; 3],
                intensity: 1.0,
                range: 10.0,
                shape: Default::default(),
                shadows: false,
                spot_angle: floptle_core::OMNI_ANGLE,
                spot_softness: 0.25,
            },
        );
        let text = to_ron(&to_doc("plain", &plain)).unwrap();
        assert!(!text.contains("spot"), "an unaimed lamp wrote a cone: {text}");

        // A scene file from before this existed loads as the omnidirectional
        // light it was — not as a spot with a zero-width beam, which would read
        // as every lamp in an old project having gone out.
        let old = from_ron(&text).unwrap();
        let mut w3 = World::new();
        spawn_into(&old, &mut w3);
        let a = w3
            .query::<Matter>()
            .find_map(|(_, m)| match m {
                Matter::PointLight { spot_angle, .. } => Some(*spot_angle),
                _ => None,
            })
            .unwrap();
        assert!(!floptle_core::is_spot(a), "an old lamp came back aimed: {a}");
    }
}
