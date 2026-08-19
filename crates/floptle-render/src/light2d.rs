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
    /// x = sorting-layer rank, y = 1 when this surface blocks light,
    /// z = the raster pass's packed tiling flags (`InstanceRaw::rim.w`:
    /// `mode + round(rotation_degrees * 10) * 4`). w is spare and is where a
    /// normal-map slot goes when step 3 lands.
    pub meta: [f32; 4],
    /// The same tiling lanes the raster pass gets (`InstanceRaw::tile`):
    /// Uv mode = (count.x, count.y, offset.x, offset.y).
    ///
    /// Here because a sprite on a spritesheet *is* a UV window — one cell of
    /// the sheet across one quad — and a G-buffer that samples the whole sheet
    /// is not drawing what the raster pass drew, which is the one thing this
    /// pass promises (see the header).
    pub tile: [f32; 4],
}

impl Light2dInstance {
    /// Per-instance attributes, continuing the mesh's own pos/normal/uv at 0..2.
    const ATTRS: [wgpu::VertexAttribute; 7] = [
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 3 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16, shader_location: 4 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 32, shader_location: 5 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 48, shader_location: 6 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 64, shader_location: 7 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 80, shader_location: 8 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 96, shader_location: 9 },
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
    /// differently. The only things added are the sorting rank and whether the
    /// surface blocks light, neither of which the raster instance has anywhere
    /// to put.
    ///
    /// "Takes them from the very value handed to the colour pass" has to mean
    /// *everything that pass samples with*, not only the transform and the
    /// tint. The tiling window was left out of it, and a spritesheet is nothing
    /// but a tiling window: the raster pass drew cell 3 and the G-buffer drew
    /// all thirty-two cells squashed across the same quad, so the composite
    /// laid a stretched copy of the whole sheet over the sprite.
    pub fn from_raster(raw: &crate::raster::InstanceRaw, rank: u32, casts: bool) -> Self {
        Self {
            model: raw.model,
            tint: raw.color,
            // rim.w is the raster pass's packed tiling flags, carried across
            // whole rather than unpacked and repacked — the two passes then
            // cannot disagree about what mode this surface is sampled in.
            meta: [rank as f32, if casts { 1.0 } else { 0.0 }, raw.rim[3], 0.0],
            tile: raw.tile,
        }
    }

    /// Whether this surface blocks light — the flag the shadow march reads back
    /// out of the G-buffer's alpha channel.
    pub fn casts(&self) -> bool {
        self.meta[1] > 0.5
    }
}

/// The 2D lights reaching one frame, in the shape the accumulation shader reads.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Light2dUniform {
    /// x = how many lights are live. z = how many steps the shadow march may
    /// take, and `0` switches shadowing off for the whole frame — the gather
    /// sets it only when something in the G-buffer actually casts, so a scene
    /// with no casters pays exactly what it paid before shadows existed
    /// (`floptle/0125`).
    pub count: [f32; 4],
    /// rgb = the flat ambient every 2D surface gets.
    ///
    /// Not optional. A scene whose lights you have not placed yet would
    /// otherwise composite to black, which reads as the feature having broken
    /// the game rather than as "there are no lights here".
    pub ambient: [f32; 4],
    /// Clip → camera-relative world, to put a G-buffer pixel back in the scene.
    pub inv_view_proj: [[f32; 4]; 4],
    /// …and back again, to find where a light *is on screen*. The shadow march
    /// walks the G-buffer in screen space, so it needs the light's pixel and not
    /// only its world position.
    pub view_proj: [[f32; 4]; 4],
    /// xy = the viewport being drawn, in pixels. The G-buffer only ever grows
    /// and one renderer serves several viewport sizes, so its dimensions are the
    /// wrong answer for "how big is this frame" — and the march converts between
    /// UV and texel with it.
    pub viewport: [f32; 4],
    /// xyz = camera-relative position, w = range.
    pub pos: [[f32; 4]; 16],
    /// rgb = colour × intensity.
    pub color: [[f32; 4]; 16],
    /// Per light: `[inner radius, exponent, casts-are-honoured, spare]`.
    ///
    /// `[0, 2, …]` is the curve every light had before `floptle/0126` — a ramp
    /// that starts at the light and falls as `x²` — so the defaults leave every
    /// existing scene where it was.
    pub falloff: [[f32; 4]; 16],
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

