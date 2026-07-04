//! Post-processing stack — full-screen color effects that run at the same
//! resolution the scene was composited at: full frame res normally, the retro
//! internal res in retro mode (the chain runs BEFORE the nearest-neighbor
//! upscale, so every effect goes chunky with the same pixels as the scene). The
//! chain is **SSAO** (screen-space ambient occlusion from the depth buffer,
//! half-res + blur, multiplied over the scene), **bloom** (bright-pass →
//! separable Gaussian blur → additive composite) and a **vignette**. Each pass is
//! the same shape as [`crate::retro::Retro::blit`]: a one-triangle fragment pass
//! reading one texture and writing another, ping-ponging between targets.
//! Settings come from the scene's PostProcess node (per-scene, not per-project).

use crate::device::Gpu;

/// Artist-facing post-processing settings (the editor maps these from the scene's
/// PostProcess node). All effects off = a single passthrough copy.
#[derive(Clone, Copy, Debug)]
pub struct PostSettings {
    pub bloom: bool,
    pub bloom_threshold: f32,
    pub bloom_intensity: f32,
    pub vignette: bool,
    pub vignette_strength: f32,
    pub vignette_radius: f32,
    /// Screen-space ambient occlusion (needs the depth the scene rendered with —
    /// see [`SsaoFrame`]; without one the effect is skipped).
    pub ssao: bool,
    /// How dark full occlusion gets (0..1).
    pub ssao_strength: f32,
    /// Occlusion reach in world units.
    pub ssao_radius: f32,
    /// Posterize: quantize the final color to this many levels per channel (a limited
    /// palette / banded look). 0 or 1 = off; 2.. = enabled. Applied in the terminal
    /// pass at the composited (retro) resolution, before the upscale.
    pub posterize_bands: u32,
    /// Ordered-dither the posterize quantization so smooth ramps don't hard-step.
    pub posterize_dither: bool,
}

impl PostSettings {
    /// True if any effect is enabled (else the stack is a no-op passthrough).
    pub fn any(&self) -> bool {
        self.bloom || self.vignette || self.ssao || self.posterize_bands >= 2
    }
}

/// Per-frame inputs the SSAO pass needs: the depth buffer the scene was rendered
/// with (full-res normally, the low-res retro depth in retro mode) and the
/// projection that produced it.
pub struct SsaoFrame<'a> {
    pub depth: &'a wgpu::TextureView,
    /// Camera projection (view → clip), column-major.
    pub proj: [[f32; 4]; 4],
    /// Its inverse (clip → view).
    pub inv_proj: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PostParams {
    /// xy = texel (1/src size), z = bloom_threshold, w = bloom_intensity.
    a: [f32; 4],
    /// x = vignette_strength, y = vignette_radius, zw = blur_dir (texels).
    b: [f32; 4],
}

/// Uniform for the SSAO pass — matches `SsaoParams` in `ssao.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SsaoParams {
    proj: [[f32; 4]; 4],
    inv_proj: [[f32; 4]; 4],
    /// x = radius (world units), y = strength, z = depth bias, w unused.
    params: [f32; 4],
}

/// One color texture + its view + a bind group that samples it.
struct Target {
    _tex: wgpu::Texture,
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
}

pub struct PostStack {
    scene: Target, // full-res: the scene renders here
    ping: Target,  // full-res chain scratch
    pong: Target,  // full-res chain scratch (so ssao + bloom can both ping-pong)
    bloom_a: Target,
    bloom_b: Target, // half-res blur scratch
    ao_a: Target,
    ao_b: Target, // half-res R8 AO factor + blur scratch
    ao_bind1: wgpu::BindGroup, // ao_a as the fs_ssao_apply group(1) input
    width: u32,
    height: u32,
    /// Pixel-perfect mode (retro): the AO factor is computed at FULL chain res —
    /// one value per (retro) pixel — with a tightened blur, instead of the
    /// half-res + wide-blur combo that suits big framebuffers. At retro sizes the
    /// half-res buffer is so coarse and the fixed ±4-texel blur so wide (in
    /// screen fractions) that contact shadows wash out entirely.
    pixel_perfect: bool,
    params_buf: wgpu::Buffer,
    ssao_buf: wgpu::Buffer,
    sampler: wgpu::Sampler,
    bind_layout: wgpu::BindGroupLayout,
    ssao_layout: wgpu::BindGroupLayout, // { depth texture, ssao uniform }
    ao_layout: wgpu::BindGroupLayout,   // { ao texture, sampler } for group(1)
    copy_pipeline: wgpu::RenderPipeline,
    bright_pipeline: wgpu::RenderPipeline,
    blur_pipeline: wgpu::RenderPipeline,
    composite_pipeline: wgpu::RenderPipeline, // additive blend
    finish_pipeline: wgpu::RenderPipeline,     // terminal vignette + posterize
    ssao_pipeline: wgpu::RenderPipeline,       // ssao.wgsl → half-res R8
    ao_blur_pipeline: wgpu::RenderPipeline,    // fs_blur onto the R8 targets
    ssao_apply_pipeline: wgpu::RenderPipeline, // scene × AO
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

