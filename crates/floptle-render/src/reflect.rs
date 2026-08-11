//! **Reflection probes**: what a room reflects, captured from inside it.
//!
//! **The gap this closes.** A screen-space reflection can only show what is
//! already on screen, and the environment map behind it holds the *sky*. Outdoors
//! that pair is nearly complete — a ray that leaves the frame leaves toward the
//! horizon, and the sky is genuinely what is out there. Indoors it is badly
//! wrong: a polished floor in a corridor reflects a strip of what the camera can
//! see and then, for everything else, **daylight** — through the ceiling, through
//! the walls, from inside a sealed room. That is not a subtle artefact. It is the
//! single most conspicuous way an interior can fail to look like an interior.
//!
//! A probe answers it the way the sky already does: capture the surroundings
//! once, from a point inside the room, and hand every surface in that room the
//! result. What changes is only *which* environment a surface falls back to.
//!
//! **Equirectangular, in an array, exactly like the sky.** The capture is six
//! 90° renders — a probe is a camera, the same conclusion the GI bake reached —
//! folded into one equirectangular map per probe. Keeping the sky's projection
//! rather than a hardware cube map buys three things: the shader's existing
//! direction→uv formula is reused unchanged, the roughness mip chain is the same
//! box filter [`crate::env`] already builds, and there are no cube-face seams to
//! show up as a cross on a mirror. The pole stretch an equirect map has instead
//! is the one artefact no reflection has ever been troubled by.
//!
//! **Parallax is the whole difference between this and a second sky.** An
//! environment map is a picture at infinity: sampled by direction alone, it slides
//! with the camera and a reflected wall never lands on the wall. Each probe
//! therefore carries a **box** — the room it was captured in — and a reflected ray
//! is intersected with that box before the map is read. The sample direction is
//! taken from the *probe* to that intersection, so the wall in the reflection sits
//! where the wall is. The box is also the probe's region of influence, so one
//! rectangle authored once says both "this is the room" and "these are its
//! surfaces".

use crate::device::Gpu;

/// How much detail a probe capture keeps.
///
/// **This is the difference between a mirror and a frosted pane**, and it was
/// not adjustable before. A probe's picture is an equirectangular map: its width
/// spans a full turn, so a 256-wide map gives one texel every 1.4° and a mirror
/// reading it can show nothing finer than that — a doorway across a room lands
/// on about four texels. No roughness setting can recover detail the capture
/// never held, which is why a polished surface came out looking frosted however
/// it was authored.
///
/// The cost of raising it is paid at CAPTURE, not per frame: six renders of the
/// scene when a probe is first seen or its room changes. Per frame the extra
/// only costs memory bandwidth on the lookup. Sitting still, `High` and `Low`
/// cost the same.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ProbeDetail {
    /// 128-wide faces → a 256-wide map. For projects where probes are a hint of
    /// colour rather than something anybody looks into.
    Low,
    /// 256-wide faces → a 512-wide map.
    Medium,
    /// 512-wide faces → a 1024-wide map. The default: a mirror can show a
    /// doorway as a doorway.
    #[default]
    High,
    /// 1024-wide faces → a 2048-wide map. For a hero mirror, at 16× the capture
    /// cost of `High` and 22 MB of the four slots.
    Ultra,
}

impl ProbeDetail {
    /// The equirectangular map's width. Height is half — the same shape and the
    /// same mapping as the sky in [`crate::env`].
    pub fn width(self) -> u32 {
        match self {
            ProbeDetail::Low => 256,
            ProbeDetail::Medium => 512,
            ProbeDetail::High => 1024,
            ProbeDetail::Ultra => 2048,
        }
    }
    pub fn height(self) -> u32 {
        self.width() / 2
    }
    /// The square cube face each of the six captures renders at.
    ///
    /// Half the map's width. A face spans 90° and the map spans 360°, so equal
    /// angular density at the equator would want a face a QUARTER of the width;
    /// half of it is deliberately generous, because the conversion then averages
    /// rather than magnifies, and a magnified capture shows its own texels in a
    /// mirror.
    pub fn face(self) -> u32 {
        self.width() / 2
    }
}