impl Light2dUniform {
    /// Which sorting **ranks** anything in this frame can change the look of,
    /// as a 64-bit set — the union of every live light's layer mask, and every
    /// rank at once when the base light is not white (`floptle/0122`).
    ///
    /// This is the filter the gather applies before it builds a single
    /// instance. `Lit2D::Auto` answers *true* for every tilemap and every
    /// sprite batch, so without it the whole flat scene is instanced, bucketed,
    /// uploaded and rasterized a second time each frame — and then discarded on
    /// a bit test in `fs_light`. Reported from a bullet hell paying that for
    /// **366 batches and ~500 sprites a frame against zero lights that could
    /// reach any of them**.
    ///
    /// Two things it must get right, both of which are why it lives here beside
    /// the uniform rather than in the gather:
    ///
    /// * **The base light is not a light.** It has no mask and it reaches
    ///   everything, so a base that has been turned down means every rank — or
    ///   a dimmed room would quietly stop being dim the moment you deleted the
    ///   last torch.
    /// * **A parked light holds no slot** and is already absent from `count`
    ///   (`floptle/0116`), so it cannot put a rank back in the set. A pool of
    ///   spares at `intensity = 0` is the shape that card blessed, and it must
    ///   stay free.
    ///
    /// `0` therefore means the pass has nothing to do at all, which is the
    /// "a scene with 2D lighting available but no light placed does zero 2D
    /// lighting work" property — as a consequence rather than a special case.
    pub fn reach(&self) -> u64 {
        // Not white: the base alone changes every flat surface in the scene.
        if self.ambient[..3] != [1.0, 1.0, 1.0] {
            return u64::MAX;
        }
        let n = (self.count[0].max(0.0) as usize).min(16);
        // Words 0 and 1 only — a sorting rank runs to 63 (`SORT_LAYER_STEP` is
        // 1/64), and words 2 and 3 are the padding a uniform array's 16-byte
        // stride pays for either way.
        self.mask[..n].iter().fold(0u64, |acc, m| acc | m[0] as u64 | ((m[1] as u64) << 32))
    }
}

impl Default for Light2dUniform {
    fn default() -> Self {
        Self {
            count: [0.0; 4],
            // White, so a scene that turns 2D lighting on without placing a
            // light looks exactly as it did rather than going dark.
            ambient: [1.0, 1.0, 1.0, 0.0],
            inv_view_proj: [[0.0; 4]; 4],
            view_proj: [[0.0; 4]; 4],
            viewport: [1.0, 1.0, 0.0, 0.0],
            pos: [[0.0; 4]; 16],
            color: [[0.0; 4]; 16],
            // `[0, 2]` is the ramp every light had before it was authorable, so
            // a caller that fills nothing here gets the old curve rather than a
            // flat disc. `z = 0` leaves shadowing off until a gather asks.
            falloff: [[0.0, 2.0, 0.0, 0.0]; 16],
            mask: [[0; 4]; 16],
        }
    }
}

/// How many samples the shadow march may take along one pixel-to-light segment.
///
/// A ceiling, not a target: the march stops the moment it hits something, and it
/// only runs for pixels actually inside a light's radius. It is fixed rather
/// than authorable because it trades against nothing a game can see — too few
/// and a thin wall leaks light, too many and it costs for no visible gain.
pub const SHADOW_STEPS: f32 = 28.0;

