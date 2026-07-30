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
    /// **Spritesheet**: the base `texture` is a `sheet_cols` × `sheet_rows` grid
    /// of frames and the surface draws exactly one of them — [`Material::cell`],
    /// row-major from the top-left. `0` in either dimension = not a sheet (the
    /// whole image, exactly as before), which is also how a texture's asset
    /// settings spell "no grid". Same field set and same cell order as a UI
    /// image, so one texture reads identically in a HUD and on a mesh.
    pub sheet_cols: u32,
    pub sheet_rows: u32,
    /// Which cell of the sheet this surface draws — the sprite-animation knob:
    /// step it per frame from a script (`setMaterial{ cell = n }` /
    /// `getcomponent("Material").cell`) or key it on a stepped property track.
    /// Clamped into the grid, so a walk-off shows the last frame, not garbage.
    pub cell: u32,
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
            sheet_cols: 0,
            sheet_rows: 0,
            cell: 0,
        }
    }
}

impl Material {
    /// A plain matte material of the given base color.
    pub fn tinted(color: [f32; 3]) -> Self {
        Self { color, ..Self::default() }
    }

    /// The spritesheet grid, clamped to at least 1×1 — `(1, 1)` means "not a
    /// sheet" (the un-set spelling is a zero in either dimension).
    pub fn sheet(&self) -> (u32, u32) {
        (self.sheet_cols.max(1), self.sheet_rows.max(1))
    }

    /// Is the base texture sliced into more than one cell?
    pub fn is_sheet(&self) -> bool {
        let (c, r) = self.sheet();
        c * r > 1
    }

    /// The UV sub-rect `[min_u, min_v, max_u, max_v]` the current [`cell`](Self::cell)
    /// occupies — the whole texture `[0, 0, 1, 1]` when this isn't a sheet.
    /// Row-major from the top-left, matching `floptle_ui::ImageSpec::cell_uv`.
    pub fn cell_uv(&self) -> [f32; 4] {
        let (cols, rows) = self.sheet();
        let n = cols * rows;
        if n <= 1 {
            return [0.0, 0.0, 1.0, 1.0];
        }
        let cell = self.cell.min(n - 1);
        let (cx, cy) = (cell % cols, cell / cols);
        let (du, dv) = (1.0 / cols as f32, 1.0 / rows as f32);
        [cx as f32 * du, cy as f32 * dv, (cx + 1) as f32 * du, (cy + 1) as f32 * dv]
    }

    /// The tiling the RENDERER should pack for this material: a sheet becomes a
    /// UV window onto its own cell, so sprite indexing costs no new instance
    /// lanes, no shader variant, and reaches a custom `.flsl`'s `baseTexture()`
    /// for free.
    ///
    /// A sheet **wins over** an authored `tiling` block: repeating or rotating a
    /// single cell would drag in its neighbours, which is never what the artist
    /// meant (the same rule UI images follow in `ImageSpec::tiled_uv`).
    pub fn effective_tiling(&self) -> Option<Tiling> {
        if !self.is_sheet() {
            return self.tiling;
        }
        let [u0, v0, u1, v1] = self.cell_uv();
        // `base_texel` in raster.wgsl scales UVs about the 0.5 CENTER before
        // adding the offset, so the window's offset is its own centre minus that
        // one — not its corner.
        Some(Tiling::Uv {
            count: [u1 - u0, v1 - v0],
            offset: [(u0 + u1) * 0.5 - 0.5, (v0 + v1) * 0.5 - 0.5],
            rotation: 0.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet(cols: u32, rows: u32, cell: u32) -> Material {
        Material { sheet_cols: cols, sheet_rows: rows, cell, ..Material::default() }
    }

    /// Cells walk left-to-right then down, exactly like a UI image's grid.
    #[test]
    fn cell_uv_walks_the_grid_row_major() {
        assert_eq!(sheet(4, 2, 0).cell_uv(), [0.0, 0.0, 0.25, 0.5]);
        assert_eq!(sheet(4, 2, 5).cell_uv(), [0.25, 0.5, 0.5, 1.0]);
        assert_eq!(sheet(4, 2, 7).cell_uv(), [0.75, 0.5, 1.0, 1.0]);
        // Past the end shows the last frame; a 1×1 "sheet" is the whole image.
        assert_eq!(sheet(4, 2, 99).cell_uv(), [0.75, 0.5, 1.0, 1.0]);
        assert_eq!(sheet(1, 1, 3).cell_uv(), [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(Material::default().cell_uv(), [0.0, 0.0, 1.0, 1.0]);
    }

    /// The packed tiling must reproduce the cell rect through the renderer's
    /// centre-scaled transform: `uv' = (uv - 0.5) * count + 0.5 + offset` has to
    /// carry the quad's corners onto the cell's corners.
    #[test]
    fn effective_tiling_maps_the_quad_onto_its_cell() {
        for (cols, rows, cell) in [(4, 4, 0), (4, 4, 6), (21, 1, 20), (3, 5, 14), (2, 2, 3)] {
            let m = sheet(cols, rows, cell);
            let Some(Tiling::Uv { count, offset, rotation }) = m.effective_tiling() else {
                panic!("a sheet must pack as a Uv window");
            };
            assert_eq!(rotation, 0.0, "a cell window is never rotated");
            let at = |uv: f32, i: usize| (uv - 0.5) * count[i] + 0.5 + offset[i];
            let [u0, v0, u1, v1] = m.cell_uv();
            for (got, want) in
                [(at(0.0, 0), u0), (at(1.0, 0), u1), (at(0.0, 1), v0), (at(1.0, 1), v1)]
            {
                assert!((got - want).abs() < 1e-6, "{got} != {want} for {cols}x{rows} #{cell}");
            }
        }
    }

    /// No sheet ⇒ the artist's own tiling block, untouched. A sheet ⇒ the cell
    /// window wins (tiling a cell would sample its neighbours).
    #[test]
    fn a_sheet_overrides_an_authored_tiling_block() {
        let tri = Tiling::triplanar();
        let mut m = Material { tiling: Some(tri), ..Material::default() };
        assert_eq!(m.effective_tiling(), Some(tri));
        m.sheet_cols = 4;
        m.sheet_rows = 4;
        assert!(matches!(m.effective_tiling(), Some(Tiling::Uv { .. })));
        // A 1×1 grid is not a sheet, so it must not steal the tiling block.
        let one = Material { tiling: Some(tri), sheet_cols: 1, sheet_rows: 1, ..Material::default() };
        assert_eq!(one.effective_tiling(), Some(tri));
    }
}

/// Per-sub-object material overrides on a Mesh node (a component): object name
/// (or, for a flattened single-object model, material name) ⏵ the Material that
/// part draws with — so ONE object inside a multi-part model can be re-skinned
/// without touching its siblings. A node-level [`Material`] still overrides the
/// whole model; entries here win for their object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ObjectMaterials(pub std::collections::BTreeMap<String, Material>);
