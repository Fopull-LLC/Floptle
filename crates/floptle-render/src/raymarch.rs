//! A raymarched SDF-matter pass, composited with the raster meshes.
//!
//! It folds two kinds of matter into one field with smin: an analytic morphing
//! blob and a **baked mesh volume** — a 3D signed-distance texture + a co-located
//! color texture produced by `floptle_field::mesh2sdf`, so an imported mesh becomes
//! textured SDF matter that blends (distance *and* color) with everything else.
//! Rays are camera-relative (from inverse(view_proj)) and the fragment writes
//! frag_depth, so it shares one depth buffer with the raster meshes.

use floptle_field::BakedSdf;

use crate::device::Gpu;

/// Uniform driving the raymarch — matches `struct Globals` in `raymarch.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct RaymarchGlobals {
    pub view_proj: [[f32; 4]; 4],
    pub inv_view_proj: [[f32; 4]; 4],
    pub light_dir: [f32; 4],
    pub bg: [f32; 4],
    /// Analytic blob: xyz camera-relative center, w = scale.
    pub center: [f32; 4],
    /// x = time.
    pub params: [f32; 4],
    /// Baked volume: xyz camera-relative box center, w = present (1.0/0.0).
    pub vol_center: [f32; 4],
    /// xyz half-extent, w = blend radius k.
    pub vol_half: [f32; 4],
}

pub struct Raymarch {
    pipeline: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    bind_layout: wgpu::BindGroupLayout,
    sampler_lin: wgpu::Sampler,
    sampler_pt: wgpu::Sampler,
    _dist_tex: wgpu::Texture,
    _color_tex: wgpu::Texture,
    bind: wgpu::BindGroup,
}

impl Raymarch {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("raymarch"),
            source: wgpu::ShaderSource::Wgsl(include_str!("raymarch.wgsl").into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("raymarch"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                vol_tex_entry(1),
                vol_tex_entry(2),
                sampler_entry(3),
                sampler_entry(4),
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("raymarch"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raymarch"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: Gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Always),
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
            label: Some("raymarch-globals"),
            size: std::mem::size_of::<RaymarchGlobals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Distance: trilinear (smooth surfaces). Color: nearest (crisp pixel-art).
        let sampler_lin = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("raymarch-lin"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let sampler_pt = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("raymarch-pt"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        // A 1³ "empty" volume so the bindings are valid before a mesh is baked.
        let empty = BakedSdf {
            dims: [1, 1, 1],
            center: [0.0; 3],
            half_extent: [1.0; 3],
            distance: vec![1.0e9],
            color: vec![[255, 255, 255, 255]],
        };
        let (dist_tex, color_tex) = upload_volume(gpu, &empty);
        let bind =
            make_bind(device, &bind_layout, &globals_buf, &dist_tex, &color_tex, &sampler_lin, &sampler_pt);

        Self {
            pipeline,
            globals_buf,
            bind_layout,
            sampler_lin,
            sampler_pt,
            _dist_tex: dist_tex,
            _color_tex: color_tex,
            bind,
        }
    }

    /// Upload a baked mesh as the volume matter (replaces any previous one). The
    /// runtime still drives `vol_center`/`vol_half`/present via `RaymarchGlobals`.
    pub fn set_volume(&mut self, gpu: &Gpu, baked: &BakedSdf) {
        let (dist_tex, color_tex) = upload_volume(gpu, baked);
        self.bind = make_bind(
            &gpu.device,
            &self.bind_layout,
            &self.globals_buf,
            &dist_tex,
            &color_tex,
            &self.sampler_lin,
            &self.sampler_pt,
        );
        self._dist_tex = dist_tex;
        self._color_tex = color_tex;
    }

    /// Clear `color`/`depth` and draw the SDF matter into them (with true depth).
    pub fn draw_into(
        &self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        globals: RaymarchGlobals,
    ) {
        gpu.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raymarch") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("raymarch"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth,
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
            rp.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}

fn vol_tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D3,
            multisampled: false,
        },
        count: None,
    }
}

fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn make_bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    globals: &wgpu::Buffer,
    dist: &wgpu::Texture,
    color: &wgpu::Texture,
    samp_lin: &wgpu::Sampler,
    samp_pt: &wgpu::Sampler,
) -> wgpu::BindGroup {
    let dist_view = dist.create_view(&wgpu::TextureViewDescriptor::default());
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("raymarch"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&dist_view) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&color_view) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(samp_lin) },
            wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(samp_pt) },
        ],
    })
}

/// Create the distance (R16Float) + color (Rgba8Unorm) 3D textures from a bake.
fn upload_volume(gpu: &Gpu, baked: &BakedSdf) -> (wgpu::Texture, wgpu::Texture) {
    let [w, h, d] = baked.dims;
    let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: d };

    let dist = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sdf-distance"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::R16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let dist_f16: Vec<u16> = baked.distance.iter().map(|&v| f32_to_f16(v)).collect();
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &dist,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&dist_f16),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 2), rows_per_image: Some(h) },
        size,
    );

    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sdf-color"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &color,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&baked.color),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
        size,
    );

    (dist, color)
}

/// Minimal `f32` → IEEE-754 half (`f16` bits). Flushes denormals to ±0 and clamps
/// overflow to ±inf — fine for distance volumes (small magnitudes).
fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = (((bits >> 23) & 0xff) as i32) - 127 + 15;
    let mant = ((bits >> 13) & 0x3ff) as u16;
    if exp <= 0 {
        sign
    } else if exp >= 0x1f {
        sign | 0x7c00
    } else {
        sign | ((exp as u16) << 10) | mant
    }
}