/// The default capture face size — see [`ProbeDetail::face`].
pub const PROBE_FACE: u32 = 512;
/// The default equirectangular map size — see [`ProbeDetail::width`].
pub const PROBE_W: u32 = 1024;
pub const PROBE_H: u32 = PROBE_W / 2;
/// How many probes one scene can have live at once.
///
/// Four is a room, a corridor, a courtyard and a cave — and the honest ceiling
/// for a technique where a surface blends every probe whose box it stands in. A
/// level with twenty rooms wants probes streamed by proximity, which is a
/// different feature and would sit on top of this one rather than replace it.
pub const MAX_PROBES: usize = 4;
/// The format the 1×1 stand-in is allocated in. The real maps take the
/// renderer's own colour format instead — see [`ReflectionProbes::new`] — and
/// the binding accepts any filterable float, so these need not agree.
pub const PROBE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Six cube faces → one equirectangular map.
///
/// The face selection below is the exact inverse of [`floptle_gi::Face::texel_dir`]
/// — the same convention the GI bake renders with, and the reason this can reuse
/// `FACES` rather than declaring a second set of cube orientations that would
/// then have to be kept in step. Getting it wrong does not fail: it produces a
/// reflection that is *almost* right, rotated or mirrored by one face, which is
/// why the conversion is one pass whose output can be looked at directly instead
/// of arithmetic buried in the shading loop.
const CONVERT_WGSL: &str = r#"
@group(0) @binding(0) var faces: texture_2d_array<f32>;
@group(0) @binding(1) var samp: sampler;

const PI: f32 = 3.14159265359;

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

// The face and in-face coordinates a world direction lands on. `u` runs right
// and `v` runs UP in the face's own frame; the image it was rendered into runs
// top-down, which is the flip at the end.
struct Hit {
    layer: i32,
    uv: vec2<f32>,
};

fn face_of(d: vec3<f32>) -> Hit {
    let a = abs(d);
    var layer = 0;
    var u = 0.0;
    var v = 0.0;
    if (a.x >= a.y && a.x >= a.z) {
        let m = max(a.x, 1e-8);
        if (d.x > 0.0) { layer = 0; u =  d.z / m; v = d.y / m; }
        else           { layer = 1; u = -d.z / m; v = d.y / m; }
    } else if (a.y >= a.z) {
        let m = max(a.y, 1e-8);
        if (d.y > 0.0) { layer = 2; u = d.x / m; v =  d.z / m; }
        else           { layer = 3; u = d.x / m; v = -d.z / m; }
    } else {
        let m = max(a.z, 1e-8);
        if (d.z > 0.0) { layer = 4; u = -d.x / m; v = d.y / m; }
        else           { layer = 5; u =  d.x / m; v = d.y / m; }
    }
    var o: Hit;
    o.layer = layer;
    o.uv = vec2<f32>(u * 0.5 + 0.5, 0.5 - v * 0.5);
    return o;
}

// The direction of an equirect texel, the inverse of the mapping `env_radiance`
// samples with — one formula, written twice on purpose in two crates, and
// checked against each other by `reflection_probe_probe`.
@fragment
fn fs(in: VO) -> @location(0) vec4<f32> {
    let phi = (in.uv.x - 0.5) * 2.0 * PI;
    let theta = in.uv.y * PI;
    let st = sin(theta);
    let dir = vec3<f32>(st * cos(phi), cos(theta), st * sin(phi));
    let h = face_of(dir);
    // Pull the sample a texel inside the face. A direction exactly on a face
    // boundary is answered by whichever side `face_of` picked, and the
    // neighbouring texel it would filter against belongs to a different face —
    // so the last texel row of every face would smear the wrong image across the
    // seam. Clamping inside costs half a degree of the picture and removes it.
    let inset = 0.5 / f32(textureDimensions(faces, 0).x);
    let uv = clamp(h.uv, vec2<f32>(inset), vec2<f32>(1.0 - inset));
    return textureSampleLevel(faces, samp, uv, h.layer, 0.0);
}
"#;

