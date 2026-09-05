//! The bring-up ladder, one rung per line on the page.
//!
//! 1. The scripting VM in a tab: eval, `pcall` recovering from a Rust error
//!    and a Lua one (this is what proves the C++ exception runtime is wired —
//!    Luau raises through it), a syntax error reported not trapped, native
//!    vectors, a burst of garbage collected, an uncaught error survived.
//! 2. A WebGPU adapter and device through the renderer's own device path.
//! 3. The raster pipelines built, a mesh with vertex paint and a skin uploaded
//!    — the storage-buffer paths WebGL2 could not carry.
//! 4. Frames: the skinned bar curling, drawn at retro resolution and upscaled
//!    into the canvas, with a frame-time readout.
//!
//! Each rung logs `RUNG n OK` or `RUNG n FAILED — why`, so a headless browser
//! reading the page can say exactly how far the engine got.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use floptle_render::{
    Globals, Gpu, MaterialParams, MeshId, PostSettings, PostStack, Projection, Raster, RenderCamera,
    Retro, SkinDraw, instance_of_mat, take_gpu_errors,
};
use glam::{Mat3, Mat4, Quat, Vec3};
use wasm_bindgen::prelude::*;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::web::{EventLoopExtWebSys, WindowAttributesExtWebSys};
use winit::window::{Window, WindowId};

use crate::bar;

/// The retro target's height in rows — a typical project `retro_height`.
const RETRO_H: u32 = 240;
/// The canvas, in CSS pixels.
const CANVAS: (f64, f64) = (640.0, 360.0);

#[wasm_bindgen]
extern "C" {
    /// `window.floptleLog`, defined by the page: append a line to its transcript.
    #[wasm_bindgen(js_namespace = window, js_name = floptleLog)]
    fn page_log(line: &str);
    /// `window.floptleReadout`: replace the frame-time readout.
    #[wasm_bindgen(js_namespace = window, js_name = floptleReadout)]
    fn page_readout(text: &str);
    /// `window.floptleCapture`: hand the page a frame as tightly packed RGBA.
    #[wasm_bindgen(js_namespace = window, js_name = floptleCapture)]
    fn page_capture(rgba: &[u8], width: u32, height: u32);
}

/// Append a line to the page's log (the page echoes it to the console).
///
/// Through a function the page defines rather than through `web_sys::window()`
/// — see `start` for why the global object is not touched here.
pub(crate) fn log(line: &str) {
    page_log(line);
}

/// Replace the frame-time readout (its own element, so the log stays a
/// stable transcript a test can read).
fn readout(text: &str) {
    page_readout(text);
}

fn now_ms() -> f64 {
    web_sys::window().and_then(|w| w.performance()).map_or(0.0, |p| p.now())
}

/// The module's own start, run by the JS glue's `init()` on both pages.
#[wasm_bindgen(start)]
pub fn start() {
    // Before anything else: the C++ static constructors, exactly once.
    crate::wasi::run_static_constructors();
    // A panic is reported to the page in two ways: the transcript every page
    // keeps (`floptleLog`), and `floptleFatal`, which the player page shows to
    // the person rather than leaving a black canvas.
    std::panic::set_hook(Box::new(|info| {
        let text = format!("PANIC {info}");
        log(&text);
        crate::player::fatal(&text);
    }));
}

/// The bring-up ladder. `probe.html` calls this after `init()`.
#[wasm_bindgen]
pub fn probe() {
    log(&format!("floptle-web {} — bring-up probe", env!("CARGO_PKG_VERSION")));
    match luau_ladder() {
        Ok((version, n)) => log(&format!("RUNG 1 OK — {version} in a tab: {n} checks passed")),
        Err(e) => {
            log(&format!("RUNG 1 FAILED — {e}"));
            return;
        }
    }
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.spawn_app(App::default());
}

