//! The editor's per-frame render: `Editor::render` is the frame loop's single entry
//! point — it steps the sim (`play_step`), gathers the World into renderer uniforms
//! (`mesh_instances`, `draw_2d`), builds the egui UI, and draws. Offscreen views go
//! through `offscreen`; GPU-side scene resources are kept in step by `scene_sync`.

use floptle_core::math::DVec3;
use crate::{Editor, anim};
use crate::offscreen::read_back_frame;
#[cfg(feature = "editor-ui")]
use crate::frame_ui::{FrameAfter, FrameUi};
#[cfg(feature = "editor-ui")]
use crate::gather::FrameGather;

/// A world-space UI canvas gathered for the frame: its draw list and placed
/// elements, world position, right and up axes, and size.
#[cfg(feature = "editor-ui")]
type WorldCanvas = (floptle_ui::DrawList, Vec<floptle_ui::Placed>, [f64; 3], [f32; 3], [f32; 3], [f32; 2]);
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use crate::mesh_instances::{wants_prepass, prepass_and_bind};
#[cfg(feature = "editor-ui")]
use crate::anim_ui;

/// The decision behind the 16-light-cap warning:
/// given how many lights just got cut and what the last warning was about,
/// what should the latch become and what (if anything) should the Console say.
///
/// A plain function with no `self` on purpose — both gathers need this, one of
/// them can't call a `&mut self` method at its call site (see
/// `Editor::warn_lights_dropped`), and keeping the actual decision in one place
/// is what stops the two copies from drifting.
///
/// Latched on the exact count so it says so again if a scene goes from 24
/// dropped to 30, and resets once the scene drops back under the cap so going
/// over it a second time re-warns rather than staying silent forever.
pub(crate) fn light_cap_warning(dropped: usize, last_warned: usize) -> (usize, Option<String>) {
    if dropped == 0 {
        return (0, None);
    }
    if dropped == last_warned {
        return (last_warned, None);
    }
    let msg = format!(
        "💡 {dropped} point light(s) are past the 16-light cap this frame and are not shading \
         anything — the sixteen contributing most at the camera win, the rest cost placement \
         time for nothing (docs/lua-api.md, node:setPointLight)."
    );
    (dropped, Some(msg))
}

impl Editor {

    /// Say once when the scene's lights have gone past the sixteen-slot cap,
    /// naming how many were cut. Called from
    /// both gathers — the Scene view and `render_world_into` — because either
    /// can be the first (or only) one to run in a given session.
    ///
    /// The main gather can't call this directly (a live `self.gpu.as_mut()`
    /// borrow through most of `render()` conflicts with a `&mut self` method
    /// call, even though the two touch disjoint fields), so the decision is
    /// `light_cap_warning` — a plain function, no `self`, callable from
    /// anywhere — and this method is the thin wrapper `render_world_into` uses.
    pub(crate) fn warn_lights_dropped(&mut self, dropped: usize) {
        if self.lights_dropped_checked_frame == self.frame_no {
            return;
        }
        self.lights_dropped_checked_frame = self.frame_no;
        let (warned, msg) = light_cap_warning(dropped, self.lights_dropped_warned);
        self.lights_dropped_warned = warned;
        if let Some(msg) = msg {
            self.console.push(floptle_script::LogLevel::Warn, msg, None);
        }
    }

    /// The project's frame pacing, applied before anything acquires a surface
    /// image. `set_vsync` early-outs when nothing changed, so this is free on
    /// every frame but the one where somebody changes the setting. A shipped
    /// game's frame calls it too; without it a build was Fifo whatever
    /// project.ron or `app.setVsync` said.
    pub(crate) fn apply_project_vsync(&mut self) {
        let want_vsync = match self.project.vsync {
            floptle_scene::VsyncDoc::On => floptle_render::Vsync::On,
            floptle_scene::VsyncDoc::Adaptive => floptle_render::Vsync::Adaptive,
            floptle_scene::VsyncDoc::Off => floptle_render::Vsync::Off,
        };
        let applied = self.gpu.as_mut().and_then(|gpu| gpu.set_vsync(want_vsync));
        if let Some(mode) = applied {
            self.console.push(
                floptle_script::LogLevel::Debug,
                format!("frame pacing: {want_vsync:?} → {mode:?}"),
                None,
            );
        }
    }

