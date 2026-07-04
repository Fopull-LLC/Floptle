//! Low-resolution offscreen rendering for a retro / PS1 look.
//!
//! The scene renders into a small color + depth target (a fraction of the window
//! resolution), then [`Retro::blit`] upscales it to the window with nearest-
//! neighbor sampling. Because the *whole image* is rasterized at low resolution,
//! every edge and gradient becomes chunky pixels — not just the low-res textures —
//! which is the defining trait of PlayStation-era rendering. It's a core,
//! toggleable engine look (the signature-visuals thesis; pairs later with affine
//! texture warping and vertex snapping for full PS1 fidelity).
//!
//! The color target uses the surface format and the depth target the engine depth
//! format, so the same forward `Raster` pipeline renders into it unchanged.

use crate::device::{Frame, Gpu};

pub struct Retro {
    color_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
    width: u32,
    height: u32,
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    bind: wgpu::BindGroup,
}

impl Retro {
    /// Create the retro target at `internal_height` rows (width derives from the
    /// window's aspect), plus the nearest-neighbor upscale pipeline.
    pub fn new(gpu: &Gpu, internal_height: u32) -> Self {
        let device = &gpu.device;

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("retro-blit"),
            source: wgpu::ShaderSource::Wgsl(include_str!("retro.wgsl").into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("retro"),
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
            label: Some("retro"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("retro"),
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

        // Nearest + clamp keeps the upscaled pixels hard-edged.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("retro-samp"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let (color_view, depth_view, width, height) = make_targets(gpu, internal_height);
        let bind = make_bind(device, &bind_layout, &color_view, &sampler);

        Self { color_view, depth_view, width, height, pipeline, bind_layout, sampler, bind }
    }

    /// Rebuild the target at a new internal height and/or window aspect.
    pub fn resize(&mut self, gpu: &Gpu, internal_height: u32) {
        let (color_view, depth_view, width, height) = make_targets(gpu, internal_height);
        self.rebind(gpu, color_view, depth_view, width, height);
    }

    /// Rebuild the target at an explicit `width × height` — for an offscreen viewport
    /// whose aspect differs from the window (e.g. a docked/split Game tab that wants the
    /// same retro look as fullscreen but at its own panel aspect).
    pub fn resize_to(&mut self, gpu: &Gpu, width: u32, height: u32) {
        let (color_view, depth_view, width, height) = make_targets_wh(gpu, width.max(1), height.max(1));
        self.rebind(gpu, color_view, depth_view, width, height);
    }

    fn rebind(
        &mut self,
        gpu: &Gpu,
        color_view: wgpu::TextureView,
        depth_view: wgpu::TextureView,
        width: u32,
        height: u32,
    ) {
        self.bind = make_bind(&gpu.device, &self.bind_layout, &color_view, &self.sampler);
        self.color_view = color_view;
        self.depth_view = depth_view;
        self.width = width;
        self.height = height;
    }

    /// The low-res color target the scene renders into (surface format).
    pub fn color_view(&self) -> &wgpu::TextureView {
        &self.color_view
    }

    /// The low-res depth target (engine depth format).
    pub fn depth_view(&self) -> &wgpu::TextureView {
        &self.depth_view
    }

    /// Current internal resolution.
    pub fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Upscale the low-res target into the window frame with nearest-neighbor.
    pub fn blit(&self, gpu: &Gpu, frame: &Frame) {
        self.blit_to(gpu, &frame.view);
    }

    /// Upscale the low-res target into an arbitrary surface-format view (e.g. a
    /// post-processing input target) with nearest-neighbor.
    pub fn blit_to(&self, gpu: &Gpu, target: &wgpu::TextureView) {
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("retro-blit") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("retro-blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
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
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &self.bind, &[]);
            rp.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}

/// Build the color + depth targets sized to `internal_height` rows at the window's
/// aspect ratio.
fn make_targets(
    gpu: &Gpu,
    internal_height: u32,
) -> (wgpu::TextureView, wgpu::TextureView, u32, u32) {
    let aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
    let height = internal_height.max(1);
    let width = ((height as f32 * aspect).round() as u32).max(1);
    make_targets_wh(gpu, width, height)
}

/// Build the color + depth targets at an explicit pixel size.
fn make_targets_wh(
    gpu: &Gpu,
    width: u32,
    height: u32,
) -> (wgpu::TextureView, wgpu::TextureView, u32, u32) {
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("retro-color"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());

    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("retro-depth"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: Gpu::DEPTH_FORMAT,
        // TEXTURE_BINDING so SSAO can sample the low-res depth in retro mode
        // (the post chain runs AT this resolution, before the upscale, so AO —
        // like every other effect — goes chunky with the pixels).
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    (color_view, depth_view, width, height)
}

fn make_bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    color_view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("retro"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(color_view),
            },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    })
}
