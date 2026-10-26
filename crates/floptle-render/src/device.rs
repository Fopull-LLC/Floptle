//! wgpu bootstrap: instance → adapter → device/queue → surface. The single GPU
//! handle the whole renderer threads through (`docs/subsystems/renderer.md`).
//!
//! wgpu is *only* the portability layer — everything above this (the
//! render graph, passes, the SDF/raymarch look) is ours. The lifecycle here
//! (resize, acquire, present) is real; `Gpu::new` is the one call that needs a
//! live window, and a battle-tested implementation already exists in
//! `crates/floptle-proof/src/main.rs` (wgpu 29) — Phase 1 lifts it here.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use winit::window::Window;

/// GPU errors waiting for the host to pick up, plus every message already
/// reported since the device was created.
///
/// The `seen` half matters as much as the queue: a bad pipeline is rejected on
/// every frame the pass runs, so reporting each occurrence would write sixty
/// identical Console lines a second and bury whatever else is wrong. Each
/// distinct message is said once, which is the same bargain the shader
/// compiler's error reporting already makes.
static GPU_ERRORS: Mutex<(Vec<String>, Vec<String>)> = Mutex::new((Vec::new(), Vec::new()));

/// What went wrong, flattened onto one line.
///
/// `Display` already carries the whole "Caused by:" chain — which pass, which
/// pipeline, which attachment — across several indented lines. That is right
/// for a terminal and wrong for a Console row, so the indentation collapses to
/// single spaces and the whole thing becomes one entry. (Walking `source()` to
/// rebuild the chain by hand is redundant: it only reprints what `Display`
/// already said.)
fn describe(e: &wgpu::Error) -> String {
    let text = match e {
        wgpu::Error::OutOfMemory { source } => format!(
            "out of video memory ({source}). Another program is probably holding most of the \
             GPU's memory: close it, or play in a smaller window. What does not fit is left out \
             rather than drawn broken"
        ),
        other => other.to_string(),
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Has any GPU error been reported since the process started? Set once and
/// never cleared. A pass that must not depend on anything that might have
/// failed (the frame's clear) checks it, and pays for its caution only after
/// something has gone wrong.
static GPU_TROUBLE: AtomicBool = AtomicBool::new(false);

/// Has the GPU been out of memory? Every texture or buffer that failed to be
/// made then fails again as "invalid" in every pass that uses it, a few dozen
/// messages a frame that all say the one thing already said.
static OUT_OF_MEMORY: AtomicBool = AtomicBool::new(false);

/// How many times the GPU has said it is out of memory since the process
/// started. A frame that sees it move had an allocation fail somewhere in
/// it, which the profiler names: the failing call is slow (the driver tries
/// to make room first) and lands in no bucket. One relaxed load a frame.
static OUT_OF_MEMORY_EVENTS: AtomicU64 = AtomicU64::new(0);

/// [`OUT_OF_MEMORY_EVENTS`]: how many out-of-memory errors the GPU has
/// reported so far. Compared frame to frame.
pub fn out_of_memory_events() -> u64 {
    OUT_OF_MEMORY_EVENTS.load(Ordering::Relaxed)
}

/// [`GPU_TROUBLE`]: has anything gone wrong on the GPU yet?
pub fn gpu_trouble() -> bool {
    GPU_TROUBLE.load(Ordering::Relaxed)
}

/// This run's file beside a pipeline cache, saying it is using the cache and
/// has not yet saved it back cleanly (see `Gpu::open_pipeline_cache`). One
/// per process, so runs side by side do not count against each other.
#[cfg(not(target_arch = "wasm32"))]
fn cache_marker(file: &std::path::Path) -> std::path::PathBuf {
    file.with_extension(format!("open.{}", std::process::id()))
}

/// Markers left beside `file` by runs that are gone: they used the cache and
/// never saved it back cleanly. A run is gone when its process is (Linux),
/// or elsewhere when its marker is over a minute old, since a healthy run
/// saves within its first frames.
#[cfg(not(target_arch = "wasm32"))]
fn abandoned_cache_markers(file: &std::path::Path) -> Vec<std::path::PathBuf> {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name().and_then(|n| n.to_str())) else { return Vec::new() };
    let prefix = format!("{name}.open.");
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_str()?.to_string();
            let pid: u32 = n.strip_prefix(&prefix)?.parse().ok()?;
            let gone = if cfg!(target_os = "linux") {
                !std::path::Path::new("/proc").join(pid.to_string()).exists()
            } else {
                e.metadata().ok()?.modified().ok()?.elapsed().is_ok_and(|age| age.as_secs() > 60)
            };
            (gone && pid != std::process::id()).then(|| e.path())
        })
        .collect()
}

