//! The game-UI render pass (docs/ui-system-proposal.md §10).
//!
//! Consumes a [`floptle_ui::DrawList`] (design units) and draws it in ONE
//! instanced pipeline: solid rounded-rect shapes (SDF mask in the fragment),
//! images (any raster-registered texture), and text (fontdue-rasterized glyphs
//! from a shared R8 atlas). Batches switch only the bound texture, so a whole
//! layer is a handful of draw calls regardless of element count — the GPU does
//! the visual work, the CPU only packs instances for layers that changed.
//!
//! Text: the engine embeds one neutral fallback font (Roboto, Apache-2.0 — a
//! technical necessity like the untextured-cube checker, not a look). Project
//! fonts land in a later phase.

use std::collections::HashMap;

use crate::device::Gpu;
use crate::raster::{Raster, TexId};
use floptle_ui::{Align, Blend, DrawList, ImageFit, Overflow, QuadKind};

/// One quad/glyph instance — mirrors `ui.wgsl`'s thirteen vec4 attributes.
///
/// **Attribute budget.** This uses shader locations 1–13; location 0 is the
/// unit-quad corner. WebGPU guarantees 16, so there are two spare. That is
/// deliberate headroom: the raster pipeline is already at 16/16 and adding an
/// attribute there is now a refactor rather than an edit. If a future feature
/// needs more than two lanes, move the gradient stops to a storage buffer
/// indexed by instance (the `vpaint` pattern) rather than packing harder.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct UiInstance {
    /// x, y, w, h in physical px.
    pub rect: [f32; 4],
    /// Flat fill, or the near stop of a gradient.
    pub color: [f32; 4],
    pub border_color: [f32; 4],
    /// feather px, kind (0 shape/image, 1 glyph, 2 shadow, 3 inset), clip
    /// radius px, unused.
    pub params: [f32; 4],
    /// u0, v0, u1, v1.
    pub uv: [f32; 4],
    /// UI-mask clip rect in px (w <= 0 = unclipped).
    pub clip: [f32; 4],
    /// Corner radii in px: TL, TR, BR, BL.
    pub radius: [f32; 4],
    /// Border widths in px: L, T, R, B.
    pub border: [f32; 4],
    /// The gradient's far stop.
    pub grad_to: [f32; 4],
    /// kind (0 none, 1 linear, 2 radial, 3 angular), angle rad, mid, extent.
    pub grad_cfg: [f32; 4],
    /// rotation rad, scale x, scale y, unused.
    pub xform: [f32; 4],
    /// grain amount, grain cell px, pivot x, pivot y (fractions of `rect`).
    pub fx: [f32; 4],
    /// Inset shadow only: offset x, offset y, spread px, unused.
    pub inset: [f32; 4],
}

impl Default for UiInstance {
    fn default() -> Self {
        UiInstance {
            rect: [0.0; 4],
            color: [1.0; 4],
            border_color: [0.0; 4],
            params: [0.0; 4],
            uv: [0.0, 0.0, 1.0, 1.0],
            clip: [0.0; 4],
            radius: [0.0; 4],
            border: [0.0; 4],
            grad_to: [0.0; 4],
            grad_cfg: [0.0; 4],
            xform: [0.0, 1.0, 1.0, 0.0],
            fx: [0.0, 1.0, 0.5, 0.5],
            inset: [0.0; 4],
        }
    }
}

/// What a batch binds at group 1.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UiTex {
    /// 1×1 white — solid shapes (color shows through unchanged).
    White,
    /// The glyph atlas.
    Atlas,
    /// A raster-registered project texture.
    Tex(TexId),
}

/// Resolves a Quad's `(flsl path, owner element id)` to a registered pipeline
/// + that element's params binding (None = fall back to a plain quad).
pub type UiShaderResolve<'a> = &'a mut dyn FnMut(&str, u32) -> Option<(UiShaderId, UiBindingId)>;

pub struct UiBatch {
    pub tex: UiTex,
    /// Custom-shader batch: the pipeline + per-element param binding to use
    /// instead of the built-in `fs_main` (a `stage ui` .flsl element).
    pub shader: Option<(UiShaderId, UiBindingId)>,
    /// Compositing mode. A batch key rather than instance data — blending is
    /// pipeline state, and a run of same-mode quads still coalesces into one
    /// draw call, so a screen that never leaves Normal pays nothing.
    pub blend: Blend,
    pub range: std::ops::Range<u32>,
}

/// The wgpu blend state for each [`Blend`] mode. Premultiplication is NOT
/// assumed anywhere in the UI pass — colours arrive straight, so `SrcAlpha`
/// appears on the source factor of every mode that respects alpha.
fn blend_state(b: Blend) -> wgpu::BlendState {
    use wgpu::{BlendComponent, BlendFactor, BlendOperation, BlendState};
    match b {
        Blend::Normal => BlendState::ALPHA_BLENDING,
        // Adds light. Alpha still gates it, so a faded-out glow disappears
        // rather than staying at full strength.
        Blend::Additive => BlendState {
            color: BlendComponent {
                src_factor: BlendFactor::SrcAlpha,
                dst_factor: BlendFactor::One,
                operation: BlendOperation::Add,
            },
            alpha: BlendComponent {
                src_factor: BlendFactor::Zero,
                dst_factor: BlendFactor::One,
                operation: BlendOperation::Add,
            },
        },
        // Darkens. Lerped by alpha so a half-transparent multiply is a half
        // multiply, not a half-strength one applied at full coverage.
        Blend::Multiply => BlendState {
            color: BlendComponent {
                src_factor: BlendFactor::Dst,
                dst_factor: BlendFactor::OneMinusSrcAlpha,
                operation: BlendOperation::Add,
            },
            alpha: BlendComponent {
                src_factor: BlendFactor::Zero,
                dst_factor: BlendFactor::One,
                operation: BlendOperation::Add,
            },
        },
        // Lightens without the blow-out Additive gives.
        Blend::Screen => BlendState {
            color: BlendComponent {
                src_factor: BlendFactor::SrcAlpha,
                dst_factor: BlendFactor::OneMinusSrc,
                operation: BlendOperation::Add,
            },
            alpha: BlendComponent {
                src_factor: BlendFactor::Zero,
                dst_factor: BlendFactor::One,
                operation: BlendOperation::Add,
            },
        },
    }
}

