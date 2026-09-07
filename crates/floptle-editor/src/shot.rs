//! `floptle shot` — **what does it look like?**
//!
//! Renders a scene to a PNG with no window, through the editor's own offscreen
//! path. This is the verb that turns "I cannot see" into "I can look", and it is
//! the other half of `run`: one says whether the project works, this one says
//! what it looks like while it does.
//!
//! ## It is `render_world_into`, not a third gather
//!
//! Every view that is not the Scene view already comes through
//! `Editor::render_world_into` — the docked Game panel, camera previews, render
//! targets, the GI bake. This is one more of those, and that is not a
//! convenience: `offscreen_draws_the_same_world` exists because the editor's two
//! gathers have drifted apart five times, each time with the same symptom — the
//! thing is right there in one view and missing from the other. A `shot` that
//! drew a slightly different world would be worse than no `shot` at all,
//! because its entire value is being believed.
//!
//! ## The whole chain, not the tonemap
//!
//! Post-processing is the project's look, so a picture without it is a picture
//! of a different game: the scene's own `PostProcess` node, the depth-of-field
//! focus resolved against the scene, screen ambient occlusion, any `stage post`
//! shaders it compiled, and — because a pixel-art project composites at its own
//! resolution and upscales — the retro presentation. This passed the tonemap
//! alone for a while and defaulted the rest, which is a quiet way of being
//! wrong: the picture still looks like a picture.
//!
//! Two are left out on purpose. **Motion blur** needs a previous frame and this
//! is a single one. **The accessibility filters** are one person's display
//! setting, and a PNG of a project should not carry them.
//!
//! ## What it shows
//!
//! The scene's **active camera**, because that is the view the game has. A
//! different one by name with `--camera`. A scene with no camera has no view,
//! and says so rather than inventing one — an angle picked by a tool is a
//! picture of the tool's opinion.
//!
//! It renders one frame of an *unplayed* scene: nothing has moved, no `start`
//! has run. That is the right default for "what did my edit do", and it is why
//! this is a separate verb from `run` rather than a flag on it.

use std::path::{Path, PathBuf};

use floptle_core::math::DVec3;
use floptle_core::Matter;
use floptle_render::{Gpu, Projection, RenderCamera};

/// Pick the camera to look through.
fn find_camera(
    ed: &crate::Editor,
    named: Option<&str>,
) -> Option<(floptle_core::Entity, f32, u32, bool, f32)> {
    let mut best: Option<(floptle_core::Entity, f32, u32, bool, f32)> = None;
    for (e, m) in ed.world.query::<Matter>() {
        let Matter::Camera { fov_y, cull_mask, ortho, ortho_height, active, .. } = m else {
            continue;
        };
        let this = (e, *fov_y, *cull_mask, *ortho, *ortho_height);
        match named {
            // By name: an exact match, and nothing else will do.
            Some(want) => {
                if ed.world.get::<floptle_core::Name>(e).is_some_and(|n| n.0 == want) {
                    return Some(this);
                }
            }
            // Otherwise the active one, falling back to the first camera there
            // is — a scene with one inactive camera still has an obvious view,
            // and refusing it would be pedantry.
            None => {
                if *active {
                    return Some(this);
                }
                best.get_or_insert(this);
            }
        }
    }
    // Only the unnamed path ever fills `best` — a named camera either matched
    // above or is not here — so this is the fallback and nothing else.
    best
}

/// Play an already-opened project for `seconds`, and leave it playing.
///
/// `None` means it never entered Play at all. Otherwise the answer is `play_t`
/// — the clock the SCRIPTS read, not `steps × DT`, because a step is not a
/// promise that anything moved: a session held at the Play-start terrain hold
/// steps happily with `dt = 0`, and reporting the span that was asked for is
/// how `run` once published sixty seconds of simulation it had not done
/// (`floptle/0157`).
///
/// It deliberately does **not** stop afterwards. `toggle_play` restores the
/// scene to how it was authored, which would undo the entire point: the picture
/// is of the live session.
fn play_for(ed: &mut crate::Editor, seconds: f32, anchor: DVec3) -> Option<f32> {
    // How long this is allowed to spend waiting on the background terrain
    // threads, in TOTAL — the same budget `shot` already gives its pre-render
    // settle, for the same reason: a world that never finishes streaming must
    // end in a picture and a warning rather than in a hang.
    const STREAM_BUDGET: std::time::Duration = std::time::Duration::from_secs(45);
    let deadline = floptle_core::time::Instant::now() + STREAM_BUDGET;

    // **Load the world around the view before pressing Play.** Play holds the
    // fixed tick until the ground exists, and a held session is a PAUSED one:
    // it steps happily with `dt = 0`, so scripts see no time pass and nothing
    // moves. Outside Play residency anchors on the editor camera, so settling
    // here is what puts terrain under the session before it starts.
    ed.settle_world_streaming(anchor, STREAM_BUDGET);

    ed.toggle_play();
    if !ed.playing {
        return None;
    }
    // Rounded and never zero: `--after 0.001` asking for no simulation at all
    // would be a confusing way to spell `shot`.
    let steps = ((seconds / crate::run::DT).round() as i64).clamp(1, u32::MAX as i64) as u32;
    for _ in 0..steps {
        // In the loop for the reason `run` documents at length: without it the
        // Play-start terrain hold never lifts, and a held session is a PAUSED
        // one — no fixed tick, so no rails, no physics, and a `dt` of zero
        // handed to every script.
        ed.pump_world_streaming();
        // **A headless loop has no wall clock, and the terrain workers need
        // one.** A windowed frame takes about sixteen milliseconds, which is
        // when the background threads get their work done; these steps run back
        // to back in microseconds, so a world that streams during Play — a
        // planet the ship is approaching — never finishes, the hold never lifts
        // and the whole span is stepped without being simulated. Giving the
        // threads the time they need is the difference between a picture of the
        // game and a picture of the loading screen.
        while ed.terrain_worker_busy() && floptle_core::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(4));
            ed.pump_world_streaming();
        }
        ed.play_step(crate::run::DT, true);
        ed.drain_script_logs();
        // A script that asked to quit has said the session is over, and stepping
        // past it would photograph a world nobody is in.
        if !ed.playing {
            break;
        }
    }
    Some(ed.play_t)
}

