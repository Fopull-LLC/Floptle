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

use glam::Mat4;

use crate::device::Gpu;
use crate::mesh::{GpuMesh, MeshData, MeshId, TextureData, Vertex};

/// Frame-global uniform: camera view·projection and the directional light.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Globals {
    pub view_proj: [[f32; 4]; 4],
    pub light_dir: [f32; 4],
    pub light_color: [f32; 4],
    pub ambient: [f32; 4],
}

/// Per-instance GPU data: model matrix, inverse-transpose normal matrix (3 padded
/// columns), and a tint color.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InstanceRaw {
    pub model: [[f32; 4]; 4],
    pub normal_mat: [[f32; 4]; 3],
    pub color: [f32; 4],
}

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

/// A mesh resident on the GPU plus the bind group holding its base-color texture.
struct RegisteredMesh {
    gpu_mesh: GpuMesh,
    tex_bind: wgpu::BindGroup,
    _texture: Option<wgpu::Texture>, // kept alive for the bind group (None = default)
}

pub struct Raster {
    pipeline: wgpu::RenderPipeline,
    /// Inverted-hull pipeline for selection outlines (front-face cull, flat color).
    outline_pipeline: wgpu::RenderPipeline,
    globals_bind: wgpu::BindGroup,
    globals_buf: wgpu::Buffer,
    tex_layout: wgpu::BindGroupLayout,
    _sampler: wgpu::Sampler, // owned by globals_bind; kept for lifetime clarity
    default_tex: wgpu::Texture,
    instance_buf: wgpu::Buffer,
    instance_cap: u32,
    meshes: Vec<RegisteredMesh>,
}

impl Raster {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("raster"),
            source: wgpu::ShaderSource::Wgsl(include_str!("raster.wgsl").into()),
        });

        // Group 0: frame globals (uniform) + the shared sampler.
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
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // Group 1: the per-material base-color texture.
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("raster-texture"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("raster"),
            bind_group_layouts: &[Some(&globals_layout), Some(&tex_layout)],
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

        // Selection-outline pipeline: an inverted hull. Cull FRONT faces so only the
        // far shell renders; the real object (drawn after) covers all but a rim. Only
        // needs the globals (no texture), and outputs the flat instance color.
        let outline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("raster-outline"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });
        let outline_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raster-outline"),
            layout: Some(&outline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Vertex::LAYOUT, INSTANCE_LAYOUT],
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: Some(wgpu::Face::Back),
                ..Default::default()
            },
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
                entry_point: Some("fs_outline"),
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

        // Nearest + REPEAT: crisp, tiling pixel-art textures.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("raster-samp"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster-globals"),
            layout: &globals_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        // 1×1 white default for meshes registered without a texture (the tint then
        // shows through unchanged).
        let default_tex = upload_texture(
            gpu,
            &TextureData { pixels: vec![255, 255, 255, 255], width: 1, height: 1 },
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
            outline_pipeline,
            globals_bind,
            globals_buf,
            tex_layout,
            _sampler: sampler,
            default_tex,
            instance_buf,
            instance_cap,
            meshes: Vec::new(),
        }
    }

    /// Upload a mesh and its base-color texture (or `None` for a white default),
    /// returning its handle.
    pub fn register(&mut self, gpu: &Gpu, data: &MeshData, texture: Option<&TextureData>) -> MeshId {
        let id = MeshId(self.meshes.len() as u32);
        let gpu_mesh = GpuMesh::upload(gpu, data);

        let owned = texture.map(|t| upload_texture(gpu, t));
        let view = owned
            .as_ref()
            .unwrap_or(&self.default_tex)
            .create_view(&wgpu::TextureViewDescriptor::default());
        let tex_bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster-mesh-tex"),
            layout: &self.tex_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            }],
        });

        self.meshes.push(RegisteredMesh { gpu_mesh, tex_bind, _texture: owned });
        id
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

    /// Clear the given color + depth targets and draw every instance, bucketed by
    /// mesh so each mesh issues one instanced `draw_indexed` with its own texture
    /// bound. The targets are passed in (rather than hard-wired to the swapchain) so
    /// the scene can render either straight to the window or into a low-res retro
    /// buffer; `color` must use the surface format and `depth` the depth format.
    ///
    /// `outline` instances (enlarged shells, flat color) are drawn first, with the
    /// inverted-hull pipeline, so the real meshes cover all but a selection rim.
    pub fn draw_scene(
        &mut self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        globals: Globals,
        instances: &[(MeshId, InstanceRaw)],
        outline: &[(MeshId, InstanceRaw)],
        clear: Option<[f64; 4]>,
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

        // Pack outline instances first, then scene instances, into one buffer.
        let mut raws: Vec<InstanceRaw> = Vec::with_capacity(outline.len() + instances.len());
        let bucket = |raws: &mut Vec<InstanceRaw>, src: &[(MeshId, InstanceRaw)], n: usize| {
            let mut b: Vec<(usize, u32, u32)> = Vec::new();
            for mesh_idx in 0..n {
                let start = raws.len() as u32;
                for (id, raw) in src {
                    if id.0 as usize == mesh_idx {
                        raws.push(*raw);
                    }
                }
                let count = raws.len() as u32 - start;
                if count > 0 {
                    b.push((mesh_idx, start, count));
                }
            }
            b
        };
        let outline_buckets = bucket(&mut raws, outline, self.meshes.len());
        let scene_buckets = bucket(&mut raws, instances, self.meshes.len());

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
            rp.set_vertex_buffer(1, self.instance_buf.slice(..));

            // Outline shells (inverted hull, flat color), drawn before the meshes.
            if !outline_buckets.is_empty() {
                rp.set_pipeline(&self.outline_pipeline);
                rp.set_bind_group(0, &self.globals_bind, &[]);
                for (mesh_idx, start, count) in &outline_buckets {
                    let mesh = &self.meshes[*mesh_idx];
                    rp.set_vertex_buffer(0, mesh.gpu_mesh.vbuf.slice(..));
                    rp.set_index_buffer(mesh.gpu_mesh.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                    rp.draw_indexed(0..mesh.gpu_mesh.index_count, 0, *start..(*start + *count));
                }
            }

            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &self.globals_bind, &[]);
            for (mesh_idx, start, count) in scene_buckets {
                let mesh = &self.meshes[mesh_idx];
                rp.set_bind_group(1, &mesh.tex_bind, &[]);
                rp.set_vertex_buffer(0, mesh.gpu_mesh.vbuf.slice(..));
                rp.set_index_buffer(mesh.gpu_mesh.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                rp.draw_indexed(0..mesh.gpu_mesh.index_count, 0, start..(start + count));
            }
        }
        gpu.queue.submit([encoder.finish()]);
    }
}

/// Upload an RGBA8 image as an sRGB texture (base-color data is sRGB-encoded).
fn upload_texture(gpu: &Gpu, t: &TextureData) -> wgpu::Texture {
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("raster-basecolor"),
        size: wgpu::Extent3d {
            width: t.width.max(1),
            height: t.height.max(1),
            depth_or_array_layers: 1,
        },
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
        &t.pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * t.width.max(1)),
            rows_per_image: Some(t.height.max(1)),
        },
        wgpu::Extent3d { width: t.width.max(1), height: t.height.max(1), depth_or_array_layers: 1 },
    );
    texture
}

/// Pack a `glam::Mat4` model matrix into an `InstanceRaw`, computing the
/// inverse-transpose normal matrix from its upper-3×3.
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