/// Which graphics backends to consider, honouring `WGPU_BACKEND`.
///
/// **This exists so a backend-specific bug can be reproduced on a machine that
/// would never pick that backend.** Everything here runs on Vulkan in practice;
/// GitHub's runners have no Vulkan driver and fall back to OpenGL, where naga's
/// GLSL output has restrictions the other backends do not — and a failure only
/// CI can see is a failure nobody can fix. `WGPU_BACKEND=gl cargo run …`
/// reproduces it in one command.
///
/// Unset means all of them, which is what shipped and what a player gets.
/// Accepts wgpu's own spellings: `vulkan`, `gl`, `metal`, `dx12`, `primary`.
pub fn backends_from_env() -> wgpu::Backends {
    match std::env::var("WGPU_BACKEND") {
        Ok(v) if !v.trim().is_empty() => {
            let want = wgpu::Backends::from_comma_list(&v);
            if want.is_empty() {
                floptle_say::say_err!("WGPU_BACKEND={v:?} names no backend floptle knows — using all of them");
                wgpu::Backends::all()
            } else {
                want
            }
        }
        _ => wgpu::Backends::all(),
    }
}

/// Record a GPU error for the host to pick up with [`take_gpu_errors`]: the
/// handler [`Gpu::new`] installs on its device. Public so a test's own
/// headless device can report through the same path a game's does.
pub fn report_gpu_error(e: &wgpu::Error) {
    GPU_TROUBLE.store(true, Ordering::Relaxed);
    let oom = matches!(e, wgpu::Error::OutOfMemory { .. });
    let message = describe(e);
    // After running out of memory, "X is invalid" is that same failure seen
    // again from a pass that uses X. Said once, here, and not again.
    let follows_oom = !oom && OUT_OF_MEMORY.load(Ordering::Relaxed) && message.contains("is invalid");
    if oom {
        OUT_OF_MEMORY.store(true, Ordering::Relaxed);
        OUT_OF_MEMORY_EVENTS.fetch_add(1, Ordering::Relaxed);
    }
    if let Ok(mut g) = GPU_ERRORS.lock() {
        let (queue, seen) = &mut *g;
        // The first few still go through: they name what failed to be made.
        // The rest are the same failure seen from every pass that uses it.
        const SHOWN_AFTER_OOM: usize = 3;
        let invalid_said = seen.iter().filter(|m| m.contains("is invalid")).count();
        if follows_oom && invalid_said >= SHOWN_AFTER_OOM {
            const AFTER: &str = "further GPU errors about invalid textures, buffers and bind groups \
                                 follow from running out of video memory, and are not listed";
            if !seen.iter().any(|m| m == AFTER) {
                floptle_say::say_err!("GPU error: {AFTER}");
                seen.push(AFTER.to_string());
                queue.push(AFTER.to_string());
            }
            return;
        }
        if seen.contains(&message) {
            return;
        }
        // Always to the terminal too: somebody running from a shell should see
        // it whether or not a host ever drains the queue.
        floptle_say::say_err!("GPU error: {message}");
        seen.push(message.clone());
        queue.push(message);
    }
}

/// Take the GPU errors reported since the last call — the editor drains this
/// into the Console each frame. Empty when the GPU is happy, which is the
/// normal case and costs one uncontended lock.
pub fn take_gpu_errors() -> Vec<String> {
    GPU_ERRORS.lock().map(|mut g| std::mem::take(&mut g.0)).unwrap_or_default()
}

/// Owns the GPU connection and (when windowed) the surface. `surface` is `None`
/// for a headless GPU — one created without a window for offscreen rendering
/// (tests, bakes, thumbnails). The passes only ever touch device/queue/config, so
/// they work identically either way.
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface: Option<wgpu::Surface<'static>>,
    pub config: wgpu::SurfaceConfiguration,
    depth_tex: wgpu::Texture,
    depth_view: wgpu::TextureView,
    /// The format every SCENE-space pass renders into — the raster, the
    /// raymarch, particles, the 2D light composite, world-space UI, the editor's
    /// grid and gizmo overlays, live render targets, and the post chain's own
    /// scratch. See [`Gpu::scene_format`].
    scene_format: wgpu::TextureFormat,
    /// The present modes this surface actually supports, so a requested one can
    /// fall back rather than fail.
    present_modes: Vec<wgpu::PresentMode>,
    vsync: Vsync,
    /// The format the frame's view has — what every pass that targets the
    /// screen renders into. Equal to `config.format` everywhere a surface
    /// offers an sRGB format directly. A browser's canvas does not: WebGPU
    /// exposes only `Rgba8Unorm`/`Bgra8Unorm` and expects an sRGB *view*
    /// (`view_formats`), so there this is the sRGB sibling of `config.format`
    /// and the swapchain texture is viewed through it. Without that, the
    /// engine's linear output reaches the glass unencoded and every colour
    /// reads dark.
    frame_format: wgpu::TextureFormat,
    /// The driver's compiled pipelines, kept on disk between runs (Vulkan,
    /// where the driver offers it). `None` until
    /// [`Gpu::use_pipeline_cache`], and on a backend without one. Every
    /// pipeline the raster and raymarch passes build is made through it, so a
    /// material shader compiled once compiles from the cache on every run
    /// after: the first frame that draws it waited over a second for the
    /// driver otherwise, on every run.
    pub pipeline_cache: Option<wgpu::PipelineCache>,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pipeline_cache_file: Option<std::path::PathBuf>,
    /// Whether this device's errors go to [`report_gpu_error`] (the window's
    /// device) or panic (a headless probe's): kept so a device made again
    /// (`open_pipeline_cache`) is set up the same way. A browser keeps no
    /// pipeline cache, so never makes its device again.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    reports_errors: bool,
}

