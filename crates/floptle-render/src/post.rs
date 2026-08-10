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

/// How the scene's unbounded linear light is mapped onto a display that stops
/// at white. See `tonemap` in post.wgsl for what each one does and why there is
/// a choice at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tonemap {
    /// Clamp each channel. What the engine did before there was a choice, and
    /// still right for flat 2D and pixel art — every colour there was authored
    /// inside 0..1, and a curve over it is a filter nobody asked for.
    #[default]
    Clip,
    /// `c / (1 + c)`. Never clips, never editorialises; everything bright washes
    /// toward grey.
    Reinhard,
    /// The filmic curve: crushed toe, long shoulder, warm highlights. Contrasty,
    /// and it has opinions about strong colours.
    Aces,
    /// Desaturates toward white the way film does, so a saturated light gets
    /// brighter instead of hitting a flat ceiling of its own hue. The kinder
    /// answer for a scene lit by coloured lights.
    Agx,
}

impl Tonemap {
    /// The lane value `fs_finish` reads (`p.g.w`).
    pub fn lane(self) -> f32 {
        match self {
            Tonemap::Clip => 0.0,
            Tonemap::Reinhard => 1.0,
            Tonemap::Aces => 2.0,
            Tonemap::Agx => 3.0,
        }
    }
    /// The spelling used in scene files and in Lua.
    pub fn as_str(self) -> &'static str {
        match self {
            Tonemap::Clip => "clip",
            Tonemap::Reinhard => "reinhard",
            Tonemap::Aces => "aces",
            Tonemap::Agx => "agx",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "clip" | "none" | "off" => Some(Tonemap::Clip),
            "reinhard" => Some(Tonemap::Reinhard),
            "aces" | "filmic" => Some(Tonemap::Aces),
            "agx" => Some(Tonemap::Agx),
            _ => None,
        }
    }
    pub const ALL: [Tonemap; 4] = [Tonemap::Clip, Tonemap::Reinhard, Tonemap::Aces, Tonemap::Agx];
}

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
    /// Posterize: quantize the **palette** to this many levels per channel (a
    /// limited-palette / banded look). 0 or 1 = off; 2.. = enabled.
    ///
    /// Not part of this chain. It runs as its own pass over the art, at the
    /// composited (retro) resolution and before the 2D light composite — see
    /// [`crate::palette`] and [`Self::palette`]. Everything in the chain below is
    /// downstream of it and is deliberately left smooth.
    pub posterize_bands: u32,
    /// Ordered-dither the posterize quantization so a smooth ramp in the *art*
    /// stipples rather than hard-stepping. It has no bearing on lighting.
    pub posterize_dither: bool,
    /// Quantize brightness and keep the chroma, rather than each channel on its
    /// own — so a warm tint steps in brightness instead of stepping through hues
    /// nobody chose (`floptle/0126`). Off = the per-channel look.
    pub posterize_chroma: bool,
    /// Colour-vision filter: 0 = off, 1 = protanopia, 2 = deuteranopia,
    /// 3 = tritanopia (`floptle_core::access::ColorFilter::lane`). Runs in the
    /// terminal pass, before the scene's own looks (`floptle/0079`).
    pub color_filter: u32,
    /// How strongly the filter applies, 0..1.
    pub color_filter_strength: f32,
    /// Show the deficiency instead of correcting it — a developer's check of
    /// what a colourblind player sees.
    pub simulate_deficiency: bool,

    // ---- the look chain (`floptle/0130`) ---------------------------------
    //
    // Each block below is one pass, and each is SKIPPED when its settings are
    // the identity — see the `*_on()` predicates. A project that uses none of
    // this renders exactly the frames it rendered before, with no extra passes
    // and no extra targets.

    /// How the scene's linear light lands on the display.
    pub tonemap: Tonemap,
    /// Colour grade: exposure in stops (0 = unchanged).
    pub exposure: f32,
    /// Contrast about 18% grey. 1 = unchanged.
    pub contrast: f32,
    /// Saturation against Rec.709 luma. 1 = unchanged, 0 = greyscale.
    pub saturation: f32,
    /// White balance along blue↔amber. 0 = unchanged.
    pub temperature: f32,
    /// White balance along green↔magenta — the axis `temperature` cannot reach.
    pub tint: f32,
    /// Lift: raise the black floor. 0 = unchanged.
    pub lift: f32,
    /// Midtone gamma. 1 = unchanged.
    pub grade_gamma: f32,
    /// Gain: scale the highlights. 1 = unchanged.
    pub gain: f32,

    /// Chromatic aberration: per-channel radial offset. 0 = off.
    pub aberration: f32,
    /// Lens distortion: positive barrels, negative pincushions. 0 = off.
    pub distortion: f32,

    /// Unsharp amount. 0 = off.
    pub sharpen: f32,
    /// Bilateral denoise amount, 0..1. 0 = off.
    pub denoise: f32,

    /// Film grain amount. 0 = off.
    pub grain: f32,
    /// Grain cell size in pixels — 1 is per-pixel, 2+ clumps it.
    pub grain_size: f32,
    /// Seconds, for anything that moves. Grain uses it; a frozen grain pattern
    /// reads as a dirty lens rather than as film.
    pub time: f32,

    /// Depth of field: distance from the camera, in world units, that is sharp.
    /// 0 = off (there is no meaningful "focus at the camera").
    pub dof_focus: f32,
    /// How far BEYOND `dof_focus` stays acceptably sharp.
    pub dof_range: f32,
    /// How far IN FRONT of it does. 0 = half of `dof_range` — the old single-range
    /// behaviour, and roughly what a lens does.
    pub dof_near_range: f32,
    /// The widest the blur gets, in pixels.
    pub dof_max_blur: f32,
    /// Aperture blades: 0/1/2 = a round iris, 3+ = a polygonal one.
    pub dof_blades: u32,
    /// Blade polygon rotation, in RADIANS (the Inspector shows degrees).
    pub dof_blade_rotation: f32,
    /// How much brighter-than-white pixels dominate the blur — the difference
    /// between bokeh and grey mush. 0 = off.
    pub dof_highlight: f32,
    /// Taps in the kernel. 0 = the default 16; clamped to 4..64.
    pub dof_quality: u32,
    /// Tint the frame by what is in focus, for tuning.
    pub dof_show_focus: bool,

    // ---- motion blur (v0.49) ---------------------------------------------
    /// Shutter: how much of the frame's motion is smeared, as a fraction of the
    /// step between the two frames. 0 = off. 1 is a shutter open for the whole
    /// frame; 0.5 is the 180° shutter a film camera has and is the value that
    /// looks like footage rather than like a smear.
    pub motion_blur: f32,
    /// Taps along the streak. 0 = the default 12; clamped to 4..32.
    pub motion_samples: u32,
    /// The longest streak, in pixels — the ceiling that keeps a violent camera
    /// whip from costing a full-screen smear.
    pub motion_max: f32,
    /// Clip → **camera-relative world** for the frame being drawn. Identity
    /// means "no camera information", which reads as no motion.
    pub motion_inv_view_proj: [[f32; 4]; 4],
    /// Camera-relative world (THIS frame's origin) → the PREVIOUS frame's clip.
    ///
    /// Both this and `motion_inv_view_proj` are per-frame camera facts rather
    /// than artist settings, and they live here for the same reason `time`
    /// does: the alternative is a second per-frame struct threaded through
    /// every call site that has no opinion about it.
    ///
    /// Equal to the current view-projection = a still camera = no blur, which
    /// is also exactly what the first frame after a load should do.
    pub motion_prev_view_proj: [[f32; 4]; 4],
}

