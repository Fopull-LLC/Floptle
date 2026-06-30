//! Post-processing stack — full-screen color effects that run after the scene is
//! composited but before the retro downsample, so they "fit the look". The first
//! slice is **bloom** (bright-pass → separable Gaussian blur → additive composite)
//! and a **vignette**; SSAO is a later slice (it needs sampled depth). Each pass is
//! the same shape as [`crate::retro::Retro::blit`]: a one-triangle fragment pass
//! reading one texture and writing another, ping-ponging between targets.

use crate::device::Gpu;

/// Artist-facing post-processing settings (the editor maps these from its project
/// config). All effects off = a single passthrough copy.
#[derive(Clone, Copy, Debug)]
pub struct PostSettings {
    pub bloom: bool,
    pub bloom_threshold: f32,
    pub bloom_intensity: f32,
    pub vignette: bool,
    pub vignette_strength: f32,
    pub vignette_radius: f32,
}

impl PostSettings {
    /// True if any effect is enabled (else the stack is a no-op passthrough).
    pub fn any(&self) -> bool {
        self.bloom || self.vignette
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PostParams {
    /// xy = texel (1/src size), z = bloom_threshold, w = bloom_intensity.
    a: [f32; 4],
    /// x = vignette_strength, y = vignette_radius, zw = blur_dir (texels).
    b: [f32; 4],
}

/// One color texture + its view + a bind group that samples it.
struct Target {
    _tex: wgpu::Texture,
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
}

pub struct PostStack {
    scene: Target, // full-res: the scene renders here
    ping: Target,  // full-res: bloom composite result
    bloom_a: Target,
    bloom_b: Target, // half-res blur scratch
    width: u32,
    height: u32,
    params_buf: wgpu::Buffer,
    sampler: wgpu::Sampler,
    bind_layout: wgpu::BindGroupLayout,
    copy_pipeline: wgpu::RenderPipeline,
    bright_pipeline: wgpu::RenderPipeline,
    blur_pipeline: wgpu::RenderPipeline,
    composite_pipeline: wgpu::RenderPipeline, // additive blend
    vignette_pipeline: wgpu::RenderPipeline,
}

impl PostStack {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Self {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("post"),
            source: wgpu::ShaderSource::Wgsl(include_str!("post.wgsl").into()),
        });

        // One layout for every pass: { src texture, sampler, params uniform }.
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post"),
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
            label: Some("post"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let fmt = gpu.surface_format();
        let make_pipeline = |fs: &str, blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("post"),
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
                    entry_point: Some(fs),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: fmt,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let copy_pipeline = make_pipeline("fs_copy", None);
        let bright_pipeline = make_pipeline("fs_bright", None);
        let blur_pipeline = make_pipeline("fs_blur", None);
        let composite_pipeline = make_pipeline(
            "fs_composite",
            Some(wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent::REPLACE,
            }),
        );
        let vignette_pipeline = make_pipeline("fs_vignette", None);

        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("post-params"),
            size: std::mem::size_of::<PostParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("post-samp"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let (width, height) = (width.max(1), height.max(1));
        let (hw, hh) = ((width / 2).max(1), (height / 2).max(1));
        let mk = |w, h| Target::new(gpu, &bind_layout, &sampler, &params_buf, fmt, w, h);
        Self {
            scene: mk(width, height),
            ping: mk(width, height),
            bloom_a: mk(hw, hh),
            bloom_b: mk(hw, hh),
            width,
            height,
            params_buf,
            sampler,
            bind_layout,
            copy_pipeline,
            bright_pipeline,
            blur_pipeline,
            composite_pipeline,
            vignette_pipeline,
        }
    }

    pub fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        let fmt = gpu.surface_format();
        let (width, height) = (width.max(1), height.max(1));
        let (hw, hh) = ((width / 2).max(1), (height / 2).max(1));
        let mk = |w, h| Target::new(gpu, &self.bind_layout, &self.sampler, &self.params_buf, fmt, w, h);
        self.scene = mk(width, height);
        self.ping = mk(width, height);
        self.bloom_a = mk(hw, hh);
        self.bloom_b = mk(hw, hh);
        self.width = width;
        self.height = height;
    }

