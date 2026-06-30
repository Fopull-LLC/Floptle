//! A node's surface look — the artist-facing material.
//!
//! Plain data (no GPU here): the renderer reads a [`Material`] component off an
//! entity and packs it into its instance stream. The property set is tuned for a
//! customizable PS1/PS2/N64 aesthetic — a base color, an emissive glow, a cheap
//! Blinn-Phong specular (color + shininess + strength), a rim/fresnel edge term,
//! an **unlit** (fullbright/flat) toggle, and an ambient-light multiplier.

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
        }
    }
}

impl Material {
    /// A plain matte material of the given base color.
    pub fn tinted(color: [f32; 3]) -> Self {
        Self { color, ..Self::default() }
    }
}