/// Column-major identity, for the two motion matrices' defaults.
const IDENTITY4: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// The identity: nothing on, and every multiplicative knob at the value that
/// leaves the picture alone.
///
/// Hand-written rather than derived because half of these are 1.0, not 0.0 —
/// a derived `Default` would give a black, contrastless, greyscale frame, and
/// that is the sort of default that gets shipped once and reported as a
/// rendering bug.
impl Default for PostSettings {
    fn default() -> Self {
        Self {
            bloom: false,
            bloom_threshold: 1.0,
            bloom_intensity: 0.7,
            vignette: false,
            vignette_strength: 0.5,
            vignette_radius: 0.7,
            ssao: false,
            ssao_strength: 0.7,
            ssao_radius: 0.5,
            posterize_bands: 0,
            posterize_dither: false,
            posterize_chroma: false,
            color_filter: 0,
            color_filter_strength: 1.0,
            simulate_deficiency: false,
            tonemap: Tonemap::Clip,
            exposure: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            temperature: 0.0,
            tint: 0.0,
            lift: 0.0,
            grade_gamma: 1.0,
            gain: 1.0,
            aberration: 0.0,
            distortion: 0.0,
            sharpen: 0.0,
            denoise: 0.0,
            grain: 0.0,
            grain_size: 1.0,
            time: 0.0,
            dof_focus: 0.0,
            dof_range: 5.0,
            dof_near_range: 0.0,
            dof_max_blur: 0.0,
            dof_blades: 0,
            dof_blade_rotation: 0.0,
            dof_highlight: 0.0,
            dof_quality: 0,
            dof_show_focus: false,
            motion_blur: 0.0,
            motion_samples: 0,
            motion_max: 32.0,
            motion_inv_view_proj: IDENTITY4,
            motion_prev_view_proj: IDENTITY4,
        }
    }
}

impl PostSettings {
    /// True if any effect is enabled (else the stack is a no-op passthrough).
    ///
    /// Posterize counts even though the chain no longer applies it, and that is
    /// load-bearing: it is what makes the caller render the scene into a post
    /// target instead of straight at the swapchain, and the palette pass has to
    /// be able to READ the frame it quantizes. A swapchain texture cannot be
    /// sampled.
    pub fn any(&self) -> bool {
        self.bloom
            || self.vignette
            || self.ssao
            || self.posterize_bands >= 2
            || self.color_filter_on()
            || self.grade_on()
            || self.lens_on()
            || self.sharpen > 0.0
            || self.denoise > 0.0
            || self.grain > 0.0
            || self.dof_on()
            || self.motion_on()
    }

    /// Is motion blur asked for? The matrices alone never enable it — a scene
    /// that does not want the effect must not pay for a full-screen pass just
    /// because the camera moved.
    pub fn motion_on(&self) -> bool {
        self.motion_blur > 0.0 && self.motion_max > 0.0
    }