/// Re-express an element-level pivot for a SUB-rect of that element.
///
/// Glyphs and 9-slice patches are drawn as their own quads, but a rotation or
/// scale belongs to the whole element — spinning each glyph about its own
/// centre would be nonsense. The transform maths is per-quad, so instead we
/// keep the transform and move the pivot: the element's pivot point, expressed
/// as a fraction of the patch's rect (routinely outside 0..1, which is fine).
/// Where a run of width `run_w` starts inside `rect`, per its alignment.
///
/// One function because the glyph pass, the caret and the selection band must
/// agree to the pixel — three copies of this arithmetic is exactly how a caret
/// ends up half a character off in centred text.
fn align_x(align: Align, rect: [f32; 4], run_w: f32) -> f32 {
    match align {
        Align::Start | Align::Stretch => rect[0],
        Align::Center => rect[0] + (rect[2] - run_w) * 0.5,
        Align::End => rect[0] + rect[2] - run_w,
    }
}

fn pivot_for(patch: [f32; 4], element: [f32; 4], pivot: [f32; 2]) -> [f32; 2] {
    let anchor = [element[0] + element[2] * pivot[0], element[1] + element[3] * pivot[1]];
    [
        if patch[2].abs() > 1e-6 { (anchor[0] - patch[0]) / patch[2] } else { 0.5 },
        if patch[3].abs() > 1e-6 { (anchor[1] - patch[1]) / patch[3] } else { 0.5 },
    ]
}

/// Expand one textured quad into the nine patches of a 9-slice.
///
/// `slice` is `[L, T, R, B]` as a fraction of the sampled UV region; `tex` is
/// the source's pixel size. Corner patches are drawn at their source pixel size
/// times `scale`, so a panel's border art keeps a constant visual weight no
/// matter how large the panel gets — which is the entire point, and the reason
/// authored panel textures were unusable before this.
///
/// Degenerate cases fall back to the single stretched quad: insets that meet or
/// cross, or an element too small to seat its own corners.
fn nine_slice(base: &UiInstance, slice: [f32; 4], tex: [f32; 2], scale: f32) -> Vec<UiInstance> {
    let (u0, v0, u1, v1) = (base.uv[0], base.uv[1], base.uv[2], base.uv[3]);
    let (su, sv) = (u1 - u0, v1 - v0);
    let (l, t, r, b) = (slice[0], slice[1], slice[2], slice[3]);
    if l + r >= 1.0 || t + b >= 1.0 || su <= 0.0 || sv <= 0.0 {
        return vec![*base];
    }
    // Patch widths in px: source fraction → source px → design px.
    let src_w = tex[0] * su;
    let src_h = tex[1] * sv;
    let (pl, pr) = (src_w * l * scale, src_w * r * scale);
    let (pt, pb) = (src_h * t * scale, src_h * b * scale);
    let (w, h) = (base.rect[2], base.rect[3]);
    if pl + pr > w || pt + pb > h {
        // The element is smaller than its own frame — stretching the whole
        // texture looks better than overlapping corners.
        return vec![*base];
    }
    let xs = [base.rect[0], base.rect[0] + pl, base.rect[0] + w - pr];
    let ws = [pl, w - pl - pr, pr];
    let ys = [base.rect[1], base.rect[1] + pt, base.rect[1] + h - pb];
    let hs = [pt, h - pt - pb, pb];
    let us = [u0, u0 + su * l, u1 - su * r, u1];
    let vs = [v0, v0 + sv * t, v1 - sv * b, v1];

    let mut out = Vec::with_capacity(9);
    for row in 0..3 {
        for col in 0..3 {
            if ws[col] <= 0.0 || hs[row] <= 0.0 {
                continue;
            }
            let mut inst = *base;
            inst.rect = [xs[col], ys[row], ws[col], hs[row]];
            inst.uv = [us[col], vs[row], us[col + 1], vs[row + 1]];
            // The frame art carries the corners now; a rounded-rect mask on
            // each patch would clip the middle of the image.
            inst.radius = [0.0; 4];
            inst.border = [0.0; 4];
            inst.fx[2] = pivot_for(inst.rect, base.rect, [base.fx[2], base.fx[3]])[0];
            inst.fx[3] = pivot_for(inst.rect, base.rect, [base.fx[2], base.fx[3]])[1];
            out.push(inst);
        }
    }
    out
}

/// Index of a blend mode in the pipeline arrays.
fn blend_index(b: Blend) -> usize {
    match b {
        Blend::Normal => 0,
        Blend::Additive => 1,
        Blend::Multiply => 2,
        Blend::Screen => 3,
    }
}

/// A registered `stage ui` .flsl pipeline (screen + world variants).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiShaderId(pub u32);

/// A per-element params UBO bound at group(2) of a UI shader pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiBindingId(pub u32);

struct UiShader {
    pipeline: wgpu::RenderPipeline,
    /// Depth-tested variant for world canvases (Scene-view authoring).
    pipeline_world: wgpu::RenderPipeline,
}

struct UiShaderBinding {
    params_buf: wgpu::Buffer,
    bind: wgpu::BindGroup,
}

/// A cached glyph's atlas placement + metrics (px at its rasterized size).
#[derive(Clone, Copy)]
struct Glyph {
    uv: [f32; 4],
    size: [f32; 2],
    /// Offset from the pen position (x bearing, y from baseline-top).
    offset: [f32; 2],
    advance: f32,
}

const ATLAS_SIZE: u32 = 1024;

/// Mirrors `ui.wgsl`'s Globals.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct UiGlobals {
    /// x, y = viewport px; z = mode (0 screen, 1 world canvas); w = time
    /// in seconds (`time` in `stage ui` .flsl shaders).
    pub viewport: [f32; 4],
    pub plane_origin: [f32; 4],
    pub plane_right: [f32; 4],
    pub plane_down: [f32; 4],
    pub view_proj: [[f32; 4]; 4],
}

/// A world canvas placement: top-left origin + the plane axes, already scaled
/// to world-units-per-design-unit.
#[derive(Clone, Copy, Debug)]
pub struct UiPlane {
    pub origin: [f32; 3],
    pub right: [f32; 3],
    pub down: [f32; 3],
}

