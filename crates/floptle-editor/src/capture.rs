//! `camera.capture`: a game photographing its own world.
//!
//! The picture is rendered by [`render_frame_texture`] — the same frame
//! `floptle shot` takes, post chain, retro and UI included — beside the
//! render-target pass, then copied into a buffer the GPU maps in its own time.
//! Nothing waits on the device, so a capture costs one extra scene render and
//! never a stall, and the same path works in a browser, where waiting is not
//! allowed. Encoding runs on a worker; the answer lands in the next frame
//! pass.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;

use floptle_core::Matter;
use floptle_render::{Projection, RenderCamera};
use floptle_script::{CaptureCamera, CaptureFormat, CaptureRequest};

/// Draw one frame of `ed`'s world from `cam` into a new `w`×`h` texture
/// (RGBA8 in the surface's format, `COPY_SRC`).
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
/// `ui` draws the game's UI as the Game view does: world canvases into the
/// scene before post, every screen-space layer over the finished picture.
/// `false` is the world alone — `shot --no-ui`, and `vfx`,
/// which photographs an effect and has no screen to show.
///
/// `shapes` keeps the script's `draw.line` / `draw.tri` shapes; `false` leaves
/// them out, which is what a cover picture wants — they are usually a debug
/// line or a gizmo. `draw.quad` trails are part of the world and always drawn.
///
/// `None` means no device — this machine has no adapter floptle can render on.
pub(crate) fn render_frame_texture(
    ed: &mut crate::Editor,
    cam: &RenderCamera,
    w: u32,
    h: u32,
    cull_mask: u32,
    ui: bool,
    shapes: bool,
) -> Option<wgpu::Texture> {
    // The frame loop's GPU prelude, which this path has no frame loop to run:
    // UI shaders, `stage post` shaders and Field Shape SDFs compile and bind
    // here, or a shot draws each of them as its fallback look and says nothing.
    // (`.flsl` materials, scene textures and effect assets are pre-warmed inside
    // `render_world_into`, which reflection captures reach without this.)
    ed.ensure_ui_shaders();
    ed.ensure_post_shaders();
    ed.sync_field_shapes();
    // The sky's shader and texture, and the map's geometry, likewise: each
    // is a no-op once the frame loop has done it, and without it a capture
    // taken before that (`shot --after`, a script's first frames) drew the
    // Skybox's plain colour where the sky and its fog belonged.
    ed.sync_sky_shader();
    ed.sync_sky_texture();
    ed.sync_map_meshes();
    ed.sync_map_paint();
    let gpu = ed.gpu.take()?;
    let aspect = w as f32 / h as f32;
    // **Retro composites at the retro resolution and upscales**, exactly as the
    // Game view does — post, AO and dither have to land on the same chunky
    // pixel grid the game uses, or a pixel-art project photographs as a crisp
    // picture of itself that no player will ever see.
    // A render scale below 1 takes the same route with a smooth upscale, so a
    // `shot --timing` of a scaled game measures what the game costs.
    let retro_on = ed.project.retro;
    let lowres = ed.project.composite_size(w, h);
    let (cw, ch) = lowres.unwrap_or((w, h));
    let retro = lowres.map(|_| {
        let mut r = floptle_render::Retro::new(&gpu, ch);
        r.resize_to(&gpu, cw, ch);
        r.set_smooth(&gpu, !retro_on);
        r.set_sharpness(&gpu, ed.render_sharpness());
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
    // The depth texture is handed over, not just its view: that is what lets the
    // opaque prepass run, and without it contact shadows, shoreline foam,
    // screen-space reflections and lamp shadows all quietly draw nothing. A
    // picture missing four effects still looks like a picture, which is exactly
    // why this is easy to get wrong and hard to notice.
    let defer_lines = ed.script_host.native_lines() && shapes;
    ed.lines_deferred = defer_lines;
    ed.hide_script_shapes = !shapes;
    ed.render_world_into(
        post.input_view(),
        &depth_view,
        cam,
        aspect,
        0.0,
        cull_mask,
        None,
        (cw, ch),
        crate::offscreen::OffscreenOpts {
            depth_tex: Some(&depth_tex),
            ..Default::default()
        },
    );
    ed.lines_deferred = false;
    ed.hide_script_shapes = false;
    // World-space UI canvases are geometry: into the scene, with its depth,
    // before post — exactly where the Game view puts them.
    if ui {
        ed.draw_world_canvases(post.input_view(), &depth_view, cam, aspect);
    }

    // The whole chain, not the tonemap: the one promise this verb makes is
    // that the picture is the editor's, bloom, vignette, AO, posterise and
    // custom post shaders included.
    // Built the way the Game view builds it: the PostProcess node's own
    // settings, the depth-of-field focus resolved against the scene, screen
    // ambient occlusion, and any `stage post` shaders the project compiled.
    //
    // Two are left out. **Motion blur** needs a previous frame and
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
        fog: crate::shading::ao_fog(&ed.world, cam.world_position),
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
        if retro_on && ed.project.retro_integer_scale {
            r.blit_integer(gpu, &color_view, dest);
        } else {
            r.blit_to(gpu, &color_view);
        }
    }

    // **The UI, last, over the finished picture** — every enabled
    // screen-space layer in `z` order at the scale its `scale_mode` gives
    // this size, the script's `draw.*`, captions: the same composite the Game
    // view shows. After the retro upscale on purpose, because that is where
    // the game draws it: a pixel-art project's HUD is crisp at window
    // resolution, not chunky with the world. The picture's own texture was
    // made samplable above, so `backdrop()` shaders frost the real scene.
    //
    // A UI-first scene photographed without its UI is wrong on first sight,
    // and nothing else would say so.
    if defer_lines {
        ed.draw_lines_over(&color_view, [w, h], cam.view_proj(aspect), cam.world_position, &depth_view);
    }
    if ui {
        ed.draw_game_ui_overlay(&color_view, w, h, true);
    }

    let gpu = ed.gpu.as_ref()?;
    if ed.gpu_timing_headless
        && let Some(t) = ed.gpu_timer.as_mut()
    {
        t.end(gpu);
    }
    Some(color)
}


/// A picture copied into a buffer, waiting for the map.
pub(crate) struct CaptureJob {
    req: CaptureRequest,
    buf: wgpu::Buffer,
    padded: u32,
    bgra: bool,
    /// 0 = waiting, 1 = mapped, 2 = the map failed.
    state: Arc<AtomicU8>,
}

/// An encode running on a worker.
pub(crate) struct CaptureEncode {
    id: u64,
    rx: Receiver<Result<Vec<u8>, String>>,
}

const NOT_A_CAMERA: &str = "is not a camera";

/// The camera a request names, as the render camera and its cull mask.
fn resolve_camera(world: &floptle_core::World, which: &CaptureCamera) -> Result<(RenderCamera, u32), String> {
    let e = match which {
        CaptureCamera::Active => floptle_core::active_camera(world)
            .ok_or_else(|| "the scene has no active camera — pass the camera to capture through".to_string())?,
        CaptureCamera::Named(name) => world
            .query::<Matter>()
            .find(|(e, m)| {
                matches!(m, Matter::Camera { .. }) && world.get::<floptle_core::Name>(*e).is_some_and(|n| &n.0 == name)
            })
            .map(|(e, _)| e)
            .ok_or_else(|| format!("there is no camera called \"{name}\""))?,
        CaptureCamera::Node(id) => {
            world.entity_with::<floptle_core::Transform>(*id).ok_or_else(|| "that node no longer exists".to_string())?
        }
    };
    let Some(Matter::Camera { fov_y, cull_mask, ortho, ortho_height, .. }) = world.get::<Matter>(e) else {
        let name = world.get::<floptle_core::Name>(e).map(|n| n.0.clone()).unwrap_or_default();
        return Err(format!("\"{name}\" {NOT_A_CAMERA}"));
    };
    let wt = floptle_core::world_transform(world, e);
    let cam = RenderCamera::new(
        wt.translation,
        wt.rotation,
        Projection::of_camera(*fov_y, *ortho, *ortho_height, 0.05, 300_000.0),
    );
    Ok((cam, *cull_mask))
}

impl crate::Editor {
    /// Once a frame, beside the render-target pass: render what scripts asked
    /// for, collect what the GPU has mapped, and answer what is encoded.
    pub(crate) fn pump_captures(&mut self) {
        let requests = self.script_host.take_capture_requests();
        for req in requests {
            self.start_capture(req);
        }
        if self.capture_jobs.is_empty() && self.capture_encodes.is_empty() {
            return;
        }
        if let Some(gpu) = self.gpu.as_ref() {
            // Non-blocking: moves finished maps along on the desktop. A
            // browser resolves them on its own.
            let _ = gpu.device.poll(wgpu::PollType::Poll);
        }
        let jobs = std::mem::take(&mut self.capture_jobs);
        for job in jobs {
            match job.state.load(Ordering::Acquire) {
                0 => self.capture_jobs.push(job),
                1 => self.finish_capture(job),
                _ => self.refuse_capture(&job.req, "the GPU could not read the picture back".into()),
            }
        }
        let mut done = Vec::new();
        self.capture_encodes.retain(|j| match j.rx.try_recv() {
            Ok(r) => {
                done.push((j.id, r));
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => {
                done.push((j.id, Err("the encode worker stopped".into())));
                false
            }
        });
        for (id, r) in done {
            self.script_host.answer_capture(id, r);
        }
    }

    /// Tell the script its picture is not coming, and why.
    fn refuse_capture(&self, req: &CaptureRequest, why: String) {
        match req.format {
            CaptureFormat::Texture => self.script_host.answer_capture_texture(req.id, Err(why)),
            _ => self.script_host.answer_capture(req.id, Err(why)),
        }
    }

    fn start_capture(&mut self, req: CaptureRequest) {
        if self.gpu.is_none() || self.raster.is_none() {
            return self.refuse_capture(&req, floptle_script::NO_RENDERER.into());
        }
        let (cam, mask) = match resolve_camera(&self.world, &req.camera) {
            Ok(c) => c,
            Err(why) => return self.refuse_capture(&req, why),
        };
        let max_side = self.gpu.as_ref().map_or(0, |g| g.device.limits().max_texture_dimension_2d);
        if req.w > max_side || req.h > max_side {
            return self.refuse_capture(
                &req,
                format!("{}x{} is larger than this GPU's {max_side}-pixel limit", req.w, req.h),
            );
        }
        let Some(tex) = render_frame_texture(self, &cam, req.w, req.h, mask, req.ui, req.draws) else {
            return self.refuse_capture(&req, floptle_script::NO_RENDERER.into());
        };
        let Some(gpu) = self.gpu.as_ref() else {
            return self.refuse_capture(&req, floptle_script::NO_RENDERER.into());
        };
        let (w, h) = (req.w, req.h);
        let padded = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("capture-readback"),
            size: (padded * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("capture") });
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        gpu.queue.submit(Some(enc.finish()));
        let state = Arc::new(AtomicU8::new(0));
        let s = state.clone();
        buf.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            s.store(if r.is_ok() { 1 } else { 2 }, Ordering::Release);
        });
        let bgra = matches!(
            gpu.surface_format(),
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        self.capture_jobs.push(CaptureJob { req, buf, padded, bgra, state });
    }

    fn finish_capture(&mut self, job: CaptureJob) {
        let (w, h) = (job.req.w, job.req.h);
        let pixels = {
            let view = job.buf.slice(..).get_mapped_range();
            unpad_rgba(&view, w, h, job.padded, job.bgra)
        };
        job.buf.unmap();
        match job.req.format {
            CaptureFormat::Texture => {
                let name = job.req.texture.clone().unwrap_or_else(|| format!("img:{}", job.req.id));
                let answer = match (self.gpu.as_ref(), self.raster.as_mut()) {
                    (Some(gpu), Some(raster)) => {
                        let data = floptle_render::TextureData { pixels, width: w, height: h };
                        let tex = raster.register_texture(gpu, &data, Default::default());
                        self.texture_registry.insert(name.clone(), tex);
                        Ok(name)
                    }
                    _ => Err(floptle_script::NO_RENDERER.to_string()),
                };
                self.script_host.answer_capture_texture(job.req.id, answer);
            }
            format => {
                let (tx, rx) = std::sync::mpsc::channel();
                crate::worker::spawn("floptle-capture-encode", move || {
                    let _ = tx.send(encode(pixels, w, h, format));
                });
                self.capture_encodes.push(CaptureEncode { id: job.req.id, rx });
            }
        }
    }

    /// Stop or a scene load: pictures still on their way have nobody to go to.
    pub(crate) fn drop_captures(&mut self) {
        self.capture_jobs.clear();
        self.capture_encodes.clear();
    }
}