    /// Is the colour grade doing anything?
    ///
    /// Asked in ONE place so a caller cannot half-remember which of eight knobs
    /// has which identity value — and it matters, because a grade pass that
    /// runs at identity is not free: it is a full-screen read and write, and on
    /// a retro target it is also a round trip through a scratch texture.
    pub fn grade_on(&self) -> bool {
        self.exposure != 0.0
            || self.contrast != 1.0
            || self.saturation != 1.0
            || self.temperature != 0.0
            || self.tint != 0.0
            || self.lift != 0.0
            || self.grade_gamma != 1.0
            || self.gain != 1.0
    }

    /// Is either lens effect doing anything? They share a pass because they are
    /// the same lens — see `fs_lens`.
    pub fn lens_on(&self) -> bool {
        self.aberration != 0.0 || self.distortion != 0.0
    }

    /// Is depth of field doing anything? Needs a focus distance AND a blur to
    /// reach; either at zero is the identity.
    pub fn dof_on(&self) -> bool {
        self.dof_focus > 0.0 && self.dof_max_blur > 0.0
    }

    /// Is the colour-vision filter doing anything? (Mode 0 or zero strength is
    /// the identity, and the chain must not pay for a pass that changes nothing.)
    pub fn color_filter_on(&self) -> bool {
        self.color_filter > 0 && self.color_filter_strength > 0.0
    }

    /// The palette quantize this scene asks for, or `None` when posterize is off.
    ///
    /// One place answers "is posterize on", so a caller cannot half-remember the
    /// `bands >= 2` rule — and the type it hands back is the only way to ask
    /// [`crate::Palette::quantize`] to do anything, so an off setting cannot
    /// reach the pass at all.
    pub fn palette(&self) -> Option<crate::palette::PaletteQuantize> {
        (self.posterize_bands >= 2).then_some(crate::palette::PaletteQuantize {
            bands: self.posterize_bands,
            dither: self.posterize_dither,
            chroma: self.posterize_chroma,
        })
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
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct PostParams {
    /// xy = texel (1/src size), z = bloom_threshold, w = bloom_intensity.
    a: [f32; 4],
    /// x = vignette_strength, y = vignette_radius, zw = blur_dir (texels).
    b: [f32; 4],
    /// grade: x exposure, y contrast, z saturation, w temperature.
    c: [f32; 4],
    /// grade: x tint, y lift, z gamma, w gain.
    d: [f32; 4],
    /// lens: x aberration, y distortion, z grain, w time.
    e: [f32; 4],
    /// x sharpen, y denoise, z dof focus, w dof range.
    f: [f32; 4],
    /// x dof max blur (texels), y aspect, z grain size, w unused.
    g: [f32; 4],
}

/// Uniform for the depth-reading passes — matches `DofCam` in `post.wgsl`.
///
/// Depth of field uses `inv_proj`; motion blur uses the other three. Shared so
/// the two passes cannot disagree about the layout they both bind.
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct DofCam {
    inv_proj: [[f32; 4]; 4],
    inv_view_proj: [[f32; 4]; 4],
    prev_view_proj: [[f32; 4]; 4],
    /// x = shutter, y = taps, z = max streak (px), w unused.
    motion: [f32; 4],
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

/// The three bind-group layouts a post pass uses, built in ONE place.
///
/// One builder rather than a descriptor per call site because a bind group is
/// only usable with a *structurally equal* layout: the moment a hand-copied
/// second version gains an entry the first one hasn't, every draw against it is
/// a validation error, at draw time, in whichever project happens to use that
/// pass. Both [`PostStack`] and [`PostShaders`] build from these — and so does
/// `post_prelude.wgsl`, by hand, which is why the comments there name the same
/// three groups in the same order.
pub(crate) mod layouts {
    /// group(0): the frame so far + the chain's params. Every built-in pass and
    /// every authored one shares it, which is what lets a custom pass ping-pong
    /// between the chain's own scratch targets with no bind group of its own.
    pub fn chain(device: &wgpu::Device) -> wgpu::BindGroupLayout {
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
        })
    }

    /// group(1): the depth buffer the scene was rendered with, and the inverse
    /// projection that turns a depth back into a position. Depth32Float is not
    /// filterable, so there is no sampler — the shader reads with textureLoad.
    pub fn depth(device: &wgpu::Device, label: &str) -> wgpu::BindGroupLayout {
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(label),
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
        })
    }

    /// group(2): one authored shader's own uniforms.
    pub fn shader_params(device: &wgpu::Device) -> wgpu::BindGroupLayout {
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post-flsl-params"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        })
    }
}

/// A registered `stage post` shader.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PostShaderId(usize);

struct CustomPost {
    pipeline: wgpu::RenderPipeline,
}

/// One entry in the scene's ordered pass list: which shader, and the knob values
/// for THIS occurrence of it.
///
/// Per-occurrence and not per-shader, because listing the same shader twice with
/// different settings is a real thing to want — two outline passes at different
/// widths make a double line — and a single buffer per shader would silently
/// give both of them whichever values were written last.
struct PassSlot {
    shader: usize,
    params: wgpu::Buffer,
    bind: wgpu::BindGroup,
    size: u64,
}

