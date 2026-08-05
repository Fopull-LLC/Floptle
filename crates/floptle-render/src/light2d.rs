//! **2D lighting** — the deferred pass (`docs/2d-lighting-proposal.md`, step 2).
//!
//! A flat scene's surfaces are drawn a second time into a small G-buffer, and one
//! full-screen pass adds every 2D light that reaches each pixel's sorting layer.
//!
//! ## Why deferred, and what that costs
//!
//! Forward accumulation would have been cheaper to build and would have inherited
//! every view for free, because it *is* the existing path. Deferred was chosen
//! (Ty, 2026-08-05) because its cost is screen pixels × lights rather than pixels
//! *drawn* × lights, so a deep parallax stack with many lights does not multiply
//! the work.
//!
//! The bill for that is a **second draw path**, in a renderer where two paths
//! drifting apart has cost three releases — most recently tilemaps invisible in
//! the Game view from v0.25.0 to v0.37.0. So the rule here is not optional:
//!
//! > **The G-buffer is filled from the list the main gather already produced.**
//!
//! The editor builds [`Light2dInstance`]s in the same loop, from the same
//! transforms, as the instances it hands the raster pass. There is no second
//! query of the world, no second `match` over `Matter`, and nothing to keep in
//! step by hand.
//!
//! ## Why the rank rides the geometry
//!
//! A light reaches a *set of sorting layers*, so accumulation has to know which
//! layer each pixel belongs to. That cannot come from a uniform (one draw covers
//! one layer, but the accumulation covers the screen), so the surface's rank is
//! written into the G-buffer by the geometry that produced it.
//!
//! It rides a vertex attribute of this pass's **own** instance type rather than
//! being packed into a spare bit of `InstanceRaw`, whose 16 attribute slots are
//! full and whose spare lanes are already carrying two other packings.

use crate::device::Gpu;

/// One flat surface in the G-buffer fill.
///
/// Deliberately small and its own type. The raster pass's `InstanceRaw` carries
/// twenty-odd lanes of material state that a deferred albedo write has no use
/// for, and its attribute budget is full at 16/16.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Light2dInstance {
    /// Camera-relative model matrix — the same one the raster pass gets, from
    /// the same gather, so a surface lands on the same pixels in both or neither.
    pub model: [[f32; 4]; 4],
    /// rgb tint × a opacity, multiplied into the sampled texture.
    pub tint: [f32; 4],
    /// x = sorting-layer rank. y/z/w are spare and are where a normal-map slot
    /// goes when step 3 lands.
    pub meta: [f32; 4],
}

impl Light2dInstance {
    /// Per-instance attributes, continuing the mesh's own pos/normal/uv at 0..2.
    const ATTRS: [wgpu::VertexAttribute; 6] = [
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 3 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16, shader_location: 4 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 32, shader_location: 5 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 48, shader_location: 6 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 64, shader_location: 7 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 80, shader_location: 8 },
    ];

    pub const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Light2dInstance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &Self::ATTRS,
    };

    /// Derive the G-buffer instance from the raster instance the main gather
    /// just built.
    ///
    /// **This is the mitigation, in one function.** The deferred pass does not
    /// re-derive a transform or re-decide a tint: it takes them from the very
    /// value handed to the colour pass, so the two cannot place a surface
    /// differently. The only thing added is the sorting rank, which the raster
    /// instance has nowhere to put.
    pub fn from_raster(raw: &crate::raster::InstanceRaw, rank: u32) -> Self {
        Self { model: raw.model, tint: raw.color, meta: [rank as f32, 0.0, 0.0, 0.0] }
    }
}

/// The 2D lights reaching one frame, in the shape the accumulation shader reads.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Light2dUniform {
    /// x = how many lights are live.
    pub count: [f32; 4],
    /// rgb = the flat ambient every 2D surface gets.
    ///
    /// Not optional. A scene whose lights you have not placed yet would
    /// otherwise composite to black, which reads as the feature having broken
    /// the game rather than as "there are no lights here".
    pub ambient: [f32; 4],
    /// Clip → camera-relative world, to put a G-buffer pixel back in the scene.
    pub inv_view_proj: [[f32; 4]; 4],
    /// xyz = camera-relative position, w = range.
    pub pos: [[f32; 4]; 16],
    /// rgb = colour × intensity.
    pub color: [[f32; 4]; 16],
    /// A bitmask over sorting-layer RANK, one `vec4` per light: bit `r` of word
    /// `r / 32` set means this light reaches rank `r`.
    ///
    /// All four words, not just `x`. A uniform array's stride is 16 bytes on
    /// every backend, so the space is paid for whether or not it is used — and a
    /// single word would cover only 32 of the 64 ranks a sorting layer can have
    /// (`SORT_LAYER_STEP` is 1/64), leaving every layer past the 32nd silently
    /// unlit by every light.
    pub mask: [[u32; 4]; 16],
}

