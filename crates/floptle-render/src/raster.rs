//! The forward raster renderer — the seed of the mesh/material path (Phase 2).
//!
//! Phase 1: it draws one **textured quad** with a camera-relative MVP. The camera
//! supplies `view_proj` (view has no translation — the camera is the render-space
//! origin, ADR-0015); each object supplies its camera-relative `model` matrix
//! (`Transform::render_matrix`), so the GPU never sees large coordinates. The
//! texture is generated procedurally on the CPU and uploaded once.
//!
//! Still single-object, no depth buffer, no vertex-buffer streaming. It grows
//! per-instance models + real meshes next, then becomes a `graph::Pass` when the
//! render-graph executor lands (Phase 4).

use glam::Mat4;

use crate::device::{Frame, Gpu};

/// One vertex of the quad: object-space position + texture coordinate.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 3],
    uv: [f32; 2],
}

/// Per-frame camera uniform: the camera's view·projection and the object's
/// camera-relative model matrix. Matches `struct Camera` in `raster.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CamUniform {
    view_proj: [[f32; 4]; 4],
    model: [[f32; 4]; 4],
}

/// A unit quad in the XY plane (object space), spanning -1..1. UV `v` is 0 at the
/// top (y=+1) so texture row 0 maps to the top edge.
const VERTS: [Vertex; 4] = [
    Vertex { pos: [-1.0, -1.0, 0.0], uv: [0.0, 1.0] }, // bottom-left
    Vertex { pos: [1.0, -1.0, 0.0], uv: [1.0, 1.0] },  // bottom-right
    Vertex { pos: [1.0, 1.0, 0.0], uv: [1.0, 0.0] },   // top-right
    Vertex { pos: [-1.0, 1.0, 0.0], uv: [0.0, 0.0] },  // top-left
];
const INDICES: [u16; 6] = [0, 1, 2, 0, 2, 3];

/// Vertex layout: location 0 = position (3×f32), location 1 = uv (2×f32).
const VATTRS: [wgpu::VertexAttribute; 2] = [
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 0, shader_location: 0 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 12, shader_location: 1 },
];

const TEX_SIZE: u32 = 256;

pub struct Raster {
    pipeline: wgpu::RenderPipeline,
    bind: wgpu::BindGroup,
    uniform: wgpu::Buffer,
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    _texture: wgpu::Texture,
    _sampler: wgpu::Sampler,
}

impl Raster {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("raster"),
            source: wgpu::ShaderSource::Wgsl(include_str!("raster.wgsl").into()),
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("raster"),
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
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("raster"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raster"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &VATTRS,
                }],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
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

        // Static geometry — written once.
        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster-verts"),
            size: std::mem::size_of_val(&VERTS) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue.write_buffer(&vbuf, 0, bytemuck::cast_slice(&VERTS));

        let ibuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster-indices"),
            size: std::mem::size_of_val(&INDICES) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue.write_buffer(&ibuf, 0, bytemuck::cast_slice(&INDICES));

        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster-cam"),
            size: std::mem::size_of::<CamUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Procedural texture: a checkerboard over a cyan→magenta gradient — clearly
        // a *texture* (so the quad reads as textured) and asymmetric (so its spin is
        // unmistakable).
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("raster-tex"),
            size: wgpu::Extent3d { width: TEX_SIZE, height: TEX_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &procedural_texels(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * TEX_SIZE),
                rows_per_image: Some(TEX_SIZE),
            },
            wgpu::Extent3d { width: TEX_SIZE, height: TEX_SIZE, depth_or_array_layers: 1 },
        );
        let tex_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("raster-samp"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&tex_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        Self { pipeline, bind, uniform, vbuf, ibuf, _texture: texture, _sampler: sampler }
    }

    /// Clear `frame` to `clear`, then draw the textured quad. `view_proj` is the
    /// camera's view·projection; `model` is the quad's camera-relative model matrix
    /// (from `Transform::render_matrix(camera_world)`).
    pub fn draw(&self, gpu: &Gpu, frame: &Frame, view_proj: Mat4, model: Mat4, clear: [f64; 4]) {
        let uni = CamUniform {
            view_proj: view_proj.to_cols_array_2d(),
            model: model.to_cols_array_2d(),
        };
        gpu.queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&uni));

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raster") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("raster"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear[0],
                            g: clear[1],
                            b: clear[2],
                            a: clear[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &self.bind, &[]);
            rp.set_vertex_buffer(0, self.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}

/// Build the quad's texture: an 8×8 checkerboard whose lit cells run a cyan→magenta
/// vertical gradient and whose dark cells fall toward deep indigo. Authored in the
/// color space we want to *see* — the `Rgba8UnormSrgb` format + sRGB surface round-
/// trip the values back out unchanged.
fn procedural_texels() -> Vec<u8> {
    let n = TEX_SIZE as usize;
    let mut data = vec![0u8; n * n * 4];
    let top = [0.30f32, 0.85, 1.00]; // cyan
    let bot = [0.95f32, 0.30, 0.70]; // magenta
    let ink = [0.05f32, 0.03, 0.10]; // deep indigo
    for y in 0..n {
        let v = y as f32 / (n - 1) as f32;
        for x in 0..n {
            let checker = ((x * 8 / n) + (y * 8 / n)) & 1;
            let k = if checker == 0 { 1.0 } else { 0.32 };
            let i = (y * n + x) * 4;
            for c in 0..3 {
                let grad = top[c] + (bot[c] - top[c]) * v;
                let val = ink[c] + (grad - ink[c]) * k;
                data[i + c] = (val.clamp(0.0, 1.0) * 255.0) as u8;
            }
            data[i + 3] = 255;
        }
    }
    data
}
