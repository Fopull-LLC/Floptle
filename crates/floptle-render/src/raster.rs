//! The forward raster pass — the seed of the mesh/material path (Phase 2).
//!
//! It draws a registry of `GpuMesh`es, each instanced any number of times, in a
//! single depth-tested render pass with simple directional diffuse lighting. Per
//! object data — the **camera-relative** model matrix (`Transform::render_matrix`,
//! ADR-0015), its inverse-transpose normal matrix, and a tint color — rides a
//! per-instance vertex buffer rewritten once per frame, so adding the Nth object
//! costs one struct push and no extra draw call. A shared neutral detail texture
//! is sampled by uv; per-object color provides the hue.
//!
//! Single self-contained pass (owns its encoder + clear) until the render-graph
//! executor lands (Phase 4); per-material shaders/textures land with the asset
//! system. No depth pre-pass, no culling, no shadows yet.

use glam::Mat4;

use crate::device::{Frame, Gpu};
use crate::mesh::{GpuMesh, MeshData, MeshId, Vertex};

/// Frame-global uniform: the camera view·projection and the single directional
/// light. `light_dir.xyz` is the normalized world-space direction *toward* the
/// light; a constant world-space direction is translation-invariant, so lighting
/// stays correct under floating-origin shifts with no adjustment (ADR-0015).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Globals {
    pub view_proj: [[f32; 4]; 4],
    pub light_dir: [f32; 4],
    pub light_color: [f32; 4],
    pub ambient: [f32; 4],
}

/// Per-instance GPU data. `normal_mat` is the inverse-transpose of the model's
/// upper-3×3, stored as three 16-byte-aligned columns (the 4th lane is padding so
/// each column is a `vec4` in the vertex layout).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InstanceRaw {
    pub model: [[f32; 4]; 4],
    pub normal_mat: [[f32; 4]; 3],
    pub color: [f32; 4],
}

/// Per-instance attributes (vertex buffer 1): model cols @3..6, normal cols @7..9,
/// color @10 — all `Float32x4`, stride 128.
const INSTANCE_ATTRS: [wgpu::VertexAttribute; 8] = [
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 3 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16, shader_location: 4 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 32, shader_location: 5 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 48, shader_location: 6 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 64, shader_location: 7 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 80, shader_location: 8 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 96, shader_location: 9 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 112, shader_location: 10 },
];

const INSTANCE_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<InstanceRaw>() as u64,
    step_mode: wgpu::VertexStepMode::Instance,
    attributes: &INSTANCE_ATTRS,
};

const TEX_SIZE: u32 = 256;

pub struct Raster {
    pipeline: wgpu::RenderPipeline,
    bind: wgpu::BindGroup,
    globals_buf: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    instance_cap: u32,
    meshes: Vec<GpuMesh>,
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
                buffers: &[Vertex::LAYOUT, INSTANCE_LAYOUT],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: Gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
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

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster-globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let instance_cap = 16;
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster-instances"),
            size: (instance_cap as u64) * std::mem::size_of::<InstanceRaw>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // A neutral grayscale detail texture: a soft checker so the per-object tint
        // drives the color while the texture reads as surface detail. Stored linear
        // (Rgba8Unorm) so its byte values act as straight brightness multipliers.
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("raster-detail"),
            size: wgpu::Extent3d { width: TEX_SIZE, height: TEX_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
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
            &detail_texels(),
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
                wgpu::BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() },
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

        Self {
            pipeline,
            bind,
            globals_buf,
            instance_buf,
            instance_cap,
            meshes: Vec::new(),
            _texture: texture,
            _sampler: sampler,
        }
    }

    /// Upload a mesh and return its handle (index into the registry).
    pub fn register(&mut self, gpu: &Gpu, data: &MeshData) -> MeshId {
        let id = MeshId(self.meshes.len() as u32);
        self.meshes.push(GpuMesh::upload(gpu, data));
        id
    }

    /// Grow the instance buffer if `count` instances won't fit.
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

    /// Clear `frame` (color + depth) and draw every instance. Instances are bucketed
    /// by mesh so each mesh issues one instanced `draw_indexed`. `view_proj` +
    /// `light` come in via `globals`; each instance carries its own camera-relative
    /// model/normal/color.
    pub fn draw_scene(
        &mut self,
        gpu: &Gpu,
        frame: &Frame,
        globals: Globals,
        instances: &[(MeshId, InstanceRaw)],
        clear: [f64; 4],
    ) {
        gpu.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        // Bucket instances by mesh into one contiguous run per mesh; record the
        // instance range each mesh draws from.
        let mut raws: Vec<InstanceRaw> = Vec::with_capacity(instances.len());
        let mut buckets: Vec<(usize, u32, u32)> = Vec::new(); // (mesh idx, start, count)
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
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: gpu.depth_view(),
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
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &self.bind, &[]);
            rp.set_vertex_buffer(1, self.instance_buf.slice(..));
            for (mesh_idx, start, count) in buckets {
                let mesh = &self.meshes[mesh_idx];
                rp.set_vertex_buffer(0, mesh.vbuf.slice(..));
                rp.set_index_buffer(mesh.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                rp.draw_indexed(0..mesh.index_count, 0, start..(start + count));
            }
        }
        gpu.queue.submit([encoder.finish()]);
    }
}

/// A soft grayscale checker (light/dark cells over a faint vertical gradient) — a
/// neutral linear detail map so per-object tint provides the hue.
fn detail_texels() -> Vec<u8> {
    let n = TEX_SIZE as usize;
    let mut data = vec![0u8; n * n * 4];
    for y in 0..n {
        let v = y as f32 / (n - 1) as f32;
        for x in 0..n {
            let checker = ((x * 8 / n) + (y * 8 / n)) & 1;
            // light vs slightly darker cells, plus a gentle top→bottom fade.
            let base = if checker == 0 { 1.0 } else { 0.72 };
            let lum = (base - 0.10 * v).clamp(0.0, 1.0);
            let i = (y * n + x) * 4;
            let b = (lum * 255.0) as u8;
            data[i] = b;
            data[i + 1] = b;
            data[i + 2] = b;
            data[i + 3] = 255;
        }
    }
    data
}

/// Helper: pack a `glam::Mat4` model matrix into an `InstanceRaw`, computing the
/// inverse-transpose normal matrix from its upper-3×3 (correct under rotation and
/// non-uniform scale; translation lives only in the 4th column and drops out).
pub fn instance_of(model: Mat4, color: [f32; 3]) -> InstanceRaw {
    // The inverse-transpose is correct under rotation + non-uniform scale; guard a
    // degenerate (zero/singular) scale, whose non-invertible 3×3 would otherwise
    // emit NaN normals and blacken that object's lighting.
    let m3 = glam::Mat3::from_mat4(model);
    let nm = if m3.determinant().abs() > 1e-12 { m3.inverse().transpose() } else { m3 };
    InstanceRaw {
        model: model.to_cols_array_2d(),
        normal_mat: [
            [nm.x_axis.x, nm.x_axis.y, nm.x_axis.z, 0.0],
            [nm.y_axis.x, nm.y_axis.y, nm.y_axis.z, 0.0],
            [nm.z_axis.x, nm.z_axis.y, nm.z_axis.z, 0.0],
        ],
        color: [color[0], color[1], color[2], 1.0],
    }
}
