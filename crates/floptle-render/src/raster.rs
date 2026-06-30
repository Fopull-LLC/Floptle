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
    /// Base color tint (rgb) + unused a.
    pub color: [f32; 4],
    /// Emissive color (rgb) + strength (a).
    pub emissive: [f32; 4],
    /// Specular color (rgb) + specular strength (a).
    pub specular: [f32; 4],
    /// x = shininess, y = rim strength, z = unlit (0/1), w = ambient multiplier.
    pub params: [f32; 4],
    /// Rim/fresnel color (rgb) + unused a.
    pub rim: [f32; 4],
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
        }
    }
}

const INSTANCE_ATTRS: [wgpu::VertexAttribute; 12] = [
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
    /// Same as `pipeline` but alpha-blended with depth-write OFF, for instances whose
    /// material opacity is < 1. Drawn after the opaque pass so they composite over the
    /// solid scene.
    transparent_pipeline: wgpu::RenderPipeline,
    /// Silhouette-mask pipeline (solid 1.0, no depth/cull) for selection outlines.
    mask_pipeline: wgpu::RenderPipeline,
    globals_bind: wgpu::BindGroup,
    globals_buf: wgpu::Buffer,
    tex_layout: wgpu::BindGroupLayout,
    _sampler: wgpu::Sampler, // owned by globals_bind; kept for lifetime clarity
    default_tex: wgpu::Texture,
    instance_buf: wgpu::Buffer,
    instance_cap: u32,
    meshes: Vec<RegisteredMesh>,
    /// Standalone material textures (decoupled from meshes), bound per-instance so
    /// a Material can re-texture any shape. Indexed by [`TexId`].
    textures: Vec<TexBind>,
}

/// A registered material texture: its bind group + the texture kept alive for it.
struct TexBind {
    bind: wgpu::BindGroup,
    _texture: wgpu::Texture,
}

/// A handle to a material texture registered with [`Raster::register_texture`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TexId(pub u32);

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

        // Transparent variant: identical vertex/fragment, but alpha-blends and does NOT
        // write depth, so an object behind it still shows through and later opaque draws
        // aren't occluded by it. (No back-to-front sort yet, so overlapping transparent
        // surfaces are approximate — enough for the basic transparency this exposes.)
        let transparent_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raster-transparent"),
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
                depth_write_enabled: Some(false),
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
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });
        let mask_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raster-mask"),
            layout: Some(&mask_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Vertex::LAYOUT, INSTANCE_LAYOUT],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
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
            transparent_pipeline,
            mask_pipeline,
            globals_bind,
            globals_buf,
            tex_layout,
            _sampler: sampler,
            default_tex,
            instance_buf,
            instance_cap,
            meshes: Vec::new(),
            textures: Vec::new(),
        }
    }

    /// Register a standalone material texture (RGBA8), returning its handle. Bound
    /// per-instance in `draw_scene` to re-texture a shape regardless of its mesh.
    pub fn register_texture(&mut self, gpu: &Gpu, data: &TextureData) -> TexId {
        let id = TexId(self.textures.len() as u32);
        let texture = upload_texture(gpu, data);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster-material-tex"),
            layout: &self.tex_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            }],
        });
        self.textures.push(TexBind { bind, _texture: texture });
        id
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
    pub fn draw_scene(
        &mut self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        globals: Globals,
        instances: &[(MeshId, Option<TexId>, InstanceRaw)],
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
            rp.set_pipeline(&self.pipeline);
            draw(&mut rp, &opaque_buckets);
            if !transparent_buckets.is_empty() {
                rp.set_pipeline(&self.transparent_pipeline);
                draw(&mut rp, &transparent_buckets);
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
        normal_mat: [
            [nm.x_axis.x, nm.x_axis.y, nm.x_axis.z, 0.0],
            [nm.y_axis.x, nm.y_axis.y, nm.y_axis.z, 0.0],
            [nm.z_axis.x, nm.z_axis.y, nm.z_axis.z, 0.0],
        ],
        color: [m.color[0], m.color[1], m.color[2], m.alpha],
        emissive: [m.emissive[0], m.emissive[1], m.emissive[2], m.emissive_strength],
        specular: [m.specular[0], m.specular[1], m.specular[2], m.specular_strength],
        params: [m.shininess, m.rim_strength, if m.unlit { 1.0 } else { 0.0 }, m.ambient],
        rim: [m.rim[0], m.rim[1], m.rim[2], 0.0],
    }
}