/// The captured surroundings of up to [`MAX_PROBES`] places in the scene.
pub struct ReflectionProbes {
    /// One equirectangular layer per probe, with a roughness mip chain.
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    /// `[probe][mip]` render targets: the conversion writes mip 0 and the chain
    /// fills the rest.
    levels: Vec<Vec<wgpu::TextureView>>,
    /// `[probe][step]` binds for the downsample chain (level m → level m+1).
    down_binds: Vec<Vec<wgpu::BindGroup>>,
    down_pipeline: wgpu::RenderPipeline,
    /// Scratch: the six cube faces of the probe currently being captured, and a
    /// depth buffer to render them against. One set, reused by every probe,
    /// because captures are sequential by construction — a frame renders at most
    /// one probe (six full scene renders is already a lot to ask of one frame).
    faces: wgpu::Texture,
    face_views: Vec<wgpu::TextureView>,
    face_depth: wgpu::Texture,
    face_depth_views: Vec<wgpu::TextureView>,
    convert_pipeline: wgpu::RenderPipeline,
    convert_bind: wgpu::BindGroup,
    mips: u32,
    detail: ProbeDetail,
    format: wgpu::TextureFormat,
}

impl ReflectionProbes {
    /// Allocate the maps and the scratch a capture renders through.
    ///
    /// **The format is [`Gpu::scene_format`], not a chosen one and not the
    /// surface's.** A capture is six ordinary scene renders, and the raster
    /// pipelines are built against exactly one colour format — so a probe target
    /// in any other format is not a quality decision, it is a pipeline that
    /// cannot be set. Windowed rendering runs in HDR while the *surface* is
    /// 8-bit sRGB, so `config.format` is the wrong question and produces a
    /// validation error on the first capture rather than a bad-looking one.
    ///
    /// Asking the right one also gets HDR for free where it exists: a window
    /// captures probes with a window blown out to 12 and a headless probe
    /// captures 8-bit sRGB, each matching what its own scene actually looks
    /// like.
    pub fn new(gpu: &Gpu) -> Self {
        Self::with_detail(gpu, ProbeDetail::default())
    }

