//! Offscreen renders of the game view: the player window, the Game panel and shots all draw the world into a texture through here.

use floptle_core::profile::Bucket;
use floptle_core::Entity;
use floptle_core::Light;
use floptle_core::Material;
use floptle_core::Matter;
use floptle_core::math::DVec3;
use floptle_render::Globals;
use floptle_render::InstanceRaw;
use floptle_render::MaterialParams;
use floptle_render::MeshId;
use floptle_render::RaymarchGlobals;
use floptle_render::RenderCamera;
use floptle_render::TexId;
use floptle_core::time::Instant;
#[cfg(feature = "editor-ui")]
use crate::dock::EditorTab;
use crate::shading::{blob_default_material, blob_mat_arrays, collect_shadow_proxies, material_params, post_process_uniforms, shadow_uniforms, skybox_uniforms, vol_fog_uniforms};
use crate::Editor;

use crate::mesh_instances::{wants_prepass, prepass_and_bind, apply_node_tint, count_draw_batches, push_mesh_instances, resolve_mesh_particles};
use crate::draw_2d::{primitive_draw, water_draw, lit_2d_ranks, light2d_uniform};

/// The extras an offscreen render needs to match what the window shows.
///
/// Bundled rather than passed as two more positional arguments because they
/// belong together conceptually — both are "this view is a real view of the
/// game, treat it like one" — and because the call already takes nine.
#[derive(Default, Clone, Copy)]
pub(crate) struct OffscreenOpts<'a> {
    /// The texture behind the depth view, which is what lets this render run the
    /// opaque depth prepass. It cannot be derived from the view: a view cannot
    /// be asked its size and cannot be copied out of.
    ///
    /// `None` means no prepass, and therefore no contact shadows, no
    /// `surfaceGap`, no reflections and no lamp shadows — right for a thumbnail
    /// and wrong for anything a player looks at.
    pub depth_tex: Option<&'a wgpu::Texture>,
    /// Which stored picture screen-space reflections read from and write to.
    /// Each view needs its own: the history carries the camera it was taken
    /// from, and two views sharing one would reproject each other's frames.
    pub history: HistorySlot,
    /// A stacked camera's layer: only the nodes on its layers, over a
    /// transparent clear — no sky, terrain, matter, particles or script
    /// shapes, which the camera beneath has already drawn. The frame's counts
    /// are left to that camera as well.
    pub overlay: bool,
    /// What the camera draws besides its nodes (`setCamera{ shadows = false }`
    /// and the rest): each switched off is a pass this view does not pay for.
    pub draws: floptle_core::CameraDraws,
}

/// The scene's light as a camera with `draws` sees it. A camera that turned
/// the sun's shadows or the fog off sees the scene as if the light said so,
/// and every uniform is built from this copy.
fn light_seen_by(mut light: Light, draws: floptle_core::CameraDraws) -> Light {
    if !draws.shadows {
        light.shadows = false;
        light.shadow_map = false;
        light.contact_shadows = false;
    }
    if !draws.fog {
        light.fog = false;
        light.fog_volumetric = false;
    }
    light
}

/// Which scene-colour history an offscreen render uses.
///
/// An enum rather than a borrow because the histories live on `Editor` and this
/// call already holds `&mut self` — naming the slot lets the render reach its
/// own without the caller having to hand out a second mutable borrow of the
/// same struct.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HistorySlot {
    /// No reflections of the scene here. Thumbnails, the Inspector's camera
    /// preview, a GI bake: none of them is a view a player sees, and each would
    /// otherwise want a full-frame mip chain of its own.
    #[default]
    None,
    /// The docked Game panel — the one offscreen view that is the game.
    GamePanel,
}

/// Copy a presented swapchain image back to the CPU as tightly packed RGBA.
///
/// For `floptle-player --shot`: the one way to see what a real build put on
/// screen. `None` when the surface was not created with `COPY_SRC` (the device
/// declined it) — said out loud by the caller rather than answered with a
/// picture that came from somewhere else.
pub(crate) fn read_back_frame(gpu: &floptle_render::Gpu, tex: &wgpu::Texture) -> Option<Vec<u8>> {
    if !tex.usage().contains(wgpu::TextureUsages::COPY_SRC) {
        return None;
    }
    let (w, h) = (tex.width(), tex.height());
    let bpp = 4u32;
    let padded =
        (w * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("player-shot"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("player-shot") });
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
    gpu.queue.submit([enc.finish()]);
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
    let view = buf.slice(..).get_mapped_range();
    // The surface may be BGRA; the PNG is RGBA.
    let bgra = tex.format().remove_srgb_suffix() == wgpu::TextureFormat::Bgra8Unorm;
    let mut px = Vec::with_capacity((w * h * bpp) as usize);
    for y in 0..h {
        let row = (y * padded) as usize;
        for x in 0..w {
            let i = row + (x * bpp) as usize;
            if bgra {
                px.extend_from_slice(&[view[i + 2], view[i + 1], view[i], view[i + 3]]);
            } else {
                px.extend_from_slice(&view[i..i + 4]);
            }
        }
    }
    drop(view);
    buf.unmap();
    Some(px)
}

impl Editor {
    /// The renderer's per-frame reset, before anything gathers the scene.
    ///
    /// Shared by the editor's frame and a shipped game's, because a build's
    /// frame does not gather through `gather_frame` and
    /// once skipped all of this: its pose table grew by every skinned draw of
    /// every frame, re-uploaded whole each time, until the build crawled, then
    /// stopped drawing the level, then exited.
    ///
    /// - One pose table per frame, not per pass: a frame gathers the scene
    ///   several times over and each pass reads pose indices an earlier gather
    ///   handed out.
    /// - The project's era artefacts, before any gather, so every view has the
    ///   same look.
    /// - Skinned-buffer copies of destroyed entities go back.
    /// - In a browser, the frame's model upload budget starts again (see
    ///   `pump_model_queue`).
    pub(crate) fn begin_draw_frame(&mut self) {
        if let Some(raster) = self.raster.as_mut() {
            raster.begin_skin_frame();
            raster.set_retro_defaults(self.project.retro_artefacts());
        }
        self.skin_variants.prune(&self.world);
        #[cfg(any(target_arch = "wasm32", test))]
        {
            self.model_queue_frame_spent = 0;
        }
    }

    /// Put what the GPU rejected since the last frame into the Console.
    ///
    /// A rejected pipeline or command does not end the session (see
    /// `Gpu::new`), so this is the only place it becomes visible. The editor's
    /// frame and a shipped game's both call it. A build has no Console tab,
    /// but its console writes errors to stderr, and in a browser to the
    /// page's log and crash record. A browser has no stderr. Before a build's
    /// frame called this, a shader a phone's browser refused left no trace
    /// anywhere: the error was written to a stderr that does not exist there.
    pub(crate) fn report_gpu_errors(&mut self) {
        for e in floptle_render::take_gpu_errors() {
            self.console.push(floptle_script::LogLevel::Error, format!("GPU: {e}"), None);
        }
    }