/// Rung 1. Every check is asserted from Rust, not read off a print.
fn luau_ladder() -> Result<(String, usize), String> {
    use mlua::{Lua, Value, Variadic};
    let lua = Lua::new();
    let g = lua.globals();
    let print = lua
        .create_function(|_, args: Variadic<Value>| {
            let s: Vec<String> = args.iter().map(|v| v.to_string().unwrap_or_default()).collect();
            log(&format!("  luau> {}", s.join("\t")));
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    g.set("print", print).map_err(|e| e.to_string())?;
    g.set(
        "boom",
        lua.create_function(|_, ()| -> mlua::Result<()> { Err(mlua::Error::runtime("rust boom")) })
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let version: String = g.get("_VERSION").map_err(|e| e.to_string())?;

    let mut checks = 0;
    let mut check = |name: &str, ok: bool, detail: String| -> Result<(), String> {
        if ok {
            checks += 1;
            Ok(())
        } else {
            Err(format!("{name}: {detail}"))
        }
    };

    let v: f64 = lua
        .load("print('hello from luau', _VERSION); return 1 + 2")
        .eval()
        .map_err(|e| e.to_string())?;
    check("eval", v == 3.0, format!("1 + 2 gave {v}"))?;

    let (ok, msg): (bool, String) = lua
        .load("local ok, e = pcall(boom); return ok, tostring(e)")
        .eval()
        .map_err(|e| e.to_string())?;
    check("pcall over a Rust error", !ok && msg.contains("rust boom"), format!("ok={ok} msg={msg}"))?;

    let (ok, msg): (bool, String) = lua
        .load("local ok, e = pcall(function() error('lua boom') end); return ok, tostring(e)")
        .eval()
        .map_err(|e| e.to_string())?;
    check("pcall over a Lua error", !ok && msg.contains("lua boom"), format!("ok={ok} msg={msg}"))?;

    let syntax = lua.load("this is not lua").exec();
    check(
        "a syntax error is reported",
        matches!(&syntax, Err(mlua::Error::SyntaxError { .. })),
        format!("{syntax:?}"),
    )?;

    let mag: f64 = lua
        .load("local v = vector.create(1,2,3) + vector.create(1,1,1); return vector.magnitude(v)")
        .eval()
        .map_err(|e| e.to_string())?;
    check("native vectors", (mag - 29f64.sqrt()).abs() < 1e-4, format!("|(2,3,4)| gave {mag}"))?;

    let (sum, ms): (f64, f64) = lua
        .load(
            "local t0 = os.clock(); local s = 0; for i = 1, 200000 do local t = {i, i * 2}; \
             s = s + t[2] end; collectgarbage('collect'); return s, (os.clock() - t0) * 1000",
        )
        .eval()
        .map_err(|e| e.to_string())?;
    log(&format!("  200k tables allocated and collected in {ms:.1} ms"));
    check("GC churn", sum == 40_000_200_000.0, format!("sum {sum}"))?;

    let uncaught = lua.load("error('uncaught')").exec();
    check("an uncaught error surfaces", uncaught.is_err(), format!("{uncaught:?}"))?;

    let alive: bool = lua.load("return true").eval().map_err(|e| e.to_string())?;
    check("the state survives it", alive, "state dead".into())?;
    Ok((version, checks))
}

/// Every shader module the engine builds, as the pipelines assemble them —
/// the same list `floptle-render`'s OpenGL-backend test keeps, for the same
/// reason: `field.wgsl` is spliced into two of these and is not a module.
fn shader_modules() -> Vec<(&'static str, String)> {
    vec![
        ("raster.wgsl + field.wgsl", floptle_render::raster::pass_prelude().to_string()),
        ("raymarch.wgsl + field.wgsl", floptle_render::raymarch::prelude().to_string()),
        ("grid.wgsl", include_str!("../../floptle-render/src/grid.wgsl").to_string()),
        ("light2d.wgsl", include_str!("../../floptle-render/src/light2d.wgsl").to_string()),
        ("outline.wgsl", include_str!("../../floptle-render/src/outline.wgsl").to_string()),
        ("palette.wgsl", include_str!("../../floptle-render/src/palette.wgsl").to_string()),
        ("particles.wgsl", include_str!("../../floptle-render/src/particles.wgsl").to_string()),
        ("post.wgsl", include_str!("../../floptle-render/src/post.wgsl").to_string()),
        ("retro.wgsl", include_str!("../../floptle-render/src/retro.wgsl").to_string()),
        ("ssao.wgsl", include_str!("../../floptle-render/src/ssao.wgsl").to_string()),
        ("ui.wgsl", include_str!("../../floptle-render/src/ui.wgsl").to_string()),
    ]
}

/// Hand every **generated** shader to the browser, through the exact call a
/// game's own `.flsl` takes.
///
/// The census below covers the engine's fixed `.wgsl` modules. It does not
/// cover what the shader compiler *writes* — the `.flsl` stdlib spliced into
/// every generated shader, and the chunk transpiled from the graph — and that
/// is the half a game actually ships. It went unchecked until a real game's
/// sky and water shaders were refused whole in a tab (2026-09-05: an
/// unparenthesised `*` before a `^` in the stdlib's hash, which naga accepts
/// and a browser does not).
///
/// Each example is compiled and REGISTERED, not reassembled here: a copy of
/// the splice would drift from the real one and pass while the real one broke.
/// Returns how many were refused.
async fn flsl_census(gpu: &Gpu, raster: &mut Raster) -> usize {
    let mut refused = 0;
    let mut checked = 0;
    let mut skipped = Vec::new();
    for (name, src) in floptle_shader::examples::EXAMPLES {
        // Only the fragment stage registers through `Raster`; the sky, post
        // and UI stages have their own passes. They share this stdlib, so the
        // stdlib is covered either way — but say which were not compiled
        // rather than let a count imply full coverage.
        let compiled = match floptle_shader::compile_fragment(src) {
            Ok(c) => c,
            Err(_) => {
                skipped.push(*name);
                continue;
            }
        };
        let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
        let blend = match compiled.blend {
            floptle_shader::Blend::Opaque => floptle_render::FlslBlend::Opaque,
            floptle_shader::Blend::Alpha => floptle_render::FlslBlend::Alpha,
            floptle_shader::Blend::Additive => floptle_render::FlslBlend::Additive,
        };
        let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
        raster.register_flsl_shader(gpu, &chunk, compiled.textures.len(), blend, None);
        checked += 1;
        if let Some(e) = scope.pop().await {
            refused += 1;
            let why = e.to_string();
            let why = why.lines().next().unwrap_or("").trim();
            log(&format!("SHADER flsl:{name} REFUSED — {why}"));
        } else {
            log(&format!("SHADER flsl:{name} ok"));
        }
    }
    if !skipped.is_empty() {
        log(&format!(
            "  ({} example(s) are not fragment shaders and were not registered here: {})",
            skipped.len(),
            skipped.join(", ")
        ));
    }
    log(&format!("  {checked} generated shader(s) through this browser's compiler"));
    refused
}

/// Hand every shader module to the browser's own WGSL compiler and report
/// each refusal by name.
///
/// This is the check native cannot make. wgpu on the desktop validates WGSL
/// with naga, which accepts a `textureSample` under non-uniform control flow;
/// a browser validates with its own compiler, which enforces the rule — and
/// refuses the *whole module*, so one such line takes every pipeline in the
/// file with it. Returns how many modules were refused.
async fn shader_census(gpu: &Gpu) -> usize {
    let mut refused = 0;
    for (name, source) in shader_modules() {
        let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let _module = gpu.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(name),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        match scope.pop().await {
            Some(e) => {
                refused += 1;
                let why = e.to_string();
                // The first line names the site; the rest is the compiler's
                // source excerpt, which the page does not need.
                let why = why.lines().next().unwrap_or("").trim();
                log(&format!("SHADER {name} REFUSED — {why}"));
            }
            None => log(&format!("SHADER {name} ok")),
        }
    }
    refused
}

/// Everything rungs 3–4 draw with. Built once the device arrives.
struct Scene {
    gpu: Gpu,
    raster: Raster,
    retro: Retro,
    /// The scene renders in linear HDR and exactly one pass maps it to the
    /// 8-bit retro target — the engine's own frame shape, not a shortcut past
    /// it. Skipping the chain draws a 16-bit-float pipeline into an 8-bit
    /// target, which a browser refuses per frame and native never sees
    /// because every headless probe keeps the two formats equal.
    post: PostStack,
    /// A frame copied for the page, waiting two frames to be mapped.
    shot: Option<Shot>,
    mesh: MeshId,
    skin_base: u32,
    paint_base: u32,
}

#[derive(Default)]
struct Stats {
    frames: u32,
    ms: Vec<f32>,
    last: Option<f64>,
}

#[derive(Default)]
struct App {
    window: Option<Arc<Window>>,
    scene: Rc<RefCell<Option<Scene>>>,
    stats: Stats,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("floptle web probe")
            .with_inner_size(winit::dpi::LogicalSize::new(CANVAS.0, CANVAS.1))
            .with_append(true);
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log(&format!("RUNG 2 FAILED — no canvas: {e}"));
                return;
            }
        };
        self.window = Some(window.clone());
        let scene = self.scene.clone();
        wasm_bindgen_futures::spawn_local(async move {
            // Rung 2: the renderer's own device path, awaited.
            let gpu = Gpu::new_async(window.clone()).await;
            let info = gpu.adapter.get_info();
            log(&format!(
                "RUNG 2 OK — {:?} adapter \"{}\", surface {}x{} {:?}",
                info.backend, info.name, gpu.config.width, gpu.config.height, gpu.config.format
            ));
            // Rung 3: every shader through this browser's compiler, then the
            // real raster pass and its storage-buffer stores.
            let refused = shader_census(&gpu).await;
            if refused > 0 {
                log(&format!("RUNG 3 FAILED — this browser's WGSL compiler refused {refused} shader module(s)"));
                return;
            }
            let mut raster = Raster::new(&gpu);
            let retro = Retro::new(&gpu, RETRO_H);
            let rig = bar::rig();
            let mesh = raster.register(&gpu, &rig.mesh, None);
            let skin_base = raster.register_skin(&gpu, &rig.joints, &rig.weights);
            let errors = take_gpu_errors();
            if !errors.is_empty() {
                log(&format!(
                    "RUNG 3 FAILED — the device refused the renderer:\n  {}",
                    errors.join("\n  ")
                ));
                return;
            }
            if skin_base == 0 {
                log("RUNG 3 FAILED — the skinning store refused the part");
                return;
            }
            // The generated half of the shader census, now that there is a
            // raster pass to register into.
            let refused = flsl_census(&gpu, &mut raster).await;
            if refused > 0 {
                log(&format!(
                    "RUNG 3 FAILED — this browser's WGSL compiler refused {refused} GENERATED shader(s)"
                ));
                return;
            }
            let paint_base = raster.mesh_paint_base(mesh);
            // The canvas reached its CSS size while the device request was in
            // flight, and the `Resized` that announced it found no scene to
            // apply to — so ask the window now, or the surface stays at the
            // 1x1 it was configured with.
            let size = window.inner_size();
            let mut gpu = gpu;
            let mut retro = retro;
            if (size.width, size.height) != (gpu.config.width, gpu.config.height) {
                gpu.resize(size.width, size.height);
                retro.resize(&gpu, RETRO_H);
            }
            let (rw, rh) = retro.resolution();
            let mut post = PostStack::new(&gpu, rw, rh);
            post.configure(&gpu, rw, rh, true);
            log(&format!(
                "RUNG 3 OK — raster pipelines built; {} vertices with paint, {} joints, retro target {rw}x{rh}",
                rig.mesh.vertices.len(),
                bar::JOINTS
            ));
            *scene.borrow_mut() = Some(Scene { gpu, raster, retro, post, shot: None, mesh, skin_base, paint_base });
            window.request_redraw();
        });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(s) = self.scene.borrow_mut().as_mut() {
                    s.gpu.resize(size.width, size.height);
                    s.retro.resize(&s.gpu, RETRO_H);
                    let (rw, rh) = s.retro.resolution();
                    s.post.resize(&s.gpu, rw, rh);
                }
            }
            WindowEvent::RedrawRequested => {
                self.frame();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }
}

