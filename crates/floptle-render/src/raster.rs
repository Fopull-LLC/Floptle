//! The forward raster pass — the seed of the mesh/material path (Phase 2).
//!
//! Draws a registry of meshes, each instanced any number of times, in a single
//! depth-tested render pass with simple directional diffuse lighting. Per-object
//! data — the **camera-relative** model matrix (`Transform::render_matrix`,
//! ADR-0015), its inverse-transpose normal matrix, and a tint — rides a
//! per-instance vertex buffer rewritten once per frame.
//!
//! Each registered mesh carries its own **base-color texture** (group 1), so an
//! imported model's per-material textures render correctly; meshes registered
//! without one get a 1×1 white default (so the tint shows through). The shared
//! sampler is **nearest-neighbor + REPEAT** — crisp, tiling pixel-art, which is
//! what low-res game textures want. Per-material shaders, transparency, and the
//! render-graph integration are later work.

use std::collections::HashMap;

use glam::Mat4;

use crate::device::Gpu;
use crate::mesh::{GpuMesh, MeshData, MeshId, TextureData, Vertex};

/// How a texture is filtered (and, for `SmoothMipmaps`, minified). The default
/// `Pixelated` is crisp nearest-neighbor — the engine's pixel-art look.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TexFilter {
    /// Nearest-neighbor — crisp pixels, no smoothing (good for pixel art).
    Pixelated,
    /// Bilinear — smooth magnification, no mipmaps.
    Smooth,
    /// Trilinear — smooth + mipmapped, so the texture doesn't shimmer/alias when
    /// minified into the distance (the quality/"compression" lever).
    SmoothMipmaps,
}

/// How a texture's coordinates wrap outside `[0,1]` (e.g. when tiled across terrain).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TexWrap {
    Repeat,
    Clamp,
    Mirror,
}

/// A texture's sampling settings (filter + wrap). Default = crisp tiling pixel-art.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TexSampling {
    pub filter: TexFilter,
    pub wrap: TexWrap,
}

impl Default for TexSampling {
    fn default() -> Self {
        Self { filter: TexFilter::Pixelated, wrap: TexWrap::Repeat }
    }
}

/// Frame-global uniform: camera view·projection and the directional light.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Globals {
    pub view_proj: [[f32; 4]; 4],
    pub light_dir: [f32; 4],
    pub light_color: [f32; 4],
    pub ambient: [f32; 4],
    /// x = active point-light count (rest pad to a vec4).
    pub point_count: [f32; 4],
    /// Up to 16 point lights: xyz = camera-relative position, w = range.
    pub point_pos: [[f32; 4]; 16],
    /// Each point light's rgb = color × intensity (w unused).
    pub point_color: [[f32; 4]; 16],
    /// Meshed-terrain splat params: x = per-slot NEAREST bitmask (bit i = palette slot i
    /// wants nearest filtering), y = triplanar world scale, z/w unused. Mirrors the
    /// raymarch's `terrain_tint.w` mask so meshed terrain filters textures identically.
    pub terrain_mask: [f32; 4],
}

impl Default for Globals {
    fn default() -> Self {
        Self {
            view_proj: [[0.0; 4]; 4],
            light_dir: [0.0; 4],
            light_color: [0.0; 4],
            ambient: [0.0; 4],
            point_count: [0.0; 4],
            point_pos: [[0.0; 4]; 16],
            point_color: [[0.0; 4]; 16],
            terrain_mask: [0.0, 0.22, 0.0, 0.0],
        }
    }
}

/// Per-instance GPU data: model matrix, inverse-transpose normal matrix (3 padded
/// columns), and a tint color.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InstanceRaw {
    pub model: [[f32; 4]; 4],
    /// The inverse-transpose normal matrix, as three vec4 columns whose `.w` is padding
    /// the shader's `mat3x3` never reads — except two lanes that were free (a vec3
    /// attribute still costs a vec4 slot) and the attribute budget is FULL at 16/16, so
    /// these are the only places a per-instance index could go without spending a slot
    /// that does not exist:
    ///   - `normal_mat[0].w` = the instance's TERRAIN color base.
    ///   - `normal_mat[1].w` = the paint-modulate flag (0/1; see `MaterialParams`).
    pub normal_mat: [[f32; 4]; 3],
    /// Base color tint (rgb) + alpha.
    pub color: [f32; 4],
    /// Emissive color (rgb) + strength (a).
    pub emissive: [f32; 4],
    /// Specular color (rgb) + specular strength (a).
    pub specular: [f32; 4],
    /// x = shininess, y = rim strength, z = unlit (0/1), w = ambient multiplier.
    pub params: [f32; 4],
    /// Rim/fresnel color (rgb); w = packed tiling flags — an exact small int:
    /// `mode (0 off | 1 uv | 2 triplanar) + round(rotation_degrees * 10) * 4`.
    pub rim: [f32; 4],
    /// Tiling data: Uv mode = (count.x, count.y, offset.x, offset.y);
    /// Triplanar = (scale, blend, 0, 0). All-zero when tiling is off.
    pub tile: [f32; 4],
}

/// The look of a surface — the artist-facing material (retro-friendly: emissive,
/// a Blinn-Phong specular, a rim/fresnel term and an unlit toggle). Packed into the
/// per-instance stream by [`instance_of_mat`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaterialParams {
    pub color: [f32; 3],
    pub emissive: [f32; 3],
    pub emissive_strength: f32,
    pub specular: [f32; 3],
    pub shininess: f32,
    pub specular_strength: f32,
    pub rim: [f32; 3],
    pub rim_strength: f32,
    pub unlit: bool,
    pub ambient: f32,
    /// Opacity (1 = opaque). Below 1 the instance is alpha-blended over the scene.
    pub alpha: f32,
    /// Base-texture tiling: 0 = off (plain mesh UVs), 1 = UV transform,
    /// 2 = triplanar. See [`InstanceRaw::tile`] for the data lanes.
    pub tile_mode: u8,
    /// Uv: (count.x, count.y, offset.x, offset.y); Triplanar: (scale, blend, 0, 0).
    pub tile: [f32; 4],
    /// UV-mode rotation in degrees around the UV center (quantized to 0.1°).
    pub tile_rotation: f32,
    /// Base offset of this instance's vertex-paint block in the `vpaint` store;
    /// 0 = unpainted (paint multiplies albedo, so unpainted reads as white).
    /// Usually [`Raster::mesh_paint_base`] — instances sharing a mesh share its block.
    pub paint_base: u32,
    /// Base offset of this instance's block in the TERRAIN color store; 0 = none.
    /// Set from [`Raster::dyn_paint_base`] for terrain chunk instances and left at 0 by
    /// everything else. Multiplies albedo exactly like `paint_base` (they are the same
    /// `vcolor` varying downstream) — an instance never has both.
    pub terrain_paint_base: u32,
    /// Modulate-2× the vertex paint (mid-grey = neutral, so paint carries BOTH light and
    /// shadow) instead of the plain darken-only multiply. True for brush paint, false for
    /// imported glTF COLOR_0 (whose spec is a linear ×1 multiply). Ignored when
    /// `paint_base == 0`. Rides the free `normal_mat[1].w` instance lane.
    pub paint_modulate: bool,
    /// Meshed-TERRAIN splat: interpret the vertex color's alpha as a 1-based palette slot
    /// and triplanar-sample the terrain palette (× the rgb tint), instead of treating alpha
    /// as opacity. True only for terrain chunk instances. Rides `normal_mat[2].w`.
    pub terrain_splat: bool,
}

impl MaterialParams {
    /// A plain matte tint — no emissive/specular/rim (what `instance_of` builds).
    pub fn flat(color: [f32; 3]) -> Self {
        Self {
            color,
            emissive: [0.0; 3],
            emissive_strength: 0.0,
            specular: [1.0; 3],
            shininess: 16.0,
            specular_strength: 0.0,
            rim: [0.0; 3],
            rim_strength: 0.0,
            unlit: false,
            ambient: 1.0,
            alpha: 1.0,
            tile_mode: 0,
            tile: [0.0; 4],
            tile_rotation: 0.0,
            paint_base: 0,
            terrain_paint_base: 0,
            paint_modulate: false,
            terrain_splat: false,
        }
    }
}

const INSTANCE_ATTRS: [wgpu::VertexAttribute; 13] = [
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 3 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16, shader_location: 4 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 32, shader_location: 5 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 48, shader_location: 6 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 64, shader_location: 7 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 80, shader_location: 8 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 96, shader_location: 9 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 112, shader_location: 10 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 128, shader_location: 11 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 144, shader_location: 12 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 160, shader_location: 13 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 176, shader_location: 14 },
    // Tiling data — location 15 (the last free slot under the 16-attribute floor).
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 192, shader_location: 15 },
];

const INSTANCE_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<InstanceRaw>() as u64,
    step_mode: wgpu::VertexStepMode::Instance,
    attributes: &INSTANCE_ATTRS,
};

/// A mesh resident on the GPU plus the bind group holding its base-color texture.
struct RegisteredMesh {
    gpu_mesh: GpuMesh,
    tex_bind: wgpu::BindGroup,
    _texture: Option<wgpu::Texture>, // kept alive for the bind group (None = default)
    /// Base offset of this mesh's vertex-paint block in `vpaint_buf`; 0 = unpainted.
    paint_base: u32,
    /// Set only for [`register_dynamic`](Raster::register_dynamic) slots — meshes whose
    /// geometry is rewritten in place. `None` = an ordinary upload-once mesh.
    dynamic: Option<DynSlot>,
}

/// A dynamic mesh slot's capacity + its paired block in the terrain color store.
/// The color block travels with the slot for its whole life (allocated on register,
/// re-used by every `replace_dynamic`, handed back on `free_dynamic`) — that pairing
/// is what keeps remeshing from leaking color memory.
struct DynSlot {
    cap_verts: u32,
    cap_indices: u32,
    /// Base offset in `tpaint_buf`; 0 = this slot carries no per-vertex color.
    tpaint_base: u32,
    /// Always == `cap_verts` when `tpaint_base != 0` (the block is sized to the slot).
    tpaint_cap: u32,
}