    /// **One frame of a shipped game** — the standalone player's whole loop.
    ///
    /// [`Editor::render`] is this same frame with an editor around it: the
    /// tools, the docked panels, the authoring overlays and egui itself. This
    /// is deliberately not that function with the UI switched off. A build has
    /// no tool state to advance, no autosave, no asset-preview turntable, no
    /// file watchers and no editor camera, and a frame that ran them anyway
    /// would be spending a player's milliseconds on an editor they cannot see.
    ///
    /// What it must never become is a second implementation of *playing the
    /// game*. The two things that are the game — [`Editor::play_step`] for the
    /// step and [`Editor::render_game_into`] for the draw — are called here
    /// exactly as the editor calls them. That shared pair is what makes the
    /// docked Game tab an honest preview of a build rather than a lookalike.
    ///
    /// The list below is therefore the interesting part: it is `render`'s
    /// prefix with every editor-only entry removed, in the same order. When a
    /// new per-frame subsystem is added to `render`, the question to ask is
    /// whether a *player* needs it, and if it does it belongs here too. That
    /// includes what `render` does inside [`Editor::gather_frame`], which this
    /// frame never calls: the renderer's reset is
    /// [`Editor::begin_draw_frame`] here, and it was once missing.
    pub(crate) fn player_frame(&mut self, capture: bool) -> Option<(Vec<u8>, u32, u32)> {
        // Before the clock is read, so the frame's dt is the paced one.
        // The frame's own work starts here; the time before it, back to the
        // last frame's end, was the window system and the OS.
        self.script_host.profile().borrow_mut().begin_frame();
        self.publish_pacing_setup();
        self.pace_frame();
        let now = Instant::now();
        let raw_dt = self.last.map(|l| (now - l).as_secs_f32()).unwrap_or(0.0);
        self.last = Some(now);
        // Smoothed the same way the editor smooths it: a build inherits the
        // frame pacing work, including the snap to the display's period.
        let dt = self.smooth_dt(raw_dt);
        let elapsed = self.started.map(|s| (now - s).as_secs_f32()).unwrap_or(0.0);
        if dt > 0.0 {
            let ms = dt * 1000.0;
            self.frame_ms = if self.frame_ms > 0.0 { self.frame_ms * 0.9 + ms * 0.1 } else { ms };
            self.fps = 1000.0 / self.frame_ms.max(1e-4);
            self.record_frame_time(ms);
        }
        // Clocks the game's own presentation reads: UI style transitions, and
        // the drift on the volumetric-fog noise.
        self.ui_style_dt = dt.min(0.25);
        self.ui_style_rt.begin_frame();
        self.ui_frame_dt = dt.min(0.25);
        self.fog_time = elapsed;
        self.poll_ui_styles(elapsed);
        // Before anything this frame draws: render targets and reflection
        // probes gather the scene too, and their pose indices must be this
        // frame's.
        self.begin_draw_frame();

        // ---- world sync, before the step (render()'s prefix, game parts only) ----
        self.sync_map_meshes();
        self.sync_map_paint();
        self.sync_tilemaps();
        // Capture the dirty flag before `sync_terrain_gpu` consumes it, exactly
        // as the editor frame does — a structural change is a full re-mesh.
        let terrain_full_rebuild = self.terrain_gpu_dirty;
        // LOD rings centre on what the player actually sees. There is no editor
        // fly-camera to fall back to here, so a scene with no active camera
        // streams around the origin rather than around nothing.
        let lod_cam = self.lod_center(
            floptle_core::active_camera(&self.world)
                .map(|e| floptle_core::world_transform(&self.world, e).translation)
                .unwrap_or(self.camera.position),
        );
        // Every part of the frame is in some bucket, so a hitch names where it
        // went rather than leaving it to `untimed`.
        self.timed_into(Bucket::Terrain, |ed| {
            let t0 = floptle_core::profile::Span::new();
            ed.sync_terrain_gpu();
            ed.script_host.profile().borrow_mut().record_part("terrainShadow", t0.ms());
            let t0 = floptle_core::profile::Span::new();
            ed.sync_terrain_palette(false);
            ed.script_host.profile().borrow_mut().record_part("terrainPalette", t0.ms());
            ed.drain_terrain_generates();
            ed.poll_terrain_generates();
            ed.update_terrain_residency(lod_cam);
            ed.stream_far_chunks(lod_cam);
            ed.publish_terrain_busy();
            ed.step_terrain_checkpoint();
            let t0 = floptle_core::profile::Span::new();
            ed.sync_terrain_meshes(terrain_full_rebuild, lod_cam);
            ed.script_host.profile().borrow_mut().record_part("terrainMeshing", t0.ms());
        });
        self.timed_into(Bucket::Assets, |ed| {
            ed.sync_sky_texture();
            ed.sync_sky_shader();
            ed.sync_tex_paint_mirrors();
        });
        // Gamepads: polled once a frame, absent hardware is fine.
        self.pump_input_devices();

        // ---- the game ----
        // `true`: a build's window is the game, so input is never somebody
        // else's. In the editor this is "does the Game view have focus".
        // Untimed as a whole: each script pass inside times itself, and physics
        // and terrain record their own buckets. Timed again out here, all of
        // it counted a second time as `scripts`.
        self.play_step(dt, true);
        // What this frame's scripts queued, before the frame that shows it.
        self.run_compute_jobs(false);
        self.finish_input_frame();
        // Times itself, as `models`.
        self.load_script_swapped_models();

        // ---- what the frame needs on the GPU ----
        self.timed_into(Bucket::Assets, |ed| {
            ed.ensure_vfx_assets();
            ed.ensure_scene_textures();
            ed.ensure_flsl_materials();
            ed.ensure_ui_shaders();
            ed.ensure_post_shaders();
        });
        self.report_gpu_errors();
        self.timed_into(Bucket::Render, |ed| {
            ed.refresh_gi();
            ed.step_reflection_probes();
            ed.sync_field_shapes();
            ed.update_render_targets(elapsed);
            ed.pump_captures();
        });

        // ---- draw ----
        self.apply_project_vsync();
        // The swapchain image is acquired first and the device borrow dropped,
        // because the draw below needs the whole editor mutably.
        // Waiting for a swapchain image is waiting on the GPU (or, with vsync,
        // on the display): `gpuWait`, not time the engine spent.
        let Some(frame) = self.timed_into(Bucket::GpuWait, |ed| ed.gpu.as_mut().and_then(|g| g.acquire())) else {
            // A surface that was outdated or lost reconfigures itself and the
            // frame is skipped — the same answer the editor gives.
            self.drain_script_logs();
            return None;
        };
        let (depth_view, depth_tex, w, h) = self.gpu.as_ref().map(|g| {
            (g.depth_view().clone(), g.depth_texture().clone(), g.config.width, g.config.height)
        })?;
        // The swapchain is the target, and whether it can be sampled is the
        // surface's answer, not ours — see `Gpu::new`, which asks for the flag
        // only where the surface offers it. A browser's canvas does not.
        let samplable = self
            .gpu
            .as_ref()
            .is_some_and(|g| g.config.usage.contains(wgpu::TextureUsages::TEXTURE_BINDING));
        let timed = self.begin_live_gpu_timing();
        self.render_game_into(
            frame.view.clone(),
            depth_view,
            Some(depth_tex),
            [0.0, 0.0],
            [w as f32, h as f32],
            elapsed,
            true,
            samplable,
        );
        if timed {
            self.end_live_gpu_timing();
        }
        // **Photographed before it is presented**, and out of the swapchain
        // image itself — so what lands in the PNG is the frame the player saw,
        // not a second render that resembles it.
        let shot = self.timed_into(Bucket::GpuWait, |ed| {
            capture.then(|| ed.gpu.as_ref().and_then(|g| read_back_frame(g, &frame.surface.texture))).flatten()
        });
        self.timed_into(Bucket::Present, |_| frame.present());

        // Anything the game's scripts printed reaches stderr (the Console
        // mirrors there in player mode), so a shipped build can still be
        // debugged from a terminal.
        self.drain_script_logs();
        // The frame is over: fold every bucket into its history, once, exactly
        // as the editor frame does.
        self.end_profile_frame();
        shot.map(|px| (px, w, h))
    }

    /// Render the whole scene from `cam` (at `aspect`) into offscreen color+depth views —
    /// the shared body behind the Inspector camera preview and the split-view Game render.
    /// `cull_mask` is the rendering camera's layer bitmask (bit i = project
    /// layer i; `u32::MAX` = everything). `skip_tex` excludes one material
    /// texture from resolution — a target camera must not sample its own
    /// render target mid-pass (wgpu forbids attachment+sampled in one pass).
    /// The scene-colour history a given slot owns, if it has one.
    pub(crate) fn history_slot(&self, slot: HistorySlot) -> Option<&floptle_render::SceneHistory> {
        match slot {
            HistorySlot::None => None,
            HistorySlot::GamePanel => self.game_scene_history.as_ref(),
        }
    }

