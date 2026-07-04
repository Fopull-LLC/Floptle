//! The billboard particle pass — instanced oriented quads.
//!
//! Each quad spans a per-instance basis the CPU packer chooses per orientation mode
//! (face-camera, upright, flat-on-ground, velocity-stretched), so a track need not
//! face the camera.
//!
//! Draws the per-frame instance data the VFX sim produces (`floptle-vfx`), into the
//! same color/depth targets the scene composited in, BEFORE post and the retro
//! upscale — so particles are depth-tested against meshes and raymarched matter,
//! captured by SSAO/bloom/vignette, and pixelate with the world in retro mode.
//!
//! Both pipelines follow the raster transparent convention (`depth_compare: Less`,
//! no depth write, drawn after all opaque work): `Alpha` composites classically
//! (the caller depth-sorts those instances back-to-front), `Additive` accumulates
//! light and is order-independent. Track textures are the raster pass's registered
//! material textures ([`TexId`]) — one registry serves both passes.

use crate::device::Gpu;
use crate::raster::{Raster, TexId};

/// How a batch composites over the scene. Discriminants are the index into the
/// pipeline array — keep them contiguous and in sync with [`ParticleBlend::ALL`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParticleBlend {
    /// Classic transparency — instances must arrive depth-sorted back-to-front.
    Alpha = 0,
    /// Light-accumulating — order-independent.
    Additive = 1,
    /// Premultiplied-alpha over — order-dependent.
    Premultiplied = 2,
    /// Screen (lighten) — order-independent.
    Screen = 3,
    /// Multiply (darken) — order-dependent.
    Multiply = 4,
}

impl ParticleBlend {
    /// Every mode, in discriminant order — one pipeline is built per entry.
    pub const ALL: [ParticleBlend; 5] = [
        ParticleBlend::Alpha,
        ParticleBlend::Additive,
        ParticleBlend::Premultiplied,
        ParticleBlend::Screen,
        ParticleBlend::Multiply,
    ];

    /// The wgpu blend state this mode composites with.
    fn state(self) -> wgpu::BlendState {
        use wgpu::{BlendComponent as C, BlendFactor as F, BlendOperation as Op};
        let add = |src, dst| C { src_factor: src, dst_factor: dst, operation: Op::Add };
        match self {
            ParticleBlend::Alpha => wgpu::BlendState::ALPHA_BLENDING,
            // Light accumulation: SrcAlpha·color summed into the target.
            ParticleBlend::Additive => wgpu::BlendState {
                color: add(F::SrcAlpha, F::One),
                alpha: add(F::One, F::One),
            },
            // Premultiplied over: color already carries its alpha.
            ParticleBlend::Premultiplied => wgpu::BlendState {
                color: add(F::One, F::OneMinusSrcAlpha),
                alpha: add(F::One, F::OneMinusSrcAlpha),
            },
            // Screen: 1−(1−src)(1−dst) — lightens, order-independent.
            ParticleBlend::Screen => {
                wgpu::BlendState { color: add(F::One, F::OneMinusSrc), alpha: add(F::One, F::OneMinusSrc) }
            }
            // Multiply: src·dst — darkens; keeps the destination alpha.
            ParticleBlend::Multiply => {
                wgpu::BlendState { color: add(F::Dst, F::Zero), alpha: add(F::Zero, F::One) }
            }
        }
    }
}

/// Per-particle GPU data: camera-relative position + spin, size, tint, and the
/// world-space basis the quad spans (so a track can face the camera, lie flat,
/// stand upright, or stretch along its motion — the CPU packer picks the basis per
/// [`BillboardOrient`]). Written by the CPU sim today; the GPU compute backend will
/// write the very same buffer on-device (proposal §4.4 — the sim's output IS the
/// instance buffer).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ParticleInstance {
    /// xyz = camera-relative position, w = spin angle (radians).
    pub pos_rot: [f32; 4],
    /// xy = width/height in world units (zw unused, kept vec4-aligned).
    pub size: [f32; 4],
    pub color: [f32; 4],
    /// The quad's in-plane +X axis in camera-relative world space (xyz; w unused).
    /// Its length scales width; the corner is spun in the `basis_right`×`basis_up`
    /// plane. For a camera-facing track this is the camera's right vector.
    pub basis_right: [f32; 4],
    /// The quad's in-plane +Y axis (xyz; w unused). Length scales height — velocity
    /// stretch bakes the motion-length here so the shader needs no stretch term.
    pub basis_up: [f32; 4],
}

/// One instanced draw: a contiguous `range` of this frame's instance array, with
/// one texture and one blend mode. Batches draw in the order given.
#[derive(Clone, Debug)]
pub struct ParticleBatch {
    /// A raster-registered material texture, or `None` for the plain white quad.
    pub texture: Option<TexId>,
    pub blend: ParticleBlend,
    pub range: std::ops::Range<u32>,
}

/// Frame globals for the particle shader: the camera transform. `cam_right`/`cam_up`
/// carry the camera basis the CPU packer reads for face-camera tracks (the shader
/// itself now spans the per-instance basis, so it no longer reads them — they stay
/// for the packer's convenience and the future on-device backend).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ParticleGlobals {
    pub view_proj: [[f32; 4]; 4],
    /// The camera's world right vector (w unused).
    pub cam_right: [f32; 4],
    /// The camera's world up vector (w unused).
    pub cam_up: [f32; 4],
    /// Depth fog: rgb = color (w unused).
    pub fog_color: [f32; 4],
    /// Depth fog: x = start dist, y = end dist, z = enabled (0/1), w unused.
    pub fog_params: [f32; 4],
}