impl App {
    /// Rung 4: one frame — pose the rig, draw it into the retro target through
    /// the raster pass, upscale into the canvas, present.
    fn frame(&mut self) {
        let mut guard = self.scene.borrow_mut();
        let Some(s) = guard.as_mut() else { return };
        let now = now_ms();
        let bend = 0.45 * ((now / 1000.0) as f32 * 1.3).sin();
        let (fallback, palette) = bar::pose(bend);
        s.raster.begin_skin_frame();
        let pose = s.raster.push_skin_pose(s.skin_base, fallback, &palette);

        let eye = Vec3::new(3.4, 1.6, 4.6);
        let fwd = (Vec3::new(0.0, 1.5, 0.0) - eye).normalize();
        let right = fwd.cross(Vec3::Y).normalize();
        let up = right.cross(fwd);
        let rot = Quat::from_mat3(&Mat3::from_cols(right, up, -fwd));
        let cam = RenderCamera::new(
            eye.as_dvec3(),
            rot,
            Projection::Perspective { fov_y: 0.7, near: 0.02, far: 1000.0 },
        );
        let (rw, rh) = s.retro.resolution();
        let mut mp = MaterialParams::flat([1.0, 1.0, 1.0]);
        mp.paint_base = s.paint_base;
        let raw = instance_of_mat(Mat4::from_translation(-eye), &mp);
        let skins = [SkinDraw { mesh: s.mesh, tex: None, instance: raw, pose }];
        let l = Vec3::new(0.5, 0.7, 0.55).normalize();
        let globals = Globals {
            view_proj: cam.view_proj(rw as f32 / rh as f32).to_cols_array_2d(),
            light_dir: [l.x, l.y, l.z, 0.0],
            light_color: [1.0, 0.98, 0.93, 0.0],
            ambient: [0.14, 0.15, 0.20, 0.0],
            ..Default::default()
        };
        s.raster.draw_scene_with(
            &s.gpu,
            s.post.input_view(),
            s.retro.depth_view(),
            globals,
            &[],
            &[],
            &skins,
            Some([0.02, 0.02, 0.05, 1.0]),
            None,
        );
        s.post.run(&s.gpu, &PostSettings::default(), None, s.retro.color_view());
        let Some(frame) = s.gpu.acquire() else { return };
        s.retro.blit(&s.gpu, &frame);
        frame.present();

        for e in take_gpu_errors() {
            log(&format!("GPU ERROR: {e}"));
        }
        if let Some(last) = self.stats.last {
            self.stats.ms.push((now - last) as f32);
        }
        self.stats.last = Some(now);
        self.stats.frames += 1;
        if self.stats.frames == 1 {
            log(&format!(
                "RUNG 4 OK — first frame presented: the skinned, painted bar at {rw}x{rh} into a {}x{} canvas",
                s.gpu.config.width, s.gpu.config.height
            ));
        }
        // Every 30 frames rather than every second: a headless tab runs at
        // whatever cadence it likes, and a readout keyed to wall time could
        // still say nothing when the harness takes its picture.
        // The picture: copied one frame after the first readout, mapped two
        // frames after that — see `capture`.
        if self.stats.frames == 31 {
            s.shot = Some(capture(s));
        }
        if self.stats.frames == 34
            && let Some(shot) = s.shot.take()
        {
            read_shot(shot);
        }
        if self.stats.frames % 30 == 0 {
            let mut v: Vec<f32> = self.stats.ms.drain(..).collect();
            v.sort_by(|a, b| a.total_cmp(b));
            let at = |q: f32| v[((v.len() - 1) as f32 * q) as usize];
            readout(&format!(
                "frame {} · {:.2} ms p50 · {:.2} ms p95 (frame-to-frame, vsync included)",
                self.stats.frames,
                at(0.5),
                at(0.95)
            ));
        }
    }
}