pub struct Ui {
    /// One pipeline per [`Blend`] mode, indexed by [`blend_index`].
    pipeline: [wgpu::RenderPipeline; 4],
    /// The world-canvas variants (Scene-view authoring): depth-tested against
    /// the scene so the layer plane sits IN the world.
    pipeline_world: [wgpu::RenderPipeline; 4],
    globals_buf: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    white_bind: wgpu::BindGroup,
    atlas: wgpu::Texture,
    atlas_bind: wgpu::BindGroup,
    /// Font 0 is the embedded fallback; project .ttf/.otf assets append.
    fonts: Vec<fontdue::Font>,
    /// Asset path → index into `fonts` (None = failed to parse, use fallback).
    font_ids: HashMap<String, Option<usize>>,
    glyphs: HashMap<(usize, char, u32), Option<Glyph>>,
    // Shelf packer cursor.
    shelf: (u32, u32, u32),
    instance_buf: wgpu::Buffer,
    instance_cap: u32,
    atlas_full_warned: bool,
    quad_vbuf: wgpu::Buffer,
    quad_ibuf: wgpu::Buffer,
    // UI-shader support (`stage ui` .flsl elements): the shared bind layouts
    // (kept for late pipeline builds), the registered pipelines, and the
    // per-element param bindings.
    globals_layout: wgpu::BindGroupLayout,
    tex_layout: wgpu::BindGroupLayout,
    params_layout: wgpu::BindGroupLayout,
    shaders: Vec<UiShader>,
    shader_binds: Vec<UiShaderBinding>,
    /// Scene time uploaded into `Globals.viewport.w` (the `time` input).
    time: f32,
    /// Sampler for the backdrop (linear, clamped) — the `backdrop()` op.
    backdrop_sampler: wgpu::Sampler,
    /// group(3) bind for `backdrop()` holding the captured scene, valid only
    /// when `has_backdrop` is set (otherwise `backdrop_default_bind` is used).
    backdrop_bind: wgpu::BindGroup,
    /// A 1×1 black fallback bind — used whenever no capture is active this frame,
    /// so `backdrop()` reads black instead of a stale scene.
    backdrop_default_bind: wgpu::BindGroup,
    /// Whether `backdrop_bind` holds a real capture for this frame's draw.
    has_backdrop: bool,
    /// Fullscreen blit that copies the composited scene into `backdrop_target`.
    capture_pipeline: wgpu::RenderPipeline,
    /// The backdrop capture target (texture, view, w, h) — lazily (re)created to
    /// match the viewport, reused across frames.
    backdrop_target: Option<(wgpu::Texture, wgpu::TextureView, u32, u32)>,
}

const QUAD_VERTS: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
const QUAD_INDICES: [u16; 6] = [0, 1, 2, 0, 2, 3];

/// Fullscreen-triangle blit used to CAPTURE the composited scene into the
/// backdrop texture (so `backdrop()` in UI shaders can read the scene behind the
/// layer). group(0) = the source texture + sampler (the `tex_layout` shape).
const CAPTURE_WGSL: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs(@builtin(vertex_index) i: u32) -> VOut {
    var xy = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0))[i];
    var o: VOut;
    o.pos = vec4<f32>(xy, 0.0, 1.0);
    o.uv = vec2<f32>((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return o;
}
@fragment fn fs(in: VOut) -> @location(0) vec4<f32> {
    return textureSample(src, samp, in.uv);
}
"#;

const CORNER_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: 8,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &[wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 0,
        shader_location: 0,
    }],
};

const INSTANCE_ATTRS: [wgpu::VertexAttribute; 13] = [
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 1 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16, shader_location: 2 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 32, shader_location: 3 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 48, shader_location: 4 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 64, shader_location: 5 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 80, shader_location: 6 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 96, shader_location: 7 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 112, shader_location: 8 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 128, shader_location: 9 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 144, shader_location: 10 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 160, shader_location: 11 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 176, shader_location: 12 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 192, shader_location: 13 },
];

const INSTANCE_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<UiInstance>() as u64,
    step_mode: wgpu::VertexStepMode::Instance,
    attributes: &INSTANCE_ATTRS,
};

