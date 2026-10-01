//! Shader backdrops: a theme's moving picture behind a panel.
//!
//! **What it costs, and why that is small.** A backdrop is not drawn per
//! panel. Each distinct shader a theme uses is drawn once per update into its
//! own window-sized texture, at a fraction of the window's resolution
//! ([`crate::Prefs::backdrop_scale`], half by default: a quarter of the
//! pixels), at most [`crate::Prefs::backdrop_fps`] times a second, and only
//! inside the rectangle the panels that show it cover this frame. Every panel
//! then shows its part of that texture as an ordinary image, which costs what
//! any other image in the UI costs. Between updates the texture is reused as
//! it is. A shader no visible panel uses is not drawn at all; with Effects set
//! to Still it is drawn once and held, and Off draws none.
//!
//! **Writing one.** A theme's `.wgsl` file defines one function:
//!
//! ```wgsl
//! fn backdrop(uv: vec2<f32>, px: vec2<f32>) -> vec4<f32> {
//!     // uv: 0–1 across the whole window. px: the same in points.
//!     let t = bd.time;
//!     return vec4<f32>(bd.color0.rgb * (0.5 + 0.5 * sin(uv.x * 6.0 + t)), 1.0);
//! }
//! ```
//!
//! and returns an sRGB colour with straight alpha. It can read the uniform
//! `bd` ([`PRELUDE`] has every field), sample `bd_image` with `bd_sampler`
//! (the layer's `image`, or white), and call the helpers the prelude defines:
//! `bd_hash`, `bd_noise`, `bd_fbm`, `bd_rot`. Because the picture spans the
//! window, two panels with the same backdrop read as one view of it.
//!
//! A shader that does not compile is refused with naga's message and the
//! panel shows its fill colour instead. It never stops the editor.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::color::Rgba;
use crate::model::Layer;
use crate::source::Effects;

macro_rules! shaders {
    ($($name:literal),* $(,)?) => {
        /// The built-in shaders a theme names as `builtin:<name>`.
        pub const BUILTIN_SHADERS: &[(&str, &str)] = &[
            $(($name, include_str!(concat!("../shaders/", $name, ".wgsl"))),)*
        ];
    };
}
shaders!("galaxy", "aurora", "grid", "scanlines", "drift", "waves", "starfield");

/// `builtin:<name>`'s source.
pub fn builtin_source(name: &str) -> Option<&'static str> {
    let n = name.strip_prefix("builtin:")?;
    BUILTIN_SHADERS.iter().find(|s| s.0 == n).map(|s| s.1)
}

/// What every backdrop shader is given. Kept in step with [`Uniforms`].
pub const PRELUDE: &str = r#"
struct Backdrop {
    // The texture being drawn, in pixels.
    resolution: vec2<f32>,
    // The window, in points.
    window: vec2<f32>,
    // Seconds, times the layer's speed. Holds still when effects are paused.
    time: f32,
    // The layer's `scale`.
    scale: f32,
    // Pixels per point of the texture (window scale times backdrop scale).
    density: f32,
    _pad0: f32,
    // The layer's colours: by default accent, accent_hi, ground, text.
    color0: vec4<f32>,
    color1: vec4<f32>,
    color2: vec4<f32>,
    color3: vec4<f32>,
    // The layer's eight `params`.
    params0: vec4<f32>,
    params1: vec4<f32>,
    // The pointer, 0-1 across the window; z is 1 while it is over the window.
    pointer: vec4<f32>,
};
@group(0) @binding(0) var<uniform> bd: Backdrop;
@group(0) @binding(1) var bd_image: texture_2d<f32>;
@group(0) @binding(2) var bd_sampler: sampler;

fn bd_hash(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}
fn bd_noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    // Quintic: no visible creases along the grid.
    let u = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    let a = bd_hash(i);
    let b = bd_hash(i + vec2<f32>(1.0, 0.0));
    let c = bd_hash(i + vec2<f32>(0.0, 1.0));
    let d = bd_hash(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}
