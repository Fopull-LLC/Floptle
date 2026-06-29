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
}

/// How fast an entity spins about Y (radians/sec) — a tiny demo behavior.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spin {
    pub speed: f32,
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
}

impl Default for Light {
    fn default() -> Self {
        Self {
            direction: [0.4, 0.9, 0.45],
            color: [1.0, 0.98, 0.92],
            ambient: [0.12, 0.12, 0.16],
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
