//! Render-agnostic "what an entity is made of" components — the data a scene file
//! places and the editor edits. The render loop interprets these (plus the
//! entity's [`Transform`](crate::transform::Transform)) into draw commands; the
//! components themselves hold no GPU handles, so they serialize cleanly and the
//! same world can be authored, saved, and replayed.

/// A human-facing name for an entity (shown in the editor hierarchy).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name(pub String);

/// A scene-graph parent link: this entity's [`Transform`](crate::transform::Transform)
/// is **local** (relative to the parent), and its world transform is the parent's
/// world transform composed with it. Moving/rotating/scaling a parent therefore
/// carries all of its descendants. A node without a `Parent` is a root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parent(pub crate::ecs::Entity);

/// Rides a **bone / sub-object** of a rigged (node-preserving) `Matter::Mesh`, so a
/// weapon, emitter, or pickup follows a character's hand/arm — including under
/// animation. Lives ALONGSIDE [`Parent`]`(target)` (which keeps the node in the
/// hierarchy and serializable): `Parent` says *under which mesh*, `BoneAttach` says
/// *which bone under it*. Each frame `resolve_attachments` sets this node's LOCAL
/// transform to `bone_local · offset` (both in the mesh's model space), and the
/// ordinary [`world_transform`] parent-walk re-applies the mesh's f64 world — so the
/// attachment stays jitter-free far from the origin and every consumer (render,
/// physics, gizmo, particles) follows the bone through the one choke point.
#[derive(Clone, Debug, PartialEq)]
pub struct BoneAttach {
    /// The rigged Mesh entity this rides (kept equal to `Parent(target)`).
    pub target: crate::ecs::Entity,
    /// The skeleton node NAME (portable across re-import; resolved to an index each
    /// frame via `Skeleton::index_of`, like animation clips).
    pub bone: String,
    /// The child's transform IN THE BONE'S LOCAL SPACE — seeded on attach so the node
    /// doesn't jump, then editable to position it on the bone.
    pub offset: crate::transform::Transform,
}

/// A procedural primitive shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Cube,
    Sphere,
    Capsule,
    // Keep new shapes LAST: the renderer indexes meshes by `shape as usize`,
    // so appending preserves the existing 0/1/2 discriminants.
    Plane,
}

/// How fast an entity spins about Y (radians/sec) — a tiny demo behavior.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spin {
    pub speed: f32,
}

/// The collision shape of a [`RigidBody`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    Sphere,
    Capsule,
    /// A box, sized by [`RigidBody::half_extents`].
    Box,
}

/// Marks an entity as a dynamic physics body, centered on the entity's world
/// translation. Read by `floptle-physics` to build the sim each Play.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RigidBody {
    pub kind: BodyKind,
    pub radius: f32,
    /// Total capsule height (ignored for a sphere).
    pub height: f32,
    /// Half-extents for a `Box` body (ignored for sphere/capsule).
    pub half_extents: [f32; 3],
    /// Bounciness 0..1 (0 = no bounce).
    pub restitution: f32,
    /// Surface friction 0..1 (0 = frictionless).
    pub friction: f32,
    /// Whether the scene's gravity field pulls on this body (false = floats; it still
    /// collides and can be driven by a script).
    pub gravity: bool,
    /// Freeze world-axis translation (x, y, z) — e.g. lock Z for a 2.5D game.
    pub lock_pos: [bool; 3],
    /// Freeze the entity's rotation about each axis (keeps a body upright during play).
    pub lock_rot: [bool; 3],
}

impl Default for RigidBody {
    fn default() -> Self {
        Self {
            kind: BodyKind::Sphere,
            radius: 0.5,
            height: 2.0,
            half_extents: [0.5, 0.5, 0.5],
            restitution: 0.0,
            friction: 0.3,
            gravity: true,
            lock_pos: [false; 3],
            lock_rot: [false; 3],
        }
    }
}

/// Marks a `Matter::Mesh` node as a STATIC collider you can walk on — the editor bakes
/// its triangles (in world space) into the physics sim at Play. The model isn't a
/// dynamic body; it's environment geometry (a level/map). Presence = collidable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MeshCollider;

/// Marks ANY node as a STATIC collider auto-shaped from its geometry — the "collidable"
/// switch. At Play the editor builds the matching static collision shape sized to the
/// node's `Matter` + world transform (Cube → box, Sphere → sphere, Capsule → capsule,
/// Mesh → triangle mesh), so a primitive is collidable WITHOUT a dynamic rigidbody (just
/// like a mesh collider). Resize/reshape it by scaling/rotating the node — the collider
/// tracks the geometry. Presence = collidable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Collidable;