fn bd_fbm(p0: vec2<f32>, octaves: i32) -> f32 {
    // Each octave turned against the last, so no two share the grid's axes.
    let m = mat2x2<f32>(0.8, -0.6, 0.6, 0.8);
    var p = p0;
    var v = 0.0;
    var a = 0.5;
    for (var i = 0; i < min(octaves, 8); i = i + 1) {
        v = v + a * bd_noise(p);
        p = m * p * 2.02 + vec2<f32>(17.1, 9.2);
        a = a * 0.5;
    }
    return v;
}
fn bd_rot(a: f32) -> mat2x2<f32> {
    let c = cos(a);
    let s = sin(a);
    return mat2x2<f32>(c, -s, s, c);
}
"#;

const MAIN: &str = r#"
struct BdOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};
@vertex
fn bd_vs(@builtin(vertex_index) i: u32) -> BdOut {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var o: BdOut;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    o.uv = vec2<f32>(x, y);
    return o;
}
@fragment
fn bd_fs(i: BdOut) -> @location(0) vec4<f32> {
    let uv = i.pos.xy / bd.resolution;
    let c = backdrop(uv, uv * bd.window);
    let a = clamp(c.a, 0.0, 1.0);
    // egui composites premultiplied, in sRGB: so is this.
    return vec4<f32>(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * a, a);
}
"#;

/// The full WGSL for a backdrop function.
pub fn full_source(body: &str) -> String {
    format!("{PRELUDE}\n{body}\n{MAIN}")
}

/// Check a backdrop compiles, with naga, before wgpu ever sees it: wgpu's own
/// answer to a bad shader is a panic on another thread. The error is
/// naga's, with the line it points at.
pub fn validate(body: &str) -> Result<(), String> {
    let src = full_source(body);
    let module = naga::front::wgsl::parse_str(&src).map_err(|e| e.emit_to_string(&src))?;
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
        .validate(&module)
        .map_err(|e| e.emit_to_string(&src))?;
    if !module.functions.iter().any(|(_, f)| f.name.as_deref() == Some("backdrop")) {
        return Err("the shader has no `fn backdrop(uv: vec2<f32>, px: vec2<f32>) -> vec4<f32>`".into());
    }
    Ok(())
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    resolution: [f32; 2],
    window: [f32; 2],
    time: f32,
    scale: f32,
    density: f32,
    _pad0: f32,
    colors: [[f32; 4]; 4],
    params: [[f32; 4]; 2],
    pointer: [f32; 4],
}

/// The identity of a shader layer's picture: everything that changes what it
/// draws. Opacity and blend are not in it; they apply when a panel shows it,
/// so two panels showing one shader at different strengths share one texture.
pub fn layer_key(layer: &Layer) -> Option<u64> {
    let Layer::Shader { shader, speed, scale, colors, params, image, .. } = layer else { return None };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    shader.hash(&mut h);
    speed.to_bits().hash(&mut h);
    scale.to_bits().hash(&mut h);
    colors.hash(&mut h);
    for p in params {
        p.to_bits().hash(&mut h);
    }
    image.hash(&mut h);
    Some(h.finish())
}

struct Pipeline {
    pipeline: wgpu::RenderPipeline,
}

struct Slot {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: [u32; 2],
    egui_id: egui::TextureId,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    last_drawn: Option<Instant>,
    last_used: Instant,
    /// The animation clock this slot last drew at, so Still holds a frame.
    drawn_at_time: f32,
}