/// The capacity a dynamic slot gets for a mesh of `n` elements: 1.5× headroom, floored
/// at 1024 and rounded to a power of two. The rounding matters as much as the headroom:
/// the free-lists are keyed by exact capacity, so collapsing thousands of chunk sizes
/// onto a handful of size classes is what makes slot re-use actually hit.
fn slot_cap(n: u32) -> u32 {
    n.saturating_mul(3).div_ceil(2).max(1024).next_power_of_two()
}

pub struct Raster {
    pipeline: wgpu::RenderPipeline,
    /// Same as `pipeline` but alpha-blended with depth-write OFF, for instances whose
    /// material opacity is < 1. Drawn after the opaque pass so they composite over the
    /// solid scene.
    transparent_pipeline: wgpu::RenderPipeline,
    /// Silhouette-mask pipeline (solid 1.0, no depth/cull) for selection outlines.
    mask_pipeline: wgpu::RenderPipeline,
    /// Depth-only prepass pipeline (see [`depth_prepass`](Self::depth_prepass)).
    prepass_pipeline: wgpu::RenderPipeline,
    /// The prepass's own sampleable depth target (recreated on size change):
    /// bound by the raymarch as its per-pixel march cap, copied over the frame's
    /// depth buffer to prime early-z for the color pass.
    prepass_tex: Option<(wgpu::Texture, wgpu::TextureView)>,
    globals_bind: wgpu::BindGroup,
    globals_buf: wgpu::Buffer,
    /// The vertex-paint block store: every painted mesh's RGBA8 colors, packed back
    /// to back, indexed in `vs` as `vpaint[paint_base + vertex_index]`. Index 0 is a
    /// reserved dummy — `paint_base == 0` means "unpainted".
    vpaint_buf: wgpu::Buffer,
    /// CPU mirror of `vpaint_buf`, kept so blocks can be appended (and the buffer
    /// re-created when it outgrows its capacity) without a readback.
    vpaint_cpu: Vec<u32>,
    /// The TERRAIN color store: chunk-mesh per-vertex colors, read in `vs` exactly like
    /// `vpaint` but through its own base offset. Separate from `vpaint` for two reasons,
    /// both learned rather than guessed:
    ///   1. `vpaint` is bump-allocated and NEVER freed (its blocks are owned by meshes
    ///      and nodes for the scene's life). Remeshing churns blocks every sculpt dab,
    ///      so terrain in that store would leak without bound.
    ///   2. `vpaint` bases are packed into `params.z` beside the unlit bit, a budget of
    ///      ~8.3M vertices *shared with the user's painting*. A large LOD'd terrain would
    ///      quietly eat it and make the brush fail. Terrain bases live in `n0.w` — a lane
    ///      that was pure padding — with a full 2^24 range of their own.
    ///
    /// Index 0 is a reserved dummy, so base == 0 means "no terrain color" as with vpaint.
    tpaint_buf: wgpu::Buffer,
    /// High-water mark of `tpaint_buf`, in u32s. No CPU mirror: colors are regenerated
    /// by the mesher, never read back, and growth copies buffer-to-buffer on the GPU.
    tpaint_len: u32,
    /// Freed terrain-color blocks by exact capacity → their bases, for re-use.
    tpaint_free: HashMap<u32, Vec<u32>>,
    /// Freed dynamic mesh slots (indices into `meshes`), re-used by `register_dynamic`.
    dyn_free: Vec<u32>,
    /// The terrain texture palette (a `TERRAIN_SLOTS`-layer array), bound to group(0) so
    /// meshed terrain can triplanar-splat it exactly like the raymarched terrain did. Its
    /// own copy (not shared with the raymarch's) keeps the two passes decoupled; the editor
    /// uploads the same layers to both. `terrain_pal_view` is kept for bind-group rebuilds.
    terrain_pal_view: wgpu::TextureView,
    _terrain_pal: wgpu::Texture,
    /// REPEAT samplers (linear + nearest) for the palette — triplanar tiles across the
    /// surface, and pixel-art slots pick nearest via the `terrain_mask` bitmask.
    terrain_samp: wgpu::Sampler,
    terrain_samp_nearest: wgpu::Sampler,
    /// Per-slot nearest bitmask, carried into every frame's `Globals.terrain_mask.x`.
    terrain_nearest_mask: u32,
    tex_layout: wgpu::BindGroupLayout,
    /// Fallback group(2) for callers without a raymarch pass: zeroed field
    /// globals (no volumes/blobs, shadows + AO off) → the field branches skip.
    empty_field_bind: wgpu::BindGroup,
    /// One sampler per distinct [`TexSampling`], built on demand and reused (textures
    /// pick theirs by filter/wrap; samplers are cheap to share).
    samplers: HashMap<TexSampling, wgpu::Sampler>,
    default_tex: wgpu::Texture,
    instance_buf: wgpu::Buffer,
    instance_cap: u32,
    meshes: Vec<RegisteredMesh>,
    /// Standalone material textures (decoupled from meshes), bound per-instance so
    /// a Material can re-texture any shape. Indexed by [`TexId`].
    textures: Vec<TexBind>,
    /// Compiled `.flsl` fragment pipelines (ADR-0007), indexed by [`FlslShaderId`].
    flsl_shaders: Vec<FlslShader>,
    /// Live material bindings (group(3) params + textures), indexed by [`FlslBindingId`].
    flsl_bindings: Vec<FlslBinding>,
    /// The group-0/1/2 layouts, kept so flsl pipelines can be built later.
    globals_layout: wgpu::BindGroupLayout,
    field_layout: wgpu::BindGroupLayout,
    /// The scene's spliced Field Shape code `(field functions, support)` —
    /// every module built here includes it so meshes receive shape shadows/AO.
    custom_field: Option<(String, String)>,
}

/// The WGSL every raster-pass module starts from: the pass shader + the shared
/// distance-field module. Public so the editor can naga-validate a generated
/// `.flsl` chunk against the REAL seam before asking for a pipeline.
pub fn pass_prelude() -> &'static str {
    concat!(include_str!("raster.wgsl"), "\n", include_str!("field.wgsl"))
}

/// Replace a marker-delimited stub block (inclusive of both marker lines) with
/// generated code — the Field Shape splice (proposal §7). Missing markers
/// return the source untouched.
pub(crate) fn splice_block(src: &str, begin: &str, end: &str, replacement: &str) -> String {
    let (Some(b), Some(e)) = (src.find(begin), src.find(end)) else {
        return src.to_string();
    };
    let e_end = src[e..].find('\n').map(|i| e + i + 1).unwrap_or(src.len());
    format!("{}{}\n{}", &src[..b], replacement, &src[e_end..])
}

/// The raster module's source with Field Shape distance code spliced into its
/// field.wgsl half: `(field_code, support)`. `None` = the baseline prelude.
pub fn raster_custom_source(code: Option<(&str, &str)>) -> String {
    match code {
        None => pass_prelude().to_string(),
        Some((field, support)) => {
            let s = splice_block(
                pass_prelude(),
                "//[flsl-field-custom-begin]",
                "//[flsl-field-custom-end]",
                field,
            );
            format!("{s}\n{support}")
        }
    }
}

/// A registered material texture: its bind group + the texture kept alive for it.
/// The view + sampling ride along so flsl material bind groups (group(3)) can
/// re-bind the same image with its own sampler.
struct TexBind {
    bind: wgpu::BindGroup,
    view: wgpu::TextureView,
    sampling: TexSampling,
    _texture: wgpu::Texture,
}

/// A handle to a material texture registered with [`Raster::register_texture`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TexId(pub u32);

/// A compiled `.flsl` fragment shader registered with
/// [`Raster::register_flsl_shader`]: one pipeline (opaque- or blended-phase)
/// whose module is `raster.wgsl + field.wgsl + the generated chunk`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlslShaderId(pub u32);

/// A material's live binding of one flsl shader: its group(3) params UBO +
/// texture slots. Created per material instance by the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlslBindingId(pub u32);

/// How a Fragment-stage `.flsl` shader composites (mirror of the shader IR's
/// blend declaration — kept local so floptle-render stays decoupled).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlslBlend {
    Opaque,
    Alpha,
    Additive,
}

struct FlslShader {
    pipeline: wgpu::RenderPipeline,
    group3_layout: wgpu::BindGroupLayout,
    tex_slots: usize,
    /// Opaque-phase shaders draw with the opaque bucket (depth write on);
    /// blended ones draw after the transparent bucket (no depth write).
    opaque: bool,
    /// The generated chunk + blend, kept so a Field Shape splice can rebuild
    /// this pipeline against the new field module.
    chunk: String,
    blend: FlslBlend,
}

struct FlslBinding {
    shader: FlslShaderId,
    params_buf: wgpu::Buffer,
    bind: wgpu::BindGroup,
}

/// One custom-shader draw: mesh + optional base-texture override (group(1),
/// also what the depth prepass alpha-tests) + the material's flsl binding.
pub type FlslDraw = (MeshId, Option<TexId>, FlslBindingId, InstanceRaw);

impl Raster {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;

