//! Runtime 3D line layer — world-space polylines with per-vertex color, drawn
//! over the scene (no depth test — see the pipeline comment) without writing
//! depth. This is the game-visible line facility (S6 v2 map screens, debug
//! draws): scripts queue segments via the Lua `draw.line` API each tick and
//! the editor feeds them here per camera. Camera-relative: callers
//! pre-subtract the camera position, so the GPU never sees a large coordinate.

use glam::Mat4;

use crate::device::Gpu;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct LineGlobals {
    view_proj: [[f32; 4]; 4],
    /// The picture being drawn into, in pixels (xy): where a native line looks
    /// up the scene depth under it.
    target: [f32; 4],
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
struct Globals { view_proj: mat4x4<f32>, size: vec4<f32> };
@group(0) @binding(0) var<uniform> g: Globals;
@group(1) @binding(0) var scene_depth: texture_depth_2d;

// How far in front of the surface under it a depth-tested line may be and
// still show, as a share of its distance: a reticle lying on the ground
// neither flickers into it nor floats visibly above it.
const DEPTH_SLOP: f32 = 0.001;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

fn project(pos: vec3<f32>, color: vec4<f32>) -> VsOut {
    var out: VsOut;
    out.clip = g.view_proj * vec4<f32>(pos, 1.0);
    out.color = color;
    return out;
}

@vertex
fn vs(@location(0) pos: vec3<f32>, @location(1) color: vec4<f32>) -> VsOut {
    return project(pos, color);
}

// For the depth-tested pass inside the scene: pulled toward the camera by the
// slop, so a line on a surface passes the test against that surface.
@vertex
fn vs_depth(@location(0) pos: vec3<f32>, @location(1) color: vec4<f32>) -> VsOut {
    var out = project(pos, color);
    out.clip.z = out.clip.z - DEPTH_SLOP * (out.clip.w - out.clip.z);
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}

// Over the finished picture: the scene depth under this pixel, read from the
// (lower-resolution) depth the scene was drawn with.
@fragment
fn fs_depth(in: VsOut) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(scene_depth));
    let at = clamp(in.clip.xy * dims / max(g.size.xy, vec2<f32>(1.0)), vec2<f32>(0.0), dims - 1.0);
    let d = textureLoad(scene_depth, vec2<i32>(at), 0);
    if (in.clip.z - DEPTH_SLOP * (1.0 - in.clip.z) > d) {
        discard;
    }
    return in.color;
}
"#;

pub struct Lines {
    pipeline: wgpu::RenderPipeline,
    /// The same, hidden behind the surfaces already drawn (`draw.depthTest`).
    pipeline_depth: wgpu::RenderPipeline,
    /// The scene depth a depth-tested native line reads.
    depth_layout: wgpu::BindGroupLayout,
    depth_pipeline_layout: wgpu::PipelineLayout,
    overlay_depth: Option<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    globals_buf: wgpu::Buffer,
    bind: wgpu::BindGroup,
    vbuf: wgpu::Buffer,
    vcap: u32,
    module: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    /// The same lines drawn into a finished picture instead of the scene: no
    /// depth attachment, the picture's own format. Built the first time it is
    /// asked for, per format.
    overlay: Option<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
}

/// The in-scene depth compare: `Always` draws through (the default, so an
/// orbit reads through a planet), `LessEqual` hides behind what is drawn.
#[derive(Clone, Copy)]
enum LineDepth {
    None,
    Through,
    Tested,
}

