//! Runtime 3D line layer — world-space polylines with per-vertex color, drawn
//! depth-tested into the scene (planets occlude orbit lines) without writing
//! depth. This is the game-visible line facility (S6 v2 map screens, debug
//! draws): scripts queue segments via the Lua `draw.line` API each tick and
//! the editor feeds them here per camera. Camera-relative (ADR-0015): callers
//! pre-subtract the camera position, so the GPU never sees a large coordinate.

use glam::Mat4;

use crate::device::Gpu;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct LineGlobals {
    view_proj: [[f32; 4]; 4],
}

/// One line endpoint: camera-relative position + RGBA color.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LineVertex {
    pub pos: [f32; 3],
    pub color: [f32; 4],
}

const VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: 28,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &[
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 0,
            shader_location: 0,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x4,
            offset: 12,
            shader_location: 1,
        },
    ],
};

const WGSL: &str = r#"
struct Globals { view_proj: mat4x4<f32> };
@group(0) @binding(0) var<uniform> g: Globals;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs(@location(0) pos: vec3<f32>, @location(1) color: vec4<f32>) -> VsOut {
    var out: VsOut;
    out.clip = g.view_proj * vec4<f32>(pos, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

pub struct Lines {
    pipeline: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    bind: wgpu::BindGroup,
    vbuf: wgpu::Buffer,
    vcap: u32,
}

impl Lines {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lines"),
            source: wgpu::ShaderSource::Wgsl(WGSL.into()),
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lines"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("lines"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("lines"),
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
            // Depth-tested against the scene (bodies occlude the lines) but never
            // written — a line must not occlude geometry drawn after it.
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
            label: Some("lines-globals"),
            size: std::mem::size_of::<LineGlobals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lines"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });
        let vcap = 4096;
        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lines-verts"),
            size: (vcap as u64) * 28,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self { pipeline, globals_buf, bind, vbuf, vcap }
    }

    /// Draw `verts` (pairs of camera-relative endpoints) into the already-filled
    /// color + depth targets. No-op on an empty list.
    pub fn draw(
        &mut self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        view_proj: Mat4,
        verts: &[LineVertex],
    ) {
        if verts.len() < 2 {
            return;
        }
        let device = &gpu.device;
        if verts.len() as u32 > self.vcap {
            self.vcap = (verts.len() as u32).next_power_of_two();
            self.vbuf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("lines-verts"),
                size: (self.vcap as u64) * 28,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        gpu.queue.write_buffer(&self.vbuf, 0, bytemuck::cast_slice(verts));
        gpu.queue.write_buffer(
            &self.globals_buf,
            0,
            bytemuck::bytes_of(&LineGlobals { view_proj: view_proj.to_cols_array_2d() }),
        );
        let mut enc =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("lines") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lines"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
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
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind, &[]);
            pass.set_vertex_buffer(0, self.vbuf.slice(..));
            pass.draw(0..verts.len() as u32 & !1, 0..1);
        }
        gpu.queue.submit(Some(enc.finish()));
    }
}