/// Attaches a layered animation controller asset (`*.actl.ron`) to a node. The
/// runtime it drives lives editor/runtime-side; this is just the reference —
/// the same discipline as `Matter::Mesh { asset_path }`. On a rigged Mesh node
/// it poses the model's parts; on any other node it animates the node itself +
/// its descendants (matched by scene `Name`) — cutscenes, doors, platforms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnimController {
    /// Controller asset key: project-relative path without extension
    /// (`animation_controllers/Player`).
    pub asset: String,
}

/// Attaches a particle effect asset (`*.vfx.ron`) to a node — the node becomes
/// the effect's emitter transform. Same reference discipline as [`AnimController`]:
/// the timeline/sim runtime lives editor/runtime-side (`floptle-vfx`); this is
/// just the asset key plus how playback starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParticleSystem {
    /// Effect asset key: project-relative path without extension (`vfx/360Slash`).
    pub asset: String,
    /// Start playing the moment Play begins (`false` = a script triggers it).
    pub play_on_start: bool,
}

/// Whether a node's geometry is drawn. A node with **no** `Visible` component renders
/// normally (visible is the default); attaching `Visible(false)` hides its mesh/shape
/// (it still has a transform, physics, and children). Scripts toggle it with
/// `node.visible = true/false` to show/hide visuals on the fly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Visible(pub bool);

impl Default for Visible {
    fn default() -> Self {
        Visible(true)
    }
}

/// A scene's lighting, held on a single mandatory "Lighting" node every scene
/// carries: a directional key light plus flat ambient. These are plain fields a
/// script can read and write to drive game-time light changes; the renderer turns
/// them into the frame's light. `direction` need not be unit — the renderer
/// normalizes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Light {
    pub direction: [f32; 3],
    pub color: [f32; 3],
    pub ambient: [f32; 3],
    /// Brightness multiplier on the key (directional) light color.
    pub intensity: f32,
    /// Sun shadows: the field is marched from each shaded point toward the light
    /// (SDF soft shadows), so terrain/blobs cast on everything and meshes cast via
    /// their collider proxy shapes. All the knobs below only apply when `true`.
    pub shadows: bool,
    /// 0 = razor-hard edge (PS1) … 1 = dreamy-soft penumbra. Maps to the penumbra
    /// sharpness `k` in the shadow march (analytic softness — no blur kernels).
    pub shadow_softness: f32,
    /// How dark full shadow gets, 0..1 (1 = the directional light fully blocked;
    /// ambient still fills, so the scene never goes pitch black).
    pub shadow_strength: f32,
    /// Shadows darken *toward this color* instead of plain black — purple dusk,
    /// sepia, horror green. Black = neutral darkening.
    pub shadow_tint: [f32; 3],
    /// 0 = smooth penumbra; 2..=8 = posterize it into that many bands (toon/retro).
    pub shadow_quantize: u32,
    /// Bayer-dither the penumbra (pairs with `shadow_quantize` + retro mode for the
    /// classic PS1 dithered shadow edge).
    pub shadow_dither: bool,
    /// Max distance (world units) a shadow ray marches before giving up — a perf
    /// fence; far geometry simply stops casting past it.
    pub shadow_distance: f32,

    /// Depth fog: blend everything toward `fog_color` between `fog_start` and
    /// `fog_end` world units from the camera. Dirt-cheap (one mix per fragment) and
    /// off by default. The skybox stays crisp, so match `fog_color` to the horizon
    /// (or the background color) to avoid a seam.
    pub fog: bool,
    pub fog_color: [f32; 3],
    /// World distance where fog begins (fully clear nearer than this).
    pub fog_start: f32,
    /// World distance where fog is full (fully `fog_color` past this).
    pub fog_end: f32,
    /// Dither the fog gradient to hide 8-bit banding on long, slow ramps.
    pub fog_dither: bool,
    /// Dither amplitude (0..1); scaled to a sub-percent nudge of the fog factor.
    pub fog_dither_strength: f32,
}

