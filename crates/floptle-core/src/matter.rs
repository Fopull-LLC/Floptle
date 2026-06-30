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

/// A procedural primitive shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Cube,
    Sphere,
    Capsule,
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
}

/// Marks an entity as a dynamic physics body, centered on the entity's world
/// translation. Read by `floptle-physics` to build the sim each Play.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RigidBody {
    pub kind: BodyKind,
    pub radius: f32,
    /// Total capsule height (ignored for a sphere).
    pub height: f32,
    /// Bounciness 0..1 (0 = no bounce).
    pub restitution: f32,
    /// Surface friction 0..1 (0 = frictionless).
    pub friction: f32,
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
            restitution: 0.0,
            friction: 0.3,
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
}

impl Default for Light {
    fn default() -> Self {
        Self {
            direction: [0.4, 0.9, 0.45],
            color: [1.0, 0.98, 0.92],
            ambient: [0.12, 0.12, 0.16],
            intensity: 1.0,
        }
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