    #[cfg(feature = "editor-ui")]
    pub(crate) fn render(&mut self) {
        // A held selection the world has emptied — the node deleted, the scene
        // switched — releases here, before anything draws. The lock's only
        // switch is on the Inspector's name row, so a lock over nothing would
        // have no way out.
        self.enforce_selection_lock();
        // Terrain brush telegraph + throttled stroke (before the destructure, so it
        // can freely borrow `self`).
        self.terrain_frame_update();
        self.vertex_paint_frame_update();
        // ◫ Tiles: keep painting while the button is held. Here rather than in the
        // winit handler because a stroke has to follow the pointer every frame,
        // not only on the events that happen to arrive.
        if self.tool == crate::gizmo::Tool::Tiles && !self.playing {
            self.tile_frame_update(self.cursor);
        }
        // Map meshes: heal duplicated ids and re-upload edited geometry so the
        // gather below always finds a current `@map/<id>` registry entry; then
        // the Map tool's hover/selection overlay.
        self.sync_map_meshes();
        // …and re-attach any paint to the surfaces that survived the edit,
        // before anything draws with a stale block.
        self.sync_map_paint();
        self.map_edit_frame_update();
        self.tile_frame_viz();
        // 2D: rebuild any tilemap whose grid or sheet changed.
        self.sync_tilemaps();
        // The 🎓 Learn tab answers its checks from a snapshot of the scene and
        // the project's files. Taken up here with the other whole-`self` passes,
        // before the GPU destructure below splits `self` apart — and only while
        // the tab is actually visible, because it reads every script in the
        // project and a panel nobody has open is not worth a file walk.
        self.refresh_learn();
        // A finished "test voice from a WAV" pick — up here with the other
        // whole-`self` passes, before the GPU destructure splits `self` apart.
        self.poll_voice_test_pick();
        // The project's packages get their frame here — before the GPU state is
        // borrowed for the rest of `render`, because an extension's hooks need
        // the whole editor and the draw path holds pieces of it. What they
        // draw is projected further down, where `view_proj` exists.
        self.ext_clock += self.ui_frame_dt as f64;
        self.ext_tick();
        // Built here rather than inside the UI pass, where only disjoint field
        // borrows exist. Constructing it reads the keyring and restores whatever
        // session the Hub already stored, off-thread — so by the time the
        // Packages window draws, it usually already knows who you are.
        if self.account.is_none() {
            self.account = Some(floptle_account::Account::new(floptle_account::DEFAULT_BASE));
        }

        // Inspector asset preview: render the spinning model/material (or load the
        // texture) before the GPU/egui destructure borrows below. `preview_dt` is a
        // cheap peek at the frame delta — only the turntable angle uses it.
        let preview_dt = self.last.map(|l| l.elapsed().as_secs_f32()).unwrap_or(0.0).min(0.1);
        self.update_asset_preview(preview_dt);
        let preview_view = self.preview_view();

        // Live Lua syntax check for the active IDE file (drives red squiggles).
        self.check_active_script_syntax();
        // Crash safety: periodically snapshot a dirty scene to `.floptle/autosave`
        // (deleted on a real save; offered for recovery at the next open).
        self.autosave_tick();
        // Reap a finished cross-target export build (Windows-from-Linux etc.).
        self.poll_export_build();
        // Terrain volumes render per-volume, each at native resolution: moving a
        // terrain needs no GPU work — only structural changes re-upload into the
        // shared 3D atlas (where shadow-only mesh occluders also live).
        //
        // Capture the terrain dirty state before `sync_terrain_gpu` consumes it: the atlas
        // upload feeds shadows/AO from each terrain's shadow proxy, then
        // `sync_terrain_meshes` re-extracts the primary-ray chunk meshes straight from the
        // authority field (Terrain 2.0 / P3). Structural change = full re-mesh; a sculpt
        // dab re-meshes only the chunks it touched (`terrain_chunks_dirty`).
        let terrain_full_rebuild = self.terrain_gpu_dirty;
        self.sync_terrain_gpu();
        // LOD rings center on what the player actually sees: the active game camera
        // during Play, the editor fly-camera otherwise.
        let lod_cam = if self.playing {
            floptle_core::active_camera(&self.world)
                .map(|e| floptle_core::world_transform(&self.world, e).translation)
                .unwrap_or(self.camera.position)
        } else {
            self.camera.position
        };
        // G1 residency: stream celestial terrain fields in/out by camera distance
        // (before the mesh sync so a landed field streams meshes this same frame;
        // outside the render borrows because a mid-Play arrival rebuilds the sim).
        // Hand queued `terrain.generatePlanet` fills to the generator before
        // residency runs: the fill marks its body generation-owned
        // (`planet_gen_pending`), and residency must see that mark the same
        // frame — or it adopts the freshly created body as cold and streams a
        // Stale same-id file into it (the authored scene's old planet loaded
        // under a rolled galaxy's spawn world — the player fell straight through it).
        self.drain_terrain_generates();
        self.update_terrain_residency(lod_cam);
        self.publish_terrain_busy();
        // Background checkpoints (terrain.flush): a few chunks of encoding per
        // frame + threaded writes — autosaves must never stutter the game.
        self.step_terrain_checkpoint();
        {
            // Terrain: residency, field generation and meshing. "I can see
            // through unloaded terrain" is the meshing queue, and this number
            // shows it.
            let _t = floptle_core::profile::Span::new();
            self.sync_terrain_meshes(terrain_full_rebuild, lod_cam);
            self.profile_record(floptle_core::profile::Bucket::Terrain, _t.ms());
        }
        self.sync_sky_texture();
        self.sync_sky_shader();
        // Texture-painted nodes keep their vertex paint via atlas-ordered mirror blocks;
        // rebuild them when vertex paint changed this frame (no-op otherwise). after
        // `vertex_paint_frame_update` above, so a dab shows the same frame it lands.
        self.sync_tex_paint_mirrors();
        // Keep the Inspector's script param list in sync with each script's `defaults`
        // (cheap: cached by file mtime, selected node only) so editing a script surfaces
        // new tunables and drops removed ones live.
        self.sync_selected_script_params();
        // Whether the Game viewport is focused (precomputed before the GPU borrow): game
        // input only feeds scripts here. `game_view()` is pointer-aware in split view, so
        // when both tabs show, input goes to whichever viewport the mouse is over and the
        // Scene view stays fully interactive.
        let game_focused = self.game_view() || self.game_trap;

        // Poll the gamepads and refresh the action layer's device levels. Must
        // run before anything resolves, and before the early-out below so a pad
        // plugged in during startup is already slotted.
        self.pump_input_devices();
        self.poll_input_map_reload();
        // The half of the live loop that ISN'T the editor's own push: a texture
        // rewritten by anything (Aseprite, a build script, a git checkout)
        // re-uploads here. See image_io.rs.
        self.poll_texture_hot_reload();

        // Nothing to drive until the window + GPU stack exist. (The borrows
        // themselves are taken per stage, and by the gather/draw core below.)
        if self.gpu.is_none()
            || self.raster.is_none()
            || self.raymarch.is_none()
            || self.retro.is_none()
            || self.outline.is_none()
            || self.grid_render.is_none()
            || self.post.is_none()
            || self.egui.is_none()
            || self.window.is_none()
        {
            return;
        }

        // Which dock tab holds focus (from last frame's dock state — the raw winit
        // key handler runs between frames, so a one-frame-old read is exact). Lets
        // that handler route Delete/arrows/F to a focused timeline panel instead of
        // the scene. Fullscreen forces its own tab.
        self.focused_tab = self.fullscreen_tab.or(self
            .dock_state
            .as_mut()
            .and_then(|d| d.find_active_focused().map(|(_, t)| *t)));
        // Cleared here, set again by the dopesheet if it draws this frame. A
        // panel that is no longer on screen must not keep claiming Ctrl+C — the
        // flag has to expire on its own rather than wait to be corrected.
        self.anim_ui.sheet_hovered = false;

        let (dt, elapsed) = self.advance_clock(game_focused);
        // 🖼 Image tab: frame playback, toasts, external-change reload, and the
        // Live re-export that keeps the mesh in step with the brush.
        let image_visible = std::mem::take(&mut self.image.tab_visible);
        self.image.tick(dt);
        if image_visible {
            self.poll_image_doc_reload();
            self.step_image_live();
        }
        // History frame boundary: capture this frame's pre-edit scene+selection
        // (what `begin_edit` coalesces a gizmo/inspector drag against), and turn
        // any selection change since the last boundary into its own undo step.
        // Skipped while playing — script-driven transforms must not enter the
        // undo history — and while recording (the world carries previewed clip
        // values then; edits go to the clip as keys, not to scene undo).
        self.begin_history_frame();

        self.play_step(dt, game_focused);
        self.finish_input_frame();
        // Register every texture + import every mesh the particle system needs
        // before the gather that resolves them (full &mut self here — no borrow
        // race, no frame lag on the open effect).
        self.frame_no = self.frame_no.wrapping_add(1);
        self.ensure_vfx_assets();
        // Every texture this scene's materials name, before any gather looks one
        // up — see `ensure_scene_textures` for what an unregistered one looks
        // like (it looks like the material was never applied).
        self.ensure_scene_textures();
        // Compile/hot-reload `.flsl` shader materials + refresh their group(3)
        // bindings — the gathers below (main, Game viewport, camera preview)
        // all read `flsl_binds`, so this must run before any of them. Field
        // Shapes follow: their sdf shaders splice into both passes on change.
        self.ensure_flsl_materials();
        self.ensure_ui_shaders();
        self.ensure_post_shaders();
        // Anything the GPU rejected since the last frame. It does not take
        // the process down (see `Gpu::new`), so this is the only place it
        // becomes visible — and it has to, or a pass that silently stops
        // drawing looks like the feature never worked.
        for e in floptle_render::take_gpu_errors() {
            self.console.push(floptle_script::LogLevel::Error, format!("GPU: {e}"), None);
        }
        // Baked GI: push any pending probe upload, then advance a bake by one
        // frame's slice. Both run before the gathers below, so this frame's
        // draws see this frame's light — and a bake, which renders the scene
        // itself, cannot be re-entered from inside one of them.
        self.refresh_gi();
        self.step_gi_bake();
        self.drive_auto_bake();
        self.drive_auto_open();
        // The navmesh's own two: take a finished background bake, then decide
        // whether the level has changed enough to want another. In that order,
        // so a bake that has just landed is the one the watcher compares
        // against rather than the one before it.
        self.poll_nav_bake();
        self.tick_nav_autobake(dt);
        // …and, on the same terms, one reflection probe's capture. Six renders
        // of the scene, so it belongs here beside the bake rather than inside a
        // gather, and at most one probe a frame.
        self.step_reflection_probes();
        self.apply_project_vsync();
        self.sync_field_shapes();

        // Edit-mode animation preview (Animating tab): pose the bound node at the
        // playhead. This must run before anything gathers draw data — the UI
        // overlay/hologram gathers and the docked Game viewport below all read the
        // ECS, so applying the preview after them meant scrubbing a property track
        // (e.g. a spritesheet `cell`) showed nothing in the editor. Scene-node
        // bindings apply transiently and are restored after the main draw list is
        // built (except while recording — see `restore_preview` below), so a
        // preview never dirties the authored scene.
        if !self.playing {
            if self.anim_ui.tab_visible {
                if let (Some(target), Some(state)) =
                    (self.anim_ui.target, self.anim_ui.sel_anim.clone())
                {
                    if self.anim_ui.preview_playing {
                        self.anim_ui.playhead += dt;
                    }
                    // Record first: capture the user's pose edits as keys before
                    // the preview re-applies the clip (which then includes them).
                    if self.anim_ui.record
                        && anim_ui::record_scan(&self.world, &mut self.anim_ui, target) {
                            self.anim_ui.clip_dirty = true;
                        }
                    // A held edit (bone gizmo/inspector drag) defers its disk save to
                    // pointer-up, so without this the preview keeps re-sampling the old
                    // clip and the bone looks frozen mid-drag. Refresh the in-memory clip
                    // + bump the revision so preview_pose rebinds to the live edit — the
                    // bone tracks the gizmo in real time. Disk save stays coalesced.
                    if self.anim_ui.clip_dirty
                        && let Some((k, d)) = self.anim_ui.clip_doc.clone() {
                            self.anim.register_clip(&k, &d);
                        }
                    anim::preview_pose(
                        &mut self.anim,
                        &mut self.world,
                        &self.mesh_registry,
                        target,
                        &state,
                        self.anim_ui.playhead,
                    );
                    if self.anim_ui.record {
                        // Re-baseline against what the preview applied, so next
                        // frame's diff sees only new user edits.
                        anim_ui::refresh_record_baseline(&self.world, &mut self.anim_ui, target);
                    }
                }
            } else {
                // Tab hidden: recording can't continue without its scan/preview
                // loop — stop it cleanly (restores the pre-record scene).
                if self.anim_ui.record {
                    anim_ui::stop_record_ui(&mut self.world, &mut self.anim_ui);
                    self.anim.forget_preview();
                }
                if !self.anim.poses.is_empty() || !self.anim.instances.is_empty() {
                    // Drop stale preview runtimes so models return to rest.
                    self.anim.poses.clear();
                    self.anim.instances.clear();
                }
            }
            self.anim_ui.tab_visible = false; // re-armed by the tab each frame it draws
        }

        // Game-UI layers: gather + solve on the CPU while `self` is free (the
        // draw core borrows the GPU stack); drawn over the finished frame below.
        // after the animation preview, so scrubbing shows live in every view.
        // Is the game drawn over the whole window this frame? Not "does the Game
        // tab have focus" — a docked tab has focus and draws into its own rect,
        // and asking the focus question here meant the overlay was also packed
        // and drawn full-window every frame, hidden under the editor's chrome.
        // In split view it was worse than wasteful: `game_view()` follows the
        // pointer there, so screen-space canvases blinked out of the Scene view
        // whenever the mouse crossed into the game.
        let ui_view = self.game_fullscreen();
        // Screen-space overlay layers (game view only). gather_game_ui skips
        // world-space layers — those live in the scene below.
        let ui_layers = if ui_view {
            let vp = self
                .gpu
                .as_ref()
                .map(|g| [g.config.width as f32, g.config.height as f32])
                .unwrap_or([0.0, 0.0]);
            self.gather_game_ui(vp)
        } else {
            Vec::new()
        };
        // World canvases: in the Scene (authoring) view, every layer renders as
        // a movable hologram at its node's transform; in game/player view, only
        // the layers whose `space` is World (screen-space ones are the overlay
        // above). Either way outlines project onto the canvas and drags come
        // back through cmd.ui_move (in design units).
        let aspect = self
            .gpu
            .as_ref()
            .map(|g| g.config.width as f32 / g.config.height.max(1) as f32)
            .unwrap_or(16.0 / 9.0);
        // …and only when the surface is actually being looked at: a docked Game
        // tab renders its own world canvases into its own target, so solving
        // every layer again for a surface hidden behind the dock is pure cost.
        let ui_world = if ui_view || self.scene_visible() {
            self.gather_ui_world(aspect, !ui_view)
        } else {
            Vec::new()
        };

        // Offscreen previews render last (after play_step advanced this frame's poses
        // and particles, and after ensure_vfx_assets registered their textures/meshes):
        // otherwise a docked/split Game view or the Inspector camera POV showed frozen
        // animation and missing effects — it was drawing a frame before the sim, with
        // VFX assets not yet resolved. Reuses `elapsed` so it costs no extra clock read.
        // Both take &mut self and must live outside the main GPU destructure below, so
        // this is the last safe point before it.
        // A1 target cameras render first, so every later pass (previews, game
        // viewport, the surface itself) samples this frame's feed.
        self.update_render_targets(elapsed);
        self.update_camera_preview(elapsed);
        self.update_game_viewport(elapsed);
        // The ◫ UI tab's canvas — the selected layer through the real UI
        // pipeline. Runs alongside the other offscreen views, and no-ops (and
        // frees nothing but time) when the tab isn't showing.
        self.sync_ui_design_guides();
        self.update_ui_design_view();
        // The ◈ Shaders tab's per-node preview atlas (only while it's visible).
        self.update_shader_graph_preview(elapsed);
        // `stage ui` shaders read `time` from the UI globals' spare lane.
        if let Some(uir) = self.ui_render.as_mut() {
            uir.set_time(elapsed);
        }

        // Terrain surface material, resolved before the GPU destructure borrows `self.raster`
        // out (`terrain_material` is `&self`): the meshed terrain draws with it in the raster
        // pass (Terrain 2.0 / P2). Cheap; only read when terrains exist.
        let terrain_base_mat = self.terrain_material();

        // This frame's sky-shader uniforms (Inspector knobs over `.flsl` defaults), also
        // resolved before the GPU destructure takes `&mut self` — both draw sites reuse it.
        let sky_active = self.sky_shader.is_some();
        let sky_uniform_vals = self.sky_uniform_values();

        // A docked (non-fullscreen) Game tab paints its own offscreen render this
        // frame, sized+blit to its rect (single-view or split) so it never spills
        // behind panels. Read here because the destructure below takes `&mut self`
        // — and read from the one predicate the input path uses, so where the
        // pixels go and where clicks are measured cannot disagree.
        let game_offscreen = self.game_offscreen();
        // Same reason: the terrain chunks' dissolve-in clock is read before the
        // destructure below takes `&mut self`.
        let chunk_now = self.now();
        // The frame profile, cloned out before the destructure below takes
        // `&mut self`. It is an `Rc<RefCell<…>>` shared with the
        // Lua `perf` table, so this is a refcount bump and the numbers a game
        // reads are the same ones written here.
        let profile = self.script_host.profile().clone();

        // Every phase below takes its own borrows of these; one check here keeps
        // the early return where it always was.
        if self.gpu.is_none()
            || self.raster.is_none()
            || self.raymarch.is_none()
            || self.retro.is_none()
            || self.outline.is_none()
            || self.grid_render.is_none()
            || self.line_layer.is_none()
            || self.tri_layer.is_none()
            || self.particles.is_none()
            || self.post.is_none()
            || self.egui.is_none()
        {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(gather) = self.gather_frame(chunk_now, elapsed, &profile, sky_active, sky_uniform_vals, terrain_base_mat)
        else {
            return;
        };

        let timing = self.gpu_timing_open
            && self.gpu_timer.as_mut().map(|t| {
                t.poll();
                t.begin()
            }) == Some(true);
        let Some((gather, ui)) = self.build_frame_ui(
            gather,
            elapsed,
            game_focused,
            game_offscreen,
            preview_view,
            &profile,
            &window,
        ) else {
            return;
        };
        self.draw_frame(gather, ui, timing, elapsed, &profile, ui_layers, ui_world, &window);
    }

    /// Draw the frame: the scene into its target, the blit, the UI on top, and
    /// the present. Takes its own borrows of the renderer; `render()` has already
    /// checked they exist.
    #[cfg(feature = "editor-ui")]
    #[allow(clippy::too_many_arguments)]
    fn draw_frame(
        &mut self,
        gather: FrameGather,
        ui: FrameUi,
        timing: bool,
        elapsed: f32,
        profile: &Rc<RefCell<floptle_core::profile::FrameProfile>>,
        ui_layers: Vec<(floptle_ui::DrawList, f32)>,
        ui_world: Vec<WorldCanvas>,
        window: &Arc<winit::window::Window>,
    ) {
        let (
            Some(gpu),
            Some(raster),
            Some(raymarch),
            Some(retro),
            Some(outline),
            Some(grid_render),
            Some(line_layer),
            Some(tri_layer),
            Some(particles),
            Some(post),
            Some(egui),
            // Not `Some(...)`: a project with no screen shaders has no registry
            // yet, and that must not stop the frame from being drawn.
            post_shaders,
            // Likewise: the scene colour history is allocated the first frame a
            // scene asks for reflections and dropped when it stops asking, so
            // "absent" is its ordinary state and not a reason to skip the frame.
            scene_history,
            // …and likewise: a device without timestamp queries has no timer, and
            // that is a missing measurement, not a missing frame.
            mut gpu_timer,
        ) = (
            self.gpu.as_mut(),
            self.raster.as_mut(),
            self.raymarch.as_mut(),
            self.retro.as_mut(),
            self.outline.as_ref(),
            self.grid_render.as_mut(),
            self.line_layer.as_mut(),
            self.tri_layer.as_mut(),
            self.particles.as_mut(),
            self.post.as_mut(),
            self.egui.as_mut(),
            self.post_shaders.as_ref(),
            &mut self.scene_history,
            self.gpu_timer.as_mut(),
        ) else {
            return;
        };
        // One pose table per frame, not per pass. A frame gathers
        // the scene several times over — the Scene view, a docked Game view, every
        // render target, the selection mask — and each of those passes reads pose
        // indices handed out by an earlier gather. Resetting between them would
        // leave the mask pointing at a table that had moved under it.
        // ⏱ Open a timing frame. `begin` refuses while the previous frame's
        // readback is still out, and `timing` carries that refusal to every mark
        // below — a frame is measured whole or not at all, because half a frame's
        // marks would report each pass against its neighbour's name.
        macro_rules! gpu_mark {
            ($label:expr) => {
                if timing {
                    if let Some(t) = gpu_timer.as_mut() {
                        t.mark(gpu, $label);
                    }
                }
            };
        }


        let FrameGather {
            aspect,
            cam,
            clear,
            contact,
            flat2d,
            flsl_draws,
            fog_color,
            game_view,
            gizmo_tool: _,
            globals,
            instances,
            light_node,
            lights_2d,
            mask_blob,
            mask_mesh,
            mask_skins,
            particle_fog,
            point_shadows,
            post_settings,
            rm,
            rm_draw,
            skin_draws,
            vfx_batches,
            vfx_instances,
            view_proj,
        } = gather;
        let FrameUi {
            ctx,
            shapes,
            textures_delta,
            egui_ppp,
            glass,
            ppp,
            ssr_on,
            after,
        } = ui;
        // ---- draw: scene into the retro target, blit, then egui on top ----
        // Timed, because blocking here is not the same thing as being slow — see
        // `present_wait_ms`.
        let wait_t = floptle_core::time::Instant::now();
        let acquired = gpu.acquire();
        let wait_ms = wait_t.elapsed().as_secs_f32() * 1000.0;
        self.present_wait_ms = if self.present_wait_ms > 0.0 {
            self.present_wait_ms * 0.9 + wait_ms * 0.1
        } else {
            wait_ms
        };
        match acquired {
            Some(frame) => {
                // The scene always renders into the post input, whether or not
                // any effect is switched on: the scene renders in the
                // floating-point scene format, the window takes 8-bit sRGB, and
                // the chain's terminal pass is the only thing that knows how to
                // get from one to the other. So "no effects" is a chain of
                // exactly one pass rather than a different route.
                let depth =
                    if self.project.retro { retro.depth_view() } else { gpu.depth_view() };
                let color = post.input_view();
                // `rm_draw` already accounts for the matter toggle + terrain presence;
                // with nothing to raymarch the globals still upload so the raster
                // pass's field group (shadows/AO/proxies) sees this frame's data.
                // …and render, second half: the passes themselves.
                let draw_t = floptle_core::profile::Span::new();
                // The sky, into the environment map, so surfaces have something
                // to reflect. Before every other pass and after the globals,
                // because the capture evaluates the sky through the very
                // uniforms `rm` carries — and every frame, because skies move:
                // a cached one would be wrong exactly when someone was watching
                // it change. It costs a 256×128 draw and eight tinier ones.
                gpu_mark!("sky / environment");
                raymarch.upload_globals(gpu, rm);
                raymarch.capture_env(gpu);
                // A custom shader that measures the gap to the surface behind it
                // (`surfaceGap` — shoreline foam, soft particles, contact glow)
                // needs the prepass too, even with nothing to raymarch. Without
                // this the effect works in a terrain scene and silently does
                // nothing in a scene made of meshes, which is the only kind of
                // scene most of these shaders are ever put in.
                let depth_wanted = raster.flsl_draws_want_depth(&flsl_draws);
                let raster_clear = if rm_draw
                    || wants_prepass(depth_wanted, ssr_on, point_shadows, contact[0] > 0.5)
                {
                    // Opaque depth prepass: primes the depth buffer (early-z kills
                    // hidden raster fragments before their shadow-marching shader
                    // runs) and caps the raymarch at the nearest mesh per pixel.
                    let depth_tex =
                        if self.project.retro { retro.depth_texture() } else { gpu.depth_texture() };
                    let hist = scene_history.as_ref().map(|h| (h.view(), h.sampler()));
                    gpu_mark!("depth prepass");
                    prepass_and_bind(
                        gpu, raster, raymarch, globals, &instances, &flsl_draws, &skin_draws,
                        depth_tex, hist,
                    );
                    if rm_draw {
                        gpu_mark!("matter / terrain");
                        raymarch.draw_into_primed(gpu, color, depth, rm);
                        None
                    } else {
                        // Nothing to raymarch: the prepass ran purely so the
                        // depth texture exists to be read. The raster pass still
                        // owns the frame, so it clears as usual — `prime_tex` is
                        // its own copy and survives that.
                        raymarch.upload_globals(gpu, rm);
                        Some(clear.map(|c| c as f64))
                    }
                } else {
                    raymarch.upload_globals(gpu, rm);
                    Some(clear.map(|c| c as f64))
                };
                gpu_mark!("opaque + lighting");
                raster.draw_scene_with(
                    gpu, color, depth, globals, &instances, &flsl_draws, &skin_draws,
                    raster_clear, Some(raymarch.field_bind()),
                );
                let composited = {
                    let d = if self.project.retro {
                        retro.depth_texture()
                    } else {
                        gpu.depth_texture()
                    }
                    .size();
                    (d.width.max(1), d.height.max(1))
                };
                // Posterize, here — over the art the raster and raymarch passes
                // just drew and before a light touches it. The palette is what
                // the setting quantizes; the light is a multiplier on top of it.
                if let Some(q) = post_settings.palette() {
                    raster.quantize_palette(gpu, color, composited, q);
                }
                // 2D lighting composites over the scene the raster pass just
                // drew, so a lit tilemap replaces its own unlit pixels. Runs on
                // both draw paths — see `lit_2d_rank`.
                gpu_mark!("2D lighting");
                raster.light2d_pass(
                    gpu,
                    color,
                    depth,
                    composited,
                    view_proj.to_cols_array_2d(),
                    &lights_2d,
                    &flat2d,
                );
                // ---- glass ------------------------------------------------
                // The scene is finished except for the things you can see
                // through. Capture it, hand that capture to the shader as "what
                // is behind", and draw them.
                //
                // The capture is what makes refraction possible at all: a
                // surface cannot sample a picture it is already in, and the only
                // picture that exists during the main pass is the previous
                // frame's — which has the glass in it. Its tint would deepen
                // every frame it stayed on screen.
                gpu_mark!("glass");
                if glass && let Some(h) = scene_history.as_mut() {
                    // The stored picture is this frame's, taken from this
                    // camera, so the reprojection is the identity — say so, or
                    // the reflections on the glass would look up last frame's
                    // matrix against a texture that is not last frame's.
                    let mut glass_rm = rm;
                    glass_rm.ssr_prev_vp = view_proj.to_cols_array_2d();
                    raymarch.upload_globals(gpu, glass_rm);
                    // Far to near, re-capturing between: each layer of glass
                    // samples a picture holding the panes behind it and none of
                    // the panes in front. One layer is one capture and one pass,
                    // exactly as before.
                    let cuts = raster.transmissive_cuts(
                        &instances,
                        &skin_draws,
                        light_node.refraction_layers,
                    );
                    for layer in 0..=cuts.len() {
                        h.capture(gpu, color, view_proj, cam.world_position);
                        // The capture writes into the same texture every time, so
                        // the bind group it belongs to stays valid — rebinding is
                        // only for the frame that (re)created it.
                        if layer == 0 {
                            raymarch.bind_frame_targets(
                                gpu,
                                raster.prepass_view(),
                                Some((h.view(), h.sampler())),
                            );
                        }
                        raster.draw_transmissive(
                            gpu, color, depth, globals, &instances, &skin_draws,
                            Some(raymarch.field_bind()), &cuts, layer,
                        );
                    }
                }
                // Script-drawn 3D lines (draw.line — the map's orbit conics).
                if !self.script_lines.is_empty() {
                    let verts: Vec<floptle_render::LineVertex> = self
                        .script_lines
                        .iter()
                        .flat_map(|l| {
                            let a = (DVec3::from(l.a) - cam.world_position).as_vec3();
                            let b = (DVec3::from(l.b) - cam.world_position).as_vec3();
                            [
                                floptle_render::LineVertex { pos: [a.x, a.y, a.z], color: l.color },
                                floptle_render::LineVertex { pos: [b.x, b.y, b.z], color: l.color },
                            ]
                        })
                        .collect();
                    line_layer.draw(gpu, color, depth, view_proj, &verts);
                }
                // The navmesh's walkable surface, filled. Before the script
                // triangles so a game's own gizmos draw on top of it rather
                // than under a translucent floor. Already camera-relative —
                // `nav_surface` was built that way in the gather pass.
                if !self.nav_surface.is_empty() {
                    tri_layer.draw(gpu, color, depth, view_proj, &self.nav_surface);
                }
                // Script-drawn filled triangles (draw.tri/cone/disc — solid gizmos).
                if !self.script_tris.is_empty() {
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
                // Script-drawn textured quads (draw.quad — sword trails, decals,
                // ground rings): in the world, depth-tested, one run per texture.
                if !self.script_quads.is_empty() {
                    let (verts, batches) = pack_script_quads(&self.script_quads, cam.world_position);
                    tri_layer.draw_textured(gpu, color, depth, view_proj, &verts, &batches, raster);
                }
                // Live particles: after all opaque work (they depth-test against
                // meshes and raymarched matter), before post/retro — so they're
                // AO'd/bloomed and pixelate with the scene.
                if !vfx_batches.is_empty() {
                    particles.draw(
                        gpu,
                        color,
                        depth,
                        crate::vfx::particle_globals(&cam, aspect, fog_color, particle_fog),
                        &vfx_instances,
                        &vfx_batches,
                        raster,
                    );
                }
                // Keep this frame's picture, for the next frame's reflections.
                //
                // Here and not later: everything that belongs to the scene has
                // drawn — the raymarched world, the meshes, the palette
                // quantise, the 2D light pass, the particles — and nothing that
                // does not has started. Post is still to come, and reflecting a
                // tonemapped, bloomed, vignetted frame would put the grade into
                // the reflection and then grade it a second time on the way out.
                //
                // The editor's own furniture (the grid, gizmos, selection
                // outlines) is on the far side of this line too: a
                // mirror must not show the reference grid.
                if let Some(h) = scene_history.as_mut() {
                    h.capture(gpu, color, view_proj, cam.world_position);
                }
                // The reference grid is an editor aid — Scene view only, and
                // after the capture above: it is not part of the
                // scene, and a mirror that reflected the editor's own graph
                // paper would be showing something that does not exist.
                if self.grid.show && !game_view {
                    let c = self.grid.color;
                    grid_render.draw(
                        gpu,
                        color,
                        depth,
                        view_proj,
                        cam.world_position,
                        self.grid.size,
                        self.grid.extent,
                        self.grid.y_offset,
                        [c[0], c[1], c[2], self.grid.alpha],
                    );
                }
                // ---- Scene-view UI canvases: the layers as world planes at
                // their node transforms, depth-tested into the scene (the
                // "physically in the world" authoring view). Also projects the
                // element outlines for the Scene tab's select/drag overlay.
                self.ui_overlay.clear();
                self.ui_canvas.clear();
                let ui_gizmos = self.show_gizmos;
                if !ui_world.is_empty()
                    && let Some(uir) = self.ui_render.as_mut()
                {
                    let vp_mat = cam.view_proj(aspect);
                    let (w_px, h_px) = (gpu.config.width as f32, gpu.config.height as f32);
                    let srect = self.scene_rect.unwrap_or(egui::Rect::NOTHING);
                    crate::ui_game::draw_ui_world(
                        gpu,
                        raster,
                        uir,
                        &self.texture_registry,
                        (&self.ui_flsl_cache, &self.ui_flsl_binds),
                        color,
                        depth,
                        cam.world_position,
                        vp_mat,
                        &ui_world,
                    );
                    // Project element rects → Scene-tab overlay entries
                    // (gizmos — the master Gizmos toggle hides them, the
                    // canvas content stays since it's your actual UI).
                    if ui_gizmos {
                        project_ui_canvases(
                            &ui_world,
                            cam.world_position,
                            vp_mat,
                            (w_px, h_px),
                            ppp,
                            srect,
                            &mut self.ui_overlay,
                            &mut self.ui_canvas,
                        );
                    }
                }

                // Post runs before any retro upscale, at the scene's composited
                // resolution. SSAO reads whichever depth the scene rendered with;
                // in retro mode the chain outputs into the retro color target so
                // the nearest-neighbor blit carries the finished effects up with
                // the same chunky pixels as the scene.
                // Capture the composited scene into the UI backdrop before post
                // consumes it, so frosted-glass UI (`backdrop()`) works in
                // fullscreen/player. Retro mode is the one case with nothing to
                // capture at this size — its offscreen is the retro internal
                // resolution, not the frame's — so there the backdrop is cleared
                // and `backdrop()` reads black rather than a stale capture.
                //
                // What is captured is the scene before the tonemap, so anything
                // brighter than white clamps on the way into the (8-bit) backdrop
                // texture. Frosted glass is a blur of what is behind it, not a
                // measurement, so that is the right trade rather than a second
                // floating-point full-screen texture.
                if !ui_layers.is_empty()
                    && let Some(uir) = self.ui_render.as_mut()
                {
                    if !self.project.retro {
                        let mut enc = gpu.device.create_command_encoder(
                            &wgpu::CommandEncoderDescriptor { label: Some("ui-backdrop") },
                        );
                        uir.capture_backdrop(
                            gpu,
                            &mut enc,
                            post.input_view(),
                            gpu.config.width,
                            gpu.config.height,
                        );
                        gpu.queue.submit(Some(enc.finish()));
                    } else {
                        uir.clear_backdrop();
                    }
                }
                {
                    let proj = cam.proj_matrix(aspect);
                    let ssao_frame = floptle_render::SsaoFrame {
                        depth: if self.project.retro { retro.depth_view() } else { gpu.depth_view() },
                        proj: proj.to_cols_array_2d(),
                        inv_proj: proj.inverse().to_cols_array_2d(),
                        fog: crate::shading::ao_fog(&self.world, cam.world_position),
                    };
                    // In retro mode the chain writes the retro colour target and
                    // the nearest-neighbour blit carries the finished picture up;
                    // otherwise it writes the window.
                    let out = if self.project.retro { retro.color_view() } else { &frame.view };
                    // Focus-on-a-node is resolved here, against the camera this
                    // view is actually rendering from, so the Scene view shows
                    // its own focus while you fly around instead of the game
                    // camera's.
                    let mut ps = post_settings;
                    if let Some(d) =
                        crate::shading::dof_focus_distance(&self.world, cam.world_position)
                    {
                        ps.dof_focus = d;
                    }
                    // Motion blur is a game-view effect. The Scene view is a
                    // tool: you have to be able to place a prop while the
                    // camera is still coasting, and a viewport that smears
                    // whenever you orbit is a viewport you fight.
                    if game_view {
                        self.motion_prev = Some(crate::shading::motion_frame(
                            &mut ps,
                            self.motion_prev,
                            cam.view_proj(aspect),
                            cam.world_position,
                            if self.project.retro {
                                retro.resolution().1
                            } else {
                                gpu.config.height
                            },
                        ));
                    } else {
                        ps.motion_blur = 0.0;
                    }
                    gpu_mark!("post (AO, bloom, blur…)");
                    post.run_with(gpu, &ps, Some(&ssao_frame), out, post_shaders);
                }
                if self.project.retro {
                    if self.project.retro_integer_scale {
                        let dest =
                            [gpu.config.width as f32, gpu.config.height.max(1) as f32];
                        retro.blit_integer(gpu, &frame.view, dest);
                    } else {
                        retro.blit(gpu, &frame);
                    }
                }

                profile
                    .borrow_mut()
                    .record(floptle_core::profile::Bucket::Render, draw_t.ms());

                // ---- game UI: over the finished frame (native res), before
                // the editor's own chrome. One instanced pass per frame.
                if !ui_layers.is_empty()
                    && let Some(uir) = self.ui_render.as_mut()
                {
                    let vp = [gpu.config.width as f32, gpu.config.height as f32];
                    let mut ui_instances = Vec::new();
                    let mut ui_batches = Vec::new();
                    for (dl, scale) in &ui_layers {
                        let reg = &self.texture_registry;
                        let uic = &self.ui_flsl_cache;
                        let uib = &self.ui_flsl_binds;
                        uir.pack(
                            gpu,
                            dl,
                            [0.0, 0.0],
                            *scale,
                            &mut |p| reg.get(p).copied(),
                                &|id| raster.texture_size(id),
                            &mut |p, owner| {
                                let shader =
                                    uic.get(p).and_then(|e| e.compiled.as_ref()).map(|(_, id)| *id)?;
                                Some((shader, uib.get(&owner)?.binding))
                            },
                            &mut ui_instances,
                            &mut ui_batches,
                        );
                    }
                    // Backdrop for frosted-glass UI was captured before post ran
                    // (post-on path); if there was no sampleable source it was
                    // cleared to black. Draw the UI over the finished frame.
                    uir.draw(gpu, &frame.view, vp, &ui_instances, &ui_batches, raster);
                }

                // Selection outline: mask the selected object's silhouette (full
                // frame res, so it stays crisp over the retro scene) then edge-detect
                // it onto the frame. Works for meshes and the SDF blob alike.
                let masked = if !mask_mesh.is_empty() {
                    raster.draw_mask(gpu, outline.mask_view(), globals, &mask_mesh, &mask_skins);
                    true
                } else if let Some(brm) = mask_blob {
                    raymarch.draw_mask(gpu, outline.mask_view(), brm);
                    true
                } else {
                    false
                };
                if masked {
                    outline.composite(gpu, &frame.view, [1.0, 1.0, 1.0, 1.0], 1.3);
                }

                // egui composited over the final frame
                let ppp = egui_ppp;
                let tris = ctx.tessellate(shapes, ppp);
                let screen = egui_wgpu::ScreenDescriptor {
                    size_in_pixels: [gpu.config.width, gpu.config.height],
                    pixels_per_point: ppp,
                };
                for (id, delta) in &textures_delta.set {
                    egui.renderer.update_texture(&gpu.device, &gpu.queue, *id, delta);
                }
                gpu_mark!("editor UI");
                let mut encoder = gpu
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("egui") });
                egui.renderer.update_buffers(&gpu.device, &gpu.queue, &mut encoder, &tris, &screen);
                {
                    let mut pass = encoder
                        .begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("egui"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &frame.view,
                                depth_slice: None,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Load,
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                            multiview_mask: None,
                        })
                        .forget_lifetime();
                    egui.renderer.render(&mut pass, &tris, &screen);
                }
                gpu.queue.submit([encoder.finish()]);
                // ⏱ Close the frame before `present`: what happens after this is
                // the display's business, and the panel's job is to account for
                // the work the engine asked for.
                if timing && let Some(t) = gpu_timer.as_mut() {
                    t.end(gpu);
                }
                for id in &textures_delta.free {
                    egui.renderer.free_texture(id);
                }
                dump_presented_frame(gpu, &frame.surface.texture, self.frame_no);
                frame.present();
            }
            None => {
                let size = window.inner_size();
                gpu.resize(size.width, size.height);
            }
        }
        self.after_draw(&ctx, elapsed, after);
    }

    /// Acts on what the frame's UI asked for, once the picture is on screen:
    /// saves and the quit, the Settings tab's upkeep, the queued editor
    /// commands, the packages' clicks, the profiler toggle; then closes the
    /// frame's profile.
    #[cfg(feature = "editor-ui")]
    fn after_draw(&mut self, ctx: &egui::Context, elapsed: f32, after: FrameAfter) {
        let FrameAfter {
            mut cmd,
            ext_menu_click,
            ext_shortcut_click,
            frame_pointer_down,
            perf_toggle,
            pkg_action,
            want_exit,
            want_save,
            want_save_all,
            want_save_project,
        } = after;
        if want_save_all {
            // Quit-time full save (scene + project + scripts), with its own toast.
            self.save_all();
            // An unnamed image opened its "save as" dialog instead of saving —
            // put the tab in front of it, or that dialog is behind whatever the
            // user was actually looking at.
            if self.image.save_name.is_some() {
                self.focus_image_tab();
            }
        } else if want_save || cmd.save_scene {
            self.save_scene();
        }
        if (want_save_project || cmd.save_project)
            && let Err(e) = floptle_scene::save_project(&self.project, &self.project_cfg_path()) {
                floptle_say::say_err!("  save project failed: {e}");
            }
        // Edit ⏵ Project Settings opens (or focuses) the ⚙ Settings TAB — it
        // docks like anything else, so there is no modal to dismiss.
        if cmd.open_settings && let Some(d) = self.dock_state.as_mut() {
            crate::dock::focus_settings_tab(d);
        }
        if let Some(edits) = cmd.input_edits.take() {
            self.apply_input_edits(edits);
        }
        // The ⚙ Settings tab drives its own edits (it has `&mut self`); what
        // remains here is the per-frame upkeep it needs while visible: keep the
        // script scan fresh, and settle a press-to-bind that just landed.
        let settings_front = self
            .dock_state
            .as_ref()
            .is_some_and(|d| crate::dock::tab_is_front(d, crate::dock::EditorTab::Settings));
        if settings_front {
            let dir = self.project_root.join("scripts");
            let now = self.play_t.max(elapsed);
            self.input_scan.poll(&dir, now);
            // Auto-commit a capture — click +, press the input, done. Escape
            // always backs out, so an accidental arm is never a trap.
            let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
            self.settle_pending_rebind(escape);
        }
        // A quit decision (Save & Quit / Discard & Quit): the save above has run, so leave
        // now — `about_to_wait` sees this and exits the winit loop.
        if want_exit {
            self.pending_exit = true;
        }

        self.apply_frame_commands(cmd, frame_pointer_down);

        // ---- what the packages asked for this frame ----
        // Menu items and shortcuts run their Lua here, not in the UI pass: a
        // callback may open a panel, edit the scene or reload the package it
        // belongs to, and none of that can happen while the host is drawing.
        if let Some(i) = ext_menu_click {
            self.ext.run_menu(i);
        }
        if let Some(i) = ext_shortcut_click {
            self.ext.run_shortcut(i);
        }
        self.apply_ext_commands();
        if let Some(dir) = pkg_action.open_folder {
            crate::project::open_in_file_manager(&dir);
        }
        if pkg_action.reload {
            self.ext_reload();
        }
        if self.ext.wants_repaint() {
            ctx.request_repaint();
        }
        // Collection on while the panel is shut can only be a script's doing, so
        // that is how ownership is known — no extra channel from Lua.
        self.perf_enabled_by_script =
            self.script_host.profile().borrow().enabled() && !self.show_perf_panel;
        // Opening ⏱ starts collecting; closing it stops. But a game that called
        // `perf.enable(true)` itself keeps it on — closing the panel must not
        // silently break the budget check a smoke test depends on.
        if let Some(on) = perf_toggle
            && (on || !self.perf_enabled_by_script)
        {
            self.script_host.profile().borrow_mut().enable(on);
        }

        // The frame is over: fold every bucket into its history.
        // Once, at the very end, so a subsystem that reported in several pieces —
        // physics per tick, scripts per pass — contributes one figure per frame.
        // A no-op while collection is off.
        self.script_host.profile().borrow_mut().end_frame();
    }
}