impl Default for Light2dUniform {
    fn default() -> Self {
        Self {
            count: [0.0; 4],
            // White, so a scene that turns 2D lighting on without placing a
            // light looks exactly as it did rather than going dark.
            ambient: [1.0, 1.0, 1.0, 0.0],
            inv_view_proj: [[0.0; 4]; 4],
            pos: [[0.0; 4]; 16],
            color: [[0.0; 4]; 16],
            mask: [[0; 4]; 16],
        }
    }
}

/// The G-buffer's albedo format. `Rgba8Unorm` rather than the surface's sRGB
/// format because the value written is a *material* colour that the accumulation
/// multiplies — going through an sRGB encode and decode between the two stages
/// would darken every lit pixel by the gamma curve.
const ALBEDO_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// r = sorting rank / 63, gb = the surface normal (flat until step 3).
const SURFACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

struct Targets {
    width: u32,
    height: u32,
    albedo: wgpu::TextureView,
    surface: wgpu::TextureView,
    depth: wgpu::TextureView,
    /// group(1) for the accumulation: the three above.
    read: wgpu::BindGroup,
}

/// Pipelines and per-frame targets for the 2D lighting pass.
pub struct Light2d {
    pub(crate) fill_pipeline: wgpu::RenderPipeline,
    pub(crate) light_pipeline: wgpu::RenderPipeline,
    fill_buf: wgpu::Buffer,
    pub(crate) fill_bind: wgpu::BindGroup,
    lights_buf: wgpu::Buffer,
    pub(crate) lights_bind: wgpu::BindGroup,
    read_layout: wgpu::BindGroupLayout,
    instances: wgpu::Buffer,
    instance_cap: u32,
    targets: Option<Targets>,
}

fn uniform_layout(device: &wgpu::Device, label: &str, vertex: bool) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: if vertex {
                wgpu::ShaderStages::VERTEX_FRAGMENT
            } else {
                wgpu::ShaderStages::FRAGMENT
            },
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    })
}

