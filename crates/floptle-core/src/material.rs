//! A node's surface look — the artist-facing material.
//!
//! Plain data (no GPU here): the renderer reads a [`Material`] component off an
//! entity and packs it into its instance stream. The property set is tuned for a
//! customizable PS1/PS2/N64 aesthetic — a base color, an emissive glow, a cheap
//! Blinn-Phong specular (color + shininess + strength), a rim/fresnel edge term,
//! an **unlit** (fullbright/flat) toggle, and an ambient-light multiplier.

/// How a texture binding tiles across a surface — per BINDING (this material's
/// use of the image), while wrap/filter stay per-texture settings. The
/// "drag on and tile, no shader required" block (proposal §8).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tiling {
    /// Transform the mesh UVs: `count` repeats across the 0..1 span, scrolled
    /// by `offset`, rotated by `rotation` degrees around the UV center.
    Uv { count: [f32; 2], offset: [f32; 2], rotation: f32 },
    /// Project from the three object axes and blend by the surface normal —
    /// clean tiling on shapes with stretched or absent UVs. `scale` = tile
    /// size in object units, `blend` = axis-edge sharpness.
    Triplanar { scale: f32, blend: f32 },
}

impl Tiling {
    pub fn uv() -> Self {
        Tiling::Uv { count: [1.0, 1.0], offset: [0.0, 0.0], rotation: 0.0 }
    }
    pub fn triplanar() -> Self {
        Tiling::Triplanar { scale: 1.0, blend: 4.0 }
    }
}

/// The surface look attached to a node (a component). Default is a plain white
/// matte — applying it changes nothing until the artist dials in properties.
#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    /// A base-color texture (project-relative path), sampled over the shape and
    /// multiplied by `color`. `None` = use the shape's own texture / flat color.
    pub texture: Option<String>,
    /// Base color tint (multiplies any texture).
    pub color: [f32; 3],
    /// Emissive color and its strength (glow that ignores lighting).
    pub emissive: [f32; 3],
    pub emissive_strength: f32,
    /// Specular highlight color, its Blinn-Phong exponent, and strength.
    pub specular: [f32; 3],
    pub shininess: f32,
    pub specular_strength: f32,
    /// Rim/fresnel edge color and strength.
    pub rim: [f32; 3],
    pub rim_strength: f32,
    /// Ignore scene lighting entirely (flat fullbright — the classic retro look).
    pub unlit: bool,
    /// Multiplier on the scene ambient term (0 = pure black shadows).
    pub ambient: f32,
    /// Opacity (1 = fully opaque, 0 = invisible). Below 1 the surface alpha-blends
    /// over what's behind it; multiplied by any base-color texture's own alpha.
    pub alpha: f32,
    /// A custom `.flsl` shader (project-relative path) — the shader-IR path
    /// (ADR-0007). `None` = the built-in look above. When set, the shader's
    /// exposed uniforms/texture slots (below) drive the surface; the fields
    /// above still feed it (`instanceColor`, `litSurface`'s specular/rim) and
    /// the base `texture` remains its `baseTexture()`.
    pub shader: Option<String>,
    /// Overrides for the shader's exposed uniforms (name → one vec4 slot,
    /// unused lanes zero). Absent names use the shader's declared defaults.
    pub shader_params: std::collections::BTreeMap<String, [f32; 4]>,
    /// Texture bindings for the shader's declared slots (slot name → project-
    /// relative texture path). Absent slots bind a 1×1 white.
    pub shader_textures: std::collections::BTreeMap<String, String>,
    /// How the base `texture` tiles (`None` = plain mesh UVs, exactly as
    /// before). Applies to the built-in look AND a shader's `baseTexture()`.
    pub tiling: Option<Tiling>,
    /// Per-slot tiling for the shader's texture slots (absent = plain UVs) —
    /// honored by the stdlib `sample()` / `sampleTriplanar()` ops.
    pub shader_tiling: std::collections::BTreeMap<String, Tiling>,
}

impl Default for Material {
    fn default() -> Self {
        Self {
            texture: None,
            color: [1.0, 1.0, 1.0],
            emissive: [0.0, 0.0, 0.0],
            emissive_strength: 0.0,
            specular: [1.0, 1.0, 1.0],
            shininess: 16.0,
            specular_strength: 0.0,
            rim: [0.0, 0.0, 0.0],
            rim_strength: 0.0,
            unlit: false,
            ambient: 1.0,
            alpha: 1.0,
            shader: None,
            shader_params: std::collections::BTreeMap::new(),
            shader_textures: std::collections::BTreeMap::new(),
            tiling: None,
            shader_tiling: std::collections::BTreeMap::new(),
        }
    }
}

impl Material {
    /// A plain matte material of the given base color.
    pub fn tinted(color: [f32; 3]) -> Self {
        Self { color, ..Self::default() }
    }
}