/// Every authored full-screen pass a project has compiled, and the pipelines
/// behind them.
///
/// Deliberately NOT owned by [`PostStack`]: a running editor holds several
/// chains (the surface, the docked Game view, each Inspector preview) and a
/// scene's screen shaders belong to the scene, not to one of its viewports. One
/// registry, and any chain can run any of its passes.
pub struct PostShaders {
    // Groups 0 and 1 are bound by the CHAIN (its scratch target, its depth), so
    // nothing here ever builds a bind group against these two — they exist to
    // shape the pipeline layout, and they are built from the same `layouts::`
    // functions the chain uses so the two agree by construction.
    _chain_layout: wgpu::BindGroupLayout,
    _depth_layout: wgpu::BindGroupLayout,
    params_layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    format: wgpu::TextureFormat,
    shaders: Vec<CustomPost>,
    slots: Vec<PassSlot>,
}

impl PostShaders {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;
        let chain_layout = layouts::chain(device);
        let depth_layout = layouts::depth(device, "post-flsl-depth");
        let params_layout = layouts::shader_params(device);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("post-flsl"),
            bind_group_layouts: &[
                Some(&chain_layout),
                Some(&depth_layout),
                Some(&params_layout),
            ],
            immediate_size: 0,
        });
        Self {
            _chain_layout: chain_layout,
            _depth_layout: depth_layout,
            params_layout,
            pipeline_layout,
            format: gpu.scene_format(),
            shaders: Vec::new(),
            slots: Vec::new(),
        }
    }

    /// Build one authored pass's pipeline. `module_src` is the COMPLETE WGSL —
    /// `floptle_shader::transpile::POST_PRELUDE` + the field shim + the stdlib
    /// support + the transpiled chunk — assembled by the caller, because this
    /// crate does not depend on the shader language (the editor owns that seam,
    /// exactly as it does for raster and UI shaders).
    ///
    /// The module must declare `vs` and `fs_flsl_post` and must already have
    /// passed naga: the caller validates against the same text, and a pipeline
    /// build that fails here is a panic inside wgpu rather than a message.
    ///
    /// `replace` rebuilds a shader in place, so a hot reload keeps its id and
    /// every scene reference to it stays valid.
    pub fn register(
        &mut self,
        gpu: &Gpu,
        module_src: &str,
        replace: Option<PostShaderId>,
    ) -> PostShaderId {
        let module = gpu.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("post-flsl"),
            source: wgpu::ShaderSource::Wgsl(module_src.into()),
        });
        let pipeline = gpu.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("post-flsl"),
            layout: Some(&self.pipeline_layout),
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
                entry_point: Some("fs_flsl_post"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: self.format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let entry = CustomPost { pipeline };
        match replace {
            Some(id) if id.0 < self.shaders.len() => {
                self.shaders[id.0] = entry;
                id
            }
            _ => {
                self.shaders.push(entry);
                PostShaderId(self.shaders.len() - 1)
            }
        }
    }

    /// Set the ordered pass list every chain will run this frame: which shaders,
    /// in which order, with which knob values.
    ///
    /// Idempotent and cheap when nothing changed — a slot whose buffer is
    /// already the right size takes a uniform write and nothing else, so holding
    /// a knob and dragging it costs one `write_buffer` per frame.
    pub fn set_passes(&mut self, gpu: &Gpu, passes: &[(PostShaderId, Vec<u8>)]) {
        for (i, (id, bytes)) in passes.iter().enumerate() {
            let size = (bytes.len() as u64).max(16);
            let fits = self.slots.get(i).is_some_and(|s| s.size == size);
            if !fits {
                let params = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("post-flsl-params"),
                    size,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("post-flsl-params"),
                    layout: &self.params_layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: params.as_entire_binding(),
                    }],
                });
                let slot = PassSlot { shader: id.0, params, bind, size };
                if i < self.slots.len() {
                    self.slots[i] = slot;
                } else {
                    self.slots.push(slot);
                }
            } else {
                self.slots[i].shader = id.0;
            }
            gpu.queue.write_buffer(&self.slots[i].params, 0, bytes);
        }
        self.slots.truncate(passes.len());
    }

    /// Does this frame have any authored pass to run?
    pub fn has_passes(&self) -> bool {
        !self.slots.is_empty()
    }

    /// Forget every compiled pass and every slot (a project switch).
    pub fn clear(&mut self) {
        self.shaders.clear();
        self.slots.clear();
    }
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
    grade_pipeline: wgpu::RenderPipeline,
    lens_pipeline: wgpu::RenderPipeline,
    sharpen_pipeline: wgpu::RenderPipeline,
    denoise_pipeline: wgpu::RenderPipeline,
    bright_pipeline: wgpu::RenderPipeline,
    blur_pipeline: wgpu::RenderPipeline,
    composite_pipeline: wgpu::RenderPipeline, // additive blend
    finish_pipeline: wgpu::RenderPipeline,     // terminal colour filter + vignette
    ssao_pipeline: wgpu::RenderPipeline,       // ssao.wgsl → half-res R8
    ao_blur_pipeline: wgpu::RenderPipeline,    // fs_blur onto the R8 targets
    ssao_apply_pipeline: wgpu::RenderPipeline, // scene × AO
    dof_pipeline: wgpu::RenderPipeline,        // CoC-weighted gather
    motion_pipeline: wgpu::RenderPipeline,     // depth-reprojected streak
    dof_layout: wgpu::BindGroupLayout,
    dof_cam_buf: wgpu::Buffer,
    /// A 1×1 depth texture left at FAR — what an authored post pass reads when
    /// the frame came with no depth. Allocated once, because the alternative is
    /// binding nothing, and a bind group with a missing entry is not a quiet
    /// no-op: it is a validation error at the moment of drawing.
    _far_depth: wgpu::Texture,
    far_depth_view: wgpu::TextureView,
}