/// What the verb was asked for. A struct rather than eight positional
/// arguments, the same shape `vfx_shot::Args` uses and for the same reason.
pub(crate) struct Args<'a> {
    pub(crate) root: &'a Path,
    pub(crate) scene: Option<&'a str>,
    pub(crate) camera: Option<&'a str>,
    pub(crate) size: (u32, u32),
    pub(crate) out: &'a Path,
    pub(crate) json: bool,
    pub(crate) timing: bool,
    /// `--after`: play the project for this many seconds BEFORE drawing.
    ///
    /// `None` is the authored frame — nothing has moved and no `start` has run,
    /// which is the right answer to "what did my edit do". `Some` is the frame a
    /// player would be looking at, which for a game that BUILDS ITS WORLD AT
    /// RUNTIME is the only one worth photographing: the solar project's scene
    /// file holds a generator, a camera and some UI, so every shot of it was a
    /// bare sphere under a black sky — a true picture of the file and a picture
    /// of nothing anybody plays (`floptle/0170`).
    pub(crate) after: Option<f32>,
    /// `--seed`: pin the game's randomness, so a project that generates its
    /// world produces the same picture twice.
    pub(crate) seed: Option<u32>,
}

/// Run the verb. Returns the process exit code.
pub(crate) fn run(args: Args) -> i32 {
    let Args { root, scene, camera, size, out, json, timing, after, seed } = args;
    if !root.join("project.ron").is_file() {
        eprintln!("{} is not a project directory (no project.ron)", root.display());
        return 2;
    }
    let (w, h) = (size.0.max(1), size.1.max(1));

    // The GPU FIRST, then the project. The windowed editor loads the scene
    // before it has a device and adopts the GPU-side halves afterwards; here
    // there is no such ordering to satisfy, and doing it this way round means
    // `open_project`'s own model import and paint adoption find a device
    // instead of bailing.
    let gpu = Gpu::headless_hdr(w, h);
    // **A driver that cannot run the engine's shaders is not a defect in the
    // engine, and must not be reported as one.**
    //
    // Without a handler, wgpu's validation failures reach the default one,
    // which panics — and this binary writes a crash report on panic, so a
    // machine whose adapter simply cannot build the renderer told its owner to
    // open a GitHub issue. That is the same shape as `inspect | head` and
    // `--size 20000x20000`, and the third time it has come up.
    //
    // It **exits** rather than recording and carrying on, which is the opposite
    // of what the windowed editor's handler does and is deliberate: there, a
    // person is looking at a window and one bad pass should not take the
    // session down. Here the only output is a picture, and a picture made after
    // a pass failed is a picture that lies. See `Gpu::headless_with`, which
    // installs no handler at all for the same reason in reverse — a probe must
    // never swallow one.
    gpu.device.on_uncaptured_error(std::sync::Arc::new(|e: wgpu::Error| {
        eprintln!("this machine's graphics driver could not build the renderer, so there is no \
                   picture to write:\n  {e}");
        // A guess, offered as one. It is the cause on every machine this has
        // been seen on — the raster pipeline binds one palette texture to a
        // filtering sampler and a nearest one, which OpenGL forbids — but the
        // handler cannot know that from here, and a confident wrong cause is
        // worse than a hint.
        eprintln!(
            "if this machine has only an OpenGL adapter, that is the likely cause: floptle's \
             shaders need Vulkan, Metal or DirectX 12."
        );
        std::process::exit(1);
    }));
    // **The Console has to go somewhere.** `run` and `exec` publish theirs as
    // the report; this verb's answer is a picture, so anything the editor says
    // while making it — a scene that failed to load, a device missing the
    // pieces a scene render binds — would be written into a buffer nobody ever
    // reads. That last one is the diagnostic added *because* this verb once
    // wrote a black PNG and exited 0. stderr, so `--json` still owns stdout.
    let mut ed = crate::Editor {
        show_gizmos: false,
        console: crate::console::ConsoleState { mirror_to_stderr: true, ..Default::default() },
        ..Default::default()
    };
    ed.attach_gpu(gpu);
    // `--timing` on a device with no timestamp queries is a request that
    // cannot be met, and saying so beats a PNG with no numbers beside it.
    if timing && ed.gpu_timer.is_none() {
        eprintln!(
            "this device has no GPU timestamp queries, so --timing has nothing to measure; \
             the picture is still rendered"
        );
    }
    ed.gpu_timing_headless = timing && ed.gpu_timer.is_some();
    ed.open_project(root.to_path_buf());
    if let Some(s) = scene {
        let Some(path) = crate::inspect::resolve_scene(root, s) else {
            eprintln!("no scene called {s} under {}", root.join("scenes").display());
            return 1;
        };
        ed.open_scene_file(&path.to_string_lossy());
    }

    // **Baked global illumination is uploaded by the frame loop, and this has
    // no frame loop.** `open_project` reads the `.fgi` beside the scene and
    // marks it dirty; the upload happens on the next frame the editor draws, so
    // a one-shot render skipped it entirely and photographed a scene with its
    // bounced light missing. That is the same shape as the post chain: an
    // effect absent from a picture whose whole promise is being the editor's.
    ed.refresh_gi();

    // **`--after`: play the world, then photograph it.**
    //
    // `run` has the played world and writes no picture; `shot` writes a picture
    // and has no played world. Neither half was missing — they were just not
    // joined, and a game that builds its world on the first frames of play could
    // therefore not be looked at at all. That matters more than a missing
    // convenience: this project's own working rule is to verify anything visual
    // by rendering a PNG and looking at it, and a runtime-generated game could
    // not follow it (`floptle/0170`).
    //
    // The same fixed `DT` `run` steps by, off the wall clock, so two runs of one
    // project produce the same picture. `pump_world_streaming` is in the loop
    // for the reason `run` documents at length: without it the Play-start
    // terrain hold never lifts, and a held session is a PAUSED one that steps
    // happily with `dt = 0` — it would report the span and simulate none of it.
    if let Some(seconds) = after {
        if let Some(seed) = seed {
            ed.script_host.set_seed(seed);
        }
        // Anchored on the camera the FILE names, which is the only presence
        // the world has before anything has run.
        let anchor = find_camera(&ed, camera)
            .map(|(e, ..)| floptle_core::world_transform(&ed.world, e).translation)
            .unwrap_or(DVec3::ZERO);
        let Some(played) = play_for(&mut ed, seconds, anchor) else {
            eprintln!("the project did not enter play mode, so there is nothing to photograph");
            return 1;
        };
        // The clock the scripts themselves read, so the line cannot disagree
        // with them about how much of the span actually ran.
        if !json {
            eprintln!("played {played:.2}s before drawing");
        }
    }

    // **After the play span, deliberately.** A game that switches its own active
    // camera during play must be photographed through the one the GAME chose,
    // not the one the file did — for a runtime-built world that is the whole
    // point, since the camera that takes over does so on the first frame.
    // `--camera` still names one and still wins.
    let Some((e, fov_y, cull_mask, ortho, ortho_height)) = find_camera(&ed, camera) else {
        match camera {
            Some(name) => eprintln!("this scene has no camera called {name}"),
            None => eprintln!(
                "this scene has no camera, so there is no view to render — add one, or name \
                 another scene with --scene"
            ),
        }
        return 1;
    };

    let wt = floptle_core::world_transform(&ed.world, e);
    let cam = RenderCamera::new(
        wt.translation,
        wt.rotation,
        Projection::of_camera(fov_y, ortho, ortho_height, 0.05, 300_000.0),
    );

    // **Stream the world in before photographing it.** Celestial terrain loads
    // on a background thread and meshes on the frame that follows, and this verb
    // has neither — so every planet drew as its impostor sphere: a smooth ball
    // where the ground, the scattered rock and the base were meant to be. It is
    // the failure this verb can least afford, because the picture still looks
    // like a picture. Anchored on the camera being photographed, since that is
    // the presence in the world here.
    if !ed.settle_world_streaming(wt.translation, std::time::Duration::from_secs(45)) {
        eprintln!(
            "warning: the world was still streaming after 45s — some terrain in this \
             shot is drawn as its impostor sphere rather than its surface"
        );
    }
    ed.sync_terrain_gpu();
    // **Map geometry is authored, not generated — but it still has to reach the
    // GPU somehow.** `sync_map_meshes` (self-heal + triangulate + upload) and
    // `sync_map_paint` (re-attach paint to whatever survived the last edit) are
    // both per-frame housekeeping the windowed editor's `render()` runs before
    // every draw. This verb has no such frame, so without them a level's every
    // `MapMesh` node points at a mesh registry entry that was never built: the
    // walls, floor and ceiling are silently absent and the shot is the props and
    // the character floating over flat grey (`floptle/0166`).
    ed.sync_map_meshes();
    ed.sync_map_paint();
    let Some(pixels) = render_frame_pixels(&mut ed, &cam, w, h, cull_mask) else {
        eprintln!("no GPU: this machine has no adapter floptle can render on");
        return 1;
    };
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty())
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("could not create {}: {e}", parent.display());
        return 1;
    }
    let Some(buf) = image::RgbaImage::from_raw(w, h, pixels) else {
        eprintln!("the render came back the wrong size");
        return 1;
    };
    if let Err(e) = buf.save(out) {
        eprintln!("could not write {}: {e}", out.display());
        return 1;
    }

    // Per-pass GPU cost, when asked. Absent — not zeroed — when it was not: a
    // `gpu_ms: 0` would read as "free", which is the wrong answer in exactly
    // the shape the `perf` API exists to refuse.
    let gpu_timing: Option<(f32, Vec<(String, f32)>)> = if ed.gpu_timing_headless {
        ed.gpu_timer.as_mut().map(|t| {
            t.poll();
            (t.total_ms(), t.spans().iter().map(|s| (s.label.clone(), s.ms)).collect())
        })
    } else {
        None
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "path": out.to_string_lossy(),
                "width": w,
                "height": h,
                "camera": ed.world.get::<floptle_core::Name>(e).map(|n| n.0.clone()),
                "timing": gpu_timing.as_ref().map(|(total, passes)| serde_json::json!({
                    "gpu_ms": total,
                    "passes": passes
                        .iter()
                        .map(|(l, ms)| serde_json::json!({ "label": l, "ms": ms }))
                        .collect::<Vec<_>>(),
                })),
            })
        );
    } else {
        println!("wrote {} ({w}x{h})", out.display());
        if let Some((total, passes)) = &gpu_timing {
            println!("gpu {total:.2} ms across {} passes at {w}x{h}:", passes.len());
            for (label, ms) in passes {
                println!("  {label:<20} {ms:7.3} ms");
            }
        }
    }
    0
}