/// How finished frames are handed to the display.
///
/// **This is a setting because `Fifo` is not always what it says it is.** Fifo
/// means "present every frame in order at the monitor's cadence", and when a
/// driver honours that it is the right default: the loop blocks in present, so
/// frame times lock to the refresh and what the simulation sampled matches what
/// reaches the glass. On some compositors it instead presents at a *fraction* of
/// the refresh — a window that does nothing but clear itself blue can sit at a
/// flat 20 fps on a 60 Hz display — and with the mode hardcoded there was
/// nothing a project could do about it but conclude the engine was slow.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Vsync {
    /// `Fifo`. Every frame shown, in order, at the display's cadence. The
    /// default, and the one whose pacing is predictable.
    #[default]
    On,
    /// `Mailbox`: render freely, and the display takes the newest frame at each
    /// refresh. No tearing and no cap — but the frames that reach the glass
    /// sampled the simulation at moments unrelated to when they are shown, which
    /// reads as movement judder that comes and goes with the window mode. Worth
    /// it when `On` is capping you far below what the scene actually costs.
    Adaptive,
    /// `Immediate`: present the instant a frame is ready, tearing and all. What
    /// you want when the question is "how expensive is this frame, really".
    Off,
}

/// The wgpu mode a [`Vsync`] asks for, before availability is considered.
fn wanted_mode(v: Vsync) -> wgpu::PresentMode {
    match v {
        Vsync::On => wgpu::PresentMode::Fifo,
        Vsync::Adaptive => wgpu::PresentMode::Mailbox,
        Vsync::Off => wgpu::PresentMode::Immediate,
    }
}

/// …and what the surface can actually do. `Fifo` is required of every surface by
/// the spec, so it is always a valid answer.
///
/// **Asked before the surface is configured, never after.** wgpu treats a mode
/// the surface does not list as a validation error inside `configure`, and its
/// default handler panics: XWayland on an NVIDIA driver offers only `Fifo` and
/// `Immediate`, and a probe that configured `Mailbox` there unchecked ended the
/// process.
fn pick_present_mode(v: Vsync, available: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    let want = wanted_mode(v);
    if available.contains(&want) { want } else { wgpu::PresentMode::Fifo }
}

/// A present mode as `perf.pacing()` and the messages name it.
pub fn present_mode_name(m: wgpu::PresentMode) -> &'static str {
    match m {
        wgpu::PresentMode::Fifo => "fifo",
        wgpu::PresentMode::FifoRelaxed => "fifoRelaxed",
        wgpu::PresentMode::Mailbox => "mailbox",
        wgpu::PresentMode::Immediate => "immediate",
        _ => "auto",
    }
}

/// What to tell the developer when a [`Vsync`] setting could not have the
/// present mode it asks for, or `None` when it got it.
///
/// A fallback the developer is not told about reads as a setting that does
/// nothing: Adaptive chosen, the frame rate unchanged, and no way to see why.
pub fn vsync_fallback_note(want: Vsync, applied: wgpu::PresentMode, available: &[wgpu::PresentMode]) -> Option<String> {
    let asked = wanted_mode(want);
    if applied == asked {
        return None;
    }
    let offered: Vec<&str> = available.iter().map(|m| present_mode_name(*m)).collect();
    let offered = if offered.is_empty() { "nothing it names".to_string() } else { offered.join(", ") };
    let behaves = match applied {
        wgpu::PresentMode::Fifo | wgpu::PresentMode::FifoRelaxed => {
            "every frame waits for the display's refresh, the same as On"
        }
        _ => "frames are presented without waiting for the display",
    };
    Some(format!(
        "frame pacing: {want:?} needs {} presentation, which this display path does not offer (it \
         offers {offered}), so frames are presented with {} instead: {behaves}.",
        present_mode_name(asked),
        present_mode_name(applied),
    ))
}