        // SSAO pass: { depth texture, ssao uniform }. Depth32Float is
        // non-filterable, so the shader reads it with textureLoad (no sampler).
        let ssao_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post-ssao"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
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
        let ssao_pl_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("post-ssao"),
            bind_group_layouts: &[Some(&ssao_layout)],
            immediate_size: 0,
        });

        // fs_ssao_apply's second group: the blurred AO factor.
        let ao_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post-ao"),
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
        let apply_pl_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("post-ssao-apply"),
            bind_group_layouts: &[Some(&bind_layout), Some(&ao_layout)],
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
        let finish_pipeline = make_pipeline("fs_finish", None);

        // The SSAO trio: its own shader module for the factor (a depth binding
        // can't share post.wgsl's group 0), fs_blur re-targeted at the R8 AO
        // textures, and the apply pass with the AO factor as a second group.
        let generic_pipeline = |module: &wgpu::ShaderModule,
                                pl: &wgpu::PipelineLayout,
                                fs: &str,
                                target_fmt: wgpu::TextureFormat| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("post"),
                layout: Some(pl),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(fs),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_fmt,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let ssao_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ssao"),
            source: wgpu::ShaderSource::Wgsl(include_str!("ssao.wgsl").into()),
        });
        const AO_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;
        let ssao_pipeline = generic_pipeline(&ssao_module, &ssao_pl_layout, "fs_ssao", AO_FORMAT);
        let ao_blur_pipeline = generic_pipeline(&module, &layout, "fs_blur", AO_FORMAT);
        let ssao_apply_pipeline = generic_pipeline(&module, &apply_pl_layout, "fs_ssao_apply", fmt);

        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("post-params"),
            size: std::mem::size_of::<PostParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let ssao_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("post-ssao-params"),
            size: std::mem::size_of::<SsaoParams>() as u64,
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
        let mk = |w, h, f| Target::new(gpu, &bind_layout, &sampler, &params_buf, f, w, h);
        let ao_a = mk(hw, hh, AO_FORMAT);
        let ao_bind1 = Self::make_ao_bind(gpu, &ao_layout, &ao_a, &sampler);
        Self {
            scene: mk(width, height, fmt),
            ping: mk(width, height, fmt),
            pong: mk(width, height, fmt),
            bloom_a: mk(hw, hh, fmt),
            bloom_b: mk(hw, hh, fmt),
            ao_a,
            ao_b: mk(hw, hh, AO_FORMAT),
            ao_bind1,
            width,
            height,
            pixel_perfect: false,
            params_buf,
            ssao_buf,
            sampler,
            bind_layout,
            ssao_layout,
            ao_layout,
            copy_pipeline,
            bright_pipeline,
            blur_pipeline,
            composite_pipeline,
            finish_pipeline,
            ssao_pipeline,
            ao_blur_pipeline,
            ssao_apply_pipeline,
        }
    }

    pub fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        let fmt = gpu.surface_format();
        let (width, height) = (width.max(1), height.max(1));
        let (hw, hh) = ((width / 2).max(1), (height / 2).max(1));
        let (aw, ah) = Self::ao_size(width, height, self.pixel_perfect);
        let mk =
            |w, h, f| Target::new(gpu, &self.bind_layout, &self.sampler, &self.params_buf, f, w, h);
        self.scene = mk(width, height, fmt);
        self.ping = mk(width, height, fmt);
        self.pong = mk(width, height, fmt);
        self.bloom_a = mk(hw, hh, fmt);
        self.bloom_b = mk(hw, hh, fmt);
        self.ao_a = mk(aw, ah, wgpu::TextureFormat::R8Unorm);
        self.ao_b = mk(aw, ah, wgpu::TextureFormat::R8Unorm);
        self.ao_bind1 = Self::make_ao_bind(gpu, &self.ao_layout, &self.ao_a, &self.sampler);
        self.width = width;
        self.height = height;
    }

    /// Per-frame idempotent (re)configuration: retargets the chain to `width` ×
    /// `height` in the given pixel-perfect mode, rebuilding targets only when
    /// something actually changed. The editor calls this every frame with the
    /// retro internal res + `pixel_perfect = true` in retro mode, or the frame
    /// res + `false` otherwise.
    pub fn configure(&mut self, gpu: &Gpu, width: u32, height: u32, pixel_perfect: bool) {
        let (width, height) = (width.max(1), height.max(1));
        if (self.width, self.height, self.pixel_perfect) == (width, height, pixel_perfect) {
            return;
        }
        self.pixel_perfect = pixel_perfect;
        self.resize(gpu, width, height);
    }

    /// The AO factor's resolution: full chain res in pixel-perfect mode (one AO
    /// value per pixel), half res otherwise (plenty at frame res, and 4× cheaper).
    fn ao_size(width: u32, height: u32, pixel_perfect: bool) -> (u32, u32) {
        if pixel_perfect {
            (width, height)
        } else {
            ((width / 2).max(1), (height / 2).max(1))
        }
    }

    /// The group(1) bind for `fs_ssao_apply`: the (blurred) AO factor in `ao_a`.
    fn make_ao_bind(
        gpu: &Gpu,
        layout: &wgpu::BindGroupLayout,
        ao: &Target,
        sampler: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("post-ao"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&ao.view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            ],
        })
    }

    /// The target the scene must render into when post is enabled (instead of the
    /// swapchain frame). Sized by `new`/`resize` — full frame res normally, the
    /// retro internal res in retro mode.
    pub fn input_view(&self) -> &wgpu::TextureView {
        &self.scene.view
    }

    /// Current chain resolution (the size `new`/`resize` was given).
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Run the enabled effect chain reading `input_view()` and writing the final
    /// image into `out`. With nothing enabled it's a single passthrough copy.
    /// `ssao` supplies the depth + projection the SSAO pass needs; when the
    /// settings ask for SSAO but no frame inputs are given, the effect is skipped.
    pub fn run(&self, gpu: &Gpu, s: &PostSettings, ssao: Option<&SsaoFrame>, out: &wgpu::TextureView) {
        let ssao_on = s.ssao && ssao.is_some();
        let posterize_on = s.posterize_bands >= 2;
        if !(ssao_on || s.bloom || s.vignette || posterize_on) {
            self.write_params(gpu, PostParams { a: [0.0; 4], b: [0.0; 4] });
            self.pass(gpu, &self.copy_pipeline, &self.scene.bind, out, wgpu::LoadOp::Clear(BLACK));
            return;
        }

        let htexel = [1.0 / (self.width / 2).max(1) as f32, 1.0 / (self.height / 2).max(1) as f32];
        let mut cur: &Target = &self.scene;

        if let (true, Some(f)) = (ssao_on, ssao) {
            // AO factor: depth → ao_a (half res, or full res in pixel-perfect
            // mode), then a separable blur (A→B→A) to wash out the sampling
            // noise, then multiply it over the scene. Pixel-perfect tightens the
            // blur step to half a texel — at retro resolutions the full ±4-texel
            // kernel spans so much of the screen it dilutes contact shadows away.
            let (aw, ah) = Self::ao_size(self.width, self.height, self.pixel_perfect);
            let atexel = [1.0 / aw as f32, 1.0 / ah as f32];
            let astep = if self.pixel_perfect { 0.5 } else { 1.0 };
            let bias = 0.02f32.max(0.03 * s.ssao_radius);
            self.write_ssao(gpu, SsaoParams {
                proj: f.proj,
                inv_proj: f.inv_proj,
                params: [s.ssao_radius, s.ssao_strength, bias, 0.0],
            });
            let depth_bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("post-ssao"),
                layout: &self.ssao_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(f.depth) },
                    wgpu::BindGroupEntry { binding: 1, resource: self.ssao_buf.as_entire_binding() },
                ],
            });
            self.pass(gpu, &self.ssao_pipeline, &depth_bind, &self.ao_a.view, wgpu::LoadOp::Clear(BLACK));
            self.write_params(gpu, PostParams { a: [atexel[0], atexel[1], 0.0, 0.0], b: [0.0, 0.0, astep, 0.0] });
            self.pass(gpu, &self.ao_blur_pipeline, &self.ao_a.bind, &self.ao_b.view, wgpu::LoadOp::Clear(BLACK));
            self.write_params(gpu, PostParams { a: [atexel[0], atexel[1], 0.0, 0.0], b: [0.0, 0.0, 0.0, astep] });
            self.pass(gpu, &self.ao_blur_pipeline, &self.ao_b.bind, &self.ao_a.view, wgpu::LoadOp::Clear(BLACK));
            self.write_params(gpu, PostParams { a: [0.0; 4], b: [0.0; 4] });
            self.pass2(
                gpu,
                &self.ssao_apply_pipeline,
                &cur.bind,
                Some(&self.ao_bind1),
                &self.ping.view,
                wgpu::LoadOp::Clear(BLACK),
            );
            cur = &self.ping;
        }

        if s.bloom {
            // Bright-pass: cur → half-res bloom_a.
            self.write_params(gpu, PostParams { a: [0.0, 0.0, s.bloom_threshold, 0.0], b: [0.0; 4] });
            self.pass(gpu, &self.bright_pipeline, &cur.bind, &self.bloom_a.view, wgpu::LoadOp::Clear(BLACK));
            // Separable blur: A→B (horizontal), B→A (vertical).
            self.write_params(gpu, PostParams { a: [htexel[0], htexel[1], 0.0, 0.0], b: [0.0, 0.0, 1.0, 0.0] });
            self.pass(gpu, &self.blur_pipeline, &self.bloom_a.bind, &self.bloom_b.view, wgpu::LoadOp::Clear(BLACK));
            self.write_params(gpu, PostParams { a: [htexel[0], htexel[1], 0.0, 0.0], b: [0.0, 0.0, 0.0, 1.0] });
            self.pass(gpu, &self.blur_pipeline, &self.bloom_b.bind, &self.bloom_a.view, wgpu::LoadOp::Clear(BLACK));
            // Composite: copy cur into the free full-res scratch, then additively
            // add the blurred bloom.
            let dst: &Target = if std::ptr::eq(cur, &self.ping) { &self.pong } else { &self.ping };
            self.write_params(gpu, PostParams { a: [0.0, 0.0, 0.0, s.bloom_intensity], b: [0.0; 4] });
            self.pass(gpu, &self.copy_pipeline, &cur.bind, &dst.view, wgpu::LoadOp::Clear(BLACK));
            self.pass(gpu, &self.composite_pipeline, &self.bloom_a.bind, &dst.view, wgpu::LoadOp::Load);
            cur = dst;
        }

        // Terminal pass: vignette and/or posterize (one shader, no-op at identity
        // params). Reuses the dead blur_dir lanes b.zw for posterize (bands, dither),
        // so no uniform-layout change. Otherwise a straight passthrough copy to `out`.
        if s.vignette || posterize_on {
            let b = [
                if s.vignette { s.vignette_strength } else { 0.0 },
                if s.vignette { s.vignette_radius } else { 1.0 },
                if posterize_on { s.posterize_bands as f32 } else { 0.0 },
                if posterize_on && s.posterize_dither { 1.0 } else { 0.0 },
            ];
            self.write_params(gpu, PostParams { a: [0.0; 4], b });
            self.pass(gpu, &self.finish_pipeline, &cur.bind, out, wgpu::LoadOp::Clear(BLACK));
        } else {
            self.write_params(gpu, PostParams { a: [0.0; 4], b: [0.0; 4] });
            self.pass(gpu, &self.copy_pipeline, &cur.bind, out, wgpu::LoadOp::Clear(BLACK));
        }
    }

    fn write_params(&self, gpu: &Gpu, params: PostParams) {
        gpu.queue.write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&params));
    }

    fn write_ssao(&self, gpu: &Gpu, params: SsaoParams) {
        gpu.queue.write_buffer(&self.ssao_buf, 0, bytemuck::bytes_of(&params));
    }

    fn pass(
        &self,
        gpu: &Gpu,
        pipeline: &wgpu::RenderPipeline,
        bind: &wgpu::BindGroup,
        target: &wgpu::TextureView,
        load: wgpu::LoadOp<wgpu::Color>,
    ) {
        self.pass2(gpu, pipeline, bind, None, target, load);
    }

    fn pass2(
        &self,
        gpu: &Gpu,
        pipeline: &wgpu::RenderPipeline,
        bind: &wgpu::BindGroup,
        bind1: Option<&wgpu::BindGroup>,
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
            if let Some(b1) = bind1 {
                rp.set_bind_group(1, b1, &[]);
            }
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