/// Strip a readback's row padding, and swap BGRA to RGBA when the surface is
/// BGRA — the picture is always handed on as RGBA.
pub(crate) fn unpad_rgba(view: &[u8], w: u32, h: u32, padded: u32, bgra: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let row = &view[(y * padded) as usize..][..(w * 4) as usize];
        if bgra {
            for p in row.as_chunks::<4>().0 {
                out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
            }
        } else {
            out.extend_from_slice(row);
        }
    }
    out
}

/// Encode RGBA pixels as the script asked. A JPEG has no alpha, so it is
/// dropped; the picture is opaque anyway.
pub(crate) fn encode(pixels: Vec<u8>, w: u32, h: u32, format: CaptureFormat) -> Result<Vec<u8>, String> {
    let img = image::RgbaImage::from_raw(w, h, pixels).ok_or("the picture came back the wrong size")?;
    let mut out = std::io::Cursor::new(Vec::new());
    match format {
        CaptureFormat::Png => img.write_to(&mut out, image::ImageFormat::Png).map_err(|e| e.to_string())?,
        CaptureFormat::Jpeg(q) => {
            let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q)
                .encode_image(&rgb)
                .map_err(|e| e.to_string())?;
        }
        CaptureFormat::Texture => return Err("a texture capture is not encoded".into()),
    }
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use floptle_core::math::{DVec3, Quat, Vec3};
    use floptle_core::{Matter, Name, ScriptInst, Scripts, Shape, Transform};

    const SCRIPT: &str = r#"