/// A surface image acquired for one frame. Render into `view`, then `present()`.
pub struct Frame {
    pub surface: wgpu::SurfaceTexture,
    pub view: wgpu::TextureView,
}

impl Frame {
    /// Hand the finished image to the compositor.
    pub fn present(self) {
        self.surface.present();
    }
}

/// The timestamp features, when this adapter has them.
///
/// **Asked for, never required.** They are what lets the editor say where a
/// frame's time went (see [`crate::gpu_timer`]), and a device that cannot offer
/// them must still start — an engine that refused to run on a card without a
/// profiler would be trading the product for the tool. `GpuTimer::new` asks the
/// DEVICE afterwards, so the two can never drift apart.
fn timing_features(adapter: &wgpu::Adapter) -> wgpu::Features {
    // A pipeline cache rides along where the adapter has one: it changes
    // nothing that is drawn, only how long the driver takes to build a
    // pipeline it has built before.
    let want = wgpu::Features::TIMESTAMP_QUERY
        | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS
        | wgpu::Features::PIPELINE_CACHE;
    adapter.features() & want
}

/// Sampled textures (and samplers) per shader stage the engine asks a device for: the
/// lighting the engine binds for every surface, a material shader's own
/// texture slots, and room to grow. wgpu's default is 16, which the engine's
/// own bindings fill on their own.
pub const SAMPLED_TEXTURES_WANTED: u32 = 32;

/// The limits to ask a device for: wgpu's defaults, with as many sampled
/// textures per stage as the adapter offers up to [`SAMPLED_TEXTURES_WANTED`].
/// Never more than it offers, so a device that has fewer still starts, and
/// [`crate::Raster::flsl_texture_slots`] says what a material can use on it.
fn wanted_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
    let base = wgpu::Limits::default();
    let have = adapter.limits();
    let ask = |offered: u32, default: u32| offered.min(SAMPLED_TEXTURES_WANTED).max(default.min(offered));
    wgpu::Limits {
        max_sampled_textures_per_shader_stage: ask(
            have.max_sampled_textures_per_shader_stage,
            base.max_sampled_textures_per_shader_stage,
        ),
        // Every material slot brings its own sampler, so samplers run out
        // alongside textures.
        max_samplers_per_shader_stage: ask(have.max_samplers_per_shader_stage, base.max_samplers_per_shader_stage),
        ..base
    }
}

/// The per-stage sampled-texture and sampler limits a device on this
/// machine's GPU is given, without making one: what `floptle check` holds a
/// shader's texture slots against. `None` with no GPU to ask.
#[cfg(not(target_arch = "wasm32"))]
pub fn adapter_stage_limits() -> Option<(u32, u32)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: backends_from_env(),
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: Default::default(),
        backend_options: Default::default(),
        display: None,
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .ok()?;
    let l = wanted_limits(&adapter);
    Some((l.max_sampled_textures_per_shader_stage, l.max_samplers_per_shader_stage))
}

