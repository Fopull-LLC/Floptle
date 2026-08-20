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

/// Run the verb. Returns the process exit code.
pub(crate) fn run(
    root: &Path,
    scene: Option<&str>,
    camera: Option<&str>,
    size: (u32, u32),
    out: &Path,
    json: bool,
) -> i32 {
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

    let Some(gpu) = ed.gpu.take() else {
        eprintln!("no GPU: this machine has no adapter floptle can render on");
        return 1;
    };
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

    // The depth TEXTURE is handed over, not just its view: that is what lets the
    // opaque prepass run, and without it contact shadows, shoreline foam,
    // screen-space reflections and lamp shadows all quietly draw nothing. A
    // picture missing four effects still looks like a picture, which is exactly
    // why this is easy to get wrong and hard to notice.
    ed.render_world_into(
        post.input_view(),
        &depth_view,
        &cam,
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
    let Some(gpu) = ed.gpu.as_ref() else { return 1 };
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
    post.run_with(gpu, &look, Some(&ssao), &composite, ed.post_shaders.as_ref());
    // …and the chunky upscale, the way the game presents it.
    if let Some(r) = &retro {
        let dest = [w as f32, h as f32];
        if ed.project.retro_integer_scale {
            r.blit_integer(gpu, &color_view, dest);
        } else {
            r.blit_to(gpu, &color_view);
        }
    }

    let pixels = readback(gpu, &color, w, h);
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

    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "path": out.to_string_lossy(),
                "width": w,
                "height": h,
                "camera": ed.world.get::<floptle_core::Name>(e).map(|n| n.0.clone()),
            })
        );
    } else {
        println!("wrote {} ({w}x{h})", out.display());
    }
    0
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

    #[test]
    fn a_shot_is_named_after_the_scene_it_shows() {
        let root = Path::new("/p");
        assert_eq!(default_out(root, Some("scenes/arena.ron")), root.join("arena.png"));
        assert_eq!(default_out(root, Some("arena")), root.join("arena.png"));
        assert_eq!(default_out(root, None), root.join("scene.png"));
    }
}