pub struct Particles {
    /// One pipeline per [`ParticleBlend`], indexed by its discriminant.
    pipelines: Vec<wgpu::RenderPipeline>,
    globals_buf: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    /// White 1×1 for untextured tracks (the tint shows through unchanged).
    default_bind: wgpu::BindGroup,
    quad_vbuf: wgpu::Buffer,
    quad_ibuf: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    instance_cap: u32,
}

const QUAD_VERTS: [[f32; 2]; 4] = [[-0.5, -0.5], [0.5, -0.5], [0.5, 0.5], [-0.5, 0.5]];
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
    array_stride: std::mem::size_of::<ParticleInstance>() as u64,
    step_mode: wgpu::VertexStepMode::Instance,
    attributes: &INSTANCE_ATTRS,
};

impl Particles {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("particles"),
            source: wgpu::ShaderSource::Wgsl(include_str!("particles.wgsl").into()),
        });

        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("particles-globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                // Fragment reads it too now (fog params), not just the vertex basis.
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        // Group 1 mirrors the raster material-texture layout exactly, so the two
        // passes share registered textures (wgpu bind groups are compatible across
        // structurally-equal layouts).
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("particles-texture"),
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
            label: Some("particles"),
            bind_group_layouts: &[Some(&globals_layout), Some(&tex_layout)],
            immediate_size: 0,
        });

        let make_pipeline = |label: &str, blend: wgpu::BlendState| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[CORNER_LAYOUT, INSTANCE_LAYOUT],
                },
                primitive: wgpu::PrimitiveState::default(),
                // Transparent convention: test against the scene, never write —
                // particles occlude nothing (not even each other).
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
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        // One pipeline per blend mode, in discriminant order (indexed by `blend as usize`).
        let pipelines: Vec<wgpu::RenderPipeline> = ParticleBlend::ALL
            .iter()
            .map(|b| make_pipeline(&format!("particles-{b:?}"), b.state()))
            .collect();

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("particles-globals"),
            size: std::mem::size_of::<ParticleGlobals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("particles-globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });

        // Own white default (can't borrow raster's — its bind group lives in the
        // other pass), same 1×1 white idea.
        let white = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("particles-white"),
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
            &[255, 255, 255, 255],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4), rows_per_image: Some(1) },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        let white_view = white.create_view(&wgpu::TextureViewDescriptor::default());
        let white_samp = device.create_sampler(&wgpu::SamplerDescriptor::default());
        let default_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("particles-white"),
            layout: &tex_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&white_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&white_samp),
                },
            ],
        });

        let quad_vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("particles-quad"),
            size: std::mem::size_of_val(&QUAD_VERTS) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue.write_buffer(&quad_vbuf, 0, bytemuck::cast_slice(&QUAD_VERTS));
        let quad_ibuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("particles-quad-idx"),
            size: std::mem::size_of_val(&QUAD_INDICES) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue.write_buffer(&quad_ibuf, 0, bytemuck::cast_slice(&QUAD_INDICES));

        let instance_cap = 256;
        let instance_buf = Self::make_instance_buf(gpu, instance_cap);

        Self {
            pipelines,
            globals_buf,
            globals_bind,
            default_bind,
            quad_vbuf,
            quad_ibuf,
            instance_buf,
            instance_cap,
        }
    }

    fn make_instance_buf(gpu: &Gpu, cap: u32) -> wgpu::Buffer {
        gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("particles-instances"),
            size: (cap as u64) * std::mem::size_of::<ParticleInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn ensure_instances(&mut self, gpu: &Gpu, count: u32) {
        if count > self.instance_cap {
            self.instance_cap = count.next_power_of_two();
            self.instance_buf = Self::make_instance_buf(gpu, self.instance_cap);
        }
    }

    /// Draw this frame's particles into `color`/`depth` (Load — the scene is
    /// already there). `instances` is the packed per-frame array; each batch draws
    /// its `range` with its texture (resolved through `raster`'s material registry)
    /// and blend. Batches draw in order — put Alpha (pre-sorted) before Additive.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        globals: ParticleGlobals,
        instances: &[ParticleInstance],
        batches: &[ParticleBatch],
        raster: &Raster,
    ) {
        if instances.is_empty() || batches.is_empty() {
            return;
        }
        gpu.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));
        self.ensure_instances(gpu, instances.len() as u32);
        gpu.queue.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(instances));

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("particles") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("particles"),
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
            rp.set_bind_group(0, &self.globals_bind, &[]);
            rp.set_vertex_buffer(0, self.quad_vbuf.slice(..));
            rp.set_vertex_buffer(1, self.instance_buf.slice(..));
            rp.set_index_buffer(self.quad_ibuf.slice(..), wgpu::IndexFormat::Uint16);
            for batch in batches {
                if batch.range.is_empty() {
                    continue;
                }
                rp.set_pipeline(&self.pipelines[batch.blend as usize]);
                let bind = batch
                    .texture
                    .and_then(|t| raster.material_bind(t))
                    .unwrap_or(&self.default_bind);
                rp.set_bind_group(1, bind, &[]);
                rp.draw_indexed(0..QUAD_INDICES.len() as u32, 0, batch.range.clone());
            }
        }
        gpu.queue.submit([encoder.finish()]);
    }
}