impl Gpu {
    /// Keep the driver's compiled pipelines in `dir` between runs: read what
    /// an earlier run left, and build every later pipeline through it. A
    /// no-op where the device has no pipeline cache (anything but Vulkan, or
    /// a driver without one). Call before the passes are made, so their own
    /// pipelines come from it too.
    pub fn use_pipeline_cache(&mut self, dir: &std::path::Path) {
        // A browser has neither a disk nor a pipeline cache to keep.
        if cfg!(target_arch = "wasm32") || !self.device.features().contains(wgpu::Features::PIPELINE_CACHE) {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.open_pipeline_cache(dir);
        #[cfg(target_arch = "wasm32")]
        let _ = dir;
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn open_pipeline_cache(&mut self, dir: &std::path::Path) {
        let Some(key) = wgpu::util::pipeline_cache_key(&self.adapter.get_info()) else { return };
        let file = dir.join(key);
        // A driver's cache keeps every pipeline it was ever handed, so one
        // grows with each shader edit; past this it starts again empty.
        const MAX_BYTES: u64 = 256 << 20;
        let fits = std::fs::metadata(&file).is_ok_and(|m| m.len() <= MAX_BYTES);
        // A cache the driver accepts can still be one it fails on: one saved
        // while the driver was failing to map memory left every device that
        // loaded it making invalid resources from the first texture on, so
        // nothing on the machine would start. The marker says a run is using
        // this cache and has not yet saved it back cleanly; found at start,
        // the run before never did, and its cache is not trusted. A bad
        // cache can cost one start, never every start.
        let abandoned = abandoned_cache_markers(&file);
        let suspect = !abandoned.is_empty();
        if suspect {
            let _ = std::fs::remove_file(&file);
            for m in &abandoned {
                let _ = std::fs::remove_file(m);
            }
            floptle_say::say_err!(
                "the GPU pipeline cache was discarded: the last run that used it never got as far as a clean frame. \
                 Shaders build again from scratch this time"
            );
        }
        let data = if fits && !suspect { std::fs::read(&file).ok() } else { None };
        let marker = cache_marker(&file);
        if let Some(dir) = marker.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&marker, b"");
        // SAFETY: the data is only ever what `save_pipeline_cache` wrote from
        // `PipelineCache::get_data` for this adapter's key, and with
        // `fallback` a cache the driver will not take (another driver
        // version, a damaged file) is replaced by an empty one rather than
        // failing.
        let cache = unsafe {
            self.device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor {
                label: Some("floptle-pipelines"),
                data: data.as_deref(),
                fallback: true,
            })
        };
        // A cache the driver takes can still leave it unable to make
        // anything: one saved while it was failing to map memory did. Asked
        // at once, with one small texture, rather than learned from a frame
        // of invalid everything.
        if !self.device_makes_textures() {
            drop(cache);
            let _ = std::fs::remove_file(&file);
            let _ = std::fs::remove_file(cache_marker(&file));
            // The device stays broken without it: ask the adapter for another.
            let healthy = self.make_device_again() && self.device_makes_textures();
            floptle_say::say_err!(
                "the GPU pipeline cache left the graphics driver unable to make textures, so it was thrown \
                 away{}",
                if healthy { "" } else { ". The GPU still cannot make them: restart, and if that does not help, reboot" }
            );
            return;
        }
        self.pipeline_cache = Some(cache);
        self.pipeline_cache_file = Some(file);
    }

    /// Replace the device and queue with new ones from the same adapter, with
    /// the same features and limits, and set up what hangs off them (the
    /// surface, the depth target). Only before anything else is made from the
    /// device: whatever was made from the old one is not made again.
    #[cfg(not(target_arch = "wasm32"))]
    fn make_device_again(&mut self) -> bool {
        let desc = wgpu::DeviceDescriptor {
            label: Some("floptle-device"),
            required_features: self.device.features(),
            required_limits: self.device.limits(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        };
        let Ok((device, queue)) = pollster::block_on(self.adapter.request_device(&desc)) else { return false };
        if self.reports_errors {
            device.on_uncaptured_error(Arc::new(|e: wgpu::Error| report_gpu_error(&e)));
        }
        self.device = device;
        self.queue = queue;
        self.resize(self.config.width, self.config.height);
        true
    }

    /// Can the device still make a texture and a view of it? Asked inside its
    /// own error scopes, so a failure is answered here and reported nowhere.
    #[cfg(not(target_arch = "wasm32"))]
    fn device_makes_textures(&self) -> bool {
        let validation = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let memory = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let t = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("floptle-canary"),
            size: wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        drop(t.create_view(&wgpu::TextureViewDescriptor::default()));
        let oom = pollster::block_on(memory.pop());
        let invalid = pollster::block_on(validation.pop());
        oom.is_none() && invalid.is_none()
    }

    /// Write the pipeline cache back to its file, on a worker thread, through
    /// a temporary file so a run killed mid-write leaves the old one, and
    /// mark it trusted for the next start (see `open_pipeline_cache`). Not
    /// after any GPU error, when the cache is thrown away instead.
    ///
    /// Call after new pipelines were built; it copies the whole cache, so not
    /// every frame.
    pub fn save_pipeline_cache(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        self.write_pipeline_cache();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn write_pipeline_cache(&self) {
        let (Some(cache), Some(file)) = (&self.pipeline_cache, &self.pipeline_cache_file) else { return };
        // After any GPU error this session the cache is not saved, and what
        // was loaded is not kept: the driver may have been failing, and a
        // cache written then is the one that broke every start after it.
        if gpu_trouble() {
            let _ = std::fs::remove_file(file);
            let _ = std::fs::remove_file(cache_marker(file));
            return;
        }
        // A clean frame came first, so the next start may trust this cache:
        // said here, not on the worker, so a run that exits before the worker
        // finishes still says it (a write cut short leaves the old file).
        let _ = std::fs::remove_file(cache_marker(file));
        let Some(data) = cache.get_data() else { return };
        let file = file.clone();
        let write = move || {
            if let Some(dir) = file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = file.with_extension("tmp");
            if std::fs::write(&tmp, &data).is_ok() {
                let _ = std::fs::rename(&tmp, &file);
            }
        };
        if std::thread::Builder::new().name("floptle-pipeline-cache".into()).spawn(write.clone()).is_err() {
            write();
        }
    }

    /// Create the GPU connection for `window` and configure its surface. Picks a
    /// high-performance adapter, an sRGB surface format when available, and Mailbox
    /// present mode (low-latency) falling back to Fifo (vsync). Lifted from the
    /// proof's proven wgpu-29 bootstrap.
    ///
    /// Blocks on the adapter and device requests. That is fine on every desktop
    /// target and impossible in a browser, where the same requests are real
    /// promises and blocking the only thread deadlocks the tab — a web build
    /// awaits [`Gpu::new_async`] instead. One body, two ways to wait for it.
    pub fn new(window: Arc<Window>) -> Self {
        pollster::block_on(Self::new_async(window))
    }

    /// [`Gpu::new`] as a future: the same adapter, device, surface and depth
    /// setup, awaited rather than blocked on. The browser's device path — the
    /// canvas is the window, `request_adapter` is a JS promise — and the only
    /// difference from the desktop is who waits.
    pub async fn new_async(window: Arc<Window>) -> Self {
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: backends_from_env(),
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });
        let surface = instance.create_surface(window).expect("create surface");

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .expect("no compatible GPU adapter");

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("floptle-device"),
                required_features: timing_features(&adapter),
                required_limits: wanted_limits(&adapter),
                experimental_features: wgpu::ExperimentalFeatures::default(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            })
            .await
            .expect("no GPU device");
        // A GPU validation error must not end the session: wgpu's default
        // handler panics, the unwind runs through a frame holding a surface
        // texture, the swapchain destructor panics in turn, and the process
        // aborts with everything unsaved gone. So it is recorded, the frame
        // kept, and the editor says so. Not installed on the headless path
        // (`headless_with`): a probe that swallowed a validation error would
        // report a pass it never made.
        device.on_uncaptured_error(Arc::new(|e: wgpu::Error| report_gpu_error(&e)));

