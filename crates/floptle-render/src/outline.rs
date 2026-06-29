//! Selection-outline post-process — a generic, silhouette-accurate outline.
//!
//! A selected object's silhouette is rendered into a single-channel MASK (by the
//! raster mask pipeline for meshes, or the raymarch mask pipeline for SDF matter).
//! This pass edge-detects the mask and draws the outline color over the final frame.
//! Because it operates on the rendered silhouette — not on geometry — one outline
//! works for ANY shape: meshes, the blob, future sculpted terrain. The mask is at
//! full frame resolution, so the outline stays crisp even over a low-res retro scene.

use crate::device::Gpu;

/// Single-channel format the silhouette mask is rendered into.
pub const MASK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct OutlineUniform {
    color: [f32; 4],
    texel: [f32; 2],
    width: f32,
    _pad: f32,
}

pub struct Outline {
    mask: wgpu::Texture,
    mask_view: wgpu::TextureView,
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    bind: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    uniform_buf: wgpu::Buffer,
    width: u32,
    height: u32,
}

impl Outline {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;
        let (width, height) = (gpu.config.width.max(1), gpu.config.height.max(1));

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("outline"),
            source: wgpu::ShaderSource::Wgsl(include_str!("outline.wgsl").into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("outline"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("outline"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("outline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
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

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("outline"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("outline-uniform"),
            size: std::mem::size_of::<OutlineUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let (mask, mask_view) = make_mask(device, width, height);
        let bind = make_bind(device, &bind_layout, &mask_view, &sampler, &uniform_buf);

        Self { mask, mask_view, pipeline, bind_layout, bind, sampler, uniform_buf, width, height }
    }

    /// Recreate the mask at a new frame size (call on window resize).
    pub fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
        let (mask, mask_view) = make_mask(&gpu.device, self.width, self.height);
        self.mask = mask;
        self.mask_view = mask_view;
        self.bind =
            make_bind(&gpu.device, &self.bind_layout, &self.mask_view, &self.sampler, &self.uniform_buf);
    }

    /// The mask render target — draw the selected object's silhouette into this
    /// (via `Raster::draw_mask` or `Raymarch::draw_mask`) before calling `composite`.
    pub fn mask_view(&self) -> &wgpu::TextureView {
        &self.mask_view
    }

    /// Edge-detect the mask and draw the outline `color` over `target` (the frame).
    /// `width_px` is the half-width of the outline line in pixels.
    pub fn composite(&self, gpu: &Gpu, target: &wgpu::TextureView, color: [f32; 4], width_px: f32) {
        let u = OutlineUniform {
            color,
            texel: [1.0 / self.width as f32, 1.0 / self.height as f32],
            width: width_px,
            _pad: 0.0,
        };
        gpu.queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&u));

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("outline") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("outline"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
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
            rp.set_bind_group(0, &self.bind, &[]);
            rp.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}

fn make_mask(device: &wgpu::Device, width: u32, height: u32) -> (wgpu::Texture, wgpu::TextureView) {
    let mask = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("outline-mask"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: MASK_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = mask.create_view(&wgpu::TextureViewDescriptor::default());
    (mask, view)
}

fn make_bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    mask_view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
    uniform_buf: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("outline"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(mask_view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            wgpu::BindGroupEntry { binding: 2, resource: uniform_buf.as_entire_binding() },
        ],
    })
}