/// The G-buffer's albedo format. Linear rather than the surface's sRGB format
/// because the value written is a *material* colour the accumulation does
/// arithmetic on — going through an sRGB encode and decode between the two
/// stages would darken every lit pixel by the gamma curve.
///
/// **Half-float and not `Rgba8Unorm`**, since `floptle/0121` made the composite a
/// *difference* rather than a redraw. Eight linear bits put a 0.004 floor under
/// every value, which is nothing when you multiply by it and a visible step when
/// you subtract it back out of a dark pixel — linear 8-bit has ~1/255 of its
/// range between "black" and "the darkest thing you can see", and a dark room
/// with a torch in it is the whole point of the feature.
const ALBEDO_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
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
    /// The two halves of the signed correction (`floptle/0121`): `dst - src` for
    /// where a light darkens, `dst + src` for where it brightens. Two pipelines
    /// and not two passes — they share every attachment and bind group, so they
    /// run back to back in one render pass.
    pub(crate) darken_pipeline: wgpu::RenderPipeline,
    pub(crate) brighten_pipeline: wgpu::RenderPipeline,
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
        // The two halves of `floptle/0121`'s signed delta. Identical but for the
        // entry point and the blend OPERATION — both take the source as-is
        // (`One`/`One`), one subtracting it from the frame and one adding it.
        //
        // Not one pipeline with a signed source: a fixed-point colour target
        // clamps the source to `[0, 1]` *before* blending, so a negative delta
        // would arrive as zero and a dark room would never get dark.
        let accumulate = |label: &str, entry: &str, op: wgpu::BlendOperation| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&light_pl),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_full"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: color_format,
                        blend: Some(wgpu::BlendState {
                            color: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::One,
                                dst_factor: wgpu::BlendFactor::One,
                                operation: op,
                            },
                            // Never touched — `write_mask` is COLOR — but a
                            // component is required, so keep the frame's own.
                            alpha: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::Zero,
                                dst_factor: wgpu::BlendFactor::One,
                                operation: wgpu::BlendOperation::Add,
                            },
                        }),
                        write_mask: wgpu::ColorWrites::COLOR,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: Gpu::DEPTH_FORMAT,
                    // The composite must not WRITE depth: it re-emits the flat
                    // surface's own depth only so that anything already in front
                    // of it wins. Writing would re-prime depth the main pass has
                    // already settled.
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let darken_pipeline = accumulate(
            "light2d-darken",
            "fs_darken",
            wgpu::BlendOperation::ReverseSubtract,
        );
        let brighten_pipeline =
            accumulate("light2d-brighten", "fs_brighten", wgpu::BlendOperation::Add);

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
            darken_pipeline,
            brighten_pipeline,
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
        // count + ambient + viewport, two 4x4 matrices, four 16-element vec4
        // arrays (pos, colour, falloff, mask).
        assert_eq!(std::mem::size_of::<Light2dUniform>(), 16 * 3 + 64 * 2 + 16 * 16 * 4);
    }

    /// The defaults are "what a light did before any of this was authorable",
    /// and that is the whole compatibility story for `floptle/0125` and `0126`:
    /// a caller that fills only what it always filled gets the old picture.
    #[test]
    fn an_unfilled_light_keeps_the_curve_it_always_had() {
        let u = Light2dUniform::default();
        assert_eq!(u.falloff[0][0], 0.0, "the ramp starts at the light");
        assert_eq!(u.falloff[0][1], 2.0, "…and falls as x², which is what it always did");
        assert_eq!(u.falloff[0][2], 0.0, "and nothing casts until a gather says so");
        assert_eq!(u.count[2], 0.0, "so the march is off");
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

    /// `floptle/0122`: what the pass can reach decides what is gathered for it,
    /// so an empty reach has to mean *nothing at all*, and a base light that has
    /// been turned down has to mean *everything*.
    #[test]
    fn reach_is_the_union_of_the_masks_and_the_base_is_not_a_light() {
        let u = Light2dUniform::default();
        assert_eq!(u.reach(), 0, "no lights and a white base reaches nothing — and costs nothing");

        // Two lights, two layers. Only the ranks they name are in the set.
        let mut two = Light2dUniform { count: [2.0, 0.0, 0.0, 0.0], ..Default::default() };
        two.mask[0] = [1 << 2, 0, 0, 0];
        two.mask[1] = [1 << 5, 0, 0, 0];
        assert_eq!(two.reach(), (1 << 2) | (1 << 5));
        // …and a rank past the 32nd lives in word 1, which is exactly the half
        // that would go missing if this only read `m[0]`.
        two.mask[1] = [0, 1 << 8, 0, 0];
        assert_eq!(two.reach(), (1 << 2) | (1u64 << 40));

        // A light beyond `count` is a parked spare and holds nothing open
        // (`floptle/0116`) — a pool at intensity 0 must stay free.
        let mut parked = Light2dUniform { count: [1.0, 0.0, 0.0, 0.0], ..Default::default() };
        parked.mask[0] = [1 << 3, 0, 0, 0];
        parked.mask[1] = [u32::MAX, u32::MAX, 0, 0];
        assert_eq!(parked.reach(), 1 << 3, "a parked light put its layers back in the set");

        // A base turned down for a dark room changes every flat surface there
        // is, with or without a light to carve it.
        let dim = Light2dUniform { ambient: [0.4, 0.4, 0.45, 0.0], ..Default::default() };
        assert_eq!(dim.reach(), u64::MAX, "a dimmed room stopped being dim with no lights in it");
    }

    /// A light that names no layers reaches all of them (`Lighting2D::reaches`),
    /// which arrives here as an all-ones mask — so this optimization correctly
    /// does nothing for the ordinary light somebody just dropped into a scene.
    /// Worth its own test so nobody later "fixes" the empty case into zero.
    #[test]
    fn an_unrestricted_light_reaches_everything() {
        let mut u = Light2dUniform { count: [1.0, 0.0, 0.0, 0.0], ..Default::default() };
        u.mask[0] = [u32::MAX, u32::MAX, u32::MAX, u32::MAX];
        assert_eq!(u.reach(), u64::MAX);
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
        assert_eq!(Light2dInstance::ATTRS[6].offset as usize, std::mem::offset_of!(Light2dInstance, tile));
        assert_eq!(Light2dInstance::LAYOUT.array_stride, 112);
    }

    /// **The G-buffer draws the cell the raster pass drew, not the whole sheet.**
    ///
    /// A `Matter::Sprite` on a spritesheet is drawn through a UV window — the
    /// material's tiling lanes are how one cell of a sheet becomes one quad.
    /// `from_raster` took the model and the tint and left the window behind, so
    /// the deferred pass sampled the WHOLE image across the quad and the delta
    /// composite laid a squashed copy of every frame of the animation over the
    /// sprite. The raster pass had the cell right the entire time, which is what
    /// made it read as a glitch rather than as a wrong frame.
    ///
    /// The header states the invariant this broke: `C` and `a` in the G-buffer
    /// must be exactly what the raster pass drew, or the difference the
    /// composite subtracts is the difference between two different pictures. A
    /// UV window is part of `C`.
    #[test]
    fn the_g_buffer_samples_the_cell_the_raster_pass_drew() {
        // One cell of a 16x2 sheet: a sixteenth across, a half down, scrolled to
        // cell 3 — the shape `MaterialParams::from_material_inset` hands over.
        let mp = crate::MaterialParams {
            tile_mode: 1,
            tile: [1.0 / 16.0, 0.5, 3.0 / 16.0, 0.0],
            ..crate::MaterialParams::flat([1.0, 1.0, 1.0])
        };
        let raw = crate::instance_of_mat(glam::Mat4::IDENTITY, &mp);
        let g = Light2dInstance::from_raster(&raw, 0, true);
        assert_eq!(g.tile, raw.tile, "the G-buffer sampled the whole sheet instead of the cell");
        assert_eq!(
            g.meta[2], raw.rim[3],
            "…and could not have windowed it anyway: the tiling MODE never arrived"
        );
    }
}