        let caps = surface.get_capabilities(&adapter);
        let format =
            caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        // No sRGB surface format at all (a browser canvas): render through an
        // sRGB view of the non-sRGB swapchain instead. See `frame_format`.
        let frame_format = if format.is_srgb() { format } else { format.add_srgb_suffix() };
        let view_formats = if frame_format == format { vec![] } else { vec![frame_format] };
        // Fifo (classic vsync) is the default — see [`Vsync`] for
        // why, and for the other choices.
        let present_modes = caps.present_modes.clone();
        let vsync = Vsync::default();
        let present_mode = pick_present_mode(vsync, &present_modes);
        // **The swapchain image is SAMPLED, not only drawn into.** The standalone
        // player renders the game straight to the surface, and the game-UI pass
        // captures whatever is already there as its backdrop (what a `backdrop()`
        // UI shader frosts). Without `TEXTURE_BINDING` that capture is a
        // validation error every frame and the UI never composites — which is
        // exactly how it failed the first time a player drew into a real
        // swapchain, where the editor's offscreen target had always had the flag.
        // Asked for only where the surface offers it, so a device that cannot
        // sample its own swapchain still starts.
        // `COPY_SRC` for the same reason, one step further on: it is what lets a
        // build photograph the frame it actually presented (`floptle-player
        // --shot`), rather than a second render that only resembles it.
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT
            | (caps.usages
                & (wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC));
        let config = wgpu::SurfaceConfiguration {
            usage,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats,
        };
        surface.configure(&device, &config);

        let (depth_tex, depth_view) = Self::make_depth(&device, config.width, config.height);