impl Ui {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ui"),
            source: wgpu::ShaderSource::Wgsl(include_str!("ui.wgsl").into()),
        });
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui-globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                // Fragment too: `stage ui` shaders read `time` from
                // Globals.viewport.w.
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        // Group 1 mirrors the raster material-texture layout, so project
        // textures bind here without re-registration (same trick particles use).
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui-tex"),
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
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ui"),
            bind_group_layouts: &[Some(&globals_layout), Some(&tex_layout)],
            immediate_size: 0,
        });
        // One pipeline per blend mode. Only the colour-target blend state
        // differs, so this is four near-identical descriptors rather than four
        // shaders — and a screen that stays on Normal never binds the others.
        let make_screen = |b: Blend| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("ui"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[CORNER_LAYOUT, INSTANCE_LAYOUT],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: gpu.surface_format(),
                        blend: Some(blend_state(b)),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let make_world = |b: Blend| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("ui-world"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[CORNER_LAYOUT, INSTANCE_LAYOUT],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: gpu.surface_format(),
                        blend: Some(blend_state(b)),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: Gpu::DEPTH_FORMAT,
                    // Test against the scene but DON'T write: an alpha-blended
                    // pass writing depth would make transparent pixels occlude
                    // later layers. Painter's order handles intra-canvas layering.
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let pipeline = [
            make_screen(Blend::Normal),
            make_screen(Blend::Additive),
            make_screen(Blend::Multiply),
            make_screen(Blend::Screen),
        ];
        let pipeline_world = [
            make_world(Blend::Normal),
            make_world(Blend::Additive),
            make_world(Blend::Multiply),
            make_world(Blend::Screen),
        ];

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-globals"),
            size: std::mem::size_of::<UiGlobals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui-globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() }],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ui-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // Repeat, so `ImageSpec::tiling` works by simply sending UVs past
            // 1. Spritesheet cells and 9-slice patches keep their UVs inside
            // [0,1], so nothing that worked under clamping changes.
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            ..Default::default()
        });
        let make_tex_bind = |tex: &wgpu::Texture, label: &str| {
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &tex_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            })
        };

        let white = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui-white"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &white,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255u8; 4],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4), rows_per_image: None },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        let white_bind = make_tex_bind(&white, "ui-white");

        // Glyph atlas: coverage in the red channel; the shader reads `.r`.
        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui-atlas"),
            size: wgpu::Extent3d { width: ATLAS_SIZE, height: ATLAS_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_bind = make_tex_bind(&atlas, "ui-atlas");

        let font = fontdue::Font::from_bytes(
            include_bytes!("../fonts/Roboto-Regular.ttf") as &[u8],
            fontdue::FontSettings::default(),
        )
        .expect("embedded fallback font parses");

        let quad_vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-quad"),
            size: std::mem::size_of_val(&QUAD_VERTS) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue.write_buffer(&quad_vbuf, 0, bytemuck::cast_slice(&QUAD_VERTS));
        let quad_ibuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-quad-idx"),
            size: std::mem::size_of_val(&QUAD_INDICES) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue.write_buffer(&quad_ibuf, 0, bytemuck::cast_slice(&QUAD_INDICES));
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-instances"),
            size: 1024 * std::mem::size_of::<UiInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let params_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui-flsl-params"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        // Backdrop (group 3): a dedicated clamped sampler + a 1×1 black default
        // so `backdrop()` reads black until a real capture is bound this frame.
        let backdrop_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ui-backdrop-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let black = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui-backdrop-black"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &black,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[0u8, 0, 0, 255],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4), rows_per_image: None },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        let make_backdrop_bind = |view: &wgpu::TextureView| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ui-backdrop"),
                layout: &tex_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&backdrop_sampler),
                    },
                ],
            })
        };
        let black_view = black.create_view(&wgpu::TextureViewDescriptor::default());
        let backdrop_default_bind = make_backdrop_bind(&black_view);
        let backdrop_bind = make_backdrop_bind(&black_view);

        // The scene→backdrop capture blit (group(0) = source tex+sampler).
        let capture_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ui-capture"),
            source: wgpu::ShaderSource::Wgsl(CAPTURE_WGSL.into()),
        });
        let capture_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ui-capture"),
            bind_group_layouts: &[Some(&tex_layout)],
            immediate_size: 0,
        });
        let capture_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ui-capture"),
            layout: Some(&capture_layout),
            vertex: wgpu::VertexState {
                module: &capture_module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &capture_module,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: gpu.surface_format(),
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Ui {
            pipeline,
            pipeline_world,
            globals_buf,
            globals_bind,
            white_bind,
            atlas,
            atlas_bind,
            fonts: vec![font],
            font_ids: HashMap::new(),
            glyphs: HashMap::new(),
            shelf: (0, 0, 0),
            instance_buf,
            instance_cap: 1024,
            atlas_full_warned: false,
            quad_vbuf,
            quad_ibuf,
            globals_layout,
            tex_layout,
            params_layout,
            shaders: Vec::new(),
            shader_binds: Vec::new(),
            time: 0.0,
            backdrop_sampler,
            backdrop_bind,
            backdrop_default_bind,
            has_backdrop: false,
            capture_pipeline,
            backdrop_target: None,
        }
    }

    /// Capture the composited scene in `src_view` (the color target holding
    /// everything drawn BEFORE this UI layer) into the backdrop texture, and
    /// point `backdrop()` at it for this frame's draw. Records into `enc`; the
    /// caller must run this BEFORE `draw`, and pass the same `src_view` its UI is
    /// about to be drawn on top of. `w`/`h` are the target's physical size.
    pub fn capture_backdrop(
        &mut self,
        gpu: &Gpu,
        enc: &mut wgpu::CommandEncoder,
        src_view: &wgpu::TextureView,
        w: u32,
        h: u32,
    ) {
        let (w, h) = (w.max(1), h.max(1));
        let need_new = !matches!(&self.backdrop_target, Some((_, _, tw, th)) if *tw == w && *th == h);
        if need_new {
            let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ui-backdrop-target"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: gpu.surface_format(),
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            self.backdrop_target = Some((tex, view, w, h));
        }
        let src_bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui-capture-src"),
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(src_view) },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.backdrop_sampler),
                },
            ],
        });
        let target_view = &self.backdrop_target.as_ref().unwrap().1;
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui-backdrop-capture"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.capture_pipeline);
            rp.set_bind_group(0, &src_bind, &[]);
            rp.draw(0..3, 0..1);
        }
        // Point group(3) at the freshly captured backdrop for this frame's draw.
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui-backdrop"),
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&self.backdrop_target.as_ref().unwrap().1),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.backdrop_sampler),
                },
            ],
        });
        self.backdrop_bind = bind;
        self.has_backdrop = true;
    }

    /// Point `backdrop()` at a captured scene texture for this frame's UI draw.
    /// Call `clear_backdrop` afterwards (or before a layer that shouldn't frost)
    /// so a stale capture never leaks in.
    pub fn set_backdrop(&mut self, gpu: &Gpu, view: &wgpu::TextureView) {
        self.backdrop_bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui-backdrop"),
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.backdrop_sampler),
                },
            ],
        });
        self.has_backdrop = true;
    }

    /// Reset `backdrop()` to the 1×1 black default for the next draw.
    pub fn clear_backdrop(&mut self) {
        self.has_backdrop = false;
    }

    /// The active group(3) backdrop bind for this frame (real capture or black).
    fn backdrop_group(&self) -> &wgpu::BindGroup {
        if self.has_backdrop { &self.backdrop_bind } else { &self.backdrop_default_bind }
    }

    /// The UI pass's WGSL source — the prelude a `stage ui` .flsl chunk is
    /// concatenated onto (the editor validates against this + the shader
    /// crate's field shim/support before registering).
    pub fn ui_prelude() -> &'static str {
        include_str!("ui.wgsl")
    }

    /// Register (or hot-replace) a `stage ui` .flsl pipeline from its WGSL
    /// chunk. Like the raster's `register_flsl_shader`, the field shim +
    /// stdlib SUPPORT arrive INSIDE `chunk` (caller-assembled); the module is
    /// `ui.wgsl + chunk`. Validate the assembly with naga BEFORE calling — a
    /// bad module aborts the device.
    pub fn register_ui_shader(
        &mut self,
        gpu: &Gpu,
        chunk: &str,
        replace: Option<UiShaderId>,
    ) -> UiShaderId {
        let src = format!("{}\n{}", include_str!("ui.wgsl"), chunk);
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ui-flsl"),
            source: wgpu::ShaderSource::Wgsl(src.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ui-flsl"),
            bind_group_layouts: &[
                Some(&self.globals_layout),
                Some(&self.tex_layout),
                Some(&self.params_layout),
                // group(3) = the backdrop (scene behind the UI). Reuses the
                // texture+sampler layout; always bound (a 1×1 default when no
                // capture is active), so every UI shader shares this layout.
                Some(&self.tex_layout),
            ],
            immediate_size: 0,
        });
        let targets = [Some(wgpu::ColorTargetState {
            format: gpu.surface_format(),
            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
        })];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ui-flsl"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[CORNER_LAYOUT, INSTANCE_LAYOUT],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_flsl_ui"),
                targets: &targets,
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let pipeline_world = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ui-flsl-world"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[CORNER_LAYOUT, INSTANCE_LAYOUT],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_flsl_ui"),
                targets: &targets,
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: Gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let shader = UiShader { pipeline, pipeline_world };
        match replace {
            Some(id) => {
                self.shaders[id.0 as usize] = shader;
                id
            }
            None => {
                self.shaders.push(shader);
                UiShaderId(self.shaders.len() as u32 - 1)
            }
        }
    }

    /// Create (or replace) a per-element params UBO + bind group for a UI
    /// shader. `params` are the packed vec4 lanes (`CompiledUi::pack_params`).
    pub fn set_ui_shader_binding(
        &mut self,
        gpu: &Gpu,
        params: &[u8],
        replace: Option<UiBindingId>,
    ) -> UiBindingId {
        let params_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-flsl-params"),
            size: params.len().max(16) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue.write_buffer(&params_buf, 0, params);
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui-flsl-params"),
            layout: &self.params_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buf.as_entire_binding(),
            }],
        });
        let binding = UiShaderBinding { params_buf, bind };
        match replace {
            Some(id) => {
                self.shader_binds[id.0 as usize] = binding;
                id
            }
            None => {
                self.shader_binds.push(binding);
                UiBindingId(self.shader_binds.len() as u32 - 1)
            }
        }
    }

    /// Update a binding's params in place — a uniform write, never a rebuild
    /// (scripts drive instrument knobs every tick through this).
    pub fn write_ui_shader_params(&self, gpu: &Gpu, id: UiBindingId, params: &[u8]) {
        if let Some(b) = self.shader_binds.get(id.0 as usize) {
            gpu.queue.write_buffer(&b.params_buf, 0, params);
        }
    }

    /// Scene time for `stage ui` shaders' `time` input (Globals.viewport.w).
    pub fn set_time(&mut self, t: f32) {
        self.time = t;
    }

    /// Register a project font (.ttf/.otf bytes) under its asset path. Parse
    /// failures are remembered (and warned once) — the text falls back to font 0.
    pub fn ensure_font(&mut self, path: &str, bytes: &[u8]) {
        if self.font_ids.contains_key(path) {
            return;
        }
        let id = match fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
            Ok(f) => {
                self.fonts.push(f);
                Some(self.fonts.len() - 1)
            }
            Err(e) => {
                log::warn!("ui font '{path}' failed to parse: {e} — using the fallback");
                None
            }
        };
        self.font_ids.insert(path.to_string(), id);
    }

    /// Whether `ensure_font` has already seen this path (ok or failed).
    pub fn has_font(&self, path: &str) -> bool {
        self.font_ids.contains_key(path)
    }

    /// The font index for an asset path (0 = fallback for empty/unknown/failed).
    pub fn font_id(&self, path: &str) -> usize {
        if path.is_empty() {
            return 0;
        }
        self.font_ids.get(path).copied().flatten().unwrap_or(0)
    }

    /// Measure a single-line run in the same units as `size` (the layout
    /// solver's Fit callback — design units in, design units out).
    pub fn measure(&self, text: &str, size: f32) -> [f32; 2] {
        self.measure_font(0, text, size)
    }

    /// Measure with a text spec's font (the solver callback for real layers).
    pub fn measure_spec(&self, t: &floptle_ui::TextSpec) -> [f32; 2] {
        self.measure_font(self.font_id(&t.font), t.text.as_str(), t.size)
    }

    fn measure_font(&self, fid: usize, text: &str, size: f32) -> [f32; 2] {
        let font = &self.fonts[fid];
        let mut w = 0.0f32;
        for c in text.chars() {
            w += font.metrics(c, size).advance_width;
        }
        let lm = font.horizontal_line_metrics(size);
        let h = lm.map(|l| l.ascent - l.descent).unwrap_or(size);
        [w, h]
    }

    /// Rasterize-or-fetch a glyph at an exact pixel size.
    fn glyph(&mut self, gpu: &Gpu, fid: usize, c: char, px: u32) -> Option<Glyph> {
        if let Some(g) = self.glyphs.get(&(fid, c, px)) {
            return *g;
        }
        let (metrics, bitmap) = self.fonts[fid].rasterize(c, px as f32);
        let g = if metrics.width == 0 || metrics.height == 0 {
            // Whitespace: advance only.
            Some(Glyph {
                uv: [0.0; 4],
                size: [0.0, 0.0],
                offset: [0.0, 0.0],
                advance: metrics.advance_width,
            })
        } else {
            let (w, h) = (metrics.width as u32, metrics.height as u32);
            let (mut cx, mut cy, mut row_h) = self.shelf;
            if cx + w + 1 > ATLAS_SIZE {
                cx = 0;
                cy += row_h + 1;
                row_h = 0;
            }
            if cy + h + 1 > ATLAS_SIZE {
                if !self.atlas_full_warned {
                    log::warn!("ui glyph atlas full — some text will not render");
                    self.atlas_full_warned = true;
                }
                self.glyphs.insert((fid, c, px), None);
                return None;
            }
            gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: cx, y: cy, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &bitmap,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w),
                    rows_per_image: None,
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            self.shelf = (cx + w + 1, cy, row_h.max(h));
            let s = ATLAS_SIZE as f32;
            Some(Glyph {
                uv: [cx as f32 / s, cy as f32 / s, (cx + w) as f32 / s, (cy + h) as f32 / s],
                size: [metrics.width as f32, metrics.height as f32],
                offset: [metrics.xmin as f32, -(metrics.ymin as f32) - metrics.height as f32],
                advance: metrics.advance_width,
            })
        };
        self.glyphs.insert((fid, c, px), g);
        g
    }

    /// Pack a layer's draw list into instances + batches. `scale` maps design
    /// units to physical px; `origin` offsets everything (the Game viewport's
    /// top-left). `resolve` maps texture asset paths to registered ids.
    #[allow(clippy::too_many_arguments)]
    pub fn pack(
        &mut self,
        gpu: &Gpu,
        list: &DrawList,
        origin: [f32; 2],
        scale: f32,
        resolve: &mut dyn FnMut(&str) -> Option<TexId>,
        tex_size: &dyn Fn(TexId) -> Option<[f32; 2]>,
        resolve_shader: UiShaderResolve,
        instances: &mut Vec<UiInstance>,
        batches: &mut Vec<UiBatch>,
    ) {
        let clip_px = |clip: &Option<floptle_ui::Clip>| -> ([f32; 4], f32) {
            match clip {
                Some(c) => (
                    [
                        origin[0] + c.rect[0] * scale,
                        origin[1] + c.rect[1] * scale,
                        c.rect[2] * scale,
                        c.rect[3] * scale,
                    ],
                    c.radius * scale,
                ),
                None => ([0.0; 4], 0.0),
            }
        };
        let push = |instances: &mut Vec<UiInstance>,
                        batches: &mut Vec<UiBatch>,
                        tex: UiTex,
                        shader: Option<(UiShaderId, UiBindingId)>,
                        blend: Blend,
                        inst: UiInstance| {
            let i = instances.len() as u32;
            instances.push(inst);
            match batches.last_mut() {
                Some(b)
                    if b.tex == tex
                        && b.shader == shader
                        && b.blend == blend
                        && b.range.end == i =>
                {
                    b.range.end = i + 1
                }
                _ => batches.push(UiBatch { tex, shader, blend, range: i..i + 1 }),
            }
        };
        for q in &list.quads {
            let (tex, tex_size) = if q.texture.is_empty() {
                (UiTex::White, None)
            } else {
                match resolve(&q.texture) {
                    Some(id) => (UiTex::Tex(id), tex_size(id)),
                    None => (UiTex::White, None), // missing texture: tinted solid
                }
            };
            // A custom-shader face: unresolved (missing/broken .flsl) falls
            // back to a plain quad, so the element still shows SOMETHING.
            let shader = q.shader.as_ref().and_then(|(p, owner)| resolve_shader(p, *owner));
            let (clip, clip_r) = clip_px(&q.clip);
            let kind = match q.kind {
                QuadKind::Shape => 0.0,
                QuadKind::Shadow => 2.0,
                QuadKind::InsetShadow => 3.0,
            };
            let mut rect = [
                origin[0] + q.rect[0] * scale,
                origin[1] + q.rect[1] * scale,
                q.rect[2] * scale,
                q.rect[3] * scale,
            ];
            let mut uv = q.uv;
            // Aspect handling. Contain/Cover need the source's pixel size, so
            // this is the one place it can happen — floptle-ui is deliberately
            // renderer-agnostic and has no texture registry.
            if q.fit != ImageFit::Stretch
                && let Some([tw, th]) = tex_size
                && tw > 0.0
                && th > 0.0
                && rect[2] > 0.0
                && rect[3] > 0.0
            {
                let src = (tw * (uv[2] - uv[0])).max(1e-4) / (th * (uv[3] - uv[1])).max(1e-4);
                let dst = rect[2] / rect[3];
                match q.fit {
                    ImageFit::Contain => {
                        // Shrink the drawn rect, keeping it centred: the whole
                        // image shows, letterboxed.
                        if src > dst {
                            let h = rect[2] / src;
                            rect[1] += (rect[3] - h) * 0.5;
                            rect[3] = h;
                        } else {
                            let w = rect[3] * src;
                            rect[0] += (rect[2] - w) * 0.5;
                            rect[2] = w;
                        }
                    }
                    ImageFit::Cover => {
                        // Crop the UVs instead: the rect stays full and the
                        // overflow is cut, centred. What a portrait wants.
                        if src > dst {
                            let keep = dst / src;
                            let m = (uv[2] - uv[0]) * (1.0 - keep) * 0.5;
                            uv[0] += m;
                            uv[2] -= m;
                        } else {
                            let keep = src / dst;
                            let m = (uv[3] - uv[1]) * (1.0 - keep) * 0.5;
                            uv[1] += m;
                            uv[3] -= m;
                        }
                    }
                    ImageFit::Stretch => {}
                }
            }
            // An OUTER shadow's soft edge spreads beyond the shape, so its quad
            // has to be bigger than the shape or the feather is clipped square
            // — which is exactly what a glow looked like before this: a hard
            // box with a gradient in it. Grow the geometry and tell the
            // fragment to inset the SDF by the same amount, leaving the drawn
            // shape unchanged.
            let pad = if q.kind == QuadKind::Shadow { q.feather.max(0.0) * scale } else { 0.0 };
            if pad > 0.0 {
                rect = [rect[0] - pad, rect[1] - pad, rect[2] + pad * 2.0, rect[3] + pad * 2.0];
            }
            let base = UiInstance {
                rect,
                color: q.color,
                border_color: q.border_color,
                params: [q.feather * scale, kind, clip_r, 0.0],
                uv,
                clip,
                radius: [
                    q.radius[0] * scale,
                    q.radius[1] * scale,
                    q.radius[2] * scale,
                    q.radius[3] * scale,
                ],
                border: [
                    q.border[0] * scale,
                    q.border[1] * scale,
                    q.border[2] * scale,
                    q.border[3] * scale,
                ],
                grad_to: q.gradient.map(|g| g.to).unwrap_or([0.0; 4]),
                grad_cfg: q.gradient.map(|g| g.pack()).unwrap_or([0.0; 4]),
                xform: [q.xform.rotation, q.xform.scale[0], q.xform.scale[1], 0.0],
                fx: [
                    q.grain.map(|g| g.amount).unwrap_or(0.0),
                    q.grain.map(|g| g.scale * scale).unwrap_or(1.0),
                    q.xform.pivot[0],
                    q.xform.pivot[1],
                ],
                inset: [
                    q.shadow_offset[0] * scale,
                    q.shadow_offset[1] * scale,
                    // Inset shadow: how far in the lit shape shrinks (its
                    // `spread`, which rides the border lane in the draw list
                    // since an inset has no border of its own).
                    // Outer shadow: how far the quad was grown, so the
                    // fragment can inset the SDF back to the real shape.
                    if q.kind == QuadKind::Shadow { pad } else { q.border[0] * scale },
                    0.0,
                ],
            };
            // 9-slice: nine patches instead of one stretched quad. The corners
            // keep their source pixel size (times `scale`, so they track the UI
            // scale rather than the element), the edges stretch on one axis and
            // the middle on both.
            if q.slice.iter().any(|v| *v > 0.0)
                && let Some(size) = tex_size
            {
                for patch in nine_slice(&base, q.slice, size, scale) {
                    push(instances, batches, tex, shader, q.blend, patch);
                }
                continue;
            }
            push(instances, batches, tex, shader, q.blend, base);
        }
        for t in &list.texts {
            let fid = self.font_id(&t.font);
            let rect_px = [
                origin[0] + t.rect[0] * scale,
                origin[1] + t.rect[1] * scale,
                t.rect[2] * scale,
                t.rect[3] * scale,
            ];
            // Dynamic sizing: `fit` scales the glyphs so the run fills the
            // element's rect (largest size that fits both axes).
            let size = if t.fit && !t.text.is_empty() {
                let natural = self.measure_font(fid, &t.text, t.size);
                let f = (t.rect[2] / natural[0].max(1e-3))
                    .min(t.rect[3] / natural[1].max(1e-3))
                    .max(0.01);
                t.size * f
            } else {
                t.size
            };
            let px = (size * scale).round().max(1.0) as u32;
            let track_px = t.tracking * scale;
            // One place decides line breaks. The layout solver measures with
            // the same helper, so a `Fit` element can never disagree with what
            // is finally drawn about where the text wraps.
            // Break, truncate and measure the lines up front. Everything below
            // rasterizes glyphs (`&mut self`), so the measuring closure — which
            // borrows `self` immutably — has to be finished with by then.
            let (lines, widths): (Vec<String>, Vec<f32>) = {
                let advance = |s: &str| {
                    self.measure_font(fid, s, size)[0] * scale
                        + track_px * s.chars().count() as f32
                };
                let mut lines = if t.wrap {
                    floptle_ui::text::wrap_lines(&t.text, rect_px[2], &advance)
                } else {
                    t.text.split('\n').map(str::to_string).collect()
                };
                if t.max_lines > 0 && lines.len() > t.max_lines as usize {
                    lines.truncate(t.max_lines as usize);
                    // The cut is only honest if the last surviving line says so.
                    if t.overflow == Overflow::Ellipsis
                        && let Some(last) = lines.last_mut()
                    {
                        *last = floptle_ui::text::ellipsize(last, rect_px[2], &advance);
                    }
                } else if t.overflow == Overflow::Ellipsis && !t.wrap {
                    for line in &mut lines {
                        *line = floptle_ui::text::ellipsize(line, rect_px[2], &advance);
                    }
                }
                let widths = lines.iter().map(|l| advance(l)).collect();
                (lines, widths)
            };

            let (ascent, descent) = self.fonts[fid]
                .horizontal_line_metrics(px as f32)
                .map(|l| (l.ascent, l.descent))
                .unwrap_or((px as f32, 0.0));
            let natural_h = ascent - descent;
            // `line_height` is a multiplier on the font's own metrics; 0 keeps
            // the historical 1.15 leading so existing multi-line text is
            // untouched.
            let line_h = if t.line_height > 0.0 { natural_h * t.line_height } else { natural_h * 1.15 };
            let block_h = natural_h + line_h * (lines.len().saturating_sub(1)) as f32;
            let vf = match t.valign {
                Align::Start => 0.0,
                Align::Center | Align::Stretch => 0.5,
                Align::End => 1.0,
            };
            let top = rect_px[1] + (rect_px[3] - block_h) * vf;
            let (clip, clip_r) = clip_px(&t.clip);

            // Shadow, then stroke, then the run itself — each is the same
            // glyphs again, so they cost quads rather than pipelines. The
            // stroke's eight offsets are what a true SDF outline would
            // approximate anyway at UI sizes.
            let mut passes: Vec<([f32; 2], [f32; 4])> = Vec::new();
            if let Some(sh) = t.shadow {
                passes.push(([sh.offset[0] * scale, sh.offset[1] * scale], sh.color));
            }
            if let Some(st) = t.stroke
                && st.width > 0.0
            {
                let w = st.width * scale;
                const DIRS: [[f32; 2]; 8] = [
                    [-1.0, 0.0], [1.0, 0.0], [0.0, -1.0], [0.0, 1.0],
                    [-0.7, -0.7], [0.7, -0.7], [-0.7, 0.7], [0.7, 0.7],
                ];
                for d in DIRS {
                    passes.push(([d[0] * w, d[1] * w], st.color));
                }
            }
            passes.push(([0.0, 0.0], t.color));

            // Drawn after the glyphs, so a caret sitting on a wide character
            // is still visible.
            let mut caret_bar: Option<UiInstance> = None;
            // ---- caret, selection, and keeping them in view -------------------
            // The run is already broken into lines and measured, so a character
            // index is one prefix walk away. This is the only place in the
            // engine that knows where character N sits, which is why the caret
            // arrives here as an index rather than as a rect.
            let mut field_shift = 0.0f32;
            if let Some(car) = t.caret {
                let line0 = lines.first().map(String::as_str).unwrap_or("");
                let x_of = |chars: usize| -> f32 {
                    line0
                        .chars()
                        .take(chars)
                        .map(|c| self.fonts[fid].metrics(c, size).advance_width * scale + track_px)
                        .sum()
                };
                let run_w = widths.first().copied().unwrap_or(0.0);
                let left = align_x(t.align, rect_px, run_w);
                let caret_x = left + x_of(car.index);
                // A value longer than its box scrolls out from under itself so
                // the caret stays on screen — clamped so a short value never
                // pulls away from the edge it was authored against.
                let pad = 2.0 * scale;
                let over = caret_x - (rect_px[0] + rect_px[2] - pad);
                if over > 0.0 {
                    field_shift = -over.min((run_w - rect_px[2] + pad * 2.0).max(0.0));
                }
                let (a, b) = (car.index.min(car.anchor), car.index.max(car.anchor));
                // Selection first: it sits behind the glyphs, so a light band
                // over dark text stays readable rather than erasing it.
                if a != b && car.selection[3] > 0.0 {
                    let (x0, x1) = (left + x_of(a), left + x_of(b));
                    push(
                        instances,
                        batches,
                        UiTex::White,
                        None,
                        Blend::Normal,
                        UiInstance {
                            rect: [x0 + field_shift, top, (x1 - x0).max(1.0), natural_h],
                            color: car.selection,
                            params: [0.0, 0.0, clip_r, 0.0],
                            clip,
                            ..Default::default()
                        },
                    );
                }
                if car.on {
                    caret_bar = Some(UiInstance {
                        rect: [
                            caret_x + field_shift,
                            top,
                            (car.width * scale).max(1.0),
                            natural_h,
                        ],
                        color: car.color,
                        params: [0.0, 0.0, clip_r, 0.0],
                        clip,
                        ..Default::default()
                    });
                }
            }

            for (shift, color) in passes {
                let mut baseline = top + ascent + shift[1];
                for (line, &run_w) in lines.iter().zip(&widths) {
                    let mut pen_x = shift[0] + field_shift + align_x(t.align, rect_px, run_w);
                    for c in line.chars() {
                        let Some(g) = self.glyph(gpu, fid, c, px) else { continue };
                        if g.size[0] > 0.0 {
                            let rect =
                                [pen_x + g.offset[0], baseline + g.offset[1], g.size[0], g.size[1]];
                            push(
                                instances,
                                batches,
                                UiTex::Atlas,
                                None,
                                Blend::Normal,
                                UiInstance {
                                    rect,
                                    color,
                                    params: [0.0, 1.0, clip_r, 0.0],
                                    uv: g.uv,
                                    clip,
                                    xform: [
                                        t.xform.rotation,
                                        t.xform.scale[0],
                                        t.xform.scale[1],
                                        0.0,
                                    ],
                                    fx: {
                                        let p = pivot_for(rect, rect_px, t.xform.pivot);
                                        [0.0, 1.0, p[0], p[1]]
                                    },
                                    ..Default::default()
                                },
                            );
                        }
                        // Tracking is added per glyph, including after the
                        // last — matching how `advance` measures, so centred
                        // tracked text stays centred.
                        pen_x += g.advance + track_px;
                    }
                    baseline += line_h;
                }
            }
            if let Some(bar) = caret_bar {
                push(instances, batches, UiTex::White, None, Blend::Normal, bar);
            }
        }
    }

    fn ensure_instances(&mut self, gpu: &Gpu, n: u32) {
        if n > self.instance_cap {
            let cap = n.next_power_of_two();
            self.instance_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ui-instances"),
                size: cap as u64 * std::mem::size_of::<UiInstance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_cap = cap;
        }
    }

    /// Draw packed instances over `color` (Load — the frame is already there).
    pub fn draw(
        &mut self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        viewport: [f32; 2],
        instances: &[UiInstance],
        batches: &[UiBatch],
        raster: &Raster,
    ) {
        if instances.is_empty() {
            return;
        }
        let g = UiGlobals {
            viewport: [viewport[0], viewport[1], 0.0, self.time],
            plane_origin: [0.0; 4],
            plane_right: [0.0; 4],
            plane_down: [0.0; 4],
            view_proj: [[0.0; 4]; 4],
        };
        gpu.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&g));
        self.ensure_instances(gpu, instances.len() as u32);
        gpu.queue.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(instances));
        let mut encoder =
            gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ui") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.pipeline[0]);
            rp.set_bind_group(0, &self.globals_bind, &[]);
            rp.set_vertex_buffer(0, self.quad_vbuf.slice(..));
            rp.set_vertex_buffer(1, self.instance_buf.slice(..));
            rp.set_index_buffer(self.quad_ibuf.slice(..), wgpu::IndexFormat::Uint16);
            // What is currently bound: `None` = a custom-shader pipeline (they
            // are always alpha-blended — a per-blend variant of every .flsl
            // would be four pipelines per shader for no demonstrated need),
            // `Some(i)` = built-in pipeline for blend index `i`.
            let mut bound: Option<usize> = Some(0);
            for b in batches {
                if b.range.is_empty() {
                    continue;
                }
                // Custom-shader batches swap the whole pipeline; groups 0/1
                // stay layout-compatible so only the pipeline + group(2)
                // binding change.
                match b.shader.and_then(|(s, p)| self.shaders.get(s.0 as usize).map(|sh| (sh, p))) {
                    Some((sh, pid)) => {
                        rp.set_pipeline(&sh.pipeline);
                        if let Some(pb) = self.shader_binds.get(pid.0 as usize) {
                            rp.set_bind_group(2, &pb.bind, &[]);
                        }
                        // group(3) = the backdrop (real capture or 1×1 black).
                        rp.set_bind_group(3, self.backdrop_group(), &[]);
                        bound = None;
                    }
                    None => {
                        let want = blend_index(b.blend);
                        if bound != Some(want) {
                            rp.set_pipeline(&self.pipeline[want]);
                            bound = Some(want);
                        }
                    }
                }
                let bind = match b.tex {
                    UiTex::White => &self.white_bind,
                    UiTex::Atlas => &self.atlas_bind,
                    UiTex::Tex(id) => raster.material_bind(id).unwrap_or(&self.white_bind),
                };
                rp.set_bind_group(1, bind, &[]);
                rp.draw_indexed(0..6, 0, b.range.clone());
            }
        }
        gpu.queue.submit([encoder.finish()]);
    }

    /// Scene-view authoring: draw packed instances as a WORLD-SPACE canvas on
    /// `plane`, depth-tested against the scene (Load both attachments).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_world(
        &mut self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        view_proj: [[f32; 4]; 4],
        plane: UiPlane,
        instances: &[UiInstance],
        batches: &[UiBatch],
        raster: &Raster,
    ) {
        if instances.is_empty() {
            return;
        }
        let g = UiGlobals {
            viewport: [1.0, 1.0, 1.0, self.time],
            plane_origin: [plane.origin[0], plane.origin[1], plane.origin[2], 0.0],
            plane_right: [plane.right[0], plane.right[1], plane.right[2], 0.0],
            plane_down: [plane.down[0], plane.down[1], plane.down[2], 0.0],
            view_proj,
        };
        gpu.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&g));
        self.ensure_instances(gpu, instances.len() as u32);
        gpu.queue.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(instances));
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ui-world") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui-world"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.pipeline_world[0]);
            rp.set_bind_group(0, &self.globals_bind, &[]);
            rp.set_vertex_buffer(0, self.quad_vbuf.slice(..));
            rp.set_vertex_buffer(1, self.instance_buf.slice(..));
            rp.set_index_buffer(self.quad_ibuf.slice(..), wgpu::IndexFormat::Uint16);
            let mut bound: Option<usize> = Some(0);
            for b in batches {
                if b.range.is_empty() {
                    continue;
                }
                match b.shader.and_then(|(s, p)| self.shaders.get(s.0 as usize).map(|sh| (sh, p))) {
                    Some((sh, pid)) => {
                        rp.set_pipeline(&sh.pipeline_world);
                        if let Some(pb) = self.shader_binds.get(pid.0 as usize) {
                            rp.set_bind_group(2, &pb.bind, &[]);
                        }
                        rp.set_bind_group(3, self.backdrop_group(), &[]);
                        bound = None;
                    }
                    None => {
                        let want = blend_index(b.blend);
                        if bound != Some(want) {
                            rp.set_pipeline(&self.pipeline_world[want]);
                            bound = Some(want);
                        }
                    }
                }
                let bind = match b.tex {
                    UiTex::White => &self.white_bind,
                    UiTex::Atlas => &self.atlas_bind,
                    UiTex::Tex(id) => raster.material_bind(id).unwrap_or(&self.white_bind),
                };
                rp.set_bind_group(1, bind, &[]);
                rp.draw_indexed(0..6, 0, b.range.clone());
            }
        }
        gpu.queue.submit([encoder.finish()]);
    }
}