    /// The full-res target the scene must render into when post is enabled (instead
    /// of the swapchain frame).
    pub fn input_view(&self) -> &wgpu::TextureView {
        &self.scene.view
    }

    /// Run the enabled effect chain reading `input_view()` and writing the final
    /// image into `out`. With nothing enabled it's a single passthrough copy.
    pub fn run(&self, gpu: &Gpu, s: &PostSettings, out: &wgpu::TextureView) {
        if !s.any() {
            self.write_params(gpu, PostParams { a: [0.0; 4], b: [0.0; 4] });
            self.pass(gpu, &self.copy_pipeline, &self.scene.bind, out, wgpu::LoadOp::Clear(BLACK));
            return;
        }

        let mut final_bind = &self.scene.bind;
        if s.bloom {
            let htexel = [1.0 / (self.width / 2).max(1) as f32, 1.0 / (self.height / 2).max(1) as f32];
            // Bright-pass: scene → half-res bloom_a.
            self.write_params(gpu, PostParams { a: [0.0, 0.0, s.bloom_threshold, 0.0], b: [0.0; 4] });
            self.pass(gpu, &self.bright_pipeline, &self.scene.bind, &self.bloom_a.view, wgpu::LoadOp::Clear(BLACK));
            // Separable blur: A→B (horizontal), B→A (vertical).
            self.write_params(gpu, PostParams { a: [htexel[0], htexel[1], 0.0, 0.0], b: [0.0, 0.0, 1.0, 0.0] });
            self.pass(gpu, &self.blur_pipeline, &self.bloom_a.bind, &self.bloom_b.view, wgpu::LoadOp::Clear(BLACK));
            self.write_params(gpu, PostParams { a: [htexel[0], htexel[1], 0.0, 0.0], b: [0.0, 0.0, 0.0, 1.0] });
            self.pass(gpu, &self.blur_pipeline, &self.bloom_b.bind, &self.bloom_a.view, wgpu::LoadOp::Clear(BLACK));
            // Composite: copy scene into ping, then additively add the blurred bloom.
            self.write_params(gpu, PostParams { a: [0.0, 0.0, 0.0, s.bloom_intensity], b: [0.0; 4] });
            self.pass(gpu, &self.copy_pipeline, &self.scene.bind, &self.ping.view, wgpu::LoadOp::Clear(BLACK));
            self.pass(gpu, &self.composite_pipeline, &self.bloom_a.bind, &self.ping.view, wgpu::LoadOp::Load);
            final_bind = &self.ping.bind;
        }

        if s.vignette {
            self.write_params(gpu, PostParams { a: [0.0; 4], b: [s.vignette_strength, s.vignette_radius, 0.0, 0.0] });
            self.pass(gpu, &self.vignette_pipeline, final_bind, out, wgpu::LoadOp::Clear(BLACK));
        } else {
            self.write_params(gpu, PostParams { a: [0.0; 4], b: [0.0; 4] });
            self.pass(gpu, &self.copy_pipeline, final_bind, out, wgpu::LoadOp::Clear(BLACK));
        }
    }

    fn write_params(&self, gpu: &Gpu, params: PostParams) {
        gpu.queue.write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&params));
    }

    fn pass(
        &self,
        gpu: &Gpu,
        pipeline: &wgpu::RenderPipeline,
        bind: &wgpu::BindGroup,
        target: &wgpu::TextureView,
        load: wgpu::LoadOp<wgpu::Color>,
    ) {
        let mut encoder =
            gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("post-pass") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("post-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(pipeline);
            rp.set_bind_group(0, bind, &[]);
            rp.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}

const BLACK: wgpu::Color = wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };

impl Target {
    fn new(
        gpu: &Gpu,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        params_buf: &wgpu::Buffer,
        format: wgpu::TextureFormat,
        w: u32,
        h: u32,
    ) -> Self {
        let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("post-target"),
            size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("post-target"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
                wgpu::BindGroupEntry { binding: 2, resource: params_buf.as_entire_binding() },
            ],
        });
        Self { _tex: tex, view, bind }
    }
}