    /// Allocate, resize or drop an offscreen view's stored picture to match what
    /// it is being asked for. Returns whether the texture behind it changed.
    ///
    /// Sized to the COMPOSITED resolution it is handed, so a docked Game panel
    /// reflects at the resolution it is drawn at — and, in retro mode, at the
    /// retro resolution, exactly as the window does. A reflection sharper than
    /// the picture around it reads as a bug in the picture.
    pub(crate) fn sync_offscreen_history(
        &mut self,
        slot: HistorySlot,
        want: bool,
        size: (u32, u32),
    ) -> bool {
        if slot == HistorySlot::None {
            return false;
        }
        let Some(gpu) = self.gpu.as_ref() else { return false };
        let fmt = gpu.scene_format();
        let (pw, ph) = (size.0.max(1), size.1.max(1));
        let dev = &gpu.device;
        let hist = match slot {
            HistorySlot::None => return false,
            HistorySlot::GamePanel => &mut self.game_scene_history,
        };
        if !want {
            return hist.take().is_some();
        }
        match hist.as_mut() {
            Some(h) => h.resize_to(dev, pw, ph, fmt),
            None => {
                *hist = Some(floptle_render::SceneHistory::new(dev, pw, ph, fmt));
                true
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_world_into(
        &mut self,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        cam: &RenderCamera,
        aspect: f32,
        elapsed: f32,
        cull_mask: u32,
        skip_tex: Option<TexId>,
        // The target's pixel size. Explicit because only a view is passed and a
        // view cannot be asked how big it is — and the 2D lighting G-buffer has
        // to match the frame exactly or the composite lands stretched.
        size: (u32, u32),
        opts: OffscreenOpts<'_>,
    ) {
        // The whole of this view is render time, net of the buckets recorded
        // inside it (assets warmed, scatter): the build's frame drew its world
        // through here with no bucket at all, and all of it read as untimed.
        let whole_t = floptle_core::profile::Span::new();
        let measured0 = self.script_host.profile().borrow().frame_measured();
        // The same pre-warm the window frame does. This path is reached without
        // one by `floptle shot` and by any embedder driving the editor headless,
        // and a gather here resolves textures through exactly the same registry:
        // without this, a shot of a scene draws every model in the texture it
        // was IMPORTED with, whatever its materials say.
        self.ensure_scene_textures();
        // Particle textures and mesh-track models live in their own registry,
        // and a live effect's billboards resolve their texture in this same
        // gather — so a shot draws every burst textured, not as a tinted square.
        self.ensure_vfx_assets();
        // `.flsl` materials compile and bind here too: the gather below reads
        // `flsl_binds`, and nothing else fills them on the `shot` path, so a
        // shot drew every shader-wearing mesh as its plain white fallback.
        self.ensure_flsl_materials();
        let view_proj = cam.view_proj(aspect);
        // Layer names resolve to bits only when a mask actually culls.
        let layer_table = (cull_mask != u32::MAX).then(|| self.project.build_layers());
        // Read from the scene's own PostProcess node rather than taken as an
        // argument: every caller of this path — the Game view, a render target, a
        // thumbnail — is showing the same scene, and posterize is that scene's
        // palette. A parameter would be one more thing three call sites have to
        // remember to pass the same way.
        let palette = crate::shading::post_process_uniforms(&self.world).0.palette();

        // Sky-shader uniforms (Inspector knobs over `.flsl` defaults), resolved before any
        // GPU borrow — the offscreen / Game render reuses the same values as the editor view.
        let sky_active = opts.draws.sky && self.sky_shader.is_some();
        let sky_uniform_vals = self.sky_uniform_values();

        let light_node = light_seen_by(
            self.world.query::<Light>().next().map(|(_, l)| *l).unwrap_or_default(),
            opts.draws,
        );
        let sun = crate::shading::sun_vec(&self.world, &light_node, cam.world_position);
        let particle_light = crate::shading::particle_light(&self.world, &light_node, cam.world_position);
        let li = light_node.intensity;
        // Whether `Auto` reads as 2D in this scene, asked once rather than per node.
        let flat_camera = floptle_core::active_camera(&self.world).is_some_and(|ce| {
            matches!(self.world.get::<Matter>(ce), Some(Matter::Camera { ortho: true, .. }))
        });
        // one split, both halves — the 3D slots the globals want and the 2D ones
        // the gather filters by, walked once rather than here and again at the
        // pass that builds the 2D uniform.
        let off_split = crate::shading::split_point_lights(
            &self.world,
            cam.world_position,
            &self.project.sorting_order(),
            flat_camera,
        );
        let lit3 = off_split.three_d;
        let (pl_count, pl_pos, pl_col, pl_shape, pl_rot, pl_cone) = (
            [lit3.count as f32, 0.0, 0.0, 0.0],
            lit3.pos,
            lit3.color,
            lit3.shape,
            lit3.rot,
            lit3.cone,
        );
        // Same question and same answer as the window path: what a reflective
        // surface sees when the screen-space march finds nothing.
        let (probe_meta, probe_pos, probe_half) = crate::reflect_capture::probe_uniforms(
            &self.world,
            &self.probe_slots,
            self.capturing_probes,
            cam.world_position,
            crate::shading::reflection_clamp(&light_node),
        );
        // Any lamp casting here? Same question and same answer as the window
        // path — a local shadow marches the prepass, so this decides whether one
        // has to run.
        let point_shadows = lit3.shape[..lit3.count.min(16)]
            .iter()
            .any(|s| (s[3] as u32) & 2 != 0);
        let (sh_params, sh_tint, sh_extra) = shadow_uniforms(&light_node);
        let contact = crate::shading::contact_uniform(&light_node);
        let ((fog_color, fog_params, fog_extra), particle_fog) =
            crate::shading::fog_uniforms_and_particles_at(
                &light_node,
                &self.world,
                &self.script_host.declared_water(),
                &crate::shading::roofed_by(self.sim.as_ref()),
                cam.world_position,
            );
        let (atmo_meta, atmo_color, atmo_body, atmo_params) =
            crate::shading::atmo_uniforms(&self.world, cam.world_position);
        let (star_meta, star_pos, star_color) =
            crate::shading::star_uniforms(&self.world, &light_node, cam.world_position);
        // Proxies are what lets a raster mesh cast at all, and a LAMP marches the
        // same list now — so the sun's switch alone can no longer decide whether
        // they are collected. A scene with the sun's shadows off and a torch
        // casting would otherwise hand the shader an empty proxy list, and the
        // torch would shine through every crate in the room.
        let (prox_count, prox_a, prox_b, prox_rot) = collect_shadow_proxies(
            &self.world,
            cam.world_position,
            light_node.shadows || point_shadows,
        );
        let (sun_vp, sun_map, sun_extra) = crate::shading::sun_map_lanes(
            &light_node,
            sun,
            (star_meta[0] >= 1.0).then_some(star_pos[0]),
            view_proj,
        );
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            sun_vp,
            sun_map,
            sun_extra,
            time: { let w = self.sway_wind(cam.world_position); [elapsed, w.x, w.y, w.z] },
            light_dir: sun,
            light_color: [light_node.color[0] * li, light_node.color[1] * li, light_node.color[2] * li, 0.0],
            ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
            point_count: pl_count,
            point_pos: pl_pos,
            point_color: pl_col,
            point_shape: pl_shape,
            point_rot: pl_rot,
            point_cone: pl_cone,
            // Meshed terrain reads the triplanar scale + the per-slot nearest /
            // GLOW bitmasks here (bitmasks as u32 — bit-exact at 32 slots).
            terrain_mask: [0.0, 0.22, 0.0, 0.0],
            terrain_bits: [
                crate::terrain_edit::terrain_nearest_mask(&self.terrain_textures, &self.texture_settings, &self.project_root),
                self.terrain_glow_mask,
                0,
                0,
            ],
        };

        // Camera-relative instances + blobs, exactly like the main gather —
        // including the frustum cull, built from this camera's matrix.
        // The sun map's casters the camera culls, before the draw loop takes
        // the renderer: see `shadow_map_casters`.
        let mut shadow_casters = self.shadow_map_casters(&globals, cam.world_position, view_proj);
        let off_frustum = floptle_render::Frustum::from_view_proj(view_proj);
        let ents: Vec<(Entity, Matter)> =
            self.world.query::<Matter>().map(|(e, m)| (e, m.clone())).collect();
        // Per-node paint, resolved before the draw loop (which borrows `raster`
        // mutably, so it can't call &self helpers). This path renders the world too, so
        // painted props must look identical here. Empty for unpainted scenes.
        // Every node's sorting-layer Z, resolved before the draw loop borrows
        // `raster` mutably. Empty for a scene that uses no sorting layers, which
        // is every scene until one opts in. (`flat_camera` was asked above, with
        // the light split that needs it.)
        let sort_z = crate::sprite2d::draw_offsets(&self.world, &self.project, cam.world_position);

        // The same one value the pass is handed below, from the same split — and
        // the same helper the Scene view uses, so this view cannot decide a
        // different set of lit surfaces from that one.
        let lights_2d = light2d_uniform(&self.world, &off_split.two_d, view_proj);
        let reach_2d = lights_2d.reach();
        // Which flat nodes take part in 2D lighting, and at which sorting rank —
        // resolved here for the same reason `sort_z` is: the draw loop below
        // borrows `raster` mutably and cannot call an `&self` helper.
        let lit2d = lit_2d_ranks(&self.world, &self.project, flat_camera, reach_2d);
        let paint_bases: std::collections::HashMap<Entity, Vec<u32>> = self
            .world
            .query::<floptle_core::VertexPaint>()
            .filter_map(|(e, vp)| {
                let b = self.paint_data.get(&vp.id)?;
                Some((e, b.parts.iter().map(|&(base, _)| base).collect()))
            })
            .collect();
        let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
        // The 2D lighting G-buffer's draw list, built in this loop from the very
        // instances the raster pass gets (`Light2dInstance::from_raster`). That
        // is the whole mitigation for deferred's second draw path: there is no
        // second walk of the world to keep in step.
        let mut flat2d: Vec<(MeshId, Option<TexId>, floptle_render::Light2dInstance)> = Vec::new();
        // GPU-skinned parts, gathered alongside the plain ones and
        // drawn through the skinned pipelines in the same passes.
        let mut skin_draws: Vec<floptle_render::SkinDraw> = Vec::new();
        // Custom `.flsl` materials draw offscreen too (bindings were refreshed
        // by ensure_flsl_materials before any gather this frame).
        let mut flsl_draws: Vec<floptle_render::FlslDraw> = Vec::new();
        let mut blobs: Vec<(DVec3, f32, MaterialParams)> = Vec::new();
        // Reused scratch for CPU vertex skinning (deformed vertices, re-uploaded per part),
        // exactly like the main gather — so offscreen views animate skinned meshes too.
        let mut skin_scratch: Vec<floptle_render::Vertex> = Vec::new();
        // How much the frustum cull skipped, published below alongside the rest
        // of this gather's counts. Without them a Game-view session's
        // `perf.counts()` would be whatever the Scene view last computed, or all
        // zero if it never
        // ran this session.
        let mut culled_nodes = 0usize;
        // The node walk, and the cull test inside it, named as render parts.
        let timing = self.script_host.profile().borrow().enabled();
        let walk_t = floptle_core::profile::Span::new();
        let mut cull_ms = 0.0f32;
        for (ent, matter) in &ents {
            if floptle_core::is_hidden(&self.world, *ent) {
                continue;
            }
            if floptle_core::is_disabled(&self.world, *ent) {
                continue;
            }
            // Camera cull mask: skip nodes on layers this camera doesn't render.
            if let Some(lt) = &layer_table
                && (cull_mask >> lt.index_for(&self.world, *ent)) & 1 == 0
            {
                continue;
            }
            let mut t = floptle_core::world_transform(&self.world, *ent);
            // …and the same nudge here, or the Game view would sort differently
            // from the Scene view — the drift this file has already had three
            // times over.
            t.translation += sort_z.get(ent).copied().unwrap_or_default();
            // …and the same cull the screen uses, against this
            // camera's frustum. An offscreen target that culled differently from
            // the window would be a mirror showing a different room.
            // A pixels-per-unit sprite is drawn at its texture's size, not at
            // its `size` field, so culling has to know the texture — otherwise a
            // sixteen-unit sprite is culled on a radius of half a unit and pops
            // out of existence at the edge of the screen.
            let sprite_px = matches!(matter, Matter::Sprite { .. })
                .then(|| {
                    let p = self.world.get::<Material>(*ent)?.texture.clone()?;
                    let id = self.texture_registry.get(&p).copied()?;
                    self.raster.as_ref()?.texture_size(id)
                })
                .flatten();
            let cull_t = timing.then(floptle_core::profile::Span::new);
            let off_screen = crate::node_bounds::node_is_off_screen(
                &self.world, &self.mesh_registry, &self.anim.poses,
                *ent, matter, &t, cam.world_position, &off_frustum, sprite_px,
            );
            if let Some(c) = cull_t {
                cull_ms += c.ms();
            }
            if off_screen {
                culled_nodes += 1;
                continue;
            }
            let mat = self.world.get::<Material>(*ent).cloned();
            // Texture-painted node → also push its paint overlay; the base draws normally
            // below (see the main path).
            if self.world.get::<floptle_core::TexturePaint>(*ent).is_some() {
                let model = t.render_matrix(cam.world_position);
                let mp = mat.as_ref().map(material_params).unwrap_or_else(|| MaterialParams::flat([1.0, 1.0, 1.0]));
                crate::paint_tex::push_painted_node(&self.world, &self.paint_tex, *ent, model, &mp, &mut instances);
            }
            // The node's texture and surface extras, resolved as the window's
            // gather resolves them: a material with surface maps comes back as
            // one combined texture, and its extras (Physical shading, the
            // retro flags, reflectivity) as an index stamped below. This view
            // took the base texture alone, so every docked Game view, shot and
            // capture drew a normal-mapped Physical material flat and Classic.
            let (tex, node_ext) = match (mat.as_ref(), self.gpu.as_ref(), self.raster.as_mut()) {
                (Some(m), Some(gpu), Some(raster)) => {
                    let (t, p) = crate::shading::material_draw(raster, gpu, m, &self.texture_registry, None);
                    (t, p.ext_index)
                }
                _ => (None, 0),
            };
            let tex = tex.filter(|id| Some(*id) != skip_tex);
            let flsl = self.flsl_binds.get(ent).map(|b| b.binding);
            // Where this node's draws begin, for the tint stamp after the match.
            let tint_from = (instances.len(), flsl_draws.len(), skin_draws.len(), flat2d.len());
            match matter {
                // Same helper as the main gather, vertex paint and all — this
                // arm used to build its own instance and forgot the paint, so a
                // painted cube was painted in the Scene view and plain in the
                // Game view.
                Matter::Primitive { shape, color } => {
                    let model = t.render_matrix(cam.world_position);
                    if let Some((mesh, raw)) = primitive_draw(
                        *shape,
                        *color,
                        mat.as_ref(),
                        model,
                        &self.mesh_ids,
                        paint_bases.get(ent).map(|v| v.as_slice()),
                        self.raster.as_ref(),
                    ) {
                        match flsl {
                            Some(b) => flsl_draws.push((mesh, tex, b, raw)),
                            None => instances.push((mesh, tex, raw)),
                        }
                    }
                }
                // …and water, which this arm claimed was drawn by the raymarch
                // and is not: it is a raster instance, and only the Scene view's
                // gather ever built one. An ocean was there while you edited the
                // scene and gone the moment you looked through the game's own
                // camera.
                Matter::WaterVolume { .. } => {
                    if let Some((mesh, raw)) = water_draw(
                        matter,
                        mat.as_ref(),
                        &t,
                        cam.world_position,
                        &self.mesh_ids,
                        self.raster.as_mut(),
                    ) {
                        match flsl {
                            Some(b) => flsl_draws.push((mesh, tex, b, raw)),
                            None => instances.push((mesh, tex, raw)),
                        }
                    }
                }
                Matter::Blob { scale } => {
                    let mp = mat.as_ref().map(material_params).unwrap_or_else(blob_default_material);
                    blobs.push((t.translation, scale * t.scale.x, mp));
                }
                Matter::Mesh { asset_path } => {
                    // Same animated/skinned gather as the main surface path (shared
                    // helper) — a docked/split Game view or camera preview must show the
                    // character moving, not frozen in bind pose. gpu/raster are freshly
                    // borrowed here (disjoint fields; the loop's earlier world/texture
                    // borrows already produced owned values).
                    if let (Some(gpu), Some(raster), Some(asset)) = (
                        self.gpu.as_ref(),
                        self.raster.as_mut(),
                        self.mesh_registry.get(asset_path),
                    ) {
                        let model = t.render_matrix(cam.world_position);
                        let mp = mat.as_ref().map(material_params);
                        let obj_mats = self.world.get::<floptle_core::ObjectMaterials>(*ent);
                        let pose = self.anim.poses.get(ent).map(|v| v.as_slice());
                        let node_paint = paint_bases.get(ent).map(|v| v.as_slice());
                        push_mesh_instances(
                            gpu, raster, asset, pose, model, tex, mp.as_ref(), obj_mats,
                            mat.is_none().then(|| self.world.get::<floptle_core::MaterialPatch>(*ent)).flatten(),
                            &self.texture_registry, node_paint,
                            *ent, &mut self.skin_variants,
                            &mut skin_scratch, &mut instances, &mut skin_draws, flsl,
                            &mut flsl_draws, &self.obj_flsl_binds,
                        );
                    }
                }
                // Blockout geometry, through the same per-part path as imported
                // models (parts = material slots). Without this arm the Game
                // view — and every camera preview / render target, which all
                // come through here — drew the level as empty air while the
                // Scene view showed it fine.
                Matter::MapMesh { id } => {
                    if let (Some(gpu), Some(raster), Some(asset)) = (
                        self.gpu.as_ref(),
                        self.raster.as_mut(),
                        self.mesh_registry.get(&crate::map_edit::map_key(*id)),
                    ) {
                        let model = t.render_matrix(cam.world_position);
                        let mp = mat.as_ref().map(material_params);
                        let obj_mats = self.world.get::<floptle_core::ObjectMaterials>(*ent);
                        let node_paint = paint_bases.get(ent).map(|v| v.as_slice());
                        push_mesh_instances(
                            gpu, raster, asset, None, model, tex, mp.as_ref(), obj_mats,
                            mat.is_none().then(|| self.world.get::<floptle_core::MaterialPatch>(*ent)).flatten(),
                            &self.texture_registry, node_paint,
                            *ent, &mut self.skin_variants,
                            &mut skin_scratch, &mut instances, &mut skin_draws, flsl,
                            &mut flsl_draws, &self.obj_flsl_binds,
                        );
                    }
                }
                // The 2D layer, for exactly the reason the arm above it exists.
                // A tilemap and a sprite batch were gathered only by the main
                // surface pass, so every view that comes through here — the
                // docked or split Game view, a camera preview, any render
                // target — drew a 2D game as an empty background while the
                // Scene view showed the level. Which is to say a 2D game was
                // invisible in the one view that is the game.
                Matter::Tilemap { .. } => {
                    let model = t.render_matrix(cam.world_position);
                    let mut draws = Vec::new();
                    crate::sprite2d::tilemap_draws(
                        &self.tilemaps,
                        &self.texture_registry,
                        *ent,
                        model,
                        mat.as_ref(),
                        tex,
                        &mut draws,
                    );
                    // A tilemap casts from its solid squares, laid over its
                    // tiles as one occluder in the tiles' own frame.
                    let lit = lit2d.get(ent).map(|&(rank, casts)| {
                        let (tiles_cast, occluder) =
                            crate::draw_2d::tilemap_casting(&self.world, &self.tilemaps, *ent, casts);
                        (rank, tiles_cast, occluder)
                    });
                    if let (Some((rank, _, Some(occ))), Some(first)) = (lit, draws.first()) {
                        flat2d.push((occ, None, floptle_render::Light2dInstance::occluder(first.2.model, rank)));
                    }
                    for mut draw in draws {
                        // On the 2D lighting path: the raster pass draws it
                        // UNLIT, and the composite corrects that by the light's
                        // difference. The G-buffer instance is
                        // taken from the very same value, so the two cannot
                        // disagree about what is being corrected.
                        if let Some((rank, casts, _)) = lit {
                            draw.2.force_unlit();
                            flat2d.push((
                                draw.0,
                                draw.1,
                                floptle_render::Light2dInstance::from_raster(&draw.2, rank, casts),
                            ));
                        }
                        match flsl {
                            Some(b) => flsl_draws.push((draw.0, draw.1, b, draw.2)),
                            None => instances.push(draw),
                        }
                    }
                }
                Matter::Sprite { ppu, size, cell, flip_x, flip_y, pivot } => {
                    if let Some(&mesh) = self.mesh_ids.get(floptle_core::Shape::Plane as usize) {
                        let model = t.render_matrix(cam.world_position);
                        let px = self
                            .raster
                            .as_ref()
                            .zip(tex)
                            .and_then(|(r, id)| r.texture_size(id));
                        let texel = px
                            .map(|[w, h]| [1.0 / w.max(1.0), 1.0 / h.max(1.0)])
                            .unwrap_or([0.0, 0.0]);
                        let raws = crate::sprite2d::sprite_draws_of(
                            *ppu, *size, *cell, *flip_x, *flip_y, *pivot,
                            model, t.scale, mat.as_ref(), px, texel,
                        );
                        for mut raw in raws {
                            // Same as a batch: unlit in the raster pass and
                            // corrected by the 2D lighting pass, so the two never
                            // light it twice.
                            if let Some(&(rank, casts)) = lit2d.get(ent) {
                                raw.force_unlit();
                                flat2d.push((
                                    mesh,
                                    tex,
                                    floptle_render::Light2dInstance::from_raster(&raw, rank, casts),
                                ));
                            }
                            match flsl {
                                Some(b) => flsl_draws.push((mesh, tex, b, raw)),
                                None => instances.push((mesh, tex, raw)),
                            }
                        }
                    }
                }
                Matter::SpriteBatch { size } => {
                    if let Some(&mesh) = self.mesh_ids.get(floptle_core::Shape::Plane as usize) {
                        let model = t.render_matrix(cam.world_position);
                        let texel = self
                            .raster
                            .as_ref()
                            .zip(tex)
                            .and_then(|(r, id)| r.texture_size(id))
                            .map(|[w, h]| [1.0 / w.max(1.0), 1.0 / h.max(1.0)])
                            .unwrap_or([0.0, 0.0]);
                        let mut raws = Vec::new();
                        crate::sprite2d::sprite_draws(
                            &self.world, *ent, *size, model, mat.as_ref(), texel, &mut raws,
                        );
                        for mut raw in raws {
                            // …and the same for a sprite batch: unlit in the
                            // raster pass, corrected by the difference.
                            if let Some(&(rank, casts)) = lit2d.get(ent) {
                                raw.force_unlit();
                                flat2d.push((
                                    mesh,
                                    tex,
                                    floptle_render::Light2dInstance::from_raster(&raw, rank, casts),
                                ));
                            }
                            match flsl {
                                Some(b) => flsl_draws.push((mesh, tex, b, raw)),
                                None => instances.push((mesh, tex, raw)),
                            }
                        }
                    }
                }
                // Listed rather than `_ => {}`, and that is the point. This
                // match silently dropped Tilemap and SpriteBatch for two
                // releases, and had already done the same to MapMesh before
                // them — a catch-all arm cannot be reviewed, and the next kind
                // of matter would have joined them without a word. Naming each
                // one makes the compiler ask.
                //
                // Everything below is drawn somewhere else in this function or
                // is not drawable at all:
                Matter::Terrain { .. } => {} // push_terrain_instances, further down
                Matter::FieldShape { .. } => {} // the raymarch pass
                Matter::Skybox { .. } => {}      // skybox_uniforms → the sky stage
                Matter::PointLight { .. } => {}  // collect_point_lights, into globals
                Matter::Camera { .. } => {}      // it is the eye, not a thing seen
                Matter::PostProcess { .. } => {} // post_process_uniforms
                Matter::LightProbes { .. } => {} // baked GI: uniforms + one texture
                // Drawn as an outline in the Scene view, and nothing at all in
                // the game: a navmesh is a thing to path on, not to look at.
                Matter::NavMesh { .. } | Matter::NavLink { .. } | Matter::NavArea { .. } => {}
                // The capture is six renders of its own, taken elsewhere; here
                // it is four uniform lanes and one texture, like the GI above.
                Matter::ReflectionProbe { .. } => {}
                Matter::GravityVolume { .. } => {} // physics only; no visual
                Matter::Empty => {}              // a transform with nothing on it
            }
            // …and the same tint the window path applies, through the same
            // function: a model tinted in the Scene view and plain in the Game
            // view is the drift this file's test exists to catch.
            apply_node_tint(
                self.world.get::<floptle_core::Tint>(*ent),
                tint_from,
                &mut instances,
                &mut flsl_draws,
                &mut skin_draws,
                &mut flat2d,
            );
            // …and the extras stamp, the window's rule: only where a part's
            // own override has not already set one.
            if node_ext != 0 {
                use floptle_render::{ext_index_of, set_ext_index};
                let fill = |raw: &mut floptle_render::InstanceRaw| {
                    if ext_index_of(raw) == 0 {
                        set_ext_index(raw, node_ext);
                    }
                };
                for (_, _, raw) in &mut instances[tint_from.0..] {
                    fill(raw);
                }
                for (_, _, _, raw) in &mut flsl_draws[tint_from.1..] {
                    fill(raw);
                }
                for d in &mut skin_draws[tint_from.2..] {
                    fill(&mut d.instance);
                }
            }
        }

        if timing {
            let mut p = self.script_host.profile().borrow_mut();
            p.record_part("renderCull", cull_ms);
            p.record_part("renderInstancing", (walk_t.ms() - cull_ms).max(0.0));
        }
        let terrain_t = floptle_core::profile::Span::new();
        let (mut sky_params, sky_tint, sky_rot, sky_solid) = skybox_uniforms(&self.world);
        // No sky: the skybox's solid colour behind everything.
        if !opts.draws.sky {
            sky_params[0] = 0.0;
        }
        // An orthographic camera's sky, through its field of view.
        sky_params[2] = cam.projection.sky_spread();
        let clear = [sky_solid[0], sky_solid[1], sky_solid[2], 1.0];
        // SDF AO from the scene's PostProcess node shades SDF matter in offscreen
        // views too (previews + the split Game viewport).
        let (_, rm_ao_params) = post_process_uniforms(&self.world);
        let terrain_mat = self.terrain_material();
        // Terrain 2.0 (P2): the meshed terrain draws in this offscreen/Game view too, or a
        // docked Game viewport would show empty ground (its volume is `w = 3`, not drawn by
        // the raymarch). Same instance push as the main Scene view.
        if !opts.overlay
            && opts.draws.terrain
            && let Some(raster) = self.raster.as_ref()
        {
            crate::terrain_edit::push_terrain_instances(
                &self.terrain_render,
                &self.terrains,
                &self.world,
                raster,
                &terrain_mat,
                cam.world_position,
                view_proj,
                self.mesh_ids[floptle_core::Shape::Sphere as usize],
                self.now(),
                &mut instances,
            );
            crate::decals::push_decal_instances(&self.decal_batches, &self.world, raster, cam.world_position, &mut instances);
        } else if opts.overlay
            && opts.draws.terrain
            && globals.sun_map[0] > 0.5
            && let Some(raster) = self.raster.as_ref()
        {
            // A stacked camera draws no terrain, yet the ground still shades
            // what it does draw: the terrain goes into its sun map, undrawn.
            // Where that map has an answer the shader takes it and marches
            // nothing, so without the terrain in it a view model in the shade
            // of a hill was lit as in open sun. Culled by the sun's frustum:
            // the hill shading the arms is as often behind the player.
            let sun_vp = floptle_core::math::Mat4::from_cols_array_2d(&globals.sun_vp);
            crate::terrain_edit::push_terrain_instances(
                &self.terrain_render,
                &self.terrains,
                &self.world,
                raster,
                &terrain_mat,
                cam.world_position,
                sun_vp,
                self.mesh_ids[floptle_core::Shape::Sphere as usize],
                self.now(),
                &mut shadow_casters,
            );
        }
        if timing {
            self.script_host.profile().borrow_mut().record_part("renderInstancing", terrain_t.ms());
        }
        // Scatter too, through the window gather's own function: a capture
        // without it photographs bare ground under everything that stands on it.
        let scatter_t = floptle_core::profile::Span::new();
        let scatter_props = match (opts.overlay, self.raster.as_mut(), self.gpu.as_ref()) {
            (false, Some(raster), Some(gpu)) => crate::scatter_draw::push_scatter(
                &mut self.script_host,
                &self.world,
                self.sim.as_ref(),
                raster,
                gpu,
                &self.scatter_protos,
                &self.scatter_proto_radius,
                &mut self.scatter_cache,
                cam.world_position,
                &off_frustum,
                &mut instances,
            ),
            _ => 0,
        };
        // In its own bucket, as the window gather records it, and out of the
        // render time around it.
        if scatter_props > 0 {
            self.script_host.profile().borrow_mut().record(Bucket::Scatter, scatter_t.ms());
        }
        let passes_t = floptle_core::profile::Span::new();
        // …and the counts a game can read via `perf.counts()`. Every view that
        // comes through this gather — the docked or split Game view, `floptle
        // shot`, a render target — would otherwise leave the profile holding whatever the
        // Scene-view gather in `render()` had last written, or all zero if that
        // gather never ran this session. That is exactly why a real 40-light
        // scene read `lights=0` in one session and correctly in another: the
        // number was never this camera's, it was whichever gather happened to
        // run last. `lights`/`lightsDropped` come from `off_split` above,
        // computed for this camera and this frame.
        if !opts.overlay {
            self.light_counts = (off_split.three_d.count + off_split.two_d.count, off_split.dropped);
            self.warn_lights_dropped(off_split.dropped);
            let chunks: usize = self.terrain_render.values().map(|r| r.slots.len()).sum();
            let particles = self.vfx.live_particles();
            let (effects, effects_dropped) = self.vfx.detached_counts();
            let (lights, lights_dropped) = self.light_counts;
            // Nothing is playing on a build with no sound card, and zero is
            // the honest number rather than a hidden row.
            #[cfg(feature = "devices")]
            let voices = self.audio.live_voices();
            #[cfg(not(feature = "devices"))]
            let voices = 0;
            let profile = self.script_host.profile().clone();
            // Same "off means off" guard as the main gather — see there.
            let (draws, unbatched) = if profile.borrow().enabled() {
                count_draw_batches(&instances, &flsl_draws, &skin_draws, &|b| {
                    self.raster.as_ref().and_then(|r| r.flsl_binding_shader(b))
                })
            } else {
                Default::default()
            };
            let (field_chunks, field_edited, field_stand_ins) =
                if profile.borrow().enabled() { self.field_residency() } else { (0, 0, 0) };
            profile.borrow_mut().set_counts(floptle_core::profile::Counts {
                nodes: ents.len(),
                culled: culled_nodes,
                instances: instances.len(),
                draws,
                unbatched,
                chunks,
                field_chunks,
                field_edited,
                field_stand_ins,
                props: scatter_props,
                particles,
                effects,
                effects_dropped,
                lights,
                lights_dropped,
                voices,
                flat2d: flat2d.len(),
                // Filled in once the volumes are known: `note_shadow_casters`.
                ..Default::default()
            });
        }
        let show_blobs = !opts.overlay && self.project.matter && !blobs.is_empty();
        // A textured skybox is drawn by the raymarch pass (missed rays sample the
        // sky) — keep it running even with no terrain/blobs in the scene.
        let rm_draw = !opts.overlay
            && (show_blobs
                || (opts.draws.terrain && !self.terrains.is_empty())
                || sky_params[0] >= 0.5
                || sky_active // a procedural sky shader must run the raymarch (sky pass)
                // …and so must an atmosphere, the same as the window gather.
                || (opts.draws.sky && atmo_meta[0] >= 1.0)
                || !self.flsl_shape_slots.is_empty());
        // Reflections in an offscreen view, on exactly the terms the window gets
        // them: this view's own stored picture, allocated the first frame it is
        // asked for and dropped when it stops being. A view with no history slot
        // (a thumbnail, a bake) reports off and reflects the sky, which is what
        // it did before any of this existed.
        let want_ssr = light_node.reflections
            && opts.history != HistorySlot::None
            && opts.depth_tex.is_some();
        // Glass wants the same picture for the opposite reason — see the window
        // path. Asked of the raster's material store, which is shared, so the
        // answer is the same one the window path gets for the same scene.
        let glass = opts.history != HistorySlot::None
            && self.raster.as_ref().is_some_and(|r| r.any_transmissive(&instances) || r.flsl_draws_read_scene(&flsl_draws));
        let ssr_rebuilt = self.sync_offscreen_history(opts.history, want_ssr || glass, size);
        let history = self.history_slot(opts.history);
        let ssr = crate::shading::ssr_uniform(
            &light_node,
            want_ssr && history.is_some_and(|h| h.is_primed()),
        );
        let ssr_prev_vp = history
            .and_then(|h| h.prev_view_proj(cam.world_position))
            .unwrap_or(floptle_core::math::Mat4::IDENTITY)
            .to_cols_array_2d();
        // Cloned out here because the draw block below borrows `self.raster` and
        // `self.raymarch` mutably, and the history lives on `self` too. A view
        // and a sampler are both refcounted handles, so this is two bumps.
        let history_bind =
            history.map(|h| (h.view().clone(), h.sampler().clone()));
        let _ = ssr_rebuilt;
        let rm = {
            let mut arr = [[0.0f32; 4]; 16];
            let n = blobs.len().min(16);
            if show_blobs {
                for (i, (c, s, _)) in blobs.iter().take(16).enumerate() {
                    let cr = (*c - cam.world_position).as_vec3();
                    arr[i] = [cr.x, cr.y, cr.z, s.max(0.05)];
                }
            }
            let (blob_tint, blob_emissive, blob_specular, blob_params, blob_rim) =
                if show_blobs { blob_mat_arrays(&blobs) } else { blob_mat_arrays(&[]) };
            let tm = &terrain_mat;
            let (vol_fog_a, vol_fog_b, vol_fog_c) =
                vol_fog_uniforms(&light_node, self.fog_time, cam.world_position.y as f32);
            let mut g = RaymarchGlobals {
                view_proj: view_proj.to_cols_array_2d(),
                inv_view_proj: view_proj.inverse().to_cols_array_2d(),
                light_dir: sun,
                light_color: [light_node.color[0] * li, light_node.color[1] * li, light_node.color[2] * li, 0.0],
                ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
                bg: [clear[0], clear[1], clear[2], 1.0],
                center: [0.0; 4],
                params: [elapsed, if show_blobs { n as f32 } else { 0.0 }, 0.0, 0.0],
                vol_center: [[0.0; 4]; floptle_render::MAX_VOLUMES],
                vol_half: [[1.0, 1.0, 1.0, 0.5]; floptle_render::MAX_VOLUMES],
                vol_atlas: [[0.0; 4]; floptle_render::MAX_VOLUMES],
                vol_dims: [[1.0, 1.0, 1.0, 0.0]; floptle_render::MAX_VOLUMES],
                // .w = per-slot nearest mask (bit i = slot i is Pixelated). The palette
                // is one texture_2d_array with one sampler, so the shader can't pick a
                // sampler per slot — it reads this mask and selects the result instead.
                terrain_tint: [
                    tm.color[0],
                    tm.color[1],
                    tm.color[2],
                    // Legacy raymarch path packs the mask in an f32 lane — exact for
                    // slots 0..23 only; the meshed raster path uses u32 terrain_bits.
                    crate::terrain_edit::terrain_nearest_mask(&self.terrain_textures, &self.texture_settings, &self.project_root)
                        as f32,
                ],
                terrain_emissive: [tm.emissive[0], tm.emissive[1], tm.emissive[2], tm.emissive_strength],
                terrain_specular: [tm.specular[0], tm.specular[1], tm.specular[2], tm.specular_strength],
                terrain_params: [tm.shininess, tm.rim_strength, if tm.unlit { 1.0 } else { 0.0 }, tm.ambient],
                terrain_rim: [tm.rim[0], tm.rim[1], tm.rim[2], 0.0],
                blobs: arr,
                point_count: pl_count,
                point_pos: pl_pos,
                point_color: pl_col,
                point_shape: pl_shape,
                point_rot: pl_rot,
                point_cone: pl_cone,
                blob_tint,
                blob_emissive,
                blob_specular,
                blob_params,
                blob_rim,
                sky_params,
                sky_tint,
                sky_rot,
                ao_params: rm_ao_params,
                shadow_params: sh_params,
                shadow_tint: sh_tint,
                shadow_extra: sh_extra,
                prox_count,
                prox_a,
                prox_b,
                prox_rot,
                fog_color,
                fog_params,
                fog_extra,
                terrain_scale: crate::terrain_edit::terrain_scale_lanes(&self.terrain_tex_scale),
                fog_body: crate::shading::fog_body_lane(&light_node, &self.world, cam.world_position),
                vol_fog_a,
                vol_fog_b,
                vol_fog_c,
                contact,
                ssr,
                ssr_prev_vp,
                probe_meta,
                probe_pos,
                probe_half,
                atmo_meta,
                atmo_color,
                atmo_body,
                atmo_params,
                star_meta,
                star_pos,
                star_color,
                // vol_tight_* are renderer-patched at draw time (default: unbounded).
                ..Default::default()
            };
            // Sky shader in the offscreen / Game view too.
            if sky_active {
                g.sky_meta = [1.0, 0.0, 0.0, 0.0];
                g.sky_uniforms = sky_uniform_vals;
            }
            crate::shading::stamp_air_style(&mut g, &light_node, &self.world, cam.world_position);
            Self::fill_terrain_volumes(&self.terrains, &self.terrain_slots, &self.mesh_occluders, &self.occluder_regions, self.project.shadow_volumes, &self.world, &mut g, cam.world_position);
            crate::shaders::apply_field_shapes(&self.world, &self.flsl_shape_slots, &self.sdf_cache, &mut g, cam.world_position, None);
            if let Some(rmarch) = self.raymarch.as_ref() {
                rmarch.gi().apply(&mut g, cam.world_position.into());
            }
            g
        };
        crate::shading::note_shadow_casters(&mut self.script_host.profile().borrow_mut(), prox_count, &rm);

        // Live particles render in offscreen views too (the split Game viewport
        // must show what the game shows).
        // The Particles tab previews effects live; a build has no tab.
        #[cfg(feature = "editor-ui")]
        let vfx_preview_on = !self.playing
            && self
                .dock_state
                .as_ref()
                .is_some_and(|d| crate::dock::tab_is_front(d, EditorTab::Particles));
        #[cfg(not(feature = "editor-ui"))]
        let vfx_preview_on = false;
        let mut vfx_instances: Vec<floptle_render::ParticleInstance> = Vec::new();
        let mut vfx_batches: Vec<floptle_render::ParticleBatch> = Vec::new();
        // A stacked camera leaves effects to the camera beneath, which has
        // already drawn every one of them.
        if !opts.overlay {
            let nudge = crate::sprite2d::draw_offsets(&self.world, &self.project, cam.world_position);
            // The lamps lit particles answer to: the same lights as the scene, and
            // in a flat scene the 2D ones reaching each emitter's sorting layer.
            let rank_of = |e: Entity| {
                flat_camera.then(|| {
                    self.world
                        .get::<floptle_core::Sorting>(e)
                        .map(|sg| self.project.sorting_rank(&sg.layer))
                        .unwrap_or(0)
                })
            };
            let lamps = crate::vfx::ParticleLamps {
                lights: Some((
                    crate::vfx::LampSlots { count: off_split.three_d.count, pos: &off_split.three_d.pos, color: &off_split.three_d.color, mask: &off_split.three_d.mask, falloff: &off_split.three_d.falloff },
                    crate::vfx::LampSlots { count: off_split.two_d.count, pos: &off_split.two_d.pos, color: &off_split.two_d.color, mask: &off_split.two_d.mask, falloff: &off_split.two_d.falloff },
                )),
                rank_of: &rank_of,
                default_rank: flat_camera.then(|| self.project.sorting_rank(floptle_core::DEFAULT_SORTING_LAYER)),
            };
            self.vfx.collect(
                &self.world,
                cam,
                &self.texture_registry,
                vfx_preview_on,
                &nudge,
                &lamps,
                &mut vfx_instances,
                &mut vfx_batches,
            );
            let vfx_mesh_draws = self.vfx.collect_mesh_draws(&self.world, cam, vfx_preview_on);
            resolve_mesh_particles(&self.mesh_registry, &vfx_mesh_draws, &mut instances);
        }

        if let (
            Some(gpu),
            Some(raster),
            Some(raymarch),
            Some(particles),
            Some(line_layer),
            Some(tri_layer),
        ) = (
            self.gpu.as_ref(),
            self.raster.as_mut(),
            self.raymarch.as_mut(),
            self.particles.as_mut(),
            self.line_layer.as_mut(),
            self.tri_layer.as_mut(),
        ) {
            // Everything below is the draw. If the tuple above does not match,
            // the `else` at the end of it says so — see there.
            // ⏱ `floptle shot --timing`: one mark before each pass, so the
            // headless render answers "which pass is slow" the way the window
            // does. A label names the region that FOLLOWS it (see `GpuTimer`).
            macro_rules! headless_mark {
                ($label:expr) => {
                    if self.gpu_timing_headless {
                        if let Some(t) = self.gpu_timer.as_mut() {
                            t.mark(gpu, $label);
                        }
                    }
                };
            }
            // The opaque depth prepass, here as well as on the window path.
            // Contact shadows, `surfaceGap`, screen-space reflections and lamp
            // shadows all read it, and without it every one of them silently
            // does nothing — which is exactly how a docked Game panel came to
            // look different from the same game fullscreen.
            //
            // It runs when something actually reads it, and needs the depth
            // texture (a view cannot be copied into), so a caller that has none
            // opts out by construction rather than by forgetting.
            // The sky into the environment map, as the window path does every
            // frame, so a metal or glossy surface has something to reflect.
            // Without it a view with no window behind it — `floptle shot`, a
            // build's first frame — drew every metal black, and a polished
            // metal model looked like a hole in the picture.
            raymarch.upload_globals(gpu, rm);
            // A camera with no sky reflects the one the frame's other views
            // captured: the map is shared, and the sky is the same sky.
            if opts.draws.sky {
                raymarch.capture_env(gpu);
            }
            // The sun's shadow map first: the volumetric fog marched with the
            // prepass reads it for its shafts.
            headless_mark!("sun shadow map");
            let map_draws = crate::shading::with_shadow_casters(&instances, &shadow_casters);
            raster.sun_shadow_pass(gpu, globals, &map_draws, &flsl_draws, &skin_draws);
            let wants_depth = wants_prepass(
                raster.flsl_draws_want_depth(&flsl_draws),
                ssr[0] > 0.5,
                point_shadows,
                contact[0] > 0.5,
                rm.volumetric_fog(),
            );
            if let Some(dtex) = opts.depth_tex.filter(|_| wants_depth || rm_draw) {
                let hist = history_bind.as_ref().map(|(v, s)| (v, s));
                headless_mark!("depth prepass");
                prepass_and_bind(
                    gpu, raster, raymarch, globals, &instances, &flsl_draws, &skin_draws,
                    dtex, hist, rm,
                );
            } else {
                // No prepass this view: unbind, or this render would march the
                // last view's depth buffer — a different camera at a different
                // size, which is worse than marching nothing.
                raymarch.bind_frame_targets(gpu, None, None);
            }
            let raster_clear = if opts.overlay {
                // Transparent, so the composite can tell what this layer drew.
                raymarch.upload_globals(gpu, rm);
                Some([0.0; 4])
            } else if rm_draw {
                raymarch.draw_into(gpu, color, depth, rm);
                None
            } else {
                // Nothing to raymarch, but the raster field group still needs this
                // frame's shadow/proxy data (mesh-only scenes cast via proxies).
                raymarch.upload_globals(gpu, rm);
                Some(clear.map(|c| c as f64))
            };
            headless_mark!("opaque + lighting");
            raster.defer_scene_readers(glass);
            raster.draw_scene_with(
                gpu, color, depth, globals, &instances, &flsl_draws, &skin_draws,
                raster_clear, Some(raymarch.field_bind()),
            );
            // The palette quantize, before the light — the same order the surface
            // path uses, and it has to be the same or a docked Game view would
            // posterize its lighting while the Scene view did not; the two
            // gathers have drifted over exactly this shape before.
            // Not over a layer: it would give the transparent clear a colour,
            // and the composite would lay that over the whole frame.
            if let Some(q) = palette.filter(|_| !opts.overlay) {
                raster.quantize_palette(gpu, color, (size.0.max(1), size.1.max(1)), q);
            }
            headless_mark!("2D lighting");
            raster.light2d_pass(
                gpu,
                color,
                depth,
                (size.0.max(1), size.1.max(1)),
                view_proj.to_cols_array_2d(),
                &lights_2d,
                &flat2d,
            );
            // Glass, on the same terms and in the same place as the window
            // path: capture what is behind, then draw the things you can see
            // through. `self.game_scene_history` is a different field from the
            // ones this block borrows, so it is reachable from inside it.
            if glass
                && let Some(h) = match opts.history {
                    HistorySlot::None => None,
                    HistorySlot::GamePanel => self.game_scene_history.as_mut(),
                }
            {
                let mut glass_rm = rm;
                glass_rm.ssr_prev_vp = view_proj.to_cols_array_2d();
                raymarch.upload_globals(gpu, glass_rm);
                let cuts = raster.transmissive_cuts(
                    &instances,
                    &skin_draws,
                    light_node.refraction_layers,
                );
                for layer in 0..=cuts.len() {
                    h.capture(gpu, color, view_proj, cam.world_position);
                    if layer == 0 {
                        raymarch.bind_frame_targets(
                            gpu,
                            raster.prepass_view(),
                            Some((h.view(), h.sampler())),
                        );
                    }
                    headless_mark!("glass");
                    raster.draw_transmissive(
                        gpu, color, depth, globals, &instances, &skin_draws,
                        Some(raymarch.field_bind()), &cuts, layer,
                    );
                }
                if raster.flsl_draws_read_scene(&flsl_draws) {
                    h.capture(gpu, color, view_proj, cam.world_position);
                    raster.draw_flsl_scene_readers(
                        gpu, color, depth, globals, &flsl_draws, &skin_draws,
                        Some(raymarch.field_bind()),
                    );
                }
            }
            // Script-drawn 3D lines (draw.line — the map's orbit conics).
            let shapes = !opts.overlay;
            if shapes && !self.script_lines.is_empty() && !self.lines_deferred && !self.hide_script_shapes {
                let (through, tested) = script_line_batches(&self.script_lines, cam.world_position);
                headless_mark!("lines");
                line_layer.draw(gpu, color, depth, view_proj, &through, false);
                line_layer.draw(gpu, color, depth, view_proj, &tested, true);
            }
            // Script-drawn FILLED triangles (draw.tri/cone/disc — solid gizmos).
            if shapes && !self.script_tris.is_empty() && !self.hide_script_shapes {
                let verts: Vec<floptle_render::TriVertex> = self
                    .script_tris
                    .iter()
                    .flat_map(|t| {
                        let a = (DVec3::from(t.a) - cam.world_position).as_vec3();
                        let b = (DVec3::from(t.b) - cam.world_position).as_vec3();
                        let c = (DVec3::from(t.c) - cam.world_position).as_vec3();
                        [
                            floptle_render::TriVertex { pos: [a.x, a.y, a.z], color: t.color },
                            floptle_render::TriVertex { pos: [b.x, b.y, b.z], color: t.color },
                            floptle_render::TriVertex { pos: [c.x, c.y, c.z], color: t.color },
                        ]
                    })
                    .collect();
                tri_layer.draw(gpu, color, depth, view_proj, &verts);
            }
            // Script-drawn textured quads (draw.quad): in the world, depth-tested.
            if shapes && !self.script_quads.is_empty() {
                headless_mark!("script-quads");
                let (verts, batches) =
                    crate::render_frame::pack_script_quads(&self.script_quads, cam.world_position);
                tri_layer.draw_textured(gpu, color, depth, view_proj, &verts, &batches, raster);
            }
            if !vfx_batches.is_empty() {
                headless_mark!("particles");
                particles.draw(
                    gpu,
                    color,
                    depth,
                    crate::vfx::particle_globals(cam, aspect, fog_color, particle_fog, particle_light),
                    &vfx_instances,
                    &vfx_batches,
                    raster,
                    None,
                );
            }
        } else {
            // **A view that cannot draw says so.** This binds six pieces of the
            // device at once, and until now a missing one meant the whole render
            // silently did nothing — the caller got a valid, entirely black
            // frame and no reason for it. That is how `floptle shot` shipped its
            // first picture: 960x540 of black, exit 0.
            //
            // Once, not per view per frame: a device that is missing a piece is
            // missing it for good, and sixty lines a second would bury it.
            if !self.warned_incomplete_device {
                self.warned_incomplete_device = true;
                let missing = [
                    ("gpu", self.gpu.is_none()),
                    ("raster", self.raster.is_none()),
                    ("raymarch", self.raymarch.is_none()),
                    ("particles", self.particles.is_none()),
                    ("lines", self.line_layer.is_none()),
                    ("tris", self.tri_layer.is_none()),
                ]
                .iter()
                .filter(|(_, missing)| *missing)
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ");
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!(
                        "nothing can be drawn: this renderer was never given {missing}. Everything a scene render needs is set up in `Editor::init_gpu_side`."
                    ),
                    None,
                );
            }
        }
        // Keep this view's picture, for its own next frame's reflections. Same
        // place in the order as the window path: everything belonging to the
        // scene has drawn and nothing that does not has started.
        if let (Some(gpu), HistorySlot::GamePanel) = (self.gpu.as_ref(), opts.history)
            && let Some(h) = self.game_scene_history.as_mut()
        {
            h.capture(gpu, color, view_proj, cam.world_position);
        }
        if timing {
            let mut p = self.script_host.profile().borrow_mut();
            p.record_part("renderPasses", passes_t.ms());
            let inside = p.frame_measured() - measured0;
            p.record(Bucket::Render, (whole_t.ms() - inside).max(0.0));
        }
    }
}

/// An editor on a headless GPU, or `None` when this machine's adapter cannot
/// build the renderer (CI's is OpenGL, which lacks what the raster textures
/// need). Said on stderr, the way every other GPU test skips: that is a fact
/// about the machine, not a failure of the thing under test.
///
/// Only an OpenGL adapter skips. On any other backend a refused pipeline is
/// the renderer's own bug, and a shader that does not parse was once read as
/// "this machine has no GPU": every test built on this passed by skipping
/// while the frame it was guarding could not be drawn at all.
#[cfg(test)]
pub(crate) fn test_editor_with_gpu() -> Option<crate::Editor> {
    let gpu = floptle_render::Gpu::headless(64, 64);
    let backend = gpu.adapter.get_info().backend;
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
    if let Ok(why) = failed.lock()
        && !why.is_empty()
    {
        assert!(
            backend == wgpu::Backend::Gl,
            "the renderer failed its own validation on {backend:?}, which can build it — a bug, \
             not this machine:\n{why}"
        );
        floptle_say::say_err!("skipped — this machine cannot build the renderer:\n{why}");
        return None;
    }
    Some(ed)
}

/// Script lines as camera-relative vertex pairs, split into those that draw
/// through the scene and those `draw.depthTest` hides behind it.
pub(crate) fn script_line_batches(
    lines: &[floptle_script::DrawLine],
    cam: DVec3,
) -> (Vec<floptle_render::LineVertex>, Vec<floptle_render::LineVertex>) {
    let (tested, through): (Vec<_>, Vec<_>) = lines.iter().cloned().partition(|l| l.depth);
    (script_line_verts(&through, cam), script_line_verts(&tested, cam))
}

/// Script lines as camera-relative vertex pairs.
pub(crate) fn script_line_verts(lines: &[floptle_script::DrawLine], cam: DVec3) -> Vec<floptle_render::LineVertex> {
    lines
        .iter()
        .flat_map(|l| {
            let a = (DVec3::from(l.a) - cam).as_vec3();
            let b = (DVec3::from(l.b) - cam).as_vec3();
            [
                floptle_render::LineVertex { pos: [a.x, a.y, a.z], color: l.color },
                floptle_render::LineVertex { pos: [b.x, b.y, b.z], color: l.color },
            ]
        })
        .collect()
}

impl Editor {
    /// Draw this frame's script lines over a finished, full-resolution picture
    /// (after post and the upscale): `draw.nativeLines`. `scene_depth` is the
    /// depth the scene was drawn with, at its own resolution, which the lines
    /// `draw.depthTest` asked for hide behind.
    pub(crate) fn draw_lines_over(
        &mut self,
        target: &wgpu::TextureView,
        size: [u32; 2],
        view_proj: floptle_core::math::Mat4,
        cam: DVec3,
        scene_depth: &wgpu::TextureView,
    ) {
        if self.script_lines.is_empty() {
            return;
        }
        let (through, tested) = script_line_batches(&self.script_lines, cam);
        let px = [size[0] as f32, size[1] as f32];
        if let (Some(gpu), Some(lines)) = (self.gpu.as_ref(), self.line_layer.as_mut()) {
            lines.draw_overlay(gpu, target, gpu.surface_format(), px, view_proj, &through, None);
            lines.draw_overlay(gpu, target, gpu.surface_format(), px, view_proj, &tested, Some(scene_depth));
        }
    }
}

#[cfg(test)]
mod tests {
    /// **A shipped game's pose table holds one frame, however long it runs.**
    ///
    /// One skinned draw a frame, the way a first-person arm rig makes one, for
    /// ten thousand frames. The build's frame never reaches the editor's
    /// gather, and when it also skipped the reset the table grew by every
    /// draw of every frame: re-uploaded whole each frame until the game
    /// stuttered, then past the storage-binding limit, where the level stopped
    /// drawing and the process exited.
    #[test]
    fn a_builds_pose_table_stays_flat_across_ten_thousand_frames() {
        let Some(mut ed) = super::test_editor_with_gpu() else { return };
        for frame in 0..10_000 {
            // What a skinned mesh's draw does inside `render_game_into`.
            ed.raster.as_mut().unwrap().push_skin_pose(0, floptle_core::math::Mat4::IDENTITY, &[floptle_core::math::Mat4::IDENTITY; 4]);
            let _ = ed.player_frame(false);
            let poses = ed.raster.as_ref().unwrap().skin_pose_count();
            assert_eq!(poses, 0, "frame {frame}: the build's frame must start a fresh pose table");
        }
    }