/// Draw one frame of `ed`'s world from `cam` and read it back as RGBA8.
///
/// **The whole presentation, not the tonemap**: the project's post chain, its
/// depth-of-field focus resolved against the scene, screen ambient occlusion,
/// its `stage post` shaders, and — because a pixel-art project composites at its
/// own resolution and upscales — the retro presentation on its own pixel grid.
///
/// Shared with `floptle vfx` rather than copied into it. That is the same rule
/// `offscreen_draws_the_same_world` exists for one level down: the editor's
/// gathers have drifted apart five times, each time with the same symptom, and a
/// second verb assembling its own post chain would be a sixth place for the
/// picture to quietly stop being the editor's.
///
/// `None` means no device — this machine has no adapter floptle can render on.
pub(crate) fn render_frame_pixels(
    ed: &mut crate::Editor,
    cam: &RenderCamera,
    w: u32,
    h: u32,
    cull_mask: u32,
) -> Option<Vec<u8>> {
    let gpu = ed.gpu.take()?;
    let aspect = w as f32 / h as f32;
    // **Retro composites at the retro resolution and upscales**, exactly as the
    // Game view does — post, AO and dither have to land on the same chunky
    // pixel grid the game uses, or a pixel-art project photographs as a crisp
    // picture of itself that no player will ever see.
    let retro_on = ed.project.retro;
    let (cw, ch) = if retro_on { ed.project.retro_size(aspect) } else { (w, h) };
    let retro = retro_on.then(|| {
        let mut r = floptle_render::Retro::new(&gpu, ch);
        r.resize_to(&gpu, cw, ch);
        r
    });
    // The picture that gets written. In retro mode the scene never draws into
    // it — the upscale blit does — so only its depth half goes unused.
    let (color, depth) = crate::viewports::offscreen_textures(
        &gpu,
        w,
        h,
        "shot",
        wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
    );
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let own_depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let mut post = floptle_render::PostStack::new(&gpu, cw, ch);
    // Always configured, not only when an effect is on: the chain is the only
    // route from the scene's floating-point target down to an sRGB texture.
    post.configure(&gpu, cw, ch, retro_on);
    ed.gpu = Some(gpu);

    let (depth_view, depth_tex) = match &retro {
        Some(r) => (r.depth_view().clone(), r.depth_texture().clone()),
        None => (own_depth_view, depth.clone()),
    };

    // ⏱ Open the timing frame. The marks themselves are inside
    // `render_world_into` and below, one per pass; `end` closes the last region
    // before the readback, whose device wait is what lands the numbers.
    if ed.gpu_timing_headless
        && let Some(t) = ed.gpu_timer.as_mut()
    {
        t.poll();
        t.begin();
    }
    // The depth TEXTURE is handed over, not just its view: that is what lets the
    // opaque prepass run, and without it contact shadows, shoreline foam,
    // screen-space reflections and lamp shadows all quietly draw nothing. A
    // picture missing four effects still looks like a picture, which is exactly
    // why this is easy to get wrong and hard to notice.
    ed.render_world_into(
        post.input_view(),
        &depth_view,
        cam,
        aspect,
        0.0,
        cull_mask,
        None,
        (cw, ch),
        crate::render_frame::OffscreenOpts {
            depth_tex: Some(&depth_tex),
            ..Default::default()
        },
    );

    // **The whole chain, not the tonemap.** This used to pass tonemap alone and
    // default everything else, so a project with bloom, vignette, AO, posterise
    // or a custom post shader photographed as a scene that has none of them —
    // and the one promise this verb makes is that the picture is the editor's.
    // Built the way the Game view builds it: the PostProcess node's own
    // settings, the depth-of-field focus resolved against the scene, screen
    // ambient occlusion, and any `stage post` shaders the project compiled.
    //
    // Two are deliberately left out. **Motion blur** needs a previous frame and
    // this is a single one, so a shutter here would smear against a frame that
    // does not exist. **The accessibility filters** are one person's display
    // preference, and a PNG of a project should not carry them.
    let mut look = crate::shading::post_process_uniforms(&ed.world).0;
    look.time = ed.fog_time;
    if let Some(d) = crate::shading::dof_focus_distance(&ed.world, cam.world_position) {
        look.dof_focus = d;
    }
    let gpu = ed.gpu.as_ref()?;
    let proj = cam.proj_matrix(aspect);
    let ssao = floptle_render::SsaoFrame {
        depth: &depth_view,
        proj: proj.to_cols_array_2d(),
        inv_proj: proj.inverse().to_cols_array_2d(),
    };
    // Where the chain lands: the retro target when there is one, the picture
    // itself otherwise. (`out` is the file; this is the texture.)
    let composite = match &retro {
        Some(r) => r.color_view().clone(),
        None => color_view.clone(),
    };
    if ed.gpu_timing_headless
        && let Some(t) = ed.gpu_timer.as_mut()
    {
        t.mark(gpu, "post");
    }
    post.run_with(gpu, &look, Some(&ssao), &composite, ed.post_shaders.as_ref());
    if ed.gpu_timing_headless
        && let Some(t) = ed.gpu_timer.as_mut()
    {
        t.mark(gpu, "retro upscale");
    }
    // …and the chunky upscale, the way the game presents it.
    if let Some(r) = &retro {
        let dest = [w as f32, h as f32];
        if ed.project.retro_integer_scale {
            r.blit_integer(gpu, &color_view, dest);
        } else {
            r.blit_to(gpu, &color_view);
        }
    }

    if ed.gpu_timing_headless
        && let Some(t) = ed.gpu_timer.as_mut()
    {
        t.end(gpu);
    }
    // The readback waits on the device, which is also what lands the timing
    // query's own readback — `run` polls it after this returns.
    Some(readback(gpu, &color, w, h))
}