impl Light2d {
    /// `tex_layout` is the raster pass's own `{ texture, sampler }` group, reused
    /// verbatim so the fill can bind the very same texture bind groups the main
    /// pass draws with — a second, parallel texture registry is exactly the kind
    /// of thing that goes out of step.
    pub fn new(gpu: &Gpu, tex_layout: &wgpu::BindGroupLayout, color_format: wgpu::TextureFormat) -> Self {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("light2d"),
            source: wgpu::ShaderSource::Wgsl(include_str!("light2d.wgsl").into()),
        });

        let fill_layout = uniform_layout(device, "light2d-fill", true);
        let lights_layout = uniform_layout(device, "light2d-lights", false);
        let read_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("light2d-gbuffer"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let fill_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("light2d-fill"),
            bind_group_layouts: &[Some(&fill_layout), Some(tex_layout)],
            immediate_size: 0,
        });
        let fill_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("light2d-fill"),
            layout: Some(&fill_pl),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_fill"),
                buffers: &[crate::mesh::Vertex::LAYOUT, Light2dInstance::LAYOUT],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_fill"),
                targets: &[
                    Some(wgpu::ColorTargetState {
                        format: ALBEDO_FORMAT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    }),
                    Some(wgpu::ColorTargetState {
                        format: SURFACE_FORMAT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    }),
                ],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: None, // a flat quad may face either way; never cull one out
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: Gpu::DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        let light_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("light2d-accumulate"),
            bind_group_layouts: &[Some(&lights_layout), Some(&read_layout)],
            immediate_size: 0,
        });
        let light_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("light2d-accumulate"),
            layout: Some(&light_pl),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_full"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_light"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    // Alpha-blended so a partly transparent sprite composites over
                    // whatever the main pass drew behind it, rather than replacing
                    // it with a half-black pixel.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::COLOR,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: Gpu::DEPTH_FORMAT,
                // The composite must not WRITE depth: it re-emits the flat
                // surface's own depth only so that anything already in front of
                // it wins. Writing would re-prime depth the main pass has
                // already settled.
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        let fill_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("light2d-fill"),
            size: std::mem::size_of::<[[f32; 4]; 4]>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let lights_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("light2d-lights"),
            size: std::mem::size_of::<Light2dUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let fill_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("light2d-fill"),
            layout: &fill_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: fill_buf.as_entire_binding() }],
        });
        let lights_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("light2d-lights"),
            layout: &lights_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: lights_buf.as_entire_binding() }],
        });
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("light2d-instances"),
            size: (std::mem::size_of::<Light2dInstance>() * 256) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            fill_pipeline,
            light_pipeline,
            fill_buf,
            fill_bind,
            lights_buf,
            lights_bind,
            read_layout,
            instances,
            instance_cap: 256,
            targets: None,
        }
    }

    /// Make sure the G-buffer is at least as big as the frame being drawn.
    /// Returns whether the targets were rebuilt.
    ///
    /// **Grows, never shrinks**, and the fill pass draws into the top-left
    /// `width × height` of it via a viewport. One `Raster` serves several
    /// viewports of different sizes in one frame — the Scene view, a docked Game
    /// view, camera previews, render targets — so sizing exactly to the frame
    /// would tear down and rebuild three textures and a bind group *between*
    /// every pair of them, every frame, for the whole session. The accumulation
    /// reads by integer texel, so a larger buffer costs nothing but the memory.
    fn ensure_targets(&mut self, gpu: &Gpu, width: u32, height: u32) -> bool {
        let (mut w, mut h) = (width.max(1), height.max(1));
        if let Some(t) = self.targets.as_ref() {
            if t.width >= w && t.height >= h {
                return false;
            }
            w = w.max(t.width);
            h = h.max(t.height);
        }
        let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let make = |label: &str, format: wgpu::TextureFormat| {
            gpu.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default())
        };
        let albedo = make("light2d-albedo", ALBEDO_FORMAT);
        let surface = make("light2d-surface", SURFACE_FORMAT);
        let depth = make("light2d-depth", Gpu::DEPTH_FORMAT);
        let read = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("light2d-gbuffer"),
            layout: &self.read_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&albedo) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&surface) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&depth) },
            ],
        });
        self.targets = Some(Targets { width: w, height: h, albedo, surface, depth, read });
        true
    }

    /// Publish this frame's camera and lights, and grow the instance buffer to
    /// fit. Returns the G-buffer views the pass draws into and reads back.
    #[allow(clippy::type_complexity)]
    pub(crate) fn begin(
        &mut self,
        gpu: &Gpu,
        width: u32,
        height: u32,
        view_proj: [[f32; 4]; 4],
        lights: &Light2dUniform,
        instances: &[Light2dInstance],
    ) {
        self.ensure_targets(gpu, width, height);
        gpu.queue.write_buffer(&self.fill_buf, 0, bytemuck::bytes_of(&view_proj));
        gpu.queue.write_buffer(&self.lights_buf, 0, bytemuck::bytes_of(lights));
        let want = instances.len().max(1) as u32;
        if want > self.instance_cap {
            let cap = want.next_power_of_two();
            self.instances = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("light2d-instances"),
                size: (std::mem::size_of::<Light2dInstance>() as u64) * cap as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_cap = cap;
        }
        if !instances.is_empty() {
            gpu.queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(instances));
        }
    }

    pub(crate) fn instance_slice(&self) -> wgpu::BufferSlice<'_> {
        self.instances.slice(..)
    }

    pub(crate) fn views(&self) -> Option<(&wgpu::TextureView, &wgpu::TextureView, &wgpu::TextureView)> {
        self.targets.as_ref().map(|t| (&t.albedo, &t.surface, &t.depth))
    }

    pub(crate) fn read_bind(&self) -> Option<&wgpu::BindGroup> {
        self.targets.as_ref().map(|t| &t.read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The uniform's layout is a contract with the WGSL, and `std140` alignment
    /// is the part nobody notices being wrong until one backend renders black.
    /// Every member starts on a 16-byte boundary and the whole thing is a
    /// multiple of 16.
    #[test]
    fn the_light_uniform_is_std140_shaped() {
        assert_eq!(std::mem::align_of::<Light2dUniform>() % 4, 0);
        assert_eq!(std::mem::size_of::<Light2dUniform>() % 16, 0);
        // count + ambient + a 4x4 matrix + three 16-element vec4 arrays.
        assert_eq!(std::mem::size_of::<Light2dUniform>(), 16 + 16 + 64 + 16 * 16 * 3);
    }

    /// A scene that turns 2D lighting on without placing a light must look
    /// exactly as it did, not go black. White ambient is what guarantees that,
    /// and it is the kind of default that gets "tidied" to zero.
    #[test]
    fn no_lights_means_unchanged_rather_than_dark() {
        let u = Light2dUniform::default();
        assert_eq!(u.count[0], 0.0);
        assert_eq!(u.ambient[..3], [1.0, 1.0, 1.0]);
    }

    /// The instance attributes have to continue where the mesh's own stop, or
    /// the model matrix arrives in the slots holding UVs.
    #[test]
    fn the_instance_attributes_follow_the_meshs_own() {
        let mesh_last =
            crate::mesh::Vertex::ATTRS.iter().map(|a| a.shader_location).max().unwrap();
        let first = Light2dInstance::ATTRS.iter().map(|a| a.shader_location).min().unwrap();
        assert_eq!(first, mesh_last + 1, "the instance stream must start after the vertex one");
        // …and every attribute lands where the struct actually put its field.
        assert_eq!(Light2dInstance::ATTRS[4].offset as usize, std::mem::offset_of!(Light2dInstance, tint));
        assert_eq!(Light2dInstance::ATTRS[5].offset as usize, std::mem::offset_of!(Light2dInstance, meta));
        assert_eq!(Light2dInstance::LAYOUT.array_stride, 96);
    }
}