function start(node)
  camera.capture({ w = 64, h = 36 }, function(b, e) front = b; frontErr = e end)
  camera.capture("Side", { w = 64, h = 36, format = "jpeg", quality = 80 }, function(b, e) side = b; sideErr = e end)
  camera.captureTexture({ w = 48, h = 20 }, function(t, e) tex = t; texErr = e end)
  camera.capture(node, { w = 8, h = 8 }, function(b, e) notCam = e end)
end
"#;

    fn camera(active: bool) -> Matter {
        Matter::Camera {
            fov_y: 60f32.to_radians(),
            active,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: Matter::TARGET_W,
            target_h: Matter::TARGET_H,
            target_hz: 0.0,
            ortho: false,
            ortho_height: Matter::ORTHO_HEIGHT,
        }
    }

    /// A white cube in front of the active camera, a second camera looking
    /// the other way, and a node whose script asks for pictures.
    fn scene(ed: &mut crate::Editor, tag: &str) -> (floptle_core::Entity, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("floptle-capture-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cap.lua"), SCRIPT).unwrap();
        let w = &mut ed.world;
        let cube = w.spawn();
        w.insert(cube, Transform { translation: DVec3::ZERO, rotation: Quat::IDENTITY, scale: Vec3::splat(2.0) });
        w.insert(cube, Matter::Primitive { shape: Shape::Cube, color: [1.0; 3] });
        let front = w.spawn();
        w.insert(front, Transform { translation: DVec3::new(0.0, 0.0, 4.0), rotation: Quat::IDENTITY, scale: Vec3::ONE });
        w.insert(front, Name("Front".into()));
        w.insert(front, camera(true));
        let side = w.spawn();
        w.insert(
            side,
            Transform {
                translation: DVec3::new(0.0, 0.0, 4.0),
                rotation: Quat::from_rotation_y(std::f32::consts::PI),
                scale: Vec3::ONE,
            },
        );
        w.insert(side, Name("Side".into()));
        w.insert(side, camera(false));
        let asker = w.spawn();
        w.insert(asker, Transform::IDENTITY);
        w.insert(
            asker,
            Scripts(vec![ScriptInst {
                kind: "cap".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        (asker, dir)
    }

    fn frames(ed: &mut crate::Editor, dir: &std::path::Path, until: impl Fn(&crate::Editor) -> bool) {
        for i in 0..600 {
            ed.script_host.run(&mut ed.world, dir, 1.0 / 60.0, i as f32 / 60.0);
            ed.pump_runtime_textures();
            ed.pump_captures();
            if until(ed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn get<T: mlua::FromLua>(ed: &crate::Editor, e: floptle_core::Entity, key: &str) -> Option<T> {
        ed.script_host.instance_env(e.index(), "cap").and_then(|env| env.get::<Option<T>>(key).ok().flatten())
    }

    /// A script asks for pictures and gets them: a PNG of what the active
    /// camera sees, a JPEG through a camera that is not active, a texture
    /// every drawing path can name, and a reason for a node that is no camera.
    #[test]
    fn a_script_photographs_its_own_world() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let (asker, dir) = scene(&mut ed, "gpu");
        let done = |ed: &crate::Editor| {
            get::<mlua::String>(ed, asker, "front").is_some()
                && get::<mlua::String>(ed, asker, "side").is_some()
                && get::<String>(ed, asker, "tex").is_some()
                && get::<String>(ed, asker, "notCam").is_some()
        };
        frames(&mut ed, &dir, done);
        for k in ["frontErr", "sideErr", "texErr"] {
            assert_eq!(get::<String>(&ed, asker, k), None, "{k}");
        }
        assert!(get::<String>(&ed, asker, "notCam").unwrap().contains("is not a camera"));

        let png = get::<mlua::String>(&ed, asker, "front").expect("no PNG came back");
        let png = image::load_from_memory_with_format(&png.as_bytes(), image::ImageFormat::Png).unwrap().to_rgba8();
        assert_eq!(png.dimensions(), (64, 36));
        let jpg = get::<mlua::String>(&ed, asker, "side").expect("no JPEG came back");
        let jpg = image::load_from_memory_with_format(&jpg.as_bytes(), image::ImageFormat::Jpeg).unwrap().to_rgba8();
        assert_eq!(jpg.dimensions(), (64, 36));

        // The front camera has the cube filling its middle; the side one looks
        // away from it. Were the camera ignored, the two would match.
        let luma = |p: &image::Rgba<u8>| p.0[0] as i32 + p.0[1] as i32 + p.0[2] as i32;
        let (fc, sc) = (luma(png.get_pixel(32, 18)), luma(jpg.get_pixel(32, 18)));
        assert!((fc - sc).abs() > 60, "front centre {fc} vs side centre {sc}: the same picture twice");
        // And the front picture is a picture, not one colour.
        let corner = luma(png.get_pixel(1, 1));
        assert!((fc - corner).abs() > 30, "centre {fc} vs corner {corner}: the cube is not in it");

        let tex = get::<String>(&ed, asker, "tex").unwrap();
        assert!(tex.starts_with("img:"), "{tex}");
        let id = ed.ensure_texture(&tex).expect("the texture is not drawable");
        assert_eq!(ed.raster.as_ref().unwrap().texture_size(id), Some([48.0, 20.0]));
        ed.release_runtime_textures();
        assert_eq!(ed.ensure_texture(&tex), None, "Stop kept a captured texture");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sky a capture draws is the scene's sky shader, not the Skybox's
    /// plain base colour — asked for from a loop that has run no frame, the
    /// way `shot --after` and a script's first frames ask. It used to come
    /// out flat grey there, fog on the sky and all missing, because the sky
    /// shader is compiled by the frame loop and the capture did not ask for it.
    #[test]
    fn a_capture_draws_the_sky_shader() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let (asker, dir) = scene(&mut ed, "sky");
        std::fs::write(
            dir.join("red.flsl"),
            "shader red {\n  stage sky\n  output color = vec3(1, 0, 0)\n}\n",
        )
        .unwrap();
        ed.project_root = dir.clone();
        let sky = ed.world.spawn();
        ed.world.insert(sky, Transform::IDENTITY);
        ed.world.insert(
            sky,
            Matter::Skybox {
                color: [0.14; 3],
                size: 500.0,
                texture: None,
                tint: [1.0; 3],
                shader: Some(dir.join("red.flsl").to_string_lossy().into_owned()),
                shader_params: Default::default(),
            },
        );
        frames(&mut ed, &dir, |ed| get::<mlua::String>(ed, asker, "front").is_some());
        let png = get::<mlua::String>(&ed, asker, "front").expect("no PNG came back");
        let png = image::load_from_memory_with_format(&png.as_bytes(), image::ImageFormat::Png).unwrap().to_rgba8();
        // A corner is sky: the cube sits in the middle.
        let [r, g, b, _] = png.get_pixel(1, 1).0;
        assert!(r > 150 && g < 60 && b < 60, "the sky came out ({r}, {g}, {b}), not the shader's red");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A host that draws nothing answers at once, and renders nothing.
    #[test]
    fn a_host_with_no_gpu_says_so() {
        let mut ed = crate::Editor::default();
        let (asker, dir) = scene(&mut ed, "nogpu");
        let done = |ed: &crate::Editor| {
            get::<String>(ed, asker, "frontErr").is_some() && get::<String>(ed, asker, "texErr").is_some()
        };
        frames(&mut ed, &dir, done);
        assert_eq!(get::<String>(&ed, asker, "frontErr").as_deref(), Some(floptle_script::NO_RENDERER));
        assert_eq!(get::<String>(&ed, asker, "texErr").as_deref(), Some(floptle_script::NO_RENDERER));
        assert!(ed.capture_jobs.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bgra_readback_comes_out_rgba_without_its_row_padding() {
        // 2×2, rows padded to 12 bytes, BGRA.
        let view = [3, 2, 1, 4, 30, 20, 10, 40, 0, 0, 0, 0, 7, 6, 5, 8, 70, 60, 50, 80, 0, 0, 0, 0];
        assert_eq!(
            super::unpad_rgba(&view, 2, 2, 12, true),
            [1, 2, 3, 4, 10, 20, 30, 40, 5, 6, 7, 8, 50, 60, 70, 80]
        );
    }
}
