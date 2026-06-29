//! Render-agnostic "what an entity is made of" components — the data a scene file
//! places and the editor edits. The render loop interprets these (plus the
//! entity's [`Transform`](crate::transform::Transform)) into draw commands; the
//! components themselves hold no GPU handles, so they serialize cleanly and the
//! same world can be authored, saved, and replayed.

/// A human-facing name for an entity (shown in the editor hierarchy).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name(pub String);

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
}