/// A frame copied into a mappable buffer, waiting to be read.
struct Shot {
    buf: wgpu::Buffer,
    width: u32,
    height: u32,
    padded: u32,
    bgra: bool,
}

/// Blit the upscaled frame into a texture this probe owns and copy it to a
/// mappable buffer — the same readback every desktop probe does, because a
/// WebGPU canvas cannot be read reliably from JavaScript (see the page). The
/// map itself happens a couple of frames later, in [`read_shot`]: a map
/// requested in the same task as the copy is legal, but a headless Chromium
/// aborts it ("a valid external Instance reference no longer exists") where a
/// windowed one completes it.
fn capture(s: &Scene) -> Shot {
    let (w, h) = (s.gpu.config.width, s.gpu.config.height);
    let format = s.gpu.surface_format();
    let tex = s.gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe-shot"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    s.retro.blit_to(&s.gpu, &view);
    let bpp = 4u32;
    let padded = (w * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = s.gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe-shot-readback"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder =
        s.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("probe-shot") });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    s.gpu.queue.submit([encoder.finish()]);
    let bgra = matches!(format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
    Shot { buf, width: w, height: h, padded, bgra }
}

/// Map the copied frame and hand its pixels to the page. The callback keeps
/// the buffer alive until the map completes.
fn read_shot(shot: Shot) {
    let Shot { buf, width: w, height: h, padded, bgra } = shot;
    let bpp = 4u32;
    let keep = buf.clone();
    buf.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        if let Err(e) = r {
            log(&format!("CAPTURE FAILED — {e}"));
            return;
        }
        let data = keep.slice(..).get_mapped_range();
        let mut rgba = Vec::with_capacity((w * h * bpp) as usize);
        for y in 0..h {
            let row = (y * padded) as usize;
            for x in 0..w {
                let i = row + (x * bpp) as usize;
                if bgra {
                    rgba.extend_from_slice(&[data[i + 2], data[i + 1], data[i], data[i + 3]]);
                } else {
                    rgba.extend_from_slice(&data[i..i + 4]);
                }
            }
        }
        drop(data);
        keep.unmap();
        page_capture(&rgba, w, h);
    });
}