/// Draws a theme's shader layers. One per window; the host calls
/// [`BackdropRenderer::render`] each frame before it draws egui.
pub struct BackdropRenderer {
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    sampler: wgpu::Sampler,
    white: wgpu::TextureView,
    pipelines: HashMap<u64, Result<Arc<Pipeline>, String>>,
    images: HashMap<String, wgpu::TextureView>,
    slots: HashMap<u64, Slot>,
    /// The animation clock: advances only while backdrops are moving, so a
    /// pause and a resume pick up where they stopped rather than jumping.
    clock: f32,
    last_tick: Option<Instant>,
    generation: u64,
    /// GPU time is not measured here; this is how many backdrop draws ran in
    /// the last second, for the theme settings to show.
    draws: std::collections::VecDeque<Instant>,
}

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

impl BackdropRenderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let tex_entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty,
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("theme-backdrop"),
            entries: &[
                tex_entry(
                    0,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                tex_entry(
                    1,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                tex_entry(2, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("theme-backdrop"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("theme-backdrop"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let white = upload(device, queue, 1, 1, &[255, 255, 255, 255]);
        Self {
            layout,
            pipeline_layout,
            sampler,
            white,
            pipelines: HashMap::new(),
            images: HashMap::new(),
            slots: HashMap::new(),
            clock: 0.0,
            last_tick: None,
            generation: u64::MAX,
            draws: Default::default(),
        }
    }

    fn pipeline(&mut self, device: &wgpu::Device, body: &str) -> Result<Arc<Pipeline>, String> {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        body.hash(&mut h);
        let key = h.finish();
        if let Some(p) = self.pipelines.get(&key) {
            return p.clone();
        }
        let made = validate(body).and_then(|()| {
            // naga said yes; a backend that still says no is caught here
            // rather than by wgpu's uncaptured-error handler, which panics.
            let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("theme-backdrop"),
                source: wgpu::ShaderSource::Wgsl(full_source(body).into()),
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("theme-backdrop"),
                layout: Some(&self.pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("bd_vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("bd_fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            match wait_polling(device, scope.pop()) {
                Some(Some(e)) => Err(e.to_string()),
                // No error, or no answer in time: a backend that has not said
                // no has said yes, and a draw that fails later is reported by
                // wgpu like any other.
                _ => Ok(Arc::new(Pipeline { pipeline })),
            }
        });
        self.pipelines.insert(key, made.clone());
        made
    }

    /// Draw whatever backdrops the frame's UI asked for. `size` is the
    /// window's framebuffer in pixels; `paused` holds every backdrop still
    /// (the editor passes "the game is playing" when the user asked for that).
    ///
    /// Call once per frame, after the UI has run and before egui is drawn.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut egui_wgpu::Renderer,
        ctx: &egui::Context,
        size: [u32; 2],
        prefs: &crate::Prefs,
        paused: bool,
    ) {
        let Some(shared) = crate::paint::shared(ctx) else { return };
        let now = Instant::now();
        let moving = prefs.effects == Effects::Full && !paused;
        let dt = self.last_tick.map_or(0.0, |t| (now - t).as_secs_f32()).min(0.25);
        self.last_tick = Some(now);
        if moving {
            // Wrapped, so a week-long session keeps f32 precision. A shader
            // sees one seam an hour.
            self.clock = (self.clock + dt) % 3600.0;
        }
        let (theme, uses, generation) = {
            let mut rt = shared.lock();
            let uses = std::mem::take(&mut rt.uses);
            (rt.theme.clone(), uses, rt.generation)
        };
        if generation != self.generation {
            // A different theme: its pictures, images and errors start over.
            for (_, s) in self.slots.drain() {
                renderer.free_texture(&s.egui_id);
            }
            self.images.clear();
            self.generation = generation;
            shared.lock().shader_tex.clear();
        }
        if prefs.effects == Effects::Off {
            return;
        }
        let ppp = ctx.pixels_per_point();
        let scale = prefs.backdrop_scale.clamp(0.25, 1.0);
        let tex_size = [((size[0] as f32 * scale).round() as u32).max(1), ((size[1] as f32 * scale).round() as u32).max(1)];
        let window = egui::vec2(size[0] as f32 / ppp, size[1] as f32 / ppp);
        let interval = Duration::from_secs_f32(1.0 / prefs.backdrop_fps.clamp(5.0, 60.0));
        let pointer = ctx.input(|i| i.pointer.latest_pos()).map_or([0.5, 0.5, 0.0, 0.0], |p| {
            [p.x / window.x.max(1.0), p.y / window.y.max(1.0), 1.0, 0.0]
        });

        let mut encoder: Option<wgpu::CommandEncoder> = None;
        let mut errors: Vec<(String, String)> = Vec::new();
        let mut published: Vec<(u64, egui::TextureId)> = Vec::new();
        for (key, u) in uses {
            let Layer::Shader { shader, speed, scale: lscale, colors, params, image, .. } = &u.layer else { continue };
            let body: String = match crate::backdrop::builtin_source(shader) {
                Some(s) => s.to_string(),
                None => match theme.assets.files.get(shader.as_str()).map(|b| String::from_utf8_lossy(b).into_owned()) {
                    Some(s) => s,
                    None => continue,
                },
            };
            let pipe = match self.pipeline(device, &body) {
                Ok(p) => p,
                Err(e) => {
                    errors.push((shader.clone(), e));
                    continue;
                }
            };
            let img_view = match image {
                None => self.white.clone(),
                Some(path) => {
                    if !self.images.contains_key(path) {
                        let view = theme
                            .assets
                            .files
                            .get(path.as_str())
                            .and_then(|b| image::load_from_memory(b).ok())
                            .map(|i| {
                                let i = i.to_rgba8();
                                upload(device, queue, i.width(), i.height(), i.as_raw())
                            })
                            .unwrap_or_else(|| self.white.clone());
                        self.images.insert(path.clone(), view);
                    }
                    self.images[path].clone()
                }
            };

            // The slot: a texture the window's size (times scale), shown to egui.
            let fresh = match self.slots.get(&key) {
                Some(s) => s.size != tex_size,
                None => true,
            };
            if fresh {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("theme-backdrop"),
                    size: wgpu::Extent3d { width: tex_size[0], height: tex_size[1], depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                });
                let view = texture.create_view(&Default::default());
                let egui_id = match self.slots.remove(&key) {
                    Some(old) => {
                        renderer.update_egui_texture_from_wgpu_texture(device, &view, wgpu::FilterMode::Linear, old.egui_id);
                        old.egui_id
                    }
                    None => renderer.register_native_texture(device, &view, wgpu::FilterMode::Linear),
                };
                let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("theme-backdrop"),
                    size: std::mem::size_of::<Uniforms>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("theme-backdrop"),
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&img_view) },
                        wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    ],
                });
                self.slots.insert(
                    key,
                    Slot {
                        texture,
                        view,
                        size: tex_size,
                        egui_id,
                        uniforms,
                        bind_group,
                        last_drawn: None,
                        last_used: now,
                        drawn_at_time: f32::NAN,
                    },
                );
            }
            let slot = self.slots.get_mut(&key).expect("just inserted");
            slot.last_used = now;
            published.push((key, slot.egui_id));

            let time = self.clock * speed;
            let animates = *speed != 0.0 && moving;
            let due = match slot.last_drawn {
                None => true,
                Some(t) => animates && now.duration_since(t) >= interval,
            } || (!animates && slot.drawn_at_time.is_nan());
            if !due {
                continue;
            }
            let c4 = |c: Rgba| c.to_f32();
            let uni = Uniforms {
                resolution: [tex_size[0] as f32, tex_size[1] as f32],
                window: [window.x, window.y],
                time,
                scale: *lscale,
                density: ppp * scale,
                _pad0: 0.0,
                colors: [c4(colors[0]), c4(colors[1]), c4(colors[2]), c4(colors[3])],
                params: [[params[0], params[1], params[2], params[3]], [params[4], params[5], params[6], params[7]]],
                pointer,
            };
            queue.write_buffer(&slot.uniforms, 0, bytemuck::bytes_of(&uni));
            // Only where a panel shows it this frame. Points to texture pixels,
            // rounded outward, a pixel of margin for the bilinear tap.
            // Held still, it is drawn whole: it will be shown as it is for as
            // long as it holds, wherever the panels move to.
            let k = ppp * scale;
            let full = !animates;
            let x0 = ((u.rect.min.x * k).floor() as i64 - 1).clamp(0, tex_size[0] as i64) as u32;
            let y0 = ((u.rect.min.y * k).floor() as i64 - 1).clamp(0, tex_size[1] as i64) as u32;
            let x1 = ((u.rect.max.x * k).ceil() as i64 + 1).clamp(0, tex_size[0] as i64) as u32;
            let y1 = ((u.rect.max.y * k).ceil() as i64 + 1).clamp(0, tex_size[1] as i64) as u32;
            let (x0, y0, x1, y1) = if full { (0, 0, tex_size[0], tex_size[1]) } else { (x0, y0, x1, y1) };
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let enc = encoder.get_or_insert_with(|| {
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("theme-backdrop") })
            });
            {
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("theme-backdrop"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &slot.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: if slot.last_drawn.is_none() {
                                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                            } else {
                                wgpu::LoadOp::Load
                            },
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&pipe.pipeline);
                pass.set_bind_group(0, &slot.bind_group, &[]);
                pass.set_scissor_rect(x0, y0, x1 - x0, y1 - y0);
                pass.draw(0..3, 0..1);
            }
            slot.last_drawn = Some(now);
            slot.drawn_at_time = if animates { f32::NAN } else { time };
            self.draws.push_back(now);
        }
        if let Some(enc) = encoder {
            queue.submit([enc.finish()]);
        }
        // A picture no panel has shown for two seconds goes; a panel that
        // comes back draws it again on its first frame.
        let stale: Vec<u64> = self
            .slots
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_used) > Duration::from_secs(2))
            .map(|(k, _)| *k)
            .collect();
        let mut rt = shared.lock();
        for k in stale {
            if let Some(s) = self.slots.remove(&k) {
                renderer.free_texture(&s.egui_id);
                drop(s.texture);
            }
            rt.shader_tex.remove(&k);
        }
        for (k, id) in published {
            rt.shader_tex.insert(k, id);
        }
        for (s, e) in errors {
            rt.shader_errors.entry(s).or_insert(e);
        }
        while self.draws.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1)) {
            self.draws.pop_front();
        }
        rt.draws_per_second = self.draws.len() as u32;
        rt.moving = moving && theme.is_animated();
    }
}

fn upload(device: &wgpu::Device, queue: &wgpu::Queue, w: u32, h: u32, rgba: &[u8]) -> wgpu::TextureView {
    let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("theme-backdrop-image"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        rgba,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * w), rows_per_image: Some(h) },
        size,
    );
    tex.create_view(&Default::default())
}

/// Wait for a wgpu future, polling the device while it is pending.
///
/// **Never `pollster::block_on` one of these.** The future resolves only once
/// the device is polled, and in the Hub nothing else polls it: blocking hung
/// the window after its first frame. Bounded, so a driver that never answers
/// costs a moment, not the program.
fn wait_polling<F: std::future::Future>(device: &wgpu::Device, f: F) -> Option<F::Output> {
    use std::task::{Context, Poll, Waker};
    let mut cx = Context::from_waker(Waker::noop());
    let mut f = std::pin::pin!(f);
    let start = Instant::now();
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return Some(v);
        }
        if start.elapsed() > Duration::from_millis(500) {
            log::warn!("a theme backdrop's pipeline never reported back; assuming it compiled");
            return None;
        }
        let _ = device.poll(wgpu::PollType::Poll);
        std::thread::yield_now();
    }
}
