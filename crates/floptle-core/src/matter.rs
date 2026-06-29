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

/// What an entity is made of, interpreted by the renderer. Placed via the
/// entity's `Transform`; deliberately free of GPU handles.
#[derive(Clone, Debug, PartialEq)]
pub enum Matter {
    /// A lit, textured polygon primitive.
    Primitive { shape: Shape, color: [f32; 3] },
    /// Raymarched analytic SDF "blob" (morphing smin-blended spheres).
    Blob { scale: f32 },
}