    /// [`ReflectionProbes::new`] at a chosen level of detail. The maps are sized
    /// once here; changing the setting rebuilds them, which is why the editor
    /// keeps the detail it built with and compares.
    pub fn with_detail(gpu: &Gpu, detail: ProbeDetail) -> Self {
        let device = &gpu.device;
        let format = gpu.scene_format();
        let (probe_w, probe_h, probe_face) = (detail.width(), detail.height(), detail.face());
        let mips = probe_w.ilog2() + 1;
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("reflection-probes"),
            size: wgpu::Extent3d {
                width: probe_w,
                height: probe_h,
                depth_or_array_layers: MAX_PROBES as u32,
            },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            // COPY_SRC so a probe can read a capture back and LOOK at it. A
            // reflection that is subtly rotated reads as "the reflections are a
            // bit odd" for a long time; the map itself says so at a glance.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor {
            label: Some("reflection-probes"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        // `u` REPEATS, for the reason the sky's does: the equirect seam is the
        // back of the room, and clamping there would smear the last column
        // across it in every rough reflection. `v` clamps — there is nothing
        // above the ceiling to wrap to.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("reflection-probe-samp"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let levels: Vec<Vec<wgpu::TextureView>> = (0..MAX_PROBES as u32)
            .map(|p| {
                (0..mips)
                    .map(|m| {
                        tex.create_view(&wgpu::TextureViewDescriptor {
                            label: Some("reflection-probe-level"),
                            dimension: Some(wgpu::TextureViewDimension::D2),
                            base_mip_level: m,
                            mip_level_count: Some(1),
                            base_array_layer: p,
                            array_layer_count: Some(1),
                            ..Default::default()
                        })
                    })
                    .collect()
            })
            .collect();

        let faces = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("reflection-probe-faces"),
            size: wgpu::Extent3d {
                width: probe_face,
                height: probe_face,
                depth_or_array_layers: 6,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let face_views: Vec<wgpu::TextureView> = (0..6)
            .map(|f| {
                faces.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("reflection-probe-face"),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: f,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        let face_depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("reflection-probe-depth"),
            size: wgpu::Extent3d {
                width: probe_face,
                height: probe_face,
                depth_or_array_layers: 6,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Gpu::DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let face_depth_views: Vec<wgpu::TextureView> = (0..6)
            .map(|f| {
                face_depth.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("reflection-probe-depth"),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: f,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();

        let array_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("reflection-probe-convert"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
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
        let faces_view = faces.create_view(&wgpu::TextureViewDescriptor {
            label: Some("reflection-probe-faces"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        // The faces are sampled with CLAMP, not the map's Repeat: a face has no
        // wrap-around, and the conversion insets its own samples anyway.
        let face_samp = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("reflection-probe-face-samp"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let convert_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("reflection-probe-convert"),
            layout: &array_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&faces_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&face_samp),
                },
            ],
        });
        let convert_pipeline =
            full_screen(device, &array_layout, CONVERT_WGSL, "reflection-probe-convert", format);

        // The chain is the SAME box filter the sky's is — see `env::DOWN_WGSL`.
        let down_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("reflection-probe-down"),
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
        let down_pipeline =
            full_screen(device, &down_layout, crate::env::DOWN_WGSL, "reflection-probe-down", format);
        let down_binds: Vec<Vec<wgpu::BindGroup>> = (0..MAX_PROBES)
            .map(|p| {
                (0..mips.saturating_sub(1) as usize)
                    .map(|m| {
                        device.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("reflection-probe-down"),
                            layout: &down_layout,
                            entries: &[
                                wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: wgpu::BindingResource::TextureView(&levels[p][m]),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 1,
                                    resource: wgpu::BindingResource::Sampler(&sampler),
                                },
                            ],
                        })
                    })
                    .collect()
            })
            .collect();

        Self {
            tex,
            view,
            sampler,
            levels,
            down_binds,
            down_pipeline,
            faces,
            face_views,
            face_depth,
            face_depth_views,
            convert_pipeline,
            convert_bind,
            mips,
            detail,
            format,
        }
    }

    /// The colour format captures are rendered and stored in — the renderer's
    /// own, so a caller can assert it matches the pass it is about to run.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// The colour target for cube face `f` of the capture in progress.
    pub fn face_target(&self, f: usize) -> &wgpu::TextureView {
        &self.face_views[f]
    }

    /// The depth target for cube face `f`. Per face rather than shared, so the
    /// six renders do not have to be ordered or cleared between each other.
    pub fn face_depth(&self, f: usize) -> &wgpu::TextureView {
        &self.face_depth_views[f]
    }

    /// Fold the six captured faces into probe `slot`'s equirectangular map and
    /// build its roughness chain. Run once, after all six faces are rendered.
    pub fn resolve(&self, gpu: &Gpu, slot: usize) {
        let Some(levels) = self.levels.get(slot) else { return };
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("reflection-probe-resolve"),
            });
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("reflection-probe-convert"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &levels[0],
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
            rp.set_pipeline(&self.convert_pipeline);
            rp.set_bind_group(0, &self.convert_bind, &[]);
            rp.draw(0..3, 0..1);
        }
        for (i, bind) in self.down_binds[slot].iter().enumerate() {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("reflection-probe-down"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &levels[i + 1],
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
        gpu.queue.submit([enc.finish()]);
    }

    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub fn sampler(&self) -> &wgpu::Sampler {
        &self.sampler
    }

    /// The detail these maps were built at — compare it against the project's
    /// setting to know whether they need rebuilding.
    pub fn detail(&self) -> ProbeDetail {
        self.detail
    }
    /// The square size one capture face renders at.
    pub fn face_size(&self) -> u32 {
        self.detail.face()
    }
    pub fn mips(&self) -> u32 {
        self.mips
    }

    /// Kept so the textures outlive the views taken from them.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.tex
    }

    pub fn faces_texture(&self) -> &wgpu::Texture {
        &self.faces
    }

    pub fn face_depth_texture(&self) -> &wgpu::Texture {
        &self.face_depth
    }

    /// The 1×1 stand-in a renderer with no probes binds instead.
    ///
    /// Same shape as the sky's and the depth prepass's empty: shading reads the
    /// probe COUNT from the uniforms and the dimensions from the texture, so
    /// "there are no probes" needs no flag anybody could forget to clear, and a
    /// scene that never places one costs a 1×1 texture.
    pub fn empty(device: &wgpu::Device) -> (wgpu::TextureView, wgpu::Sampler) {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("reflection-probes-empty"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: PROBE_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor {
            label: Some("reflection-probes-empty"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("reflection-probes-empty-samp"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        (view, sampler)
    }
}

fn full_screen(
    device: &wgpu::Device,
    bind: &wgpu::BindGroupLayout,
    src: &str,
    label: &str,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(src.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(bind)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
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
                format,
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
    })
}

/// Where a probe's camera looks for cube face `f`, and how big a square it sees.
///
/// A cube face IS a 90° square frustum. This re-exports the GI bake's face table
/// rather than declaring a second one, because the conversion shader's
/// `face_of` is written as the inverse of *that* table — two independent sets of
/// six orientations is exactly the kind of thing that drifts by one flip and
/// produces a reflection nobody can quite say is wrong.
pub fn face_rotation(f: usize) -> floptle_core::math::Quat {
    floptle_gi::FACES[f.min(5)].rotation()
}

#[cfg(test)]
mod tests {
    use floptle_core::math::Vec3;

    /// `face_of` in `CONVERT_WGSL`, on the CPU. Kept in step by hand and checked
    /// against `texel_dir` below — the shader cannot be run without a GPU, and
    /// the property that matters is arithmetic rather than pixels.
    fn face_of(d: Vec3) -> (usize, f32, f32) {
        let a = d.abs();
        if a.x >= a.y && a.x >= a.z {
            let m = a.x.max(1e-8);
            if d.x > 0.0 { (0, d.z / m, d.y / m) } else { (1, -d.z / m, d.y / m) }
        } else if a.y >= a.z {
            let m = a.y.max(1e-8);
            if d.y > 0.0 { (2, d.x / m, d.z / m) } else { (3, d.x / m, -d.z / m) }
        } else {
            let m = a.z.max(1e-8);
            if d.z > 0.0 { (4, -d.x / m, d.y / m) } else { (5, d.x / m, d.y / m) }
        }
    }

    /// The round trip: every direction lands on a face, and the face's own
    /// `texel_dir` at the coordinates it landed at points back where it started.
    ///
    /// This is the check that the conversion shader and the bake's cube
    /// orientations agree. A mismatch does not crash or look broken — it
    /// reflects the wall on the left onto the surfaces on the right, which reads
    /// as "the reflections are a bit odd" and can survive a long time.
    #[test]
    fn every_direction_round_trips_through_its_face() {
        let mut worst = 0.0f32;
        // A spread that covers all six faces and every edge between them,
        // deliberately including directions exactly on a face boundary.
        for i in 0..31 {
            for j in 0..31 {
                let phi = (i as f32 / 30.0) * std::f32::consts::TAU;
                let theta = (j as f32 / 30.0) * std::f32::consts::PI;
                let d = Vec3::new(
                    theta.sin() * phi.cos(),
                    theta.cos(),
                    theta.sin() * phi.sin(),
                );
                let (f, u, v) = face_of(d);
                let back = floptle_gi::FACES[f].texel_dir(u, v);
                worst = worst.max((back - d).length());
            }
        }
        assert!(
            worst < 1e-4,
            "a direction came back {worst} away from itself after a trip through \
             its cube face — the conversion shader's face table and the bake's \
             have drifted apart"
        );
    }

    /// …and the six faces are actually all used. A `face_of` that answered "face
    /// 0" for everything would pass the round trip above only if `texel_dir`
    /// were broken the same way, but it would fail this instantly.
    #[test]
    fn all_six_faces_are_reachable() {
        let mut seen = [false; 6];
        for d in [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z] {
            seen[face_of(d).0] = true;
        }
        assert_eq!(seen, [true; 6], "the six axis directions do not reach six distinct faces");
    }
}