/// Copy the rendered texture back into RGBA8, un-swizzling if the adapter's
/// surface format is BGRA.
fn readback(gpu: &Gpu, tex: &wgpu::Texture, w: u32, h: u32) -> Vec<u8> {
    // A texture copy's rows are aligned; the image's are not, so the padding
    // has to come off on the way out or every row after the first lands shifted.
    let padded =
        (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("shot-readback"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("shot-readback") });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
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
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let row = (y * padded) as usize;
        for x in 0..w {
            let p = row + (x * 4) as usize;
            let (r, g, b, a) = (view[p], view[p + 1], view[p + 2], view[p + 3]);
            if bgra {
                out.extend_from_slice(&[b, g, r, a]);
            } else {
                out.extend_from_slice(&[r, g, b, a]);
            }
        }
    }
    drop(view);
    buf.unmap();
    out
}

/// The largest frame this can render.
///
/// `Gpu::headless_with` asks for `wgpu::Limits::default()`, and that is exactly
/// where the ceiling comes from — so this is the real number for the device this
/// verb creates, not a guess at one.
fn max_side() -> u32 {
    wgpu::Limits::default().max_texture_dimension_2d
}

/// Parse `--size` as `WxH`, or a single number meaning a square.
///
/// **The bounds are checked here, not at the texture.** `--size 20000x20000`
/// used to reach `create_texture`, and wgpu's validation failure is a panic —
/// so a typed number too large ended in a crash report asking the caller to open
/// a GitHub issue about having asked for a big picture. That is the same shape
/// as `inspect | head` panicking on SIGPIPE: a mistake in the command line
/// reported as a defect in the engine.
/// `--after`: how long to play before drawing, in seconds.
///
/// `30s`, `1.5s` or a bare `30` are seconds; `900f` is frames, converted at the
/// same fixed rate `run` steps by. Frames are offered because a game tuned to
/// the tick thinks in them, and "the 900th frame" is a more exact thing to ask
/// for than "fifteen seconds" when a step is what moved.
///
/// **Every rejected value is rejected loudly.** A float that is not a number is
/// the shape that costs a session: `nan` passes any comparison written against
/// it and casts to 0, so the verb would play nothing and exit 0 with a picture
/// of the unplayed scene — a confident wrong answer, which is the failure this
/// verb set was built to refuse.
pub(crate) fn parse_after(s: &str) -> Result<f32, String> {
    let t = s.trim();
    let (num, frames) = match t.strip_suffix(['f', 'F']) {
        Some(n) => (n, true),
        None => (t.strip_suffix(['s', 'S']).unwrap_or(t), false),
    };
    let n: f32 = num
        .trim()
        .parse()
        .map_err(|_| format!("--after wants a span like 30s, 1.5s or 900f — not {s:?}"))?;
    if !n.is_finite() || n <= 0.0 {
        return Err(format!(
            "--after {s:?} is not a length of time to play for; ask for something above zero"
        ));
    }
    Ok(if frames { n * crate::run::DT } else { n })
}

