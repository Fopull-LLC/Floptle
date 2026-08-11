//! The scene's environment map: the sky, captured into an equirectangular
//! texture with a mip chain, so every surface has something to reflect.
//!
//! **Why this exists.** The metal/rough shading model computes a perfectly good
//! specular lobe and, until now, had nothing to put in it but the sun and the
//! placed point lights. A metal at roughness 0 came out as a sun dot on black,
//! which is why mirrors and crystal balls could not be made: the missing piece
//! was never a material setting, it was that the renderer had no notion of an
//! environment at all.
//!
//! **Why a captured texture rather than calling the sky directly.** The sky has
//! three sources — a solid vault, an equirectangular skybox image, and a `stage
//! sky` shader spliced into the raymarch module — and only the raymarch pass can
//! evaluate any of them. Capturing once per frame gets all three at once, hands
//! the result to every other pass through the shared field bind group, and
//! produces the roughness mip chain a reflection needs anyway. Evaluating a
//! procedural sky per shaded pixel would do none of those things and cost far
//! more.
//!
//! **Equirectangular, not a cube.** The engine's own skybox images are already
//! equirectangular and `sky_color` already speaks that mapping, so the capture
//! and the lookup share one formula with nothing to keep in sync. `u` wraps, so
//! the seam at the back costs nothing; the poles stretch, which no reflection
//! has ever been troubled by.
//!
//! **The mip chain is a box filter, not a GGX prefilter.** Each level is a plain
//! 2×2 average of the one above, and roughness picks a level. That is an
//! approximation — a true prefilter would integrate the GGX lobe per level — and
//! it is a good one for skies, which are overwhelmingly low-frequency. What it
//! gets exactly right is the case that matters most here: at roughness 0 a
//! mirror samples level 0, which IS the sky.

/// Width of the captured sky. Height is half (equirectangular), and the chain
/// runs down to 1×1 — nine levels, which is enough that the roughest surface
/// reflects a single averaged sky colour.
pub const ENV_W: u32 = 256;
pub const ENV_H: u32 = ENV_W / 2;

/// The captured sky and its roughness chain.
pub struct EnvMap {
    tex: wgpu::Texture,
    /// All mips — what shading samples, with an explicit level per roughness.
    view: wgpu::TextureView,
    /// One view per level, as a render target for the downsample chain.
    levels: Vec<wgpu::TextureView>,
    sampler: wgpu::Sampler,
    down_pipeline: wgpu::RenderPipeline,
    down_layout: wgpu::BindGroupLayout,
    /// `src` bind for each downsample step (level i → level i+1).
    down_binds: Vec<wgpu::BindGroup>,
    mips: u32,
}

/// Shared with [`crate::ssr`], which builds the same box chain over the scene
/// colour history. One downsample is one downsample; the two differ only in
/// format and size, which are pipeline state rather than shader code.
pub(crate) const DOWN_WGSL: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VO {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VO {
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let xy = p[vi];
    var o: VO;
    o.clip = vec4<f32>(xy, 0.0, 1.0);
    o.uv = vec2<f32>(xy.x * 0.5 + 0.5, 0.5 - xy.y * 0.5);
    return o;
}

// One linear tap at the destination texel's centre IS the 2x2 average of the
// source block it covers, which is the whole of a box mip chain.
@fragment
fn fs(in: VO) -> @location(0) vec4<f32> {
    return textureSampleLevel(src, samp, in.uv, 0.0);
}
"#;

/// The HDR format the sky is captured in. A sky is genuinely brighter than 1 —
/// a sun, a bloom-worthy horizon — and clipping it here would make every
/// reflection of it flat.
pub const ENV_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

impl EnvMap {
    pub fn new(device: &wgpu::Device) -> Self {
        let mips = ENV_W.ilog2() + 1;
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("env-map"),
            size: wgpu::Extent3d { width: ENV_W, height: ENV_H, depth_or_array_layers: 1 },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: ENV_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let levels: Vec<wgpu::TextureView> = (0..mips)
            .map(|m| {
                tex.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("env-level"),
                    base_mip_level: m,
                    mip_level_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        // `u` REPEATS: the equirect seam is the back of the scene, and a clamp
        // there would smear the last column across it in every rough reflection.
        // `v` clamps — there is nothing above the pole to wrap to.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("env-samp"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let down_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("env-down"),
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
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("env-down"),
            source: wgpu::ShaderSource::Wgsl(DOWN_WGSL.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("env-down"),
            bind_group_layouts: &[Some(&down_layout)],
            immediate_size: 0,
        });
        let down_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("env-down"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: ENV_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        // Level i is the SOURCE for level i+1, so there is one bind per step.
        let down_binds = (0..mips.saturating_sub(1))
            .map(|m| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("env-down"),
                    layout: &down_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&levels[m as usize]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&sampler),
                        },
                    ],
                })
            })
            .collect();

        Self { tex, view, levels, sampler, down_pipeline, down_layout, down_binds, mips }
    }

    /// The whole chain, for sampling.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub fn sampler(&self) -> &wgpu::Sampler {
        &self.sampler
    }

    /// Level 0 — where the sky is captured before the chain is built.
    pub fn top(&self) -> &wgpu::TextureView {
        &self.levels[0]
    }

    pub fn mips(&self) -> u32 {
        self.mips
    }

    /// Fill levels 1.. from level 0. Run straight after the capture; each step
    /// reads the level the step before it wrote.
    pub fn build_chain(&self, encoder: &mut wgpu::CommandEncoder) {
        for (i, bind) in self.down_binds.iter().enumerate() {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("env-down"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.levels[i + 1],
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
            rp.set_pipeline(&self.down_pipeline);
            rp.set_bind_group(0, bind, &[]);
            rp.draw(0..3, 0..1);
        }
    }

    /// A 1×1 stand-in for a renderer with no environment yet — same role as the
    /// depth prepass's 1×1 fallback. Shading reads the DIMENSIONS to decide
    /// whether there is a sky to reflect, so there is no flag to keep in step.
    pub fn empty(device: &wgpu::Device) -> (wgpu::TextureView, wgpu::Sampler) {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("env-empty"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: ENV_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("env-empty-samp"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        (tex.create_view(&wgpu::TextureViewDescriptor::default()), sampler)
    }

    /// Unused today, kept so the texture is not dropped while views live.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.tex
    }

    /// Unused today; the layout is handed back so a future pass (terrain, a
    /// compute prefilter) can build its own binds against the same chain.
    pub fn down_layout(&self) -> &wgpu::BindGroupLayout {
        &self.down_layout
    }
}