impl Default for Light {
    fn default() -> Self {
        Self {
            direction: [0.4, 0.9, 0.45],
            color: [1.0, 0.98, 0.92],
            ambient: [0.12, 0.12, 0.16],
            intensity: 1.0,
            shadows: true,
            shadow_softness: 0.35,
            shadow_strength: 1.0,
            shadow_tint: [0.0, 0.0, 0.0],
            shadow_quantize: 0,
            shadow_dither: false,
            shadow_distance: 150.0,
            fog: false,
            fog_color: [0.6, 0.65, 0.72],
            fog_start: 40.0,
            fog_end: 200.0,
            fog_dither: false,
            fog_dither_strength: 0.5,
        }
    }
}

/// Whether a node's collider shape casts sun shadows (as a proxy occluder in the
/// shadow march). A node with **no** `CastShadow` component casts by default —
/// attach `CastShadow(false)` to opt a collider out of shadowing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CastShadow(pub bool);

impl Default for CastShadow {
    fn default() -> Self {
        CastShadow(true)
    }
}

/// What an entity is made of, interpreted by the renderer. Placed via the
/// entity's `Transform`; deliberately free of GPU handles.
#[derive(Clone, Debug, PartialEq)]
pub enum Matter {
    /// A lit, textured polygon primitive.
    Primitive { shape: Shape, color: [f32; 3] },
    /// Raymarched analytic SDF "blob" (morphing smin-blended spheres).
    Blob { scale: f32 },
    /// An imported polygon mesh (glTF), referenced by its asset path. The renderer
    /// (editor) maps the path to its registered GPU mesh parts.
    Mesh { asset_path: String },
    /// A group / "empty" — renders nothing, but has a transform and can parent other
    /// nodes (a folder for organizing the scene, or a rig root like a player).
    Empty,
    /// Editable SDF terrain — like a blob, but a sculptable/paintable voxel field.
    /// The transform places its volume; the field data lives alongside the scene.
    /// `id` is a stable per-terrain key (Entity indices aren't stable across load),
    /// so each terrain's field file + combine slot can be matched back on reload.
    Terrain { id: u32 },
    /// A camera viewpoint — its transform is the camera pose; `fov_y` is the vertical
    /// field of view in radians. One camera holds play-mode authority at a time
    /// (`active`); the gameplay view renders from it, switchable for cutscenes.
    Camera { fov_y: f32, active: bool },
    /// A placeable point/omni light. Its world position is the node's transform
    /// translation; `range` is the radius at which its contribution falls to ~zero.
    /// (The scene's single directional/ambient key stays the special `Light` node.)
    PointLight { color: [f32; 3], intensity: f32, range: f32 },
    /// A gravity source for the physics sim — `Down` for normal-style level gravity,
    /// `Radial` for a planet (Mario-Galaxy) gravity well centered on the node.
    GravityVolume { mode: GravityMode, strength: f32, radius: f32 },
    /// The scene's environment background — a face-inverted sphere of radius `size`
    /// drawn behind everything. `color` is the solid sky color (grey by default); when
    /// `texture` is set it's sampled equirectangularly (seamless loop) and multiplied by
    /// `tint`. The node's transform rotation orients the sky (a script can spin it).
    Skybox { color: [f32; 3], size: f32, texture: Option<String>, tint: [f32; 3] },
    /// The scene's post-processing chain — a mandatory scene node (self-healed on
    /// load, like the Skybox), so every scene tunes its own look. `enabled` gates
    /// the whole chain; each effect then has its own switch and knobs. `ao` picks
    /// how ambient occlusion is computed (screen-space by default; SDF samples the
    /// real distance field). The node's transform is unused.
    PostProcess {
        enabled: bool,
        bloom: bool,
        bloom_threshold: f32,
        bloom_intensity: f32,
        vignette: bool,
        vignette_strength: f32,
        vignette_radius: f32,
        ao: AoMode,
        /// How dark full occlusion gets (0 = off, 1 = black creases).
        ao_strength: f32,
        /// Occlusion reach in world units.
        ao_radius: f32,
        /// Posterize the final image to this many color levels per channel (a limited
        /// palette / banded retro look). 0 or 1 = off; 2.. = on. Runs last, at the
        /// composited (retro) resolution, so bands land on the same chunky pixels.
        posterize_bands: u32,
        /// Ordered-dither the posterize so smooth gradients don't hard-step.
        posterize_dither: bool,
    },
}

impl Matter {
    /// The default skybox: solid mid-grey, a large radius, no texture.
    pub fn default_skybox() -> Self {
        Matter::Skybox { color: [0.5, 0.5, 0.52], size: 500.0, texture: None, tint: [1.0, 1.0, 1.0] }
    }