/// Projects each Scene-view UI canvas — its element rects and its design
/// viewport — into Scene-tab overlay entries, for the select/drag overlay
/// and the canvas bounds gizmo.
#[cfg(feature = "editor-ui")]
#[allow(clippy::too_many_arguments)] // the frame's projection facts, and the two lists they fill
fn project_ui_canvases(
    ui_world: &[WorldCanvas],
    cam_pos: DVec3,
    vp_mat: floptle_core::math::Mat4,
    (w_px, h_px): (f32, f32),
    ppp: f32,
    srect: egui::Rect,
    ui_overlay: &mut Vec<(u32, [f32; 4], f32)>,
    ui_canvas: &mut Vec<[[f32; 2]; 4]>,
) {
    for (_, placed, origin, right, down, design_vp) in ui_world {
        let rel = floptle_core::math::Vec3::new(
            (origin[0] - cam_pos.x) as f32,
            (origin[1] - cam_pos.y) as f32,
            (origin[2] - cam_pos.z) as f32,
        );
        let r3 = floptle_core::math::Vec3::from(*right);
        let d3 = floptle_core::math::Vec3::from(*down);
        let to_screen = |p: floptle_core::math::Vec3| -> Option<egui::Pos2> {
            let clip = vp_mat * p.extend(1.0);
            if clip.w <= 0.01 {
                return None;
            }
            let ndc = clip / clip.w;
            Some(egui::pos2(
                (ndc.x * 0.5 + 0.5) * w_px,
                (1.0 - (ndc.y * 0.5 + 0.5)) * h_px,
            ))
        };
        for pl in placed {
            let [x, y, w, h] = pl.rect;
            let corners = [
                rel + r3 * x + d3 * y,
                rel + r3 * (x + w) + d3 * y,
                rel + r3 * x + d3 * (y + h),
                rel + r3 * (x + w) + d3 * (y + h),
            ];
            let pts: Vec<egui::Pos2> =
                corners.iter().filter_map(|c| to_screen(*c)).collect();
            if pts.len() < 4 {
                continue;
            }
            let (mut minx, mut miny, mut maxx, mut maxy) =
                (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for p in &pts {
                minx = minx.min(p.x);
                miny = miny.min(p.y);
                maxx = maxx.max(p.x);
                maxy = maxy.max(p.y);
            }
            // px → egui points, relative to the Scene rect.
            let sx = (minx / ppp) - srect.min.x;
            let sy = (miny / ppp) - srect.min.y;
            let sw = (maxx - minx) / ppp;
            let sh = (maxy - miny) / ppp;
            // Drag scale: overlay points per design unit.
            let scale = if w > 0.5 { sw / w } else { 1.0 };
            ui_overlay.push((pl.id, [sx, sy, sw, sh], scale.max(0.001)));
        }
        // Canvas bounds gizmo: the layer's design viewport as a
        // projected quadrilateral (Scene-tab points).
        let (cw, chh) = (design_vp[0], design_vp[1]);
        let corners = [
            rel,
            rel + r3 * cw,
            rel + r3 * cw + d3 * chh,
            rel + d3 * chh,
        ];
        let pts: Vec<egui::Pos2> =
            corners.iter().filter_map(|c| to_screen(*c)).collect();
        if pts.len() == 4 {
            let mut quad = [[0.0f32; 2]; 4];
            for (i, p) in pts.iter().enumerate() {
                quad[i] = [p.x / ppp - srect.min.x, p.y / ppp - srect.min.y];
            }
            ui_canvas.push(quad);
        }
    }
}

/// `FLOPTLE_FRAME_DUMP=<dir>`: photographs every presented frame into that
/// directory, out of the swapchain image itself. A glitch that lasts one frame
/// while the camera moves cannot be caught by a screenshot key or a headless
/// render; a stream of the frames the player actually saw can be scanned for
/// it afterwards. A diagnostic, not a feature — a readback per frame.
#[cfg(feature = "editor-ui")]
fn dump_presented_frame(gpu: &floptle_render::Gpu, tex: &wgpu::Texture, frame_no: u64) {
    if let Some(dir) = std::env::var_os("FLOPTLE_FRAME_DUMP")
        && let Some(px) = read_back_frame(gpu, tex)
    {
        let (w, h) = (tex.width(), tex.height());
        let path = std::path::PathBuf::from(dir)
            .join(format!("frame-{:05}.png", frame_no));
        // Encoded off the frame thread, at most a handful at a
        // time: a frame that arrives while the encoders are all
        // busy is dropped, named by its number, rather than
        // queued up until the machine runs out of memory.
        static BUSY: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        use std::sync::atomic::Ordering::SeqCst;
        if BUSY.fetch_add(1, SeqCst) < 6 {
            std::thread::spawn(move || {
                if let Some(buf) = image::RgbaImage::from_raw(w, h, px) {
                    let _ = std::fs::create_dir_all(
                        path.parent().unwrap_or(std::path::Path::new(".")),
                    );
                    let _ = buf.save(&path);
                }
                BUSY.fetch_sub(1, SeqCst);
            });
        } else {
            BUSY.fetch_sub(1, SeqCst);
        }
    }
}

/// Pack this tick's `draw.quad`s (already texture-resolved and sorted by
/// texture) into camera-relative textured triangles, two per quad, with one
/// batch per run of the same texture. Corner 0 maps to `(u0, v0)`, 1 to
/// `(u1, v0)`, 2 to `(u1, v1)`, 3 to `(u0, v1)`.
pub(crate) fn pack_script_quads(
    quads: &[(floptle_render::TexId, floptle_script::DrawQuad)],
    cam_pos: DVec3,
) -> (Vec<floptle_render::TexTriVertex>, Vec<floptle_render::TexTriBatch>) {
    let mut verts: Vec<floptle_render::TexTriVertex> = Vec::with_capacity(quads.len() * 6);
    let mut batches: Vec<floptle_render::TexTriBatch> = Vec::new();
    for (id, q) in quads {
        let start = verts.len() as u32;
        let [u0, v0, u1, v1] = q.uv;
        let corner = |i: usize, u: f32, v: f32| {
            let p = (DVec3::from(q.p[i]) - cam_pos).as_vec3();
            floptle_render::TexTriVertex { pos: [p.x, p.y, p.z], color: q.color, uv: [u, v] }
        };
        let (c0, c1, c2, c3) = (corner(0, u0, v0), corner(1, u1, v0), corner(2, u1, v1), corner(3, u0, v1));
        verts.extend_from_slice(&[c0, c1, c2, c0, c2, c3]);
        match batches.last_mut() {
            Some(b) if b.texture == *id => b.range.end = verts.len() as u32,
            _ => batches.push(floptle_render::TexTriBatch { texture: *id, range: start..verts.len() as u32 }),
        }
    }
    (verts, batches)
}