pub(crate) fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("--size wants WxH (say 960x540), not {s:?}");
    let (w, h) = match s.split_once(['x', 'X']) {
        Some((w, h)) => {
            let (Ok(w), Ok(h)) = (w.trim().parse(), h.trim().parse()) else { return Err(bad()) };
            (w, h)
        }
        None => s.trim().parse().map(|n: u32| (n, n)).map_err(|_| bad())?,
    };
    if w == 0 || h == 0 {
        return Err(format!("--size {w}x{h} has no picture in it"));
    }
    let max = max_side();
    if w > max || h > max {
        return Err(format!(
            "--size {w}x{h} is larger than this machine can render — {max} is the limit on \
             either side"
        ));
    }
    Ok((w, h))
}

/// Where a shot lands when the caller did not say.
pub(crate) fn default_out(root: &Path, scene: Option<&str>) -> PathBuf {
    let stem = scene
        .map(|s| Path::new(s).file_stem().map(|f| f.to_string_lossy().into_owned()))
        .unwrap_or_default()
        .unwrap_or_else(|| "scene".into());
    root.join(format!("{stem}.png"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_to_play_is_seconds_or_frames_and_never_a_quiet_zero() {
        assert_eq!(parse_after("30"), Ok(30.0), "a bare number is seconds");
        assert_eq!(parse_after("30s"), Ok(30.0));
        assert_eq!(parse_after("1.5S"), Ok(1.5));
        assert_eq!(parse_after(" 30s "), Ok(30.0));
        // Frames, at the same rate `run` steps by.
        assert_eq!(parse_after("60f"), Ok(1.0), "60 frames is a second");
        // 900 × (1/60) does not land exactly on 15 in f32, and pretending it
        // does is how a parser test starts asserting the float's rounding.
        assert!((parse_after("900F").unwrap() - 15.0).abs() < 1e-3);

        // **The values that would otherwise be a confident wrong answer.**
        // `nan` passes every comparison written against it and casts to 0, so
        // the verb would play nothing, photograph the unplayed scene and exit 0
        // — a picture of an empty room presented as a picture of the game.
        for bad in ["nan", "inf", "-inf", "-5", "0", "0s", "soon", "", "30ms"] {
            assert!(parse_after(bad).is_err(), "--after {bad:?} must be refused, not guessed");
        }
        assert!(
            parse_after("soon").unwrap_err().contains("30s"),
            "and the refusal has to show what a span looks like"
        );
    }

    /// `floptle/0170`: `run` had the played world and wrote no picture; `shot`
    /// wrote a picture and had no played world. This is the join.
    ///
    /// The fixture is the case that made the card: a project whose scene file
    /// holds a camera and a generator, and whose world does not exist until it
    /// has run. Photographed as authored it is an empty room — a true picture
    /// of the file and a picture of nothing anybody plays.
    ///
    /// No GPU here on purpose. `shot::run` installs an uncaptured-error handler
    /// that EXITS the process, which on a machine with only an OpenGL adapter
    /// (CI has one) would take the whole test binary down with it. What is
    /// under test is the join — play first, then look — and that needs no
    /// renderer.
    #[test]
    fn playing_first_is_what_makes_a_generated_world_photographable() {
        let d = std::env::temp_dir().join(format!(
            "flshot-after-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("scenes")).unwrap();
        std::fs::create_dir_all(d.join("scripts")).unwrap();
        // Naming the entry scene matters: opening a project that names none
        // builds the starter scene, and the fixture would then be testing the
        // crate-and-ball demo rather than this one.
        std::fs::write(
            d.join("project.ron"),
            "(title: Some(\"t\"), entry_scene: Some(\"scenes/first.ron\"))",
        )
        .unwrap();
        // The generator: the world it makes does not exist in the file at all,
        // and it hands the view to a camera of its own on the first frame —
        // which is exactly what the reporting project's `planet_camera` does.
        std::fs::write(
            d.join("scripts/gen.lua"),
            "function start()\n\
             \x20 createNode(\"Rock\")\n\
             \x20 createNode(\"Chosen\", function(c)\n\
             \x20   c.position = vec3(0, 2, 9)\n\
             \x20   c:setCamera{ fovY = 1.0, active = true }\n\
             \x20 end)\n\
             end\n",
        )
        .unwrap();
        std::fs::write(
            d.join("scenes/first.ron"),
            "(name: \"s\", nodes: [\
               (name: \"Authored\", matter: Camera(active: true)), \
               (name: \"Gen\", scripts: [(kind: \"gen\")])\
             ])",
        )
        .unwrap();

        let mut ed = crate::Editor::default();
        ed.open_project(d.clone());
        ed.open_scene_file(&d.join("scenes/first.ron").to_string_lossy());

        // As authored: the generated world is simply not there.
        assert!(
            ed.world.query::<floptle_core::Name>().all(|(_, n)| n.0 != "Rock"),
            "the fixture is wrong — the rock must not exist before anything runs"
        );
        let (authored, ..) = find_camera(&ed, None).expect("the file's own camera");

        let played =
            play_for(&mut ed, 0.25, DVec3::ZERO).expect("the project must enter play mode");
        assert!(played > 0.0, "the span has to actually simulate — a held session steps at dt=0");

        // …and after playing it is, which is the whole card.
        assert!(
            ed.world.query::<floptle_core::Name>().any(|(_, n)| n.0 == "Rock"),
            "the world a script builds must exist by the time the picture is taken"
        );

        // The camera the GAME chose, not the one the file did.
        let (chosen, ..) = find_camera(&ed, None).expect("a camera after play");
        assert_ne!(
            chosen, authored,
            "a game that takes over the view must be photographed through its own camera"
        );
        assert_eq!(
            ed.world.get::<floptle_core::Name>(chosen).map(|n| n.0.clone()),
            Some("Chosen".into())
        );

        // …unless --camera names one, which still wins outright.
        let (named, ..) = find_camera(&ed, Some("Authored")).expect("--camera still selects");
        assert_eq!(named, authored);

        // Still playing: stopping would restore the scene and throw away the
        // world that was just built.
        assert!(ed.playing, "the session must be live when the picture is taken");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_size_is_wide_by_high_or_a_single_square() {
        assert_eq!(parse_size("960x540"), Ok((960, 540)));
        assert_eq!(parse_size("960X540"), Ok((960, 540)));
        assert_eq!(parse_size(" 512 "), Ok((512, 512)));
        assert!(parse_size("960*540").is_err());
        assert!(parse_size("wide").is_err());
        // A picture with no pixels in it is a typo, not a request.
        assert!(parse_size("0x540").is_err());
        assert!(parse_size("0").is_err());
        // The error names the flag and shows the shape, because the caller that
        // typed this cannot see the source.
        assert!(parse_size("960by540").unwrap_err().contains("960x540"));
    }

    /// **A number too big is a wrong command line, not a crash.**
    ///
    /// It reached `create_texture` before this, and wgpu answers a texture it
    /// cannot make with a panic — which the editor turns into a crash report
    /// asking the caller to file a GitHub issue. Somebody who typed a large
    /// number would have been told the engine was broken.
    #[test]
    fn a_frame_bigger_than_the_device_is_refused_by_name() {
        let max = max_side();
        assert!(parse_size(&format!("{max}x{max}")).is_ok(), "the limit itself is renderable");
        let over = format!("{}x{}", max + 1, max + 1);
        let err = parse_size(&over).unwrap_err();
        assert!(err.contains(&max.to_string()), "the refusal did not say what the limit is: {err}");
        // …and one side over is enough.
        assert!(parse_size(&format!("{}x64", max + 1)).is_err());
        assert!(parse_size(&format!("64x{}", max + 1)).is_err());
    }

    /// **The failure `floptle/0166` was about.** A scene holding nothing but a
    /// `MapMesh` box and a camera renders, unfixed, as a perfectly flat clear
    /// color: nothing else in the frame varies it, so "not one uniform color"
    /// is exactly the pixel-coverage floor the card asked for — any pixel that
    /// differs from the corner is the box, and their total absence is the bug.
    /// Built by hand (no project on disk) the way `map_edit`'s own tests build
    /// a `MapMesh` node, so this exercises the same `sync_map_meshes` +
    /// `render_world_into` path `shot::run` does without needing a fixture
    /// project.
    #[test]
    fn a_shot_draws_map_mesh_geometry() {
        let gpu = Gpu::headless_hdr(64, 64);
        // Same skip-gracefully idiom `retro_exempt_water_does_not_share_the_
        // projects_dithered_neutral_entry` uses: a device that cannot build the
        // raster pipeline has nothing to say about whether THIS test's box drew.
        let failed = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = failed.clone();
        gpu.device.on_uncaptured_error(std::sync::Arc::new(move |e: wgpu::Error| {
            if let Ok(mut s) = sink.lock()
                && s.is_empty()
            {
                *s = e.to_string();
            }
        }));

        let mut ed = crate::Editor::default();
        ed.attach_gpu(gpu);
        if let Some(g) = ed.gpu.as_ref() {
            let _ = g.device.poll(wgpu::PollType::wait_indefinitely());
        }

        fn blank_node() -> floptle_scene::NodeDoc {
            floptle_scene::NodeDoc {
                name: String::new(),
                transform: Default::default(),
                matter: floptle_scene::MatterDoc::Empty,
                scripts: Vec::new(),
                material: None,
                object_materials: Default::default(),
                tint: None,
                rigidbody: None,
                celestial: None,
                mesh_collider: false,
                disabled: false,
                paint: None,
                tex_paint: None,
                terrain_gen: None,
                collidable: false,
                trigger: false,
                nav_exclude: false,
                visible: true,
                cast_shadow: true,
                anim_controller: None,
                particles: None,
                id: None,
                parent_id: None,
                parent: None,
                attachment: None,
                net: None,
                ui_layer: None,
                ui: None,
                audio: None,
                layer: None,
                tags: Vec::new(),
                sorting: None,
                sort_mode: None,
                parallax: None,
                camera_2d: None,
                lit_2d: None,
                light_layers: Vec::new(),
                shadow_2d: None,
                light_inner: None,
                light_falloff: None,
                light_shadows: None,
            }
        }

        // The box, dead centre.
        let geo = crate::map_edit::MapShape::Box.mesh(crate::map_edit::MapOpts::default());
        ed.spawn_node(&floptle_scene::NodeDoc {
            matter: floptle_scene::MatterDoc::MapMesh { id: 0, geo: Some(geo) },
            ..blank_node()
        });
        // A camera looking straight down -Z at it.
        ed.spawn_node(&floptle_scene::NodeDoc {
            transform: floptle_scene::TransformDoc {
                translation: [0.0, 0.0, 8.0],
                ..Default::default()
            },
            matter: floptle_scene::MatterDoc::Camera {
                fov_y: 1.0,
                active: true,
                target: String::new(),
                cull_mask: u32::MAX,
                target_w: floptle_core::Matter::TARGET_W,
                target_h: floptle_core::Matter::TARGET_H,
                target_hz: 0.0,
                ortho: false,
                ortho_height: floptle_core::Matter::ORTHO_HEIGHT,
            },
            ..blank_node()
        });
        // Light it — no ambient/intensity means every surface shades to the
        // same black the empty background would already be, which would make
        // this test pass even on the bug it exists to catch.
        let light_e = ed.world.spawn();
        ed.world.insert(
            light_e,
            floptle_core::Light {
                color: [1.0, 1.0, 1.0],
                ambient: [0.6, 0.6, 0.6],
                intensity: 1.5,
                direction: [0.3, -0.7, 0.2],
                ..Default::default()
            },
        );

        // The fix under test: without these, the box's node exists but its
        // geometry was never uploaded to the registry `render_world_into` reads
        // from — see `sync_map_meshes`'s own doc comment.
        ed.sync_map_meshes();
        ed.sync_map_paint();

        if let Ok(why) = failed.lock()
            && !why.is_empty()
        {
            eprintln!("skipped — this machine cannot build the raster pipeline:\n{why}");
            return;
        }
        let Some(gpu_ref) = ed.gpu.as_ref() else { return };
        let (w, h) = (64u32, 64u32);
        // `render_world_into` draws into an HDR target — the raster pipeline is
        // built for one — so this mirrors `shot::run` exactly: an HDR input
        // through `PostStack`, tonemapped down into the SRGB texture that gets
        // read back, rather than drawing straight into an SRGB target the
        // pipeline was never built to hit (a format mismatch, not the bug this
        // test is about).
        let (color, depth) = crate::viewports::offscreen_textures(
            gpu_ref,
            w,
            h,
            "shot-test",
            wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let mut post = floptle_render::PostStack::new(gpu_ref, w, h);
        post.configure(gpu_ref, w, h, false);
        let cam = RenderCamera::new(
            floptle_core::math::DVec3::new(0.0, 0.0, 8.0),
            floptle_core::math::Quat::IDENTITY,
            Projection::of_camera(1.0, false, floptle_core::Matter::ORTHO_HEIGHT, 0.05, 300_000.0),
        );
        ed.render_world_into(
            post.input_view(),
            &depth_view,
            &cam,
            1.0,
            0.0,
            u32::MAX,
            None,
            (w, h),
            crate::render_frame::OffscreenOpts::default(),
        );
        let look = crate::shading::post_process_uniforms(&ed.world).0;

        let Some(gpu_ref) = ed.gpu.as_ref() else { return };
        if let Ok(why) = failed.lock()
            && !why.is_empty()
        {
            eprintln!("skipped — this machine cannot build the raster pipeline:\n{why}");
            return;
        }
        post.run_with(gpu_ref, &look, None, &color_view, None);
        let pixels = readback(gpu_ref, &color, w, h);
        let (chunks, _) = pixels.as_chunks::<4>();
        let corner = chunks[0];
        let differs = chunks
            .iter()
            .filter(|p| {
                p[0].abs_diff(corner[0]) > 6 || p[1].abs_diff(corner[1]) > 6 || p[2].abs_diff(corner[2]) > 6
            })
            .count();
        assert!(
            differs > 16,
            "the frame is {differs} pixels different from its own corner out of {} — a scene \
             with nothing in it but a MapMesh box and a camera photographed as an empty room \
             (floptle/0166)",
            pixels.len() / 4
        );
    }

    #[test]
    fn a_shot_is_named_after_the_scene_it_shows() {
        let root = Path::new("/p");
        assert_eq!(default_out(root, Some("scenes/arena.ron")), root.join("arena.png"));
        assert_eq!(default_out(root, Some("arena")), root.join("arena.png"));
        assert_eq!(default_out(root, None), root.join("scene.png"));
    }
}