    /// **A build's frame counts a script's time once.** The whole game step
    /// was timed as `scripts` on top of the script passes inside it, which time
    /// themselves: a script's own work was counted twice, and physics and
    /// terrain inside the step a second time under `scripts`. A frame's buckets
    /// then added up to more than the frame, and a dig read as a hitch twice
    /// its size. A script that burns 8 ms in `update` costs `scripts` about 8 ms.
    #[test]
    fn a_builds_frame_counts_a_scripts_time_once() {
        use floptle_core::{Matter, Name, ScriptInst, Scripts, Transform};
        let dir = std::env::temp_dir().join(format!("floptle-once-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(
            dir.join("scripts/burn.lua"),
            "function update(node, dt)\n  local t = os.clock()\n  while os.clock() - t < 0.008 do end\nend\n",
        )
        .unwrap();
        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        let e = ed.world.spawn();
        ed.world.insert(e, Transform::IDENTITY);
        ed.world.insert(e, Name("Burn".into()));
        ed.world.insert(e, Matter::Empty);
        ed.world.insert(
            e,
            Scripts(vec![ScriptInst { kind: "burn".into(), enabled: true, params: Vec::new(), refs: Vec::new(), strs: Vec::new(), pinned_bools: None }]),
        );
        ed.toggle_play();
        assert!(ed.playing, "did not enter play");
        ed.script_host.profile().borrow_mut().enable(true);
        for _ in 0..40 {
            let _ = ed.player_frame(false);
            ed.end_profile_frame();
        }
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        let ms = ed.script_host.profile().borrow().bucket(floptle_core::profile::Bucket::Scripts).map_or(0.0, |c| c.ms);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ms > 7.0, "the script's 8 ms went missing: scripts {ms:.2} ms");
        assert!(ms < 12.0, "the script's 8 ms was counted more than once: scripts {ms:.2} ms");
    }

