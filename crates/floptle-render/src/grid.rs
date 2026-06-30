//! Editor reference grid — a depth-tested wireframe grid on a horizontal plane that
//! sits just below the camera (snapped to the grid spacing), centered near the camera
//! so it's always underfoot at any altitude. Camera-relative (ADR-0015): line
//! endpoints are offset to the camera before upload, so the GPU never sees a large
//! coordinate. Cheap enough to regenerate every frame.

use glam::{DVec3, Mat4};

use crate::device::Gpu;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GridGlobals {
    view_proj: [[f32; 4]; 4],
    color: [f32; 4],
}

const VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: 12,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &[wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 0,
        shader_location: 0,
    }],
};

pub struct Grid {
    pipeline: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    bind: wgpu::BindGroup,
    vbuf: wgpu::Buffer,
    vcap: u32,
}

impl Grid {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grid"),
            source: wgpu::ShaderSource::Wgsl(include_str!("grid.wgsl").into()),
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("grid"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("grid"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("grid"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[VERTEX_LAYOUT],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                ..Default::default()
            },
            // Test against the scene depth (objects occlude the grid) but don't write
            // depth (the grid never occludes objects).
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

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("grid-globals"),
            size: std::mem::size_of::<GridGlobals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("grid"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() }],
        });
        let vcap = 1024;
        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("grid-verts"),
            size: (vcap as u64) * 12,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self { pipeline, globals_buf, bind, vbuf, vcap }
    }

    /// Draw the grid into the (already-filled) color + depth targets. `size` is the
    /// spacing between lines and `extent` the number of cells out from the center
    /// (which tracks the camera in X/Z/Y, snapped to `size` — the plane sits on the
    /// grid line just below the camera).
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        view_proj: Mat4,
        cam_world: DVec3,
        size: f32,
        extent: i32,
        y_offset: f32,
        rgba: [f32; 4],
    ) {
        let size = size.max(0.05) as f64;
        let n = extent.clamp(1, 200);
        let cx = (cam_world.x / size).round() * size;
        let cz = (cam_world.z / size).round() * size;
        // Follow the camera's height too: place the plane on the grid line at or just
        // below the camera (floor-snap), shifted down by `y_offset`, so it stays a
        // useful floor reference at any altitude (and you can drop it further below).
        let cy = ((cam_world.y - y_offset as f64) / size).floor() * size;
        let half = n as f64 * size;
        let rel = |wx: f64, wz: f64| -> [f32; 3] {
            [(wx - cam_world.x) as f32, (cy - cam_world.y) as f32, (wz - cam_world.z) as f32]
        };
        let mut verts: Vec<[f32; 3]> = Vec::with_capacity(((2 * n + 1) * 4) as usize);
        for i in -n..=n {
            let off = i as f64 * size;
            let z = cz + off;
            verts.push(rel(cx - half, z));
            verts.push(rel(cx + half, z));
            let x = cx + off;
            verts.push(rel(x, cz - half));
            verts.push(rel(x, cz + half));
        }

        if verts.len() as u32 > self.vcap {
            self.vcap = (verts.len() as u32).next_power_of_two();
            self.vbuf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("grid-verts"),
                size: (self.vcap as u64) * 12,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        gpu.queue.write_buffer(&self.vbuf, 0, bytemuck::cast_slice(&verts));
        gpu.queue.write_buffer(
            &self.globals_buf,
            0,
            bytemuck::bytes_of(&GridGlobals { view_proj: view_proj.to_cols_array_2d(), color: rgba }),
        );

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("grid") });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("grid"),
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
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &self.bind, &[]);
            rp.set_vertex_buffer(0, self.vbuf.slice(..));
            rp.draw(0..verts.len() as u32, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}
