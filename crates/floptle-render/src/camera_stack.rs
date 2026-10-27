//! Camera stacks — a second camera's picture laid over the first.
//!
//! A stacked camera renders into a [`StackLayer`] of its own: cleared to
//! transparent black, with a depth buffer of its own, so the world the base
//! camera drew cannot hide anything in it. [`StackComposite`] then lays the
//! layer over the frame. This is what keeps first-person arms out of walls:
//! the arms are drawn by a camera that never sees the wall.
//!
//! The layer is composited before post-processing, so the whole picture is
//! tonemapped, bloomed and graded as one. Where the layer drew something solid
//! its depth goes into the frame's depth too
//! ([`StackComposite::composite_depth`]), as a distance from the base camera,
//! so depth of field and AO see a cockpit 0.6 m away as 0.6 m away rather
//! than as the hills behind it.

use crate::device::Gpu;

/// A stacked camera's render target: colour in the scene format and a depth
/// buffer that can be read back by the composite.
pub struct StackLayer {
    color_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
    size: (u32, u32),
}

impl StackLayer {
    pub fn new(gpu: &Gpu, w: u32, h: u32) -> Self {
        let (w, h) = (w.max(1), h.max(1));
        let extent = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("camera-stack-color"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.scene_format(),
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("camera-stack-depth"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Gpu::DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        Self {
            color_view: color.create_view(&wgpu::TextureViewDescriptor::default()),
            depth_view: depth.create_view(&wgpu::TextureViewDescriptor::default()),
            size: (w, h),
        }
    }

    pub fn color_view(&self) -> &wgpu::TextureView {
        &self.color_view
    }

    pub fn depth_view(&self) -> &wgpu::TextureView {
        &self.depth_view
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }
}

/// The pass that lays a [`StackLayer`] over a frame in the scene format.
pub struct StackComposite {
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    depth_pipeline: wgpu::RenderPipeline,
    depth_layout: wgpu::BindGroupLayout,
    depth_buf: wgpu::Buffer,
}

impl StackComposite {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("camera-stack"),
            source: wgpu::ShaderSource::Wgsl(include_str!("camera_stack.wgsl").into()),
        });
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("camera-stack"),
            entries: &[texture(0), texture(1)],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("camera-stack"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("camera-stack"),
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
                    format: gpu.scene_format(),
                    // Premultiplied: a solid pixel replaces the frame, a
                    // translucent one was already multiplied by its alpha
                    // when it blended over the layer's clear.
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        // The depth pass: the layer's depth texture, and the matrix from the
        // layer's clip space to the base camera's.
        let depth_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("camera-stack-depth"),
            entries: &[
                texture(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
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
        let depth_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("camera-stack-depth"),
            bind_group_layouts: &[Some(&depth_layout)],
            immediate_size: 0,
        });
        let depth_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("camera-stack-depth"),
            layout: Some(&depth_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            // Always: the layer is drawn over the frame whatever the frame had
            // there, so its depth replaces the frame's where it drew.
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
                entry_point: Some("fs_depth"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[],
            }),
            multiview_mask: None,
            cache: None,
        });
        let depth_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("camera-stack-depth"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self { pipeline, bind_layout, depth_pipeline, depth_layout, depth_buf }
    }

    /// Write where `layer` drew something solid into the frame's `depth`, as
    /// the base camera would have seen it there. `layer_to_base` takes the
    /// layer's clip space to the base camera's: the base camera's view-
    /// projection, times the move from the layer camera's position to the base
    /// camera's, times the inverse of the layer camera's view-projection.
    pub fn composite_depth(&self, gpu: &Gpu, layer: &StackLayer, depth: &wgpu::TextureView, layer_to_base: glam::Mat4) {
        gpu.queue.write_buffer(&self.depth_buf, 0, bytemuck::cast_slice(&layer_to_base.to_cols_array()));
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("camera-stack-depth"),
            layout: &self.depth_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(layer.depth_view()),
                },
                wgpu::BindGroupEntry { binding: 3, resource: self.depth_buf.as_entire_binding() },
            ],
        });
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("camera-stack-depth") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("camera-stack-depth"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.depth_pipeline);
            rp.set_bind_group(0, &bind, &[]);
            rp.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
    }

    /// Lay `layer` over `target`, which keeps everything the layer did not draw.
    pub fn composite(&self, gpu: &Gpu, layer: &StackLayer, target: &wgpu::TextureView) {
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("camera-stack"),
            layout: &self.bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(layer.color_view()),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(layer.depth_view()),
                },
            ],
        });
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("camera-stack") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("camera-stack"),
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
            rp.set_bind_group(0, &bind, &[]);
            rp.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}