        // The shared distance-field module (field.wgsl) is concatenated on: the
        // fragment shader marches the raymarch pass's field (bound at group(2))
        // so meshes RECEIVE field sun-shadows and true SDF AO.
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("raster"),
            source: wgpu::ShaderSource::Wgsl(pass_prelude().into()),
        });

        // Group 0: frame globals (uniform) + the vertex-paint block store (storage).
        // `vpaint` is here, not on the per-mesh group(1), because it is ONE global
        // buffer every draw indexes with its own base offset — which is what keeps
        // painted nodes inside their instanced batches (see docs/vertex-paint-proposal.md
        // §2.1/§4.1). It is VERTEX-visible only: `vs` resolves paint to a varying.
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("raster-globals"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // Binding 2: the terrain color store (see `tpaint_buf`) — same shape and
                // same vertex-only visibility as vpaint, its own address space.
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // Binding 3/4/5: the terrain palette array + its two REPEAT samplers
                // (linear + nearest), for meshed-terrain triplanar splatting. FRAGMENT-only.
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // Group 1: the per-material base-color texture + its own sampler (so each
        // texture can choose its own filtering / wrap mode).
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("raster-texture"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        // Group 2: the shared SDF field (the raymarch pass's globals + distance
        // atlas). The editor passes `Raymarch::field_bind`; standalone callers get
        // the empty fallback below (zeroed globals → every field branch skips).
        let field_layout = crate::raymarch::field_bind_layout(device);
        let (pipeline, transparent_pipeline, mask_pipeline, prepass_pipeline) =
            Self::build_core_pipelines(gpu, &module, &globals_layout, &tex_layout, &field_layout);

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster-globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Slot 0 is a RESERVED dummy: `paint_base == 0` means "unpainted", so no real
        // block ever starts there, and the vs's clamped index always has something
        // in-bounds to read even before any paint exists.
        let vpaint_cpu: Vec<u32> = vec![0xFFFF_FFFF];
        let vpaint_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster-vpaint"),
            size: (vpaint_cpu.len() * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue.write_buffer(&vpaint_buf, 0, bytemuck::cast_slice(&vpaint_cpu));

        // The terrain color store starts as its lone reserved dummy slot, same rule.
        // COPY_SRC as well as COPY_DST: growth copies the live blocks across on the GPU
        // rather than keeping a CPU mirror (a big terrain's mirror would be tens of MB
        // of memory this store never reads).
        let tpaint_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster-tpaint"),
            size: 4,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        gpu.queue.write_buffer(&tpaint_buf, 0, bytemuck::cast_slice(&[0xFFFF_FFFFu32]));

        // The terrain palette starts all-white (no textures): meshed terrain then reads
        // white × tint = flat tint, exactly the untextured look, until a palette is set.
        let terrain_pal = crate::raymarch::make_terrain_array(gpu, &[]);
        let terrain_pal_view = terrain_pal.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        // REPEAT so triplanar tiles; one linear, one nearest (pixel-art slots).
        let repeat = |filter: wgpu::FilterMode| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("terrain-palette-samp"),
                address_mode_u: wgpu::AddressMode::Repeat,
                address_mode_v: wgpu::AddressMode::Repeat,
                address_mode_w: wgpu::AddressMode::Repeat,
                mag_filter: filter,
                min_filter: filter,
                ..Default::default()
            })
        };
        let terrain_samp = repeat(wgpu::FilterMode::Linear);
        let terrain_samp_nearest = repeat(wgpu::FilterMode::Nearest);

        let globals_bind = Self::make_globals_bind(
            device,
            &globals_layout,
            &globals_buf,
            &vpaint_buf,
            &tpaint_buf,
            &terrain_pal_view,
            &terrain_samp,
            &terrain_samp_nearest,
        );

        // 1×1 white default for meshes registered without a texture (the tint then
        // shows through unchanged).
        let default_tex = upload_texture(
            gpu,
            &TextureData { pixels: vec![255, 255, 255, 255], width: 1, height: 1 },
        );

        // The empty field fallback: a zeroed globals buffer (wgpu zero-initializes)
        // + a 1³ distance texture that's never actually sampled.
        let empty_field_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster-empty-field"),
            size: std::mem::size_of::<crate::raymarch::RaymarchGlobals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        });
        let empty_dist = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("raster-empty-field-dist"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::R16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let empty_field_samp = device.create_sampler(&wgpu::SamplerDescriptor::default());
        let empty_field_bind = crate::raymarch::make_field_bind(
            device, &field_layout, &empty_field_buf, &empty_dist, &empty_field_samp,
        );

        let instance_cap = 16;
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster-instances"),
            size: (instance_cap as u64) * std::mem::size_of::<InstanceRaw>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            transparent_pipeline,
            mask_pipeline,
            prepass_pipeline,
            prepass_tex: None,
            globals_bind,
            globals_buf,
            vpaint_buf,
            vpaint_cpu,
            tpaint_buf,
            tpaint_len: 1, // slot 0 is the reserved dummy
            tpaint_free: HashMap::new(),
            dyn_free: Vec::new(),
            terrain_pal_view,
            _terrain_pal: terrain_pal,
            terrain_samp,
            terrain_samp_nearest,
            terrain_nearest_mask: 0,
            tex_layout,
            empty_field_bind,
            samplers: HashMap::new(),
            default_tex,
            instance_buf,
            instance_cap,
            meshes: Vec::new(),
            textures: Vec::new(),
            flsl_shaders: Vec::new(),
            flsl_bindings: Vec::new(),
            globals_layout,
            field_layout,
            custom_field: None,
        }
    }


    /// The four core pipelines (opaque / transparent / mask / depth-prepass)
    /// from one module — extracted so a Field Shape splice
    /// ([`set_custom_field`](Self::set_custom_field)) can rebuild them.
    fn build_core_pipelines(
        gpu: &Gpu,
        module: &wgpu::ShaderModule,
        globals_layout: &wgpu::BindGroupLayout,
        tex_layout: &wgpu::BindGroupLayout,
        field_layout: &wgpu::BindGroupLayout,
    ) -> (wgpu::RenderPipeline, wgpu::RenderPipeline, wgpu::RenderPipeline, wgpu::RenderPipeline) {
        let device = &gpu.device;
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("raster"),
            bind_group_layouts: &[Some(globals_layout), Some(tex_layout), Some(field_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raster"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Vertex::LAYOUT, INSTANCE_LAYOUT],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: Gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                // LessEqual (not Less): when the depth prepass primed the buffer,
                // the color pass's fragments arrive at depths EQUAL to their own
                // prepass writes and must still shade.
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some("fs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: gpu.surface_format(),
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        // Depth-only prepass (opaque instances, conservative alpha discard): primes
        // the frame's depth buffer so the color pass early-z-kills hidden fragments
        // (their shading marches the shadow field — the expensive part) and gives
        // the raymarch a per-pixel march cap. Needs only groups 0 + 1.
        let prepass_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("raster-prepass"),
            bind_group_layouts: &[Some(globals_layout), Some(tex_layout)],
            immediate_size: 0,
        });
        let prepass_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raster-prepass"),
            layout: Some(&prepass_layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Vertex::LAYOUT, INSTANCE_LAYOUT],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: Gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some("fs_depth"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[],
            }),
            multiview_mask: None,
            cache: None,
        });

        // Transparent variant: identical vertex/fragment, but alpha-blends and does NOT
        // write depth, so an object behind it still shows through and later opaque draws
        // aren't occluded by it. (No back-to-front sort yet, so overlapping transparent
        // surfaces are approximate — enough for the basic transparency this exposes.)
        let transparent_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raster-transparent"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Vertex::LAYOUT, INSTANCE_LAYOUT],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: Gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                // LESS-EQUAL, not Less: texture-paint overlays are COPLANAR with the mesh
                // they decorate (identical positions through the same vertex shader, so
                // byte-identical depth) — under Less the opaque base's prepass depth would
                // reject every overlay fragment. Equal-depth translucents draw over the
                // surface they sit on, which is exactly what an overlay wants.
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some("fs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: gpu.surface_format(),
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        // Silhouette-mask pipeline: rasterizes a selected mesh as solid 1.0 into a
        // single-channel mask (no depth, no cull → the full screen silhouette), which
        // a post-pass edge-detects into a selection outline. Needs only the globals.
        let mask_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("raster-mask"),
            bind_group_layouts: &[Some(globals_layout)],
            immediate_size: 0,
        });
        let mask_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raster-mask"),
            layout: Some(&mask_layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Vertex::LAYOUT, INSTANCE_LAYOUT],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some("fs_mask"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: crate::outline::MASK_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        (pipeline, transparent_pipeline, mask_pipeline, prepass_pipeline)
    }

    /// Register (or hot-swap) a compiled `.flsl` fragment shader: `chunk` is the
    /// transpiler's generated WGSL **including its stdlib support**, concatenated
    /// onto [`pass_prelude`] here. The caller MUST have naga-validated the
    /// assembled source first (`floptle_shader::validate` with this prelude) —
    /// this builds the pipeline unconditionally. `replace` swaps an existing
    /// shader in place (hot reload): live bindings stay valid when the slot
    /// count is unchanged; the editor rebuilds them right after anyway.
    pub fn register_flsl_shader(
        &mut self,
        gpu: &Gpu,
        chunk: &str,
        tex_slots: usize,
        blend: FlslBlend,
        replace: Option<FlslShaderId>,
    ) -> FlslShaderId {
        let device = &gpu.device;
        // The base includes any spliced Field Shape code (support arrives once,
        // inside `chunk`), so custom-material meshes see shape shadows/AO too.
        let base = match &self.custom_field {
            Some((field, _)) => splice_block(
                pass_prelude(),
                "//[flsl-field-custom-begin]",
                "//[flsl-field-custom-end]",
                field,
            ),
            None => pass_prelude().to_string(),
        };
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("raster-flsl"),
            source: wgpu::ShaderSource::Wgsl(format!("{base}\n{chunk}").into()),
        });

        // Group 3: the shader's param UBO + its declared texture slots.
        let mut entries = vec![wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }];
        for i in 0..tex_slots as u32 {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: 1 + 2 * i,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            });
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: 2 + 2 * i,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            });
        }
        let group3_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("raster-flsl-material"),
            entries: &entries,
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("raster-flsl"),
            bind_group_layouts: &[
                Some(&self.globals_layout),
                Some(&self.tex_layout),
                Some(&self.field_layout),
                Some(&group3_layout),
            ],
            immediate_size: 0,
        });

        let opaque = matches!(blend, FlslBlend::Opaque);
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raster-flsl"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Vertex::LAYOUT, INSTANCE_LAYOUT],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: Gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(opaque),
                depth_compare: Some(if opaque {
                    wgpu::CompareFunction::LessEqual
                } else {
                    wgpu::CompareFunction::Less
                }),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_flsl"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: gpu.surface_format(),
                    blend: match blend {
                        FlslBlend::Opaque => None,
                        FlslBlend::Alpha => Some(wgpu::BlendState::ALPHA_BLENDING),
                        FlslBlend::Additive => Some(wgpu::BlendState {
                            color: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::One,
                                dst_factor: wgpu::BlendFactor::One,
                                operation: wgpu::BlendOperation::Add,
                            },
                            alpha: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::One,
                                dst_factor: wgpu::BlendFactor::One,
                                operation: wgpu::BlendOperation::Add,
                            },
                        }),
                    },
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let shader = FlslShader {
            pipeline,
            group3_layout,
            tex_slots,
            opaque,
            chunk: chunk.to_string(),
            blend,
        };
        match replace {
            Some(id) if (id.0 as usize) < self.flsl_shaders.len() => {
                self.flsl_shaders[id.0 as usize] = shader;
                id
            }
            _ => {
                self.flsl_shaders.push(shader);
                FlslShaderId(self.flsl_shaders.len() as u32 - 1)
            }
        }
    }

    /// Splice (or clear, with `None`) the scene's Field Shape code into EVERY
    /// module this pass owns — the core pipelines and each registered flsl
    /// shader — so meshes (built-in and custom-material alike) receive shape
    /// shadows and AO. `code` = `(field distance functions, stdlib support)`;
    /// the caller MUST have naga-validated [`raster_custom_source`] first.
    pub fn set_custom_field(&mut self, gpu: &Gpu, code: Option<(&str, &str)>) {
        self.custom_field = code.map(|(f, s)| (f.to_string(), s.to_string()));
        let module = gpu.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("raster"),
            source: wgpu::ShaderSource::Wgsl(raster_custom_source(code).into()),
        });
        let (pipeline, transparent_pipeline, mask_pipeline, prepass_pipeline) =
            Self::build_core_pipelines(
                gpu,
                &module,
                &self.globals_layout,
                &self.tex_layout,
                &self.field_layout,
            );
        self.pipeline = pipeline;
        self.transparent_pipeline = transparent_pipeline;
        self.mask_pipeline = mask_pipeline;
        self.prepass_pipeline = prepass_pipeline;
        // Rebuild every custom-shader pipeline against the new field module.
        for i in 0..self.flsl_shaders.len() {
            let (chunk, tex_slots, blend) = {
                let s = &self.flsl_shaders[i];
                (s.chunk.clone(), s.tex_slots, s.blend)
            };
            self.register_flsl_shader(gpu, &chunk, tex_slots, blend, Some(FlslShaderId(i as u32)));
        }
    }

    /// Whether a registered flsl shader draws in the opaque phase (depth-write)
    /// — opaque flsl instances also join the depth prepass.
    pub fn flsl_shader_is_opaque(&self, id: FlslShaderId) -> bool {
        self.flsl_shaders.get(id.0 as usize).is_some_and(|s| s.opaque)
    }

    /// Create (or, with `replace`, rebuild in place) a material's live binding
    /// of a compiled shader: its params UBO (packed by the transpiler's layout)
    /// + one texture per declared slot (`None` = the 1×1 white default).
    pub fn set_flsl_binding(
        &mut self,
        gpu: &Gpu,
        replace: Option<FlslBindingId>,
        shader: FlslShaderId,
        params: &[u8],
        textures: &[Option<TexId>],
    ) -> FlslBindingId {
        let default_sampler = self.sampler_for(gpu, TexSampling::default());
        let sh = &self.flsl_shaders[shader.0 as usize];
        let params_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster-flsl-params"),
            size: (params.len().max(16)) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue.write_buffer(&params_buf, 0, params);

        let default_view = self.default_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let mut entries =
            vec![wgpu::BindGroupEntry { binding: 0, resource: params_buf.as_entire_binding() }];
        // Per-slot samplers reuse the texture's own registered sampling.
        let samplers: Vec<wgpu::Sampler> = (0..sh.tex_slots)
            .map(|i| {
                let sampling = textures
                    .get(i)
                    .copied()
                    .flatten()
                    .and_then(|t| self.textures.get(t.0 as usize))
                    .map(|t| t.sampling)
                    .unwrap_or_default();
                self.samplers.get(&sampling).cloned().unwrap_or_else(|| default_sampler.clone())
            })
            .collect();
        for (i, sampler) in samplers.iter().enumerate() {
            let view = textures
                .get(i)
                .copied()
                .flatten()
                .and_then(|t| self.textures.get(t.0 as usize))
                .map(|t| &t.view)
                .unwrap_or(&default_view);
            entries.push(wgpu::BindGroupEntry {
                binding: (1 + 2 * i) as u32,
                resource: wgpu::BindingResource::TextureView(view),
            });
            entries.push(wgpu::BindGroupEntry {
                binding: (2 + 2 * i) as u32,
                resource: wgpu::BindingResource::Sampler(sampler),
            });
        }
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster-flsl-material"),
            layout: &sh.group3_layout,
            entries: &entries,
        });
        let binding = FlslBinding { shader, params_buf, bind };
        match replace {
            Some(id) if (id.0 as usize) < self.flsl_bindings.len() => {
                self.flsl_bindings[id.0 as usize] = binding;
                id
            }
            _ => {
                self.flsl_bindings.push(binding);
                FlslBindingId(self.flsl_bindings.len() as u32 - 1)
            }
        }
    }

    /// Update a binding's param block in place — the "param edits are uniform
    /// writes, never a recompile" contract.
    pub fn write_flsl_params(&self, gpu: &Gpu, id: FlslBindingId, params: &[u8]) {
        if let Some(b) = self.flsl_bindings.get(id.0 as usize) {
            gpu.queue.write_buffer(&b.params_buf, 0, params);
        }
    }

    /// A sampler for the given settings, created on first use and cached.
    fn sampler_for(&mut self, gpu: &Gpu, s: TexSampling) -> wgpu::Sampler {
        if let Some(samp) = self.samplers.get(&s) {
            return samp.clone();
        }
        let (mag, min, mip) = match s.filter {
            TexFilter::Pixelated => (
                wgpu::FilterMode::Nearest,
                wgpu::FilterMode::Nearest,
                wgpu::MipmapFilterMode::Nearest,
            ),
            TexFilter::Smooth => (
                wgpu::FilterMode::Linear,
                wgpu::FilterMode::Linear,
                wgpu::MipmapFilterMode::Nearest,
            ),
            TexFilter::SmoothMipmaps => (
                wgpu::FilterMode::Linear,
                wgpu::FilterMode::Linear,
                wgpu::MipmapFilterMode::Linear,
            ),
        };
        let addr = match s.wrap {
            TexWrap::Repeat => wgpu::AddressMode::Repeat,
            TexWrap::Clamp => wgpu::AddressMode::ClampToEdge,
            TexWrap::Mirror => wgpu::AddressMode::MirrorRepeat,
        };
        let samp = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("raster-samp"),
            address_mode_u: addr,
            address_mode_v: addr,
            address_mode_w: addr,
            mag_filter: mag,
            min_filter: min,
            mipmap_filter: mip,
            ..Default::default()
        });
        self.samplers.insert(s, samp.clone());
        samp
    }

    /// The bind group of a registered material texture — shared with the particle
    /// pass, whose group(1) layout is structurally identical, so one registry
    /// textures both meshes and billboards.
    pub(crate) fn material_bind(&self, id: TexId) -> Option<&wgpu::BindGroup> {
        self.textures.get(id.0 as usize).map(|t| &t.bind)
    }

    /// The raw texture view of a registered material texture — for editor
    /// passes (the shader-graph preview) that bind scene textures themselves.
    pub fn texture_view(&self, id: TexId) -> Option<&wgpu::TextureView> {
        self.textures.get(id.0 as usize).map(|t| &t.view)
    }

    /// Register a standalone material texture (RGBA8) with the given sampling, returning
    /// its handle. Bound per-instance in `draw_scene` to re-texture a shape regardless
    /// of its mesh. Re-registering the same image with new settings returns a fresh id.
    pub fn register_texture(&mut self, gpu: &Gpu, data: &TextureData, sampling: TexSampling) -> TexId {
        let id = TexId(self.textures.len() as u32);
        let texture = upload_texture_mips(gpu, data, matches!(sampling.filter, TexFilter::SmoothMipmaps));
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = self.sampler_for(gpu, sampling);
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster-material-tex"),
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        self.textures.push(TexBind { bind, view, sampling, _texture: texture });
        id
    }

    /// Register a live RENDER-TARGET texture as a material texture (A1): a
    /// `w×h` color texture in the surface format (render attachment + sampled)
    /// with a matching depth buffer. The editor renders a target-camera into
    /// the returned views each frame; any material or UI image whose texture
    /// path resolves to the `TexId` shows the live picture (cockpit screens,
    /// monitors, mirrors). Returned views: (color attachment, depth).
    pub fn register_render_target(
        &mut self,
        gpu: &Gpu,
        w: u32,
        h: u32,
    ) -> (TexId, wgpu::TextureView, wgpu::TextureView) {
        let (w, h) = (w.max(1), h.max(1));
        let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("raster-render-target"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // The surface format, so every scene pipeline renders into it
            // unchanged; sampling the sRGB view decodes to linear exactly like
            // a regular sRGB material texture.
            format: gpu.surface_format(),
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let attach = color.create_view(&wgpu::TextureViewDescriptor::default());
        let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("raster-render-target-depth"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Gpu::DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        // Smooth + clamp: a live feed reads like video, and a screen quad must
        // never tile its edges.
        let sampling = TexSampling { filter: TexFilter::Smooth, wrap: TexWrap::Clamp };
        let sampler = self.sampler_for(gpu, sampling);
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster-render-target"),
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        let id = TexId(self.textures.len() as u32);
        self.textures.push(TexBind { bind, view, sampling, _texture: color });
        (id, attach, depth_view)
    }

    /// Overwrite a registered texture's pixels in place (same dimensions). For a paint
    /// brush stamping into a per-node texture every dab — re-registering would leak a new
    /// `TexId` per stroke. Dimensions must match what it was registered with; a mismatch or
    /// unknown id is ignored. (No mip regen — paint textures use a plain filter.)
    pub fn update_texture(&self, gpu: &Gpu, id: TexId, data: &TextureData) {
        let Some(t) = self.textures.get(id.0 as usize) else { return };
        let size = t._texture.size();
        if size.width != data.width || size.height != data.height {
            log::error!("update_texture size mismatch: {}×{} vs {}×{}", size.width, size.height, data.width, data.height);
            return;
        }
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &t._texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &data.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(data.width * 4),
                rows_per_image: Some(data.height),
            },
            wgpu::Extent3d { width: data.width, height: data.height, depth_or_array_layers: 1 },
        );
    }

    /// Overwrite a rectangular sub-region of a registered texture. The paint brush touches
    /// only the texels under a dab, so re-uploading the whole (up to 2048²) atlas every dab
    /// is wasteful — this uploads just the dirty rect. `pixels` is tightly packed RGBA8,
    /// `w * h * 4` bytes, row-major. `write_texture` (unlike a buffer copy) imposes no
    /// 256-byte row alignment, so `bytes_per_row = w * 4` is fine.
    #[allow(clippy::too_many_arguments)]
    pub fn update_texture_region(&self, gpu: &Gpu, id: TexId, x: u32, y: u32, w: u32, h: u32, pixels: &[u8]) {
        let Some(t) = self.textures.get(id.0 as usize) else { return };
        let size = t._texture.size();
        if x + w > size.width || y + h > size.height {
            log::error!("update_texture_region out of bounds: {x},{y} {w}×{h} in {}×{}", size.width, size.height);
            return;
        }
        if pixels.len() < (w * h * 4) as usize {
            return;
        }
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &t._texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    /// Build the group(0) bind group over the frame globals + the paint store. Called
    /// at startup and again whenever `vpaint_buf` is RE-CREATED (a bind group holds the
    /// buffer it was built from, so growing the store invalidates it).
    #[allow(clippy::too_many_arguments)]
    fn make_globals_bind(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        globals_buf: &wgpu::Buffer,
        vpaint_buf: &wgpu::Buffer,
        tpaint_buf: &wgpu::Buffer,
        pal_view: &wgpu::TextureView,
        pal_samp: &wgpu::Sampler,
        pal_samp_nearest: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster-globals"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: vpaint_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: tpaint_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(pal_view) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(pal_samp) },
                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(pal_samp_nearest) },
            ],
        })
    }

    /// Upload the terrain texture palette (already resized to 256² per layer, same order
    /// as the raymarch's `set_terrain_textures`) and the per-slot nearest bitmask, so meshed
    /// terrain splats textures identically to the old raymarched path. Rebuilds the globals
    /// bind group (the palette texture is recreated).
    pub fn set_terrain_palette(&mut self, gpu: &Gpu, layers: &[TextureData], nearest_mask: u32) {
        let tex = crate::raymarch::make_terrain_array(gpu, layers);
        self._terrain_pal = tex;
        self.terrain_pal_view = self._terrain_pal.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        self.globals_bind = Self::make_globals_bind(
            &gpu.device,
            &self.globals_layout,
            &self.globals_buf,
            &self.vpaint_buf,
            &self.tpaint_buf,
            &self.terrain_pal_view,
            &self.terrain_samp,
            &self.terrain_samp_nearest,
        );
        self.terrain_nearest_mask = nearest_mask;
    }

    /// The stored per-slot nearest bitmask, folded into `Globals.terrain_mask.x` at draw
    /// time (the editor sets the palette once, then every frame's globals carry the mask).
    pub fn terrain_nearest_mask(&self) -> u32 {
        self.terrain_nearest_mask
    }

    /// Append a paint block to the store and return its `paint_base` (0 = unpainted).
    /// Blocks are bump-allocated and shared freely: any number of instances may point
    /// at the same base (proposal §9.0 — sharing is just a repeated offset).
    fn alloc_paint(&mut self, gpu: &Gpu, colors: &[[u8; 4]]) -> u32 {
        if colors.is_empty() {
            return 0;
        }
        let base = self.vpaint_cpu.len() as u32;
        // `params.z` packs the base as `base << 1` beside the unlit bit, and f32 only
        // holds integers exactly to 2^24 — past that the offset would silently decode
        // wrong and read another block's colors. Refuse instead of corrupting.
        if (base as u64 + colors.len() as u64) > (1 << 23) {
            log::error!(
                "vertex paint store full ({} verts): this mesh renders unpainted. \
                 The params.z packing holds ~8.3M painted vertices per scene.",
                base
            );
            return 0;
        }
        self.vpaint_cpu.extend(colors.iter().map(|c| u32::from_le_bytes(*c)));

        let needed = (self.vpaint_cpu.len() * 4) as u64;
        if needed > self.vpaint_buf.size() {
            // Grow by powers of two so a scene of many painted meshes doesn't
            // re-create the buffer once per mesh.
            let cap = needed.next_power_of_two();
            self.vpaint_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("raster-vpaint"),
                size: cap,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.globals_bind = Self::make_globals_bind(
                &gpu.device,
                &self.globals_layout,
                &self.globals_buf,
                &self.vpaint_buf,
                &self.tpaint_buf,
                &self.terrain_pal_view,
                &self.terrain_samp,
                &self.terrain_samp_nearest,
            );
            gpu.queue.write_buffer(&self.vpaint_buf, 0, bytemuck::cast_slice(&self.vpaint_cpu));
        } else {
            // Buffer still fits: push only the new block.
            gpu.queue.write_buffer(
                &self.vpaint_buf,
                (base * 4) as u64,
                bytemuck::cast_slice(&self.vpaint_cpu[base as usize..]),
            );
        }
        base
    }

    /// Allocate a paint block of `count` vertices filled with `fill`, returning its
    /// base (0 on failure). This is how the brush gives a node its OWN paint, separate
    /// from whatever its mesh imported with.
    pub fn paint_alloc(&mut self, gpu: &Gpu, count: u32, fill: [u8; 4]) -> u32 {
        self.alloc_paint(gpu, &vec![fill; count as usize])
    }

    /// Allocate a paint block seeded from existing colors — the copy half of
    /// copy-on-write (proposal §9.0): forking a shared block, or duplicating a node.
    pub fn paint_alloc_from(&mut self, gpu: &Gpu, colors: &[[u8; 4]]) -> u32 {
        self.alloc_paint(gpu, colors)
    }

    /// Read one vertex's painted color out of the CPU mirror.
    pub fn paint_get(&self, base: u32, i: u32) -> [u8; 4] {
        self.vpaint_cpu
            .get((base + i) as usize)
            .map_or([255; 4], |c| c.to_le_bytes())
    }

    /// Write one vertex's color to the CPU mirror. The GPU does NOT see this until
    /// [`Raster::paint_flush`] — a brush dab touches many vertices, and one upload per
    /// dab beats one per vertex.
    pub fn paint_set(&mut self, base: u32, i: u32, c: [u8; 4]) {
        if let Some(slot) = self.vpaint_cpu.get_mut((base + i) as usize) {
            *slot = u32::from_le_bytes(c);
        }
    }

    /// Upload the vertex range `lo..=hi` of a block. Partial by design: re-uploading a
    /// whole 100k-vertex block per dab would be pure waste (the same reason terrain
    /// tracks a dirty sub-box).
    pub fn paint_flush(&self, gpu: &Gpu, base: u32, lo: u32, hi: u32) {
        let (s, e) = ((base + lo) as usize, (base + hi) as usize + 1);
        if s >= e || e > self.vpaint_cpu.len() {
            return;
        }
        gpu.queue.write_buffer(
            &self.vpaint_buf,
            (s * 4) as u64,
            bytemuck::cast_slice(&self.vpaint_cpu[s..e]),
        );
    }

    /// Copy a block out of the CPU mirror — the undo snapshot, and the source for a
    /// copy-on-write fork.
    pub fn paint_block(&self, base: u32, count: u32) -> Vec<[u8; 4]> {
        (0..count).map(|i| self.paint_get(base, i)).collect()
    }

    /// Overwrite a whole block and upload it — how undo/redo restores a stroke.
    pub fn paint_restore(&mut self, gpu: &Gpu, base: u32, colors: &[[u8; 4]]) {
        for (i, c) in colors.iter().enumerate() {
            self.paint_set(base, i as u32, *c);
        }
        if !colors.is_empty() {
            self.paint_flush(gpu, base, 0, colors.len() as u32 - 1);
        }
    }

    /// The paint block a registered mesh owns, or 0 if it imported unpainted.
    /// Instances of the same mesh SHARE this base — one block, N instances, still
    /// one draw call.
    pub fn mesh_paint_base(&self, id: MeshId) -> u32 {
        self.meshes.get(id.0 as usize).map_or(0, |m| m.paint_base)
    }

    // ---- Dynamic meshes ------------------------------------------------------------
    //
    // Terrain chunks are the first citizen: geometry that is re-extracted (surface nets
    // over the sparse SDF) whenever a brush dirties a chunk, then again at a coarser
    // stride when it crosses an LOD ring. They are ordinary `MeshId`s on purpose — that
    // way chunks flow through the depth prepass, the instance bucketing, `.flsl`
    // materials, the selection mask and field shadows with no parallel plumbing at all.
    // The only differences live behind `RegisteredMesh::dynamic`: buffers with headroom,
    // a rewritable index count, and a paired terrain-color block.

    /// Allocate (or re-use) a terrain-color block of exactly `cap` vertices, returning
    /// its base. Growth copies the live prefix on the GPU, so nothing is mirrored on
    /// the CPU. Returns 0 if `cap` is 0 or the store's f32-exact range is exhausted.
    fn tpaint_alloc(&mut self, gpu: &Gpu, cap: u32) -> u32 {
        if cap == 0 {
            return 0;
        }
        // Exact-capacity re-use: freed blocks come back at the capacity they left with,
        // so this never fragments (and `slot_cap`'s size classes keep the hit rate high).
        if let Some(v) = self.tpaint_free.get_mut(&cap)
            && let Some(base) = v.pop()
        {
            return base;
        }
        let base = self.tpaint_len;
        // The base rides `n0.w` as a plain f32 — exact only to 2^24. Refuse rather than
        // silently decode to a neighbouring block's colors (the vpaint rule, same reason).
        if base as u64 + cap as u64 > (1 << 24) {
            log::error!(
                "terrain color store full ({base} verts): these chunks render untinted. \
                 The n0.w lane holds ~16.7M terrain vertices per scene."
            );
            return 0;
        }
        self.tpaint_len += cap;

        let needed = (self.tpaint_len as u64) * 4;
        if needed > self.tpaint_buf.size() {
            let new_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("raster-tpaint"),
                size: needed.next_power_of_two(),
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let mut enc = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("tpaint-grow") });
            enc.copy_buffer_to_buffer(&self.tpaint_buf, 0, &new_buf, 0, self.tpaint_buf.size());
            gpu.queue.submit([enc.finish()]);
            self.tpaint_buf = new_buf;
            self.globals_bind = Self::make_globals_bind(
                &gpu.device,
                &self.globals_layout,
                &self.globals_buf,
                &self.vpaint_buf,
                &self.tpaint_buf,
                &self.terrain_pal_view,
                &self.terrain_samp,
                &self.terrain_samp_nearest,
            );
        }
        base
    }

    /// Create a dynamic mesh slot sized for `verts`/`indices` (with headroom) and, when
    /// `colored`, its paired terrain-color block. The slot draws nothing until
    /// [`replace_dynamic`](Self::replace_dynamic) fills it.
    pub fn register_dynamic(
        &mut self,
        gpu: &Gpu,
        verts: u32,
        indices: u32,
        colored: bool,
    ) -> MeshId {
        let cap_verts = slot_cap(verts);
        let cap_indices = slot_cap(indices);
        let tpaint_cap = if colored { cap_verts } else { 0 };
        let tpaint_base = self.tpaint_alloc(gpu, tpaint_cap);
        let gpu_mesh = GpuMesh::with_capacity(gpu, cap_verts, cap_indices);
        let slot = DynSlot { cap_verts, cap_indices, tpaint_base, tpaint_cap };

        let view = self.default_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = self.sampler_for(gpu, TexSampling::default());
        let tex_bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster-dyn-tex"),
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        let mesh = RegisteredMesh {
            gpu_mesh,
            tex_bind,
            _texture: None,
            paint_base: 0,
            dynamic: Some(slot),
        };
        match self.dyn_free.pop() {
            Some(i) => {
                self.meshes[i as usize] = mesh;
                MeshId(i)
            }
            None => {
                self.meshes.push(mesh);
                MeshId(self.meshes.len() as u32 - 1)
            }
        }
    }

    /// Rewrite a dynamic slot's geometry (and, if it has one, its terrain-color block).
    /// Returns `false` — leaving the slot untouched — when `id` is not a dynamic slot;
    /// data that outgrows the slot's capacity re-creates its buffers in place (logged,
    /// because steady-state sculpting is meant never to hit it).
    pub fn replace_dynamic(&mut self, gpu: &Gpu, id: MeshId, data: &MeshData) -> bool {
        let Some(m) = self.meshes.get(id.0 as usize) else { return false };
        let Some(slot) = m.dynamic.as_ref() else { return false };
        let (nv, ni) = (data.vertices.len() as u32, data.indices.len() as u32);

        if nv > slot.cap_verts || ni > slot.cap_indices {
            let (cap_verts, cap_indices) = (slot_cap(nv), slot_cap(ni));
            log::debug!(
                "terrain chunk outgrew its slot ({} > {} verts / {} > {} indices) — regrowing",
                nv,
                slot.cap_verts,
                ni,
                slot.cap_indices
            );
            // The color block is sized to the slot, so it regrows with it: hand the old
            // one back to the free-list (it is re-usable at its own capacity) and take a
            // new one. Skipping this is exactly the leak the separate store exists to stop.
            let (old_base, old_cap, colored) =
                (slot.tpaint_base, slot.tpaint_cap, slot.tpaint_base != 0);
            if old_base != 0 {
                self.tpaint_free.entry(old_cap).or_default().push(old_base);
            }
            let tpaint_cap = if colored { cap_verts } else { 0 };
            let tpaint_base = self.tpaint_alloc(gpu, tpaint_cap);
            let m = &mut self.meshes[id.0 as usize];
            m.gpu_mesh = GpuMesh::with_capacity(gpu, cap_verts, cap_indices);
            m.dynamic = Some(DynSlot { cap_verts, cap_indices, tpaint_base, tpaint_cap });
        }

        let m = &mut self.meshes[id.0 as usize];
        if !m.gpu_mesh.write(gpu, data) {
            return false;
        }
        let slot = m.dynamic.as_ref().expect("dynamic slot");
        if slot.tpaint_base != 0
            && let Some(colors) = data.colors.as_ref()
        {
            // A short/long stream would tint the wrong vertices — the `register` rule.
            if colors.len() != data.vertices.len() {
                log::error!(
                    "chunk color stream is {} long but has {} vertices — ignoring the color",
                    colors.len(),
                    data.vertices.len()
                );
            } else if !colors.is_empty() {
                let packed: Vec<u32> = colors.iter().map(|c| u32::from_le_bytes(*c)).collect();
                gpu.queue.write_buffer(
                    &self.tpaint_buf,
                    (slot.tpaint_base as u64) * 4,
                    bytemuck::cast_slice(&packed),
                );
            }
        }
        true
    }

    /// Return a dynamic slot (and its color block) to the free-lists. The `MeshId` is
    /// recycled by a later `register_dynamic`, so callers must drop it here: terrain
    /// owns its chunk ids exclusively, which is what makes that safe.
    pub fn free_dynamic(&mut self, id: MeshId) {
        let Some(m) = self.meshes.get_mut(id.0 as usize) else { return };
        let Some(slot) = m.dynamic.take() else { return };
        if slot.tpaint_base != 0 {
            self.tpaint_free.entry(slot.tpaint_cap).or_default().push(slot.tpaint_base);
        }
        m.gpu_mesh.index_count = 0; // stop drawing immediately, even before re-use
        if !self.dyn_free.contains(&id.0) {
            self.dyn_free.push(id.0);
        }
    }

    /// The terrain-color base a dynamic slot owns (0 = none) — goes in
    /// [`MaterialParams::terrain_paint_base`] for that chunk's instance.
    pub fn dyn_paint_base(&self, id: MeshId) -> u32 {
        self.meshes
            .get(id.0 as usize)
            .and_then(|m| m.dynamic.as_ref())
            .map_or(0, |s| s.tpaint_base)
    }

    /// Bytes the terrain color store currently holds on the GPU (for budget probes).
    pub fn tpaint_bytes(&self) -> u64 {
        self.tpaint_buf.size()
    }

    /// Upload a mesh and its base-color texture (or `None` for a white default),
    /// returning its handle. The mesh's own texture uses the default (crisp) sampling.
    /// `data.colors`, if present, becomes the mesh's shared vertex-paint block.
    pub fn register(&mut self, gpu: &Gpu, data: &MeshData, texture: Option<&TextureData>) -> MeshId {
        let id = MeshId(self.meshes.len() as u32);
        let gpu_mesh = GpuMesh::upload(gpu, data);
        // A short/long color stream would silently misalign paint against geometry —
        // drop it rather than paint the wrong vertices.
        let paint_base = match data.colors.as_ref() {
            Some(c) if c.len() == data.vertices.len() => self.alloc_paint(gpu, c),
            Some(c) => {
                log::error!(
                    "mesh color stream is {} long but has {} vertices — ignoring the paint",
                    c.len(),
                    data.vertices.len()
                );
                0
            }
            None => 0,
        };

        let owned = texture.map(|t| upload_texture(gpu, t));
        let view = owned
            .as_ref()
            .unwrap_or(&self.default_tex)
            .create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = self.sampler_for(gpu, TexSampling::default());
        let tex_bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster-mesh-tex"),
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });

        self.meshes.push(RegisteredMesh {
            gpu_mesh,
            tex_bind,
            _texture: owned,
            paint_base,
            dynamic: None,
        });
        id
    }

    /// Re-upload a registered mesh's vertex data in place (its buffer is `COPY_DST`).
    /// Used by CPU vertex skinning to push each frame's deformed vertices. `verts` must
    /// have the same length the mesh was registered with (the index buffer is unchanged);
    /// a mismatch or unknown id is ignored.
    pub fn update_mesh_vertices(&self, gpu: &Gpu, id: MeshId, verts: &[Vertex]) {
        if let Some(m) = self.meshes.get(id.0 as usize) {
            gpu.queue.write_buffer(&m.gpu_mesh.vbuf, 0, bytemuck::cast_slice(verts));
        }
    }

    fn ensure_instances(&mut self, gpu: &Gpu, count: u32) {
        if count <= self.instance_cap {
            return;
        }
        let cap = count.next_power_of_two();
        self.instance_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster-instances"),
            size: (cap as u64) * std::mem::size_of::<InstanceRaw>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.instance_cap = cap;
    }

    /// Render the given instances (bucketed by mesh) into a single-channel mask as
    /// solid 1.0 — the selected object's silhouette, for the selection-outline post
    /// pass. Clears the mask first; no depth, no culling (the full screen silhouette).
    pub fn draw_mask(
        &mut self,
        gpu: &Gpu,
        mask: &wgpu::TextureView,
        globals: Globals,
        instances: &[(MeshId, InstanceRaw)],
    ) {
        gpu.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        let mut raws: Vec<InstanceRaw> = Vec::with_capacity(instances.len());
        let mut buckets: Vec<(usize, u32, u32)> = Vec::new();
        for mesh_idx in 0..self.meshes.len() {
            let start = raws.len() as u32;
            for (id, raw) in instances {
                if id.0 as usize == mesh_idx {
                    raws.push(*raw);
                }
            }
            let count = raws.len() as u32 - start;
            if count > 0 {
                buckets.push((mesh_idx, start, count));
            }
        }
        self.ensure_instances(gpu, raws.len().max(1) as u32);
        if !raws.is_empty() {
            gpu.queue.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&raws));
        }

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raster-mask") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("raster-mask"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: mask,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.mask_pipeline);
            rp.set_bind_group(0, &self.globals_bind, &[]);
            rp.set_vertex_buffer(1, self.instance_buf.slice(..));
            for (mesh_idx, start, count) in buckets {
                let mesh = &self.meshes[mesh_idx];
                rp.set_vertex_buffer(0, mesh.gpu_mesh.vbuf.slice(..));
                rp.set_index_buffer(mesh.gpu_mesh.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                rp.draw_indexed(0..mesh.gpu_mesh.index_count, 0, start..(start + count));
            }
        }
        gpu.queue.submit([encoder.finish()]);
    }

    /// Clear the given color + depth targets and draw every instance, bucketed by
    /// mesh so each mesh issues one instanced `draw_indexed` with its own texture
    /// bound. The targets are passed in (rather than hard-wired to the swapchain) so
    /// the scene can render either straight to the window or into a low-res retro
    /// buffer; `color` must use the surface format and `depth` the depth format.
    /// `field`: the raymarch pass's [`field_bind`](crate::Raymarch::field_bind) so
    /// meshes receive field shadows + SDF AO — or `None` for a standalone draw
    /// (previews, probes) where every field effect is simply off.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_scene(
        &mut self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        globals: Globals,
        instances: &[(MeshId, Option<TexId>, InstanceRaw)],
        clear: Option<[f64; 4]>,
        field: Option<&wgpu::BindGroup>,
    ) {
        self.draw_scene_with(gpu, color, depth, globals, instances, &[], clear, field);
    }

    /// [`draw_scene`](Self::draw_scene) plus custom-shader draws: flsl
    /// instances bucket by (mesh, texture, binding) and draw in the same pass —
    /// opaque-phase shaders right after the built-in opaque bucket (before
    /// transparency), blended ones last.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_scene_with(
        &mut self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        globals: Globals,
        instances: &[(MeshId, Option<TexId>, InstanceRaw)],
        flsl: &[FlslDraw],
        clear: Option<[f64; 4]>,
        field: Option<&wgpu::BindGroup>,
    ) {
        gpu.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        // Clear when we own the frame; Load when a prior pass (raymarch) already
        // filled the color + depth targets, so the two compose in one depth buffer.
        let (color_load, depth_load) = match clear {
            Some(c) => (
                wgpu::LoadOp::Clear(wgpu::Color { r: c[0], g: c[1], b: c[2], a: c[3] }),
                wgpu::LoadOp::Clear(1.0),
            ),
            None => (wgpu::LoadOp::Load, wgpu::LoadOp::Load),
        };

        // Bucket by (mesh, texture-override) — each unique combo is one draw with
        // its own bound texture. A material texture (Some) re-textures the shape;
        // None uses the mesh's own base-color texture. Opaque and transparent draws are
        // bucketed separately (and packed contiguously into one instance buffer) so the
        // transparent ones can render last, blended, in a second pass.
        const OPAQUE_CUTOFF: f32 = 0.999;
        let mut raws: Vec<InstanceRaw> = Vec::with_capacity(instances.len());
        let bucketize =
            |want_opaque: bool, raws: &mut Vec<InstanceRaw>| -> Vec<(usize, Option<u32>, u32, u32)> {
                let mut buckets: Vec<(usize, Option<u32>, u32, u32)> = Vec::new();
                let mut keys: Vec<(usize, Option<u32>)> = Vec::new();
                for (id, tex, raw) in instances {
                    if (raw.color[3] >= OPAQUE_CUTOFF) != want_opaque {
                        continue;
                    }
                    let k = (id.0 as usize, tex.map(|t| t.0));
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
                for (mesh_idx, tex_key) in keys {
                    let start = raws.len() as u32;
                    for (id, tex, raw) in instances {
                        if (raw.color[3] >= OPAQUE_CUTOFF) != want_opaque {
                            continue;
                        }
                        if id.0 as usize == mesh_idx && tex.map(|t| t.0) == tex_key {
                            raws.push(*raw);
                        }
                    }
                    let count = raws.len() as u32 - start;
                    if count > 0 {
                        buckets.push((mesh_idx, tex_key, start, count));
                    }
                }
                buckets
            };
        let opaque_buckets = bucketize(true, &mut raws);
        let transparent_buckets = bucketize(false, &mut raws);

        // flsl buckets: (mesh, texture, binding) — phase comes from the SHADER
        // (its blend declaration), not the instance alpha.
        let flsl_bucketize = |want_opaque: bool,
                              raws: &mut Vec<InstanceRaw>|
         -> Vec<(usize, Option<u32>, u32, u32, u32)> {
            let mut buckets = Vec::new();
            let mut keys: Vec<(usize, Option<u32>, u32)> = Vec::new();
            for (id, tex, bind, _) in flsl {
                let Some(b) = self.flsl_bindings.get(bind.0 as usize) else { continue };
                let Some(sh) = self.flsl_shaders.get(b.shader.0 as usize) else { continue };
                if sh.opaque != want_opaque {
                    continue;
                }
                let k = (id.0 as usize, tex.map(|t| t.0), bind.0);
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
            for (mesh_idx, tex_key, bind_id) in keys {
                let start = raws.len() as u32;
                for (id, tex, bind, raw) in flsl {
                    if id.0 as usize == mesh_idx && tex.map(|t| t.0) == tex_key && bind.0 == bind_id
                    {
                        raws.push(*raw);
                    }
                }
                let count = raws.len() as u32 - start;
                if count > 0 {
                    buckets.push((mesh_idx, tex_key, bind_id, start, count));
                }
            }
            buckets
        };
        let flsl_opaque = flsl_bucketize(true, &mut raws);
        let flsl_blended = flsl_bucketize(false, &mut raws);

        self.ensure_instances(gpu, raws.len() as u32);
        if !raws.is_empty() {
            gpu.queue.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&raws));
        }

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raster") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("raster"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: color_load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth,
                    depth_ops: Some(wgpu::Operations { load: depth_load, store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_bind_group(0, &self.globals_bind, &[]);
            rp.set_bind_group(2, field.unwrap_or(&self.empty_field_bind), &[]);
            rp.set_vertex_buffer(1, self.instance_buf.slice(..));
            let draw = |rp: &mut wgpu::RenderPass<'_>, buckets: &[(usize, Option<u32>, u32, u32)]| {
                for &(mesh_idx, tex_key, start, count) in buckets {
                    let mesh = &self.meshes[mesh_idx];
                    // A material texture overrides the mesh's own base-color texture.
                    let bind = match tex_key {
                        Some(t) => &self.textures[t as usize].bind,
                        None => &mesh.tex_bind,
                    };
                    rp.set_bind_group(1, bind, &[]);
                    rp.set_vertex_buffer(0, mesh.gpu_mesh.vbuf.slice(..));
                    rp.set_index_buffer(mesh.gpu_mesh.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                    rp.draw_indexed(0..mesh.gpu_mesh.index_count, 0, start..(start + count));
                }
            };
            let draw_flsl =
                |rp: &mut wgpu::RenderPass<'_>, buckets: &[(usize, Option<u32>, u32, u32, u32)]| {
                    for &(mesh_idx, tex_key, bind_id, start, count) in buckets {
                        let binding = &self.flsl_bindings[bind_id as usize];
                        let shader = &self.flsl_shaders[binding.shader.0 as usize];
                        let mesh = &self.meshes[mesh_idx];
                        let bind = match tex_key {
                            Some(t) => &self.textures[t as usize].bind,
                            None => &mesh.tex_bind,
                        };
                        rp.set_pipeline(&shader.pipeline);
                        rp.set_bind_group(1, bind, &[]);
                        rp.set_bind_group(3, &binding.bind, &[]);
                        rp.set_vertex_buffer(0, mesh.gpu_mesh.vbuf.slice(..));
                        rp.set_index_buffer(mesh.gpu_mesh.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                        rp.draw_indexed(0..mesh.gpu_mesh.index_count, 0, start..(start + count));
                    }
                };
            rp.set_pipeline(&self.pipeline);
            draw(&mut rp, &opaque_buckets);
            draw_flsl(&mut rp, &flsl_opaque);
            if !transparent_buckets.is_empty() {
                rp.set_pipeline(&self.transparent_pipeline);
                draw(&mut rp, &transparent_buckets);
            }
            draw_flsl(&mut rp, &flsl_blended);
        }
        gpu.queue.submit([encoder.finish()]);
    }

    /// The prepass depth target's view (valid after [`depth_prepass`](Self::depth_prepass)
    /// ran at least once) — what `Raymarch::set_depth_prime` binds as the march cap.
    pub fn prepass_view(&self) -> Option<&wgpu::TextureView> {
        self.prepass_tex.as_ref().map(|(_, v)| v)
    }

    /// Depth-only prepass over the OPAQUE instances (per-texel conservative alpha
    /// discard — see `fs_depth`), rendered into the raster's own sampleable depth
    /// target and then copied over `main_depth`:
    ///
    /// - the copied depth PRIMES early-z for the color pass, so hidden opaque
    ///   fragments never run the (field-marching) fragment shader regardless of
    ///   draw order — the color pass must therefore Load, not Clear, the depth;
    /// - the sampleable copy caps the raymarch per pixel (`set_depth_prime`), so
    ///   SDF rays stop at the nearest mesh instead of marching the field behind it.
    ///
    /// Returns `true` when the prepass target was (re)created (size change) — the
    /// caller must then re-bind it on the raymarch, whose bind group is immutable.
    pub fn depth_prepass(
        &mut self,
        gpu: &Gpu,
        globals: Globals,
        instances: &[(MeshId, Option<TexId>, InstanceRaw)],
        main_depth: &wgpu::Texture,
    ) -> bool {
        self.depth_prepass_with(gpu, globals, instances, &[], main_depth)
    }

    /// [`depth_prepass`](Self::depth_prepass) plus custom-shader draws:
    /// opaque-phase flsl instances prime depth too (their group(1) base texture
    /// drives the same conservative alpha discard).
    pub fn depth_prepass_with(
        &mut self,
        gpu: &Gpu,
        globals: Globals,
        instances: &[(MeshId, Option<TexId>, InstanceRaw)],
        flsl: &[FlslDraw],
        main_depth: &wgpu::Texture,
    ) -> bool {
        let size = main_depth.size();
        let recreated = match &self.prepass_tex {
            Some((t, _)) => t.size() != size,
            None => true,
        };
        if recreated {
            let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("raster-prepass-depth"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: Gpu::DEPTH_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            self.prepass_tex = Some((tex, view));
        }
        gpu.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        // Opaque instances only, bucketed by (mesh, texture) exactly like
        // `draw_scene` (the texture is bound for the per-texel alpha discard).
        // Opaque-SHADER flsl draws join in — their phase comes from the shader,
        // not the instance alpha.
        const OPAQUE_CUTOFF: f32 = 0.999;
        let flsl_opaque = |bind: &FlslBindingId| {
            self.flsl_bindings
                .get(bind.0 as usize)
                .and_then(|b| self.flsl_shaders.get(b.shader.0 as usize))
                .is_some_and(|s| s.opaque)
        };
        let mut raws: Vec<InstanceRaw> = Vec::new();
        let mut buckets: Vec<(usize, Option<u32>, u32, u32)> = Vec::new();
        let mut keys: Vec<(usize, Option<u32>)> = Vec::new();
        for (id, tex, raw) in instances {
            if raw.color[3] >= OPAQUE_CUTOFF {
                let k = (id.0 as usize, tex.map(|t| t.0));
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
        for (id, tex, bind, _) in flsl {
            if flsl_opaque(bind) {
                let k = (id.0 as usize, tex.map(|t| t.0));
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
        for (mesh_idx, tex_key) in keys {
            let start = raws.len() as u32;
            for (id, tex, raw) in instances {
                if raw.color[3] >= OPAQUE_CUTOFF
                    && id.0 as usize == mesh_idx
                    && tex.map(|t| t.0) == tex_key
                {
                    raws.push(*raw);
                }
            }
            for (id, tex, bind, raw) in flsl {
                if flsl_opaque(bind) && id.0 as usize == mesh_idx && tex.map(|t| t.0) == tex_key {
                    raws.push(*raw);
                }
            }
            let count = raws.len() as u32 - start;
            if count > 0 {
                buckets.push((mesh_idx, tex_key, start, count));
            }
        }
        self.ensure_instances(gpu, raws.len() as u32);
        if !raws.is_empty() {
            gpu.queue.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&raws));
        }
        let (prepass_tex, prepass_view) = self.prepass_tex.as_ref().expect("prepass target");

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raster-prepass") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("raster-prepass"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: prepass_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.prepass_pipeline);
            rp.set_bind_group(0, &self.globals_bind, &[]);
            rp.set_vertex_buffer(1, self.instance_buf.slice(..));
            for &(mesh_idx, tex_key, start, count) in &buckets {
                let mesh = &self.meshes[mesh_idx];
                let bind = match tex_key {
                    Some(t) => &self.textures[t as usize].bind,
                    None => &mesh.tex_bind,
                };
                rp.set_bind_group(1, bind, &[]);
                rp.set_vertex_buffer(0, mesh.gpu_mesh.vbuf.slice(..));
                rp.set_index_buffer(mesh.gpu_mesh.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                rp.draw_indexed(0..mesh.gpu_mesh.index_count, 0, start..(start + count));
            }
        }
        // Prime the frame's depth buffer with the prepass result.
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: prepass_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: main_depth,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            size,
        );
        gpu.queue.submit([encoder.finish()]);
        recreated
    }
}

/// Upload an RGBA8 image as a single-level sRGB texture (base-color data is sRGB).
fn upload_texture(gpu: &Gpu, t: &TextureData) -> wgpu::Texture {
    upload_texture_mips(gpu, t, false)
}

/// Upload an RGBA8 image as an sRGB texture; if `gen_mips`, generate a full mip chain
/// (box-filtered on the CPU) so it can be sampled trilinearly without shimmering when
/// minified into the distance.
fn upload_texture_mips(gpu: &Gpu, t: &TextureData, gen_mips: bool) -> wgpu::Texture {
    let w0 = t.width.max(1);
    let h0 = t.height.max(1);
    let mip_count = if gen_mips { 1 + (w0.max(h0) as f32).log2().floor() as u32 } else { 1 };
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("raster-basecolor"),
        size: wgpu::Extent3d { width: w0, height: h0, depth_or_array_layers: 1 },
        mip_level_count: mip_count.max(1),
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let write = |level: u32, w: u32, h: u32, pixels: &[u8]| {
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: level,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * w), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    };
    write(0, w0, h0, &t.pixels);
    if gen_mips {
        let mut cur = t.pixels.clone();
        let (mut cw, mut ch) = (w0, h0);
        for level in 1..mip_count {
            let nw = (cw >> 1).max(1);
            let nh = (ch >> 1).max(1);
            let mut next = vec![0u8; (nw * nh * 4) as usize];
            for y in 0..nh {
                for x in 0..nw {
                    let sx = (x * 2).min(cw - 1);
                    let sy = (y * 2).min(ch - 1);
                    let sx1 = (sx + 1).min(cw - 1);
                    let sy1 = (sy + 1).min(ch - 1);
                    for c in 0..4u32 {
                        let p = |px: u32, py: u32| cur[((py * cw + px) * 4 + c) as usize] as u32;
                        let avg = (p(sx, sy) + p(sx1, sy) + p(sx, sy1) + p(sx1, sy1) + 2) / 4;
                        next[((y * nw + x) * 4 + c) as usize] = avg as u8;
                    }
                }
            }
            write(level, nw, nh, &next);
            cur = next;
            cw = nw;
            ch = nh;
        }
    }
    texture
}

/// Pack a model matrix + a plain matte color into an `InstanceRaw`.
pub fn instance_of(model: Mat4, color: [f32; 3]) -> InstanceRaw {
    instance_of_mat(model, &MaterialParams::flat(color))
}

/// Pack a model matrix + a full [`MaterialParams`] into an `InstanceRaw`, computing
/// the inverse-transpose normal matrix from its upper-3×3.
pub fn instance_of_mat(model: Mat4, m: &MaterialParams) -> InstanceRaw {
    // The inverse-transpose is correct under rotation + non-uniform scale; guard a
    // degenerate (zero/singular) scale, whose non-invertible 3×3 would otherwise
    // emit NaN normals and blacken that object's lighting.
    let m3 = glam::Mat3::from_mat4(model);
    let nm = if m3.determinant().abs() > 1e-12 { m3.inverse().transpose() } else { m3 };
    InstanceRaw {
        model: model.to_cols_array_2d(),
        // n0.w = the terrain color base (0 = none): a plain index, NOT bit-packed, so
        // unlike params.z it needs no decode ceremony — but it is still read only in
        // `vs`, where it is exact off the attribute rather than interpolated.
        normal_mat: [
            [nm.x_axis.x, nm.x_axis.y, nm.x_axis.z, m.terrain_paint_base as f32],
            [nm.y_axis.x, nm.y_axis.y, nm.y_axis.z, f32::from(m.paint_modulate)],
            [nm.z_axis.x, nm.z_axis.y, nm.z_axis.z, f32::from(m.terrain_splat)],
        ],
        color: [m.color[0], m.color[1], m.color[2], m.alpha],
        emissive: [m.emissive[0], m.emissive[1], m.emissive[2], m.emissive_strength],
        specular: [m.specular[0], m.specular[1], m.specular[2], m.specular_strength],
        // params.z packs TWO things: bit 0 = unlit, bits 1.. = the vertex-paint base
        // (0 = unpainted). Exact in f32 up to 2^24; `alloc_paint` refuses past that.
        //
        // The fragment shader NEVER sees this packing — `vs` decodes it and re-emits a
        // clean 0/1 into `VsOut.params.z`, because fs reads it as `> 0.5` (a THRESHOLD,
        // not a bit test): a raw packed value there would make every painted node
        // silently render unlit. Keep the decode in `vs`. See raster.wgsl's `vs`.
        params: [
            m.shininess,
            m.rim_strength,
            (u32::from(m.unlit) | (m.paint_base << 1)) as f32,
            m.ambient,
        ],
        // rim.w = packed tiling flags: mode + rotation deci-degrees (exact small
        // int in f32 — well under 2^24). Rotation only means anything in mode 1.
        rim: [
            m.rim[0],
            m.rim[1],
            m.rim[2],
            (m.tile_mode.min(2) as u32
                + (m.tile_rotation.rem_euclid(360.0) * 10.0).round() as u32 * 4)
                as f32,
        ],
        tile: m.tile,
    }
}