    /// The default post-processing node: chain on, screen-space ambient occlusion
    /// at a gentle strength, bloom and vignette off (matching the old project-wide
    /// defaults).
    pub fn default_post_process() -> Self {
        Matter::PostProcess {
            enabled: true,
            bloom: false,
            bloom_threshold: 1.0,
            bloom_intensity: 0.7,
            vignette: false,
            vignette_strength: 0.5,
            vignette_radius: 0.7,
            ao: AoMode::ScreenSpace,
            ao_strength: 0.7,
            ao_radius: 0.5,
            posterize_bands: 0,
            posterize_dither: false,
        }
    }
}

/// How a [`Matter::PostProcess`] node computes ambient occlusion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AoMode {
    /// No ambient occlusion.
    Off,
    /// Screen-space AO (SSAO): a post pass over the depth buffer. Cheap, and it
    /// darkens everything on screen — meshes and SDF matter alike. The default.
    ScreenSpace,
    /// Geometric AO sampled from the actual SDF field along the surface normal —
    /// "true" occlusion with no screen-space artifacts. Everything receives it -
    /// the raster pass marches the same field for its mesh fragments - but only
    /// SDF matter (terrain/blobs) *occludes*; meshes aren't in the field.
    Sdf,
}

/// How a [`Matter::GravityVolume`] pulls bodies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GravityMode {
    /// Constant downward (−Y) gravity everywhere — a normal-style game's level gravity.
    Down,
    /// Radial pull toward the node — a planet. `radius` bounds the gravity well.
    Radial,
}

/// The absolute (world) transform of `e`: its local [`Transform`] composed under
/// every ancestor's, so a parent's placement carries its descendants. Roots return
/// their own transform. The walk is bounded to guard against accidental cycles.
pub fn world_transform(world: &crate::ecs::World, e: crate::ecs::Entity) -> crate::transform::Transform {
    use crate::transform::Transform;
    let mut t = world.get::<Transform>(e).copied().unwrap_or(Transform::IDENTITY);
    let mut cur = e;
    for _ in 0..64 {
        let Some(Parent(p)) = world.get::<Parent>(cur).copied() else { break };
        let plocal = world.get::<Transform>(p).copied().unwrap_or(Transform::IDENTITY);
        t = plocal.mul_transform(&t);
        cur = p;
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::World;
    use crate::math::DVec3;
    use crate::transform::Transform;

    #[test]
    fn parent_carries_child() {
        let mut w = World::default();
        let p = w.spawn();
        w.insert(p, Transform::from_translation(DVec3::new(2.0, 0.0, 0.0)));
        let c = w.spawn();
        w.insert(c, Transform::from_translation(DVec3::new(0.0, 1.0, 0.0)));
        w.insert(c, Parent(p));
        // child's local (0,1,0) under parent at (2,0,0) -> world (2,1,0)
        let wt = world_transform(&w, c);
        assert!((wt.translation - DVec3::new(2.0, 1.0, 0.0)).length() < 1e-9, "{:?}", wt.translation);
        // grandchild stacks too
        let g = w.spawn();
        w.insert(g, Transform::from_translation(DVec3::new(0.0, 0.0, 3.0)));
        w.insert(g, Parent(c));
        let gt = world_transform(&w, g);
        assert!((gt.translation - DVec3::new(2.0, 1.0, 3.0)).length() < 1e-9, "{:?}", gt.translation);
    }

    #[test]
    fn parent_rotation_carries_child() {
        use crate::math::{Quat, Vec3};
        let mut w = World::default();
        let p = w.spawn();
        w.insert(
            p,
            Transform {
                rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
                ..Transform::IDENTITY
            },
        );
        let c = w.spawn();
        w.insert(c, Transform::from_translation(DVec3::new(1.0, 0.0, 0.0)));
        w.insert(c, Parent(p));
        // +X spun 90° about Y → -Z, so the child orbits to ~(0,0,-1).
        let wt = world_transform(&w, c);
        assert!((wt.translation - DVec3::new(0.0, 0.0, -1.0)).length() < 1e-5, "{:?}", wt.translation);
        // and the child inherits the parent's orientation.
        assert!((wt.rotation * Vec3::Z - (Quat::from_rotation_y(std::f32::consts::FRAC_PI_2) * Vec3::Z)).length() < 1e-5);
    }
}
