//! The palette pass: posterize, run over the art and before the light.
//!
//! See `palette.wgsl` for why this is its own pass and not the tail of the post
//! chain (`floptle/0127`). The short version: posterize quantizes the *palette*,
//! and a light is a multiplier on the palette rather than a member of it, so the
//! quantize has to happen while the frame still holds only art. Everything the
//! renderer adds after this point — the 2D light delta, SSAO, bloom, the
//! vignette — is light-shaped and stays smooth.
//!
//! Two full-screen passes, because a pass cannot read the target it writes: the
//! frame is quantized into a scratch target and copied straight back. At the
//! resolutions this runs at (the retro internal res in retro mode) that is
//! nothing, and it keeps the caller's contract to one line — hand it the view
//! the scene was drawn into and it comes back quantized in place.

use crate::device::Gpu;

/// The artist's posterize settings, resolved to the three numbers the pass
/// needs. Built by [`crate::PostSettings::palette`], which answers `None` when
/// the setting is off — so "posterize is off" is one check at one place rather
/// than a `bands >= 2` scattered through every call site.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaletteQuantize {
    /// Levels per channel. Always >= 2 — [`crate::PostSettings::palette`] does
    /// not build one otherwise.
    pub bands: u32,
    /// Ordered-dither the step, so a smooth ramp in the *art* becomes a stipple
    /// rather than a hard edge. It no longer has anything to do with lighting.
    pub dither: bool,
    /// Step brightness and carry the chroma, rather than each channel on its own
    /// (`floptle/0126`).
    pub chroma: bool,
}

struct Scratch {
    width: u32,
    height: u32,
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
}

pub struct Palette {
    quantize_pipeline: wgpu::RenderPipeline,
    copy_pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    params: wgpu::Buffer,
    format: wgpu::TextureFormat,
    scratch: Option<Scratch>,
}

impl Palette {
    /// `format` is the frame's colour format. Both pipelines are built for it,
    /// exactly as the 2D light composite is (`Light2d::new`) — every target this
    /// renderer composites a scene into is the surface format.
    pub fn new(gpu: &Gpu, format: wgpu::TextureFormat) -> Self {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("palette"),
            source: wgpu::ShaderSource::Wgsl(include_str!("palette.wgsl").into()),
        });
        // No sampler: both passes read by integer texel (see the shader header).
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("palette"),
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
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("palette"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let make = |fs: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("palette"),
                layout: Some(&pl),
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
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("palette-params"),
            size: 32, // two vec4s: the quantize settings, then the frame size

            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            quantize_pipeline: make("fs_quantize"),
            copy_pipeline: make("fs_copy"),
            layout,
            params,
            format,
            scratch: None,
        }
    }

    /// Quantize `color` in place. `size` is the frame's pixel size — a view
    /// cannot be asked how big it is, and the scratch is usually larger.
    ///
    /// `color` must be sampleable (`TEXTURE_BINDING`), which every target a
    /// scene composites into already is: posterize forces the post chain on
    /// ([`crate::PostSettings::any`]), so the frame is a post target rather than
    /// the swapchain, and the offscreen paths render into textures that exist to
    /// be read back.
    pub fn quantize(
        &mut self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        size: (u32, u32),
        q: PaletteQuantize,
    ) {
        let (w, h) = (size.0.max(1), size.1.max(1));
        self.ensure_scratch(gpu, w, h);
        let Some(scratch) = self.scratch.as_ref() else { return };
        gpu.queue.write_buffer(
            &self.params,
            0,
            bytemuck::cast_slice(&[
                q.bands as f32,
                if q.dither { 1.0 } else { 0.0 },
                if q.chroma { 1.0 } else { 0.0 },
                0.0,
                w as f32,
                h as f32,
                0.0,
                0.0,
            ]),
        );
        let src = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("palette-src"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(color) },
                wgpu::BindGroupEntry { binding: 1, resource: self.params.as_entire_binding() },
            ],
        });
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("palette") });
        // Out: a viewport confines the write to this frame's corner of the
        // (grow-only) scratch, so frame pixel (0,0) is scratch texel (0,0) and
        // the integer reads line up whatever size it has grown to.
        pass(&mut encoder, &self.quantize_pipeline, &src, &scratch.view, Some((w, h)), CLEAR);
        // …and back, with NO viewport: the shader discards past the frame rect
        // instead, which is safe in BOTH directions a reported size can be wrong.
        // See `fs_copy`. It loads rather than clears for the same reason —
        // `LoadOp::Clear` applies to the whole attachment, not to a viewport.
        pass(&mut encoder, &self.copy_pipeline, &scratch.bind, color, None, wgpu::LoadOp::Load);
        gpu.queue.submit([encoder.finish()]);
    }

    /// Grow-only, for the reason [`crate::light2d`]'s G-buffer is: one renderer
    /// serves several viewports of different sizes in a frame, and sizing to the
    /// frame would tear the scratch down and rebuild it between every pair of
    /// them, forever.
    fn ensure_scratch(&mut self, gpu: &Gpu, width: u32, height: u32) {
        let (mut w, mut h) = (width, height);
        if let Some(s) = self.scratch.as_ref() {
            if s.width >= w && s.height >= h {
                return;
            }
            w = w.max(s.width);
            h = h.max(s.height);
        }
        let view = gpu
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("palette-scratch"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default());
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("palette-scratch"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: self.params.as_entire_binding() },
            ],
        });
        self.scratch = Some(Scratch { width: w, height: h, view, bind });
    }
}

const CLEAR: wgpu::LoadOp<wgpu::Color> = wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT);

fn pass(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    bind: &wgpu::BindGroup,
    target: &wgpu::TextureView,
    viewport: Option<(u32, u32)>,
    load: wgpu::LoadOp<wgpu::Color>,
) {
    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("palette"),
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
    if let Some((w, h)) = viewport {
        rp.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
    }
    rp.set_pipeline(pipeline);
    rp.set_bind_group(0, bind, &[]);
    rp.draw(0..3, 0..1);
}
