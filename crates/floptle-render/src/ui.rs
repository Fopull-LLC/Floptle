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
use floptle_ui::{Align, DrawList};

/// One quad/glyph instance — mirrors `ui.wgsl`'s five vec4 attributes.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct UiInstance {
    /// x, y, w, h in physical px.
    pub rect: [f32; 4],
    pub color: [f32; 4],
    pub border_color: [f32; 4],
    /// radius px, border px, kind (0 = shape/image, 1 = glyph), unused.
    pub params: [f32; 4],
    /// u0, v0, u1, v1.
    pub uv: [f32; 4],
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

pub struct UiBatch {
    pub tex: UiTex,
    pub range: std::ops::Range<u32>,
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
    /// x, y = viewport px; z = mode (0 screen, 1 world canvas).
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
    pipeline: wgpu::RenderPipeline,
    /// The world-canvas variant (Scene-view authoring): depth-tested against
    /// the scene so the layer plane sits IN the world.
    pipeline_world: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    white_bind: wgpu::BindGroup,
    atlas: wgpu::Texture,
    atlas_bind: wgpu::BindGroup,
    font: fontdue::Font,
    glyphs: HashMap<(char, u32), Option<Glyph>>,
    // Shelf packer cursor.
    shelf: (u32, u32, u32),
    instance_buf: wgpu::Buffer,
    instance_cap: u32,
    atlas_full_warned: bool,
    quad_vbuf: wgpu::Buffer,
    quad_ibuf: wgpu::Buffer,
}

const QUAD_VERTS: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
const QUAD_INDICES: [u16; 6] = [0, 1, 2, 0, 2, 3];

const CORNER_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: 8,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &[wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 0,
        shader_location: 0,
    }],
};

const INSTANCE_ATTRS: [wgpu::VertexAttribute; 5] = [
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 1 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16, shader_location: 2 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 32, shader_location: 3 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 48, shader_location: 4 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 64, shader_location: 5 },
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
                visibility: wgpu::ShaderStages::VERTEX,
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
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
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
        let pipeline_world = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: Gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                // LessEqual, NOT Less: an element's shape/image/text stack at
                // the SAME plane depth — painter's order must keep layering.
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

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

        Ui {
            pipeline,
            pipeline_world,
            globals_buf,
            globals_bind,
            white_bind,
            atlas,
            atlas_bind,
            font,
            glyphs: HashMap::new(),
            shelf: (0, 0, 0),
            instance_buf,
            instance_cap: 1024,
            atlas_full_warned: false,
            quad_vbuf,
            quad_ibuf,
        }
    }

    /// Measure a single-line run in the same units as `size` (the layout
    /// solver's Fit callback — design units in, design units out).
    pub fn measure(&self, text: &str, size: f32) -> [f32; 2] {
        let mut w = 0.0f32;
        for c in text.chars() {
            w += self.font.metrics(c, size).advance_width;
        }
        let lm = self.font.horizontal_line_metrics(size);
        let h = lm.map(|l| l.ascent - l.descent).unwrap_or(size);
        [w, h]
    }

    /// Rasterize-or-fetch a glyph at an exact pixel size.
    fn glyph(&mut self, gpu: &Gpu, c: char, px: u32) -> Option<Glyph> {
        if let Some(g) = self.glyphs.get(&(c, px)) {
            return *g;
        }
        let (metrics, bitmap) = self.font.rasterize(c, px as f32);
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
                self.glyphs.insert((c, px), None);
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
        self.glyphs.insert((c, px), g);
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
        instances: &mut Vec<UiInstance>,
        batches: &mut Vec<UiBatch>,
    ) {
        let push = |instances: &mut Vec<UiInstance>,
                        batches: &mut Vec<UiBatch>,
                        tex: UiTex,
                        inst: UiInstance| {
            let i = instances.len() as u32;
            instances.push(inst);
            match batches.last_mut() {
                Some(b) if b.tex == tex && b.range.end == i => b.range.end = i + 1,
                _ => batches.push(UiBatch { tex, range: i..i + 1 }),
            }
        };
        for q in &list.quads {
            let tex = if q.texture.is_empty() {
                UiTex::White
            } else {
                match resolve(&q.texture) {
                    Some(id) => UiTex::Tex(id),
                    None => UiTex::White, // missing texture: tinted solid
                }
            };
            push(
                instances,
                batches,
                tex,
                UiInstance {
                    rect: [
                        origin[0] + q.rect[0] * scale,
                        origin[1] + q.rect[1] * scale,
                        q.rect[2] * scale,
                        q.rect[3] * scale,
                    ],
                    color: q.color,
                    border_color: q.border_color,
                    params: [q.radius * scale, q.border * scale, 0.0, 0.0],
                    uv: [0.0, 0.0, 1.0, 1.0],
                },
            );
        }
        for t in &list.texts {
            let px = (t.size * scale).round().max(1.0) as u32;
            let run_w = self.measure(&t.text, t.size)[0] * scale;
            let rect_px = [
                origin[0] + t.rect[0] * scale,
                origin[1] + t.rect[1] * scale,
                t.rect[2] * scale,
                t.rect[3] * scale,
            ];
            let mut pen_x = match t.align {
                Align::Start | Align::Stretch => rect_px[0],
                Align::Center => rect_px[0] + (rect_px[2] - run_w) * 0.5,
                Align::End => rect_px[0] + rect_px[2] - run_w,
            };
            // Baseline: vertically center the line box in the rect.
            let (ascent, descent) = self
                .font
                .horizontal_line_metrics(px as f32)
                .map(|l| (l.ascent, l.descent))
                .unwrap_or((px as f32, 0.0));
            let line_h = ascent - descent;
            let baseline = rect_px[1] + (rect_px[3] - line_h) * 0.5 + ascent;
            for c in t.text.chars() {
                let Some(g) = self.glyph(gpu, c, px) else { continue };
                if g.size[0] > 0.0 {
                    push(
                        instances,
                        batches,
                        UiTex::Atlas,
                        UiInstance {
                            rect: [
                                pen_x + g.offset[0],
                                baseline + g.offset[1],
                                g.size[0],
                                g.size[1],
                            ],
                            color: t.color,
                            border_color: [0.0; 4],
                            params: [0.0, 0.0, 1.0, 0.0],
                            uv: g.uv,
                        },
                    );
                }
                pen_x += g.advance;
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
            viewport: [viewport[0], viewport[1], 0.0, 0.0],
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
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &self.globals_bind, &[]);
            rp.set_vertex_buffer(0, self.quad_vbuf.slice(..));
            rp.set_vertex_buffer(1, self.instance_buf.slice(..));
            rp.set_index_buffer(self.quad_ibuf.slice(..), wgpu::IndexFormat::Uint16);
            for b in batches {
                if b.range.is_empty() {
                    continue;
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
            viewport: [1.0, 1.0, 1.0, 0.0],
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
            rp.set_pipeline(&self.pipeline_world);
            rp.set_bind_group(0, &self.globals_bind, &[]);
            rp.set_vertex_buffer(0, self.quad_vbuf.slice(..));
            rp.set_vertex_buffer(1, self.instance_buf.slice(..));
            rp.set_index_buffer(self.quad_ibuf.slice(..), wgpu::IndexFormat::Uint16);
            for b in batches {
                if b.range.is_empty() {
                    continue;
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