        Self {
            instance,
            adapter,
            device,
            queue,
            surface: Some(surface),
            config,
            depth_tex,
            depth_view,
            scene_format: Self::HDR_FORMAT,
            frame_format,
            pipeline_cache: None,
            reports_errors: true,
            pipeline_cache_file: None,
            present_modes,
            vsync,
        }
    }

    /// How this GPU is presenting, and how to change it.
    ///
    /// Changing it reconfigures the surface, which is cheap and takes effect on
    /// the next frame. A mode the surface does not support falls back to `Fifo`
    /// rather than failing — every surface supports `Fifo`.
    pub fn vsync(&self) -> Vsync {
        self.vsync
    }

    /// Whether `mode` is actually available here. The Inspector greys out what
    /// this says no to, rather than offering a choice that silently does nothing
    /// — which is how "I turned it off and it did not change" happens.
    pub fn supports_vsync(&self, mode: Vsync) -> bool {
        self.surface.is_some() && self.present_modes.contains(&wanted_mode(mode))
    }

    /// The present modes the surface offers; empty without one.
    pub fn present_modes(&self) -> &[wgpu::PresentMode] {
        &self.present_modes
    }

    /// Returns the wgpu mode actually applied — which is not always the one
    /// asked for, since a surface need only support `Fifo`. The caller reports
    /// it, so a setting that quietly did nothing is visible instead of being
    /// mistaken for a setting that did not help.
    pub fn set_vsync(&mut self, mode: Vsync) -> Option<wgpu::PresentMode> {
        if self.vsync == mode || self.surface.is_none() {
            return None;
        }
        self.vsync = mode;
        self.config.present_mode = pick_present_mode(mode, &self.present_modes);
        if let Some(surface) = self.surface.as_ref() {
            surface.configure(&self.device, &self.config);
        }
        Some(self.config.present_mode)
    }

    /// Create a headless GPU (no window/surface) for offscreen rendering at
    /// `width`×`height`. `config` carries the same sRGB format the windowed path
    /// uses, so pipelines built against `surface_format()` render identically; the
    /// caller supplies its own color target (a texture with `COPY_SRC`) to read
    /// back. Used by render tests and tools.
    pub fn headless(width: u32, height: u32) -> Self {
        Self::headless_with(width, height, None, None)
    }

    /// [`headless`](Self::headless) on a device asked for exactly `textures`
    /// sampled textures and `samplers` samplers per shader stage (no more than
    /// the adapter offers), so a test can stand at a smaller GPU's limit on a
    /// bigger one.
    #[doc(hidden)]
    pub fn headless_with_stage_limits(width: u32, height: u32, textures: u32, samplers: u32) -> Self {
        Self::headless_with(width, height, None, Some((textures, samplers)))
    }

    /// [`headless`](Self::headless) with the HDR scene format the windowed path
    /// uses — for a probe that means to exercise the pipeline as it SHIPS.
    ///
    /// The plain `headless` keeps the 8-bit surface format for the
    /// scene as well, because forty render probes read their target back as
    /// tightly-packed RGBA8 and none of them is about the format. This is the
    /// opt-in for the ones that are.
    pub fn headless_hdr(width: u32, height: u32) -> Self {
        Self::headless_with(width, height, Some(Self::HDR_FORMAT), None)
    }

    fn headless_with(width: u32, height: u32, scene: Option<wgpu::TextureFormat>, stage: Option<(u32, u32)>) -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: backends_from_env(),
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .expect("no GPU adapter (headless)");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("floptle-device-headless"),
            // The same ask the window makes: timestamp queries when the adapter
            // has them, so `floptle shot --timing` can name a pass. An
            // intersection with what the adapter offers, so it cannot refuse a
            // device that lacks them — see `timing_features`.
            required_features: timing_features(&adapter),
            required_limits: {
                let mut l = wanted_limits(&adapter);
                if let Some((textures, samplers)) = stage {
                    let have = adapter.limits();
                    l.max_sampled_textures_per_shader_stage = textures.min(have.max_sampled_textures_per_shader_stage);
                    l.max_samplers_per_shader_stage = samplers.min(have.max_samplers_per_shader_stage);
                }
                l
            },
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("no GPU device (headless)");
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };
        let (depth_tex, depth_view) = Self::make_depth(&device, config.width, config.height);
        let scene_format = scene.unwrap_or(config.format);
        Self {
            instance,
            adapter,
            device,
            queue,
            surface: None,
            depth_tex,
            depth_view,
            scene_format,
            frame_format: config.format,
            pipeline_cache: None,
            reports_errors: false,
            pipeline_cache_file: None,
            config,
            // Headless has no surface, so no presentation to configure.
            present_modes: Vec::new(),
            vsync: Vsync::On,
        }
    }

    /// The depth format the renderer uses everywhere (always available as a depth
    /// attachment; matches wgpu's `0..1` reverse-Z-free convention with `Less`).
    pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

    /// Build a depth target sized to the surface. Recreated on every resize so it
    /// can never desync from the swapchain (a size mismatch is a hard validation
    /// error at draw time). `TEXTURE_BINDING` so post passes (SSAO) can sample it;
    /// `COPY_DST` so the opaque depth prepass can prime it (see `Raster`).
    fn make_depth(device: &wgpu::Device, width: u32, height: u32) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }

    /// The depth view passes attach for depth testing.
    pub fn depth_view(&self) -> &wgpu::TextureView {
        &self.depth_view
    }

    /// The depth texture behind [`depth_view`](Self::depth_view) — the copy target
    /// when the opaque depth prepass primes the frame's depth buffer.
    pub fn depth_texture(&self) -> &wgpu::Texture {
        &self.depth_tex
    }

    /// The format every pass that targets the screen renders into: the
    /// swapchain's own format, or — where the surface offers no sRGB format,
    /// as a browser canvas does not — the sRGB view the frame is seen through.
    /// See `frame_format`.
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.frame_format
    }

    /// The format every SCENE-space pass renders into, which is **not** the
    /// surface format.
    ///
    /// A window's surface is 8-bit sRGB: nothing brighter than white fits, and
    /// a channel that clips shifts the hue toward the other two. A sunlit wall
    /// and a bulb ten times brighter would store as the same pixel, leaving
    /// bloom and exposure nothing to work with. So the scene renders into a
    /// floating-point target and stays in linear light through the whole post
    /// chain, and exactly one pass maps it to the display
    /// ([`PostSettings::tonemap`](crate::PostSettings)).
    ///
    /// Windowed rendering uses `Rgba16Float`. A headless GPU keeps the surface
    /// format unless it was built by [`headless_hdr`](Self::headless_hdr), so
    /// the render probes go on reading 8-bit RGBA the way they always have.
    pub fn scene_format(&self) -> wgpu::TextureFormat {
        self.scene_format
    }

    /// The scene format the windowed path uses. Half floats rather than full:
    /// a 16-bit float holds ~5 decimal digits and reaches 65504, which is far
    /// past any light anybody sets, at half the bandwidth of `Rgba32Float`.
    pub const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

    /// Reconfigure the surface after the window resizes. Clamps to a minimum of 1
    /// so a minimized window (0×0) doesn't produce an invalid configuration, and
    /// rebuilds the depth target to match.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        if let Some(surface) = self.surface.as_ref() {
            surface.configure(&self.device, &self.config);
        }
        let (depth_tex, depth_view) = Self::make_depth(&self.device, self.config.width, self.config.height);
        self.depth_tex = depth_tex;
        self.depth_view = depth_view;
    }

    /// Acquire the next swapchain image to render into. Returns `None` on a
    /// transient surface state (Outdated/Lost — reconfigured here) or failure, in
    /// which case the caller simply skips the frame.
    pub fn acquire(&mut self) -> Option<Frame> {
        use wgpu::CurrentSurfaceTexture as C;
        let surface = self.surface.as_ref()?;
        let surface = match surface.get_current_texture() {
            C::Success(t) | C::Suboptimal(t) => t,
            C::Outdated | C::Lost => {
                surface.configure(&self.device, &self.config);
                return None;
            }
            _ => return None,
        };
        let view = surface.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.frame_format),
            ..Default::default()
        });
        Some(Frame { surface, view })
    }

    /// Clear a frame to a solid linear-RGBA color — the minimal Phase-1 render so
    /// the window proves the whole window→device→loop→present path. The render
    /// graph + real passes supersede this in Phase 4; it keeps `wgpu` out of the
    /// runtime in the meantime.
    pub fn clear(&self, frame: &Frame, color: [f64; 4]) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("clear") });
        {
            let _rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: color[0],
                            g: color[1],
                            b: color[2],
                            a: color[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.queue.submit([encoder.finish()]);
    }
}