fn line_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    depth: LineDepth,
    (vs, fs): (&str, &str),
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("lines"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some(vs),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[VERTEX_LAYOUT],
        },
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::LineList, ..Default::default() },
        // Drawn over the scene (no depth test): map orbit lines span tens of
        // thousands of units, where the depth buffer's precision made
        // segments flicker in and out against far geometry — and KSP-style
        // orbit lines should read through planets anyway. Never writes depth.
        depth_stencil: (!matches!(depth, LineDepth::None)).then(|| wgpu::DepthStencilState {
            format: Gpu::DEPTH_FORMAT,
            depth_write_enabled: Some(false),
            depth_compare: Some(match depth {
                LineDepth::Tested => wgpu::CompareFunction::LessEqual,
                _ => wgpu::CompareFunction::Always,
            }),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fs),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
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
                // The fragment stage reads the picture size to find the scene
                // depth under a native line.
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
            label: Some("lines"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = line_pipeline(device, &layout, &module, gpu.scene_format(), LineDepth::Through, ("vs", "fs"));
        let pipeline_depth =
            line_pipeline(device, &layout, &module, gpu.scene_format(), LineDepth::Tested, ("vs_depth", "fs"));
        let depth_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lines-scene-depth"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let depth_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("lines-overlay-depth"),
            bind_group_layouts: &[Some(&bind_layout), Some(&depth_layout)],
            immediate_size: 0,
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
        Self {
            pipeline,
            pipeline_depth,
            depth_layout,
            depth_pipeline_layout,
            overlay_depth: None,
            globals_buf,
            bind,
            vbuf,
            vcap,
            module,
            layout,
            overlay: None,
        }
    }

    fn upload(&mut self, gpu: &Gpu, view_proj: Mat4, verts: &[LineVertex]) {
        self.upload_for(gpu, view_proj, verts, [0.0, 0.0]);
    }

    fn upload_for(&mut self, gpu: &Gpu, view_proj: Mat4, verts: &[LineVertex], target: [f32; 2]) {
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
            bytemuck::bytes_of(&LineGlobals {
                view_proj: view_proj.to_cols_array_2d(),
                target: [target[0], target[1], 0.0, 0.0],
            }),
        );
    }

    /// Draw `verts` over a finished picture of format `format` (after the
    /// post chain and any upscale), at that picture's resolution. The colours
    /// land as given: no tonemap, no bloom. No-op on an empty list.
    ///
    /// With `scene_depth` (the depth the scene was drawn with, at whatever
    /// resolution) the lines hide behind what is in front of them; without it
    /// they draw over everything.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_overlay(
        &mut self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        target: [f32; 2],
        view_proj: Mat4,
        verts: &[LineVertex],
        scene_depth: Option<&wgpu::TextureView>,
    ) {
        if verts.len() < 2 {
            return;
        }
        if scene_depth.is_some() && self.overlay_depth.as_ref().is_none_or(|(f, _)| *f != format) {
            let p = line_pipeline(
                &gpu.device,
                &self.depth_pipeline_layout,
                &self.module,
                format,
                LineDepth::None,
                ("vs", "fs_depth"),
            );
            self.overlay_depth = Some((format, p));
        }
        if scene_depth.is_none() && self.overlay.as_ref().is_none_or(|(f, _)| *f != format) {
            let p = line_pipeline(&gpu.device, &self.layout, &self.module, format, LineDepth::None, ("vs", "fs"));
            self.overlay = Some((format, p));
        }
        self.upload_for(gpu, view_proj, verts, target);
        let depth_bind = scene_depth.map(|view| {
            gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("lines-scene-depth"),
                layout: &self.depth_layout,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) }],
            })
        });
        let pipeline = match &depth_bind {
            Some(_) => self.overlay_depth.as_ref().map(|(_, p)| p),
            None => self.overlay.as_ref().map(|(_, p)| p),
        };
        let Some(pipeline) = pipeline else { return };
        let mut enc =
            gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("lines-overlay") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lines-overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &self.bind, &[]);
            if let Some(b) = &depth_bind {
                pass.set_bind_group(1, b, &[]);
            }
            pass.set_vertex_buffer(0, self.vbuf.slice(..));
            pass.draw(0..verts.len() as u32 & !1, 0..1);
        }
        gpu.queue.submit(Some(enc.finish()));
    }

    /// Draw `verts` (pairs of camera-relative endpoints) into the already-filled
    /// color + depth targets. No-op on an empty list. `depth_tested` hides them
    /// behind the surfaces already drawn; otherwise they draw through.
    pub fn draw(
        &mut self,
        gpu: &Gpu,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        view_proj: Mat4,
        verts: &[LineVertex],
        depth_tested: bool,
    ) {
        if verts.len() < 2 {
            return;
        }
        self.upload(gpu, view_proj, verts);
        let device = &gpu.device;
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
            pass.set_pipeline(if depth_tested { &self.pipeline_depth } else { &self.pipeline });
            pass.set_bind_group(0, &self.bind, &[]);
            pass.set_vertex_buffer(0, self.vbuf.slice(..));
            pass.draw(0..verts.len() as u32 & !1, 0..1);
        }
        gpu.queue.submit(Some(enc.finish()));
    }
}