    /// **What the GPU refused reaches a build's console.**
    ///
    /// The editor's frame moved GPU errors into its Console; a build's frame
    /// did not, and their only other way out was stderr. A browser has no
    /// stderr, so on a phone a refused shader or pipeline was invisible: not
    /// in the page's log, not in its crash record. A build's console sends
    /// errors to stderr on the desktop and to the page in a browser, so
    /// reaching the console is reaching the player's report.
    #[test]
    fn a_builds_frame_reports_what_the_gpu_refused() {
        let Some(mut ed) = super::test_editor_with_gpu() else { return };
        let label = "a-builds-frame-reports-this-buffer";
        {
            let gpu = ed.gpu.as_ref().unwrap();
            // The handler a game's device has (`Gpu::new`); the test device
            // was given a sink of its own.
            gpu.device.on_uncaptured_error(std::sync::Arc::new(|e: wgpu::Error| floptle_render::report_gpu_error(&e)));
            // A write into a buffer that was not made to be written to.
            let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: 16,
                usage: wgpu::BufferUsages::VERTEX,
                mapped_at_creation: false,
            });
            gpu.queue.write_buffer(&buf, 0, &[0u8; 16]);
            let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
        }
        let _ = ed.player_frame(false);
        let reported = ed.console.entries.iter().any(|e| {
            e.level == floptle_script::LogLevel::Error && e.msg.starts_with("GPU: ") && e.msg.contains(label)
        });
        let said: Vec<&str> = ed.console.entries.iter().map(|e| e.msg.as_str()).collect();
        assert!(reported, "the build's frame never put the GPU's error in its console: {said:?}");
    }
}

#[cfg(test)]
mod camera_draws_tests {
    use super::*;

    /// **A camera's switches reach the light it renders with**, and leave the
    /// rest of it alone; all on, the light is the scene's exactly.
    #[test]
    fn a_camera_s_switches_turn_off_its_shadows_and_fog() {
        let scene = Light { shadows: true, shadow_map: true, contact_shadows: true, fog: true, fog_volumetric: true, ..Default::default() };
        assert_eq!(light_seen_by(scene, Default::default()), scene, "all on changed the light");
        let dark = light_seen_by(scene, floptle_core::CameraDraws { shadows: false, ..Default::default() });
        assert!(!dark.shadows && !dark.shadow_map && !dark.contact_shadows, "shadows stayed on");
        assert!(dark.fog && dark.fog_volumetric, "turning shadows off touched the fog");
        let clear = light_seen_by(scene, floptle_core::CameraDraws { fog: false, ..Default::default() });
        assert!(!clear.fog && !clear.fog_volumetric, "fog stayed on");
        assert!(clear.shadows && clear.shadow_map, "turning fog off touched the shadows");
    }
}