#[cfg(test)]
mod present_mode_tests {
    use super::{Vsync, pick_present_mode, vsync_fallback_note};
    use wgpu::PresentMode as P;

    /// **A mode the surface does not offer falls back before `configure`
    /// sees it, and says so.** XWayland on NVIDIA offers `[Fifo, Immediate]`;
    /// configuring `Mailbox` there is a wgpu validation panic.
    #[test]
    fn adaptive_without_mailbox_falls_back_to_fifo_and_says_why() {
        let xwayland = [P::Fifo, P::Immediate];
        let applied = pick_present_mode(Vsync::Adaptive, &xwayland);
        assert_eq!(applied, P::Fifo);
        let said = vsync_fallback_note(Vsync::Adaptive, applied, &xwayland).expect("a silent fallback");
        assert!(said.contains("Adaptive needs mailbox presentation"), "{said}");
        assert!(said.contains("it offers fifo, immediate"), "{said}");
        assert!(said.contains("presented with fifo instead"), "{said}");
    }

    /// A mode that is offered is used, and there is nothing to say.
    #[test]
    fn an_offered_mode_is_used_and_says_nothing() {
        let wayland = [P::Mailbox, P::Fifo, P::Immediate];
        for v in [Vsync::On, Vsync::Adaptive, Vsync::Off] {
            let applied = pick_present_mode(v, &wayland);
            assert_eq!(vsync_fallback_note(v, applied, &wayland), None, "{v:?} -> {applied:?}");
        }
        assert_eq!(pick_present_mode(Vsync::Adaptive, &wayland), P::Mailbox);
    }
}