impl PostStack {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Self {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("post"),
            source: wgpu::ShaderSource::Wgsl(include_str!("post.wgsl").into()),
        });

        // One layout for every pass: { src texture, sampler, params uniform }.
        // Shared with `PostShaders` through `layouts::chain` so an authored pass
        // and a built-in one bind identically.
        let bind_layout = layouts::chain(device);
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
        // Group 1 for depth of field: the depth buffer plus the inverse
        // projection it was rendered with. Its own layout rather than reusing
        // the SSAO one because SSAO's uniform is a different (much larger)
        // struct, and a bind group whose buffer is the wrong shape is a
        // validation error at the moment of drawing rather than at build time.
        // Depth of field's second group — the SAME shape an authored post pass
        // reads its depth through (`layouts::depth`), so the two cannot drift.
        let dof_layout = layouts::depth(device, "post-dof");
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

        // TWO formats, and the split is the whole point of the chain.
        //
        // `chain` is the scene format — floating point when a window is driving
        // it — and every scratch target and every intermediate pass lives there,
        // in linear light at whatever intensity the scene actually has. `out` is
        // the display: 8-bit sRGB, nothing brighter than white.
        //
        // Exactly one pass crosses between them, the terminal `fs_finish`, which
        // is where the tonemap lives. That is what makes an exposure mean
        // something and what lets bloom tell a lit wall from a light bulb.
        let chain = gpu.scene_format();
        let out_fmt = gpu.surface_format();
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
                        format: chain,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let copy_pipeline = make_pipeline("fs_copy", None);
        // The look chain. All five read one texture and write one, so they share
        // the copy pipeline's layout exactly; only the entry point differs.
        let grade_pipeline = make_pipeline("fs_grade", None);
        let lens_pipeline = make_pipeline("fs_lens", None);
        let sharpen_pipeline = make_pipeline("fs_sharpen", None);
        let denoise_pipeline = make_pipeline("fs_denoise", None);
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
        // The ONE pass that writes the display format — the tonemap's home, and
        // therefore always the last thing that runs (see `run`).
        let finish_pipeline = {
            let layout_only = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("post-finish"),
                bind_group_layouts: &[Some(&bind_layout)],
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("post-finish"),
                layout: Some(&layout_only),
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
                    entry_point: Some("fs_finish"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: out_fmt,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };

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
        let ssao_apply_pipeline = generic_pipeline(&module, &apply_pl_layout, "fs_ssao_apply", chain);
        let dof_pl_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("post-dof-layout"),
            bind_group_layouts: &[Some(&bind_layout), Some(&dof_layout)],
            ..Default::default()
        });
        let dof_pipeline = generic_pipeline(&module, &dof_pl_layout, "fs_dof", chain);
        // Motion blur shares depth-of-field's bind-group layout and its camera
        // buffer: both passes are "the frame's own geometry", and one struct
        // cannot drift from the other by a field.
        let motion_pipeline = generic_pipeline(&module, &dof_pl_layout, "fs_motion", chain);
        let dof_cam_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("post-dof-cam"),
            size: std::mem::size_of::<DofCam>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // Never rendered into, so it keeps its cleared value of 1.0 — far, i.e.
        // "sky" to everything that reads depth.
        let far_depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("post-far-depth"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let far_depth_view = far_depth.create_view(&wgpu::TextureViewDescriptor::default());

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
            scene: mk(width, height, chain),
            ping: mk(width, height, chain),
            pong: mk(width, height, chain),
            bloom_a: mk(hw, hh, chain),
            bloom_b: mk(hw, hh, chain),
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
            grade_pipeline,
            lens_pipeline,
            sharpen_pipeline,
            denoise_pipeline,
            bright_pipeline,
            blur_pipeline,
            composite_pipeline,
            finish_pipeline,
            ssao_pipeline,
            ao_blur_pipeline,
            ssao_apply_pipeline,
            dof_pipeline,
            motion_pipeline,
            dof_layout,
            dof_cam_buf,
            _far_depth: far_depth,
            far_depth_view,
        }
    }

    pub fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        let fmt = gpu.scene_format();
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
        self.run_with(gpu, s, ssao, out, None);
    }

    /// [`run`](Self::run), plus the scene's authored `stage post` passes.
    ///
    /// They run after depth of field and the denoise and BEFORE the colour
    /// grade, and every part of that is a decision:
    ///
    /// - *after depth of field*, because focus is a property of the scene and an
    ///   authored pass should see the picture the camera actually took.
    /// - *after the denoise*, because the denoise wants raw sampling noise and
    ///   an authored pass is not it.
    /// - *before the grade, the lens and the grain*, because whatever a pass
    ///   draws is ART: an ink outline should be graded and vignetted like
    ///   everything else in the frame, not stencilled on top of the finished
    ///   picture. It also keeps the pass upstream of the lens distortion, so the
    ///   depth it reads still lines up with the pixels it is reading.
    pub fn run_with<'a>(
        &'a self,
        gpu: &Gpu,
        s: &PostSettings,
        ssao: Option<&SsaoFrame>,
        out: &wgpu::TextureView,
        custom: Option<&'a PostShaders>,
    ) {
        let custom = custom.filter(|c| c.has_passes());
        let ssao_on = s.ssao && ssao.is_some();
        let filter_on = s.color_filter_on();
        // Posterize is deliberately absent: it ran before the 2D light composite,
        // upstream of everything here (`floptle/0127`). A scene whose only post
        // setting is posterize therefore takes the passthrough below — the frame
        // it hands us is already quantized.
        let grade_on = s.grade_on();
        let lens_on = s.lens_on();
        let sharpen_on = s.sharpen > 0.0;
        let denoise_on = s.denoise > 0.0;
        let grain_on = s.grain > 0.0;
        // The chain always ENDS in `fs_finish`, even with every effect off.
        //
        // Not for tidiness: `finish` is the only pass that writes the display
        // format, and the only one that runs the tonemap. A "nothing is on, just
        // copy it" shortcut would hand an sRGB surface a floating-point image
        // and skip the one step that knows how to land it — so the fast path is
        // a `finish` with identity parameters, not a different pass.
        if !(ssao_on
            || s.bloom
            || s.vignette
            || filter_on
            || grade_on
            || lens_on
            || sharpen_on
            || denoise_on
            || grain_on
            || custom.is_some()
            || (s.dof_on() && ssao.is_some())
            || (s.motion_on() && ssao.is_some()))
        {
            self.finish(gpu, s, &self.scene, out, [0.0, 0.0], 1.0);
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
            self.write_params(gpu, PostParams { a: [atexel[0], atexel[1], 0.0, 0.0], b: [0.0, 0.0, astep, 0.0], ..Default::default() });
            self.pass(gpu, &self.ao_blur_pipeline, &self.ao_a.bind, &self.ao_b.view, wgpu::LoadOp::Clear(BLACK));
            self.write_params(gpu, PostParams { a: [atexel[0], atexel[1], 0.0, 0.0], b: [0.0, 0.0, 0.0, astep], ..Default::default() });
            self.pass(gpu, &self.ao_blur_pipeline, &self.ao_b.bind, &self.ao_a.view, wgpu::LoadOp::Clear(BLACK));
            self.write_params(gpu, PostParams { a: [0.0; 4], b: [0.0; 4], ..Default::default() });
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
            self.write_params(gpu, PostParams { a: [0.0, 0.0, s.bloom_threshold, 0.0], b: [0.0; 4], ..Default::default() });
            self.pass(gpu, &self.bright_pipeline, &cur.bind, &self.bloom_a.view, wgpu::LoadOp::Clear(BLACK));
            // Separable blur: A→B (horizontal), B→A (vertical).
            self.write_params(gpu, PostParams { a: [htexel[0], htexel[1], 0.0, 0.0], b: [0.0, 0.0, 1.0, 0.0], ..Default::default() });
            self.pass(gpu, &self.blur_pipeline, &self.bloom_a.bind, &self.bloom_b.view, wgpu::LoadOp::Clear(BLACK));
            self.write_params(gpu, PostParams { a: [htexel[0], htexel[1], 0.0, 0.0], b: [0.0, 0.0, 0.0, 1.0], ..Default::default() });
            self.pass(gpu, &self.blur_pipeline, &self.bloom_b.bind, &self.bloom_a.view, wgpu::LoadOp::Clear(BLACK));
            // Composite: copy cur into the free full-res scratch, then additively
            // add the blurred bloom.
            let dst: &Target = if std::ptr::eq(cur, &self.ping) { &self.pong } else { &self.ping };
            self.write_params(gpu, PostParams { a: [0.0, 0.0, 0.0, s.bloom_intensity], b: [0.0; 4], ..Default::default() });
            self.pass(gpu, &self.copy_pipeline, &cur.bind, &dst.view, wgpu::LoadOp::Clear(BLACK));
            self.pass(gpu, &self.composite_pipeline, &self.bloom_a.bind, &dst.view, wgpu::LoadOp::Load);
            cur = dst;
        }

        // ---- the look chain -------------------------------------------------
        //
        // ORDER, and every step of it is a decision:
        //
        //   denoise → grade → [bloom, above] → lens → sharpen → finish(grain)
        //
        // *denoise first*, because it is the only pass that wants the RAW
        // sampling noise; run it after a grade and it is trying to separate
        // noise from detail in a picture whose contrast has already been pushed.
        //
        // *grade before the lens*, because a grade after chromatic aberration is
        // grading the coloured fringe — which is not a colour anybody chose, and
        // which saturation will happily amplify into a rainbow.
        //
        // *sharpen after the lens*, because the lens RESAMPLES: sharpen first
        // and the bilinear fetch that bends the picture throws it away again.
        //
        // *grain last* (inside `fs_finish`), because grain is what the picture is
        // recorded on. Sharpen it and it crawls; blur it and it is gone.
        //
        // Each step ping-pongs between the two full-res scratch targets, and a
        // step that is off costs nothing: no pass, no copy, no target.
        let texel = [1.0 / self.width.max(1) as f32, 1.0 / self.height.max(1) as f32];
        let aspect = self.width.max(1) as f32 / self.height.max(1) as f32;
        // `scene` is never a write target — it is what the world was drawn into
        // and the caller may still be reading it — so the free scratch is
        // whichever of ping/pong we are not currently on.
        // Takes the current target and RETURNS the next one, rather than writing
        // through a `&mut &Target` — inference ties the borrow in that form to
        // `'static` and the closure stops compiling.
        let look = |pipeline: &wgpu::RenderPipeline, params: PostParams, cur: &'a Target| -> &'a Target {
            let dst: &Target =
                if std::ptr::eq(cur, &self.ping) { &self.pong } else { &self.ping };
            self.write_params(gpu, params);
            self.pass(gpu, pipeline, &cur.bind, &dst.view, wgpu::LoadOp::Clear(BLACK));
            dst
        };

        // Depth of field goes FIRST of the look chain, before even the denoise:
        // it is the only pass here that is about the SCENE rather than about the
        // picture, and it needs the frame's own depth to still describe what is
        // in the frame. Everything downstream — grade, bloom, lens, sharpen — is
        // then working on an image whose focus is already decided, which is the
        // order a camera imposes and the order they read in.
        if let (true, Some(f)) = (s.dof_on(), ssao) {
            gpu.queue.write_buffer(
                &self.dof_cam_buf,
                0,
                bytemuck::bytes_of(&DofCam { inv_proj: f.inv_proj, ..Default::default() }),
            );
            let dof_bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("post-dof"),
                layout: &self.dof_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(f.depth),
                    },
                    wgpu::BindGroupEntry { binding: 1, resource: self.dof_cam_buf.as_entire_binding() },
                ],
            });
            let dst: &Target = if std::ptr::eq(cur, &self.ping) { &self.pong } else { &self.ping };
            // The DoF pass is the only user of `b` and `c` here, so its extra
            // knobs ride those lanes rather than growing the struct — spelled
            // out because the next person to reach for a spare lane needs to
            // know these are spoken for.
            //   b = far range, blades, blade rotation (rad), highlight boost
            //   c = taps, show-focus
            self.write_params(gpu, PostParams {
                a: [texel[0], texel[1], 0.0, 0.0],
                b: [
                    s.dof_range.max(1e-3),
                    s.dof_blades as f32,
                    s.dof_blade_rotation,
                    s.dof_highlight.max(0.0),
                ],
                c: [
                    if s.dof_quality == 0 { 16.0 } else { s.dof_quality.clamp(4, 64) as f32 },
                    if s.dof_show_focus { 1.0 } else { 0.0 },
                    0.0,
                    0.0,
                ],
                // A near range of 0 means "half the far range" — the behaviour
                // before there were two, resolved HERE so the shader never has
                // to know about the sentinel.
                f: [
                    0.0,
                    0.0,
                    s.dof_focus,
                    if s.dof_near_range > 0.0 {
                        s.dof_near_range
                    } else {
                        s.dof_range * 0.5
                    }
                    .max(1e-3),
                ],
                g: [s.dof_max_blur, aspect, 0.0, 0.0],
                ..Default::default()
            });
            self.pass2(
                gpu,
                &self.dof_pipeline,
                &cur.bind,
                Some(&dof_bind),
                &dst.view,
                wgpu::LoadOp::Clear(BLACK),
            );
            cur = dst;
        }

        // Motion blur sits directly after depth of field and before everything
        // else, for the same reason: both are about the SCENE, and both need the
        // frame's own depth to still describe what is in the frame. It goes
        // after DoF rather than before because a lens defocuses light and then
        // the shutter smears what the lens produced — that is the order the two
        // happen in a camera, and swapping them gives sharp streaks through a
        // blurred image, which reads as a rendering fault.
        if let (true, Some(f)) = (s.motion_on(), ssao) {
            gpu.queue.write_buffer(
                &self.dof_cam_buf,
                0,
                bytemuck::bytes_of(&DofCam {
                    inv_proj: f.inv_proj,
                    inv_view_proj: s.motion_inv_view_proj,
                    prev_view_proj: s.motion_prev_view_proj,
                    motion: [
                        s.motion_blur.max(0.0),
                        if s.motion_samples == 0 {
                            12.0
                        } else {
                            s.motion_samples.clamp(4, 32) as f32
                        },
                        s.motion_max.max(0.0),
                        0.0,
                    ],
                }),
            );
            let motion_bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("post-motion"),
                layout: &self.dof_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(f.depth),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.dof_cam_buf.as_entire_binding(),
                    },
                ],
            });
            let dst: &Target = if std::ptr::eq(cur, &self.ping) { &self.pong } else { &self.ping };
            self.write_params(gpu, PostParams {
                a: [texel[0], texel[1], 0.0, 0.0],
                ..Default::default()
            });
            self.pass2(
                gpu,
                &self.motion_pipeline,
                &cur.bind,
                Some(&motion_bind),
                &dst.view,
                wgpu::LoadOp::Clear(BLACK),
            );
            cur = dst;
        }

        if denoise_on {
            cur = look(
                &self.denoise_pipeline,
                PostParams {
                    a: [texel[0], texel[1], 0.0, 0.0],
                    f: [0.0, s.denoise, 0.0, 0.0],
                    ..Default::default()
                },
                cur,
            );
        }
        // ---- the scene's own screen shaders ---------------------------------
        if let Some(c) = custom {
            // ONE depth bind for all of them. Without a frame (a 2D project, or
            // a viewport that renders no depth) they get a 1×1 texture cleared
            // to far, so `sceneDepth` reads sky everywhere and `sceneNormal`
            // faces the camera: an outline finds no edges and quietly draws
            // nothing, rather than reading uninitialised memory.
            let depth_view = ssao.map(|f| f.depth).unwrap_or(&self.far_depth_view);
            let inv_proj = ssao.map(|f| f.inv_proj).unwrap_or_else(|| {
                glam::Mat4::IDENTITY.to_cols_array_2d()
            });
            gpu.queue.write_buffer(
                &self.dof_cam_buf,
                0,
                bytemuck::bytes_of(&DofCam { inv_proj, ..Default::default() }),
            );
            let depth_bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("post-flsl-depth"),
                layout: &self.dof_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(depth_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.dof_cam_buf.as_entire_binding(),
                    },
                ],
            });
            // The chain params an authored pass can read: the texel (so an
            // effect can step to a neighbour and stay one pixel wide under a
            // retro upscale), the aspect, and the clock.
            self.write_params(gpu, PostParams {
                a: [texel[0], texel[1], 0.0, 0.0],
                e: [0.0, 0.0, 0.0, s.time],
                g: [0.0, aspect, 0.0, 0.0],
                ..Default::default()
            });
            for slot in &c.slots {
                // A slot can outlive a failed recompile; it simply doesn't draw.
                let Some(shader) = c.shaders.get(slot.shader) else { continue };
                let dst: &Target =
                    if std::ptr::eq(cur, &self.ping) { &self.pong } else { &self.ping };
                self.pass3(
                    gpu,
                    &shader.pipeline,
                    &cur.bind,
                    Some(&depth_bind),
                    Some(&slot.bind),
                    &dst.view,
                    wgpu::LoadOp::Clear(BLACK),
                );
                cur = dst;
            }
        }

        if grade_on {
            cur = look(
                &self.grade_pipeline,
                PostParams {
                    c: [s.exposure, s.contrast, s.saturation, s.temperature],
                    d: [s.tint, s.lift, s.grade_gamma, s.gain],
                    ..Default::default()
                },
                cur,
            );
        }
        if lens_on {
            cur = look(
                &self.lens_pipeline,
                PostParams {
                    e: [s.aberration, s.distortion, 0.0, 0.0],
                    g: [0.0, aspect, 0.0, 0.0],
                    ..Default::default()
                },
                cur,
            );
        }
        if sharpen_on {
            cur = look(
                &self.sharpen_pipeline,
                PostParams {
                    a: [texel[0], texel[1], 0.0, 0.0],
                    f: [s.sharpen, 0.0, 0.0, 0.0],
                    ..Default::default()
                },
                cur,
            );
        }

        self.finish(gpu, s, cur, out, texel, aspect);
    }

    /// The terminal pass: tonemap, colour-vision filter, vignette, film grain —
    /// one shader, every part of it a no-op at identity parameters. Always runs,
    /// because it is the only pass that writes the display format.
    fn finish(
        &self,
        gpu: &Gpu,
        s: &PostSettings,
        src: &Target,
        out: &wgpu::TextureView,
        texel: [f32; 2],
        aspect: f32,
    ) {
        let filter_on = s.color_filter_on();
        let b = [
            if s.vignette { s.vignette_strength } else { 0.0 },
            if s.vignette { s.vignette_radius } else { 1.0 },
            0.0,
            0.0,
        ];
        // The colour-vision filter rides the bloom lanes, which this pass
        // does not use (`floptle/0079`).
        let a = [
            if s.simulate_deficiency { 1.0 } else { 0.0 },
            0.0,
            if filter_on { s.color_filter as f32 } else { 0.0 },
            if filter_on { s.color_filter_strength } else { 0.0 },
        ];
        // a.y is the one lane in `a` neither the filter nor the bloom uses,
        // so the grain's cell hash reads its texel height there and its cell
        // size from g.z. Spelt out rather than folded, so whoever adds the
        // next lane can see what is already spoken for.
        self.write_params(gpu, PostParams {
            a: [a[0], texel[1], a[2], a[3]],
            b,
            e: [0.0, 0.0, s.grain, s.time],
            g: [0.0, aspect, s.grain_size.max(1.0), s.tonemap.lane()],
            ..Default::default()
        });
        self.pass(gpu, &self.finish_pipeline, &src.bind, out, wgpu::LoadOp::Clear(BLACK));
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
        self.pass3(gpu, pipeline, bind, bind1, None, target, load);
    }

    #[allow(clippy::too_many_arguments)]
    fn pass3(
        &self,
        gpu: &Gpu,
        pipeline: &wgpu::RenderPipeline,
        bind: &wgpu::BindGroup,
        bind1: Option<&wgpu::BindGroup>,
        bind2: Option<&wgpu::BindGroup>,
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
            if let Some(b2) = bind2 {
                rp.set_bind_group(2, b2, &[]);
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
