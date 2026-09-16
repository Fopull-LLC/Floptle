//! The editor's per-frame render: `Editor::render` is the frame loop's single entry
//! point — it steps the sim (`play_step`), gathers the World into renderer uniforms
//! (`mesh_instances`, `draw_2d`), builds the egui UI, and draws. Offscreen views go
//! through `offscreen`; GPU-side scene resources are kept in step by `scene_sync`.

use floptle_core::Entity;
use floptle_core::Matter;
use floptle_core::Name;
use floptle_core::math::DVec3;
use std::collections::HashMap;
use std::path::Path;
use crate::assets::{AssetPayload, collect_texture_paths};
#[cfg(feature = "editor-ui")]
use crate::dock::{EditorTab, default_dock};
#[cfg(feature = "editor-ui")]
use crate::gizmo::Tool;
#[cfg(feature = "editor-ui")]
use crate::hierarchy::{node_new_menu};
use crate::prefs::{DEFAULT_PLAY_TINT, GridConfig};
#[cfg(feature = "editor-ui")]
use crate::theme::{CODE_THEMES, ENGINE_THEMES};
#[cfg(feature = "editor-ui")]
use crate::export::EXPORT_TARGETS;
use crate::{Editor, ProjectAction, anim};
#[cfg(feature = "editor-ui")]
use crate::{EditorCmd, EditorTabViewer, scene_hit};
use crate::offscreen::read_back_frame;
use crate::perf_readout::{PerfSnapshot, Pacing};
#[cfg(feature = "editor-ui")]
use crate::gather::FrameGather;
#[cfg(feature = "editor-ui")]
use crate::perf_readout::perf_readout;
use crate::mesh_instances::{wants_prepass, prepass_and_bind};
#[cfg(feature = "editor-ui")]
use crate::anim_ui;

/// The decision behind the 16-light-cap warning (`floptle/0116`, `floptle/0168`):
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
    /// naming how many were cut (`floptle/0116`, `floptle/0168`). Called from
    /// both gathers — the Scene view and `render_world_into` — because either
    /// can be the first (or only) one to run in a given session.
    ///
    /// The main gather can't call this directly (a live `self.gpu.as_mut()`
    /// borrow through most of `render()` conflicts with a `&mut self` method
    /// call, even though the two touch disjoint fields), so the DECISION is
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
        // 2D: rebuild any tilemap whose grid or sheet changed (`floptle/0058`).
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
        // `sync_terrain_meshes` re-extracts the PRIMARY-ray chunk meshes straight from the
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
        // STALE same-id file into it (the authored scene's old planet loaded
        // under a rolled galaxy's spawn world — the player fell straight through it).
        self.drain_terrain_generates();
        self.update_terrain_residency(lod_cam);
        self.publish_terrain_busy();
        // Background checkpoints (terrain.flush): a few chunks of encoding per
        // frame + threaded writes — autosaves must never stutter the game.
        self.step_terrain_checkpoint();
        {
            // TERRAIN (`floptle/0077`): residency, field generation and meshing.
            // `0074` came in as "I can see through unloaded terrain" and was a
            // priority bug; a number here would have shown the meshing queue.
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
        // values then; edits go to the CLIP as keys, not to scene undo).
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
        // Anything the GPU rejected since the last frame. It no longer takes
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
        // The project's frame pacing, applied before anything acquires a
        // surface image. `set_vsync` early-outs when nothing changed, so this is
        // free on every frame but the one where somebody changes the setting.
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
                    // A held edit (bone gizmo/inspector DRAG) defers its disk save to
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
        // Is the game drawn over the whole WINDOW this frame? Not "does the Game
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
        // destructure below takes `&mut self` (`floptle/0067`).
        let chunk_now = self.now();
        // The frame profile, cloned out before the destructure below takes
        // `&mut self` (`floptle/0077`). It is an `Rc<RefCell<…>>` shared with the
        // Lua `perf` table, so this is a refcount bump and the numbers a game
        // reads are the same ones written here.
        let profile = self.script_host.profile().clone();

        let Some(FrameGather {
            aspect,
            cam,
            clear,
            contact,
            flat2d,
            flsl_draws,
            fog_color,
            game_view,
            gizmo_tool,
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
        }) = self.gather_frame(chunk_now, elapsed, &profile, sky_active, sky_uniform_vals, terrain_base_mat)
        else {
            return;
        };

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
            Some(window),
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
            self.window.as_ref(),
            self.post_shaders.as_ref(),
            &mut self.scene_history,
            self.gpu_timer.as_mut(),
        ) else {
            return;
        };
        let window = window.clone();
        // One pose table per frame, not per pass (`floptle/0080`). A frame gathers
        // the scene several times over — the Scene view, a docked Game view, every
        // render target, the selection mask — and each of those passes reads pose
        // indices handed out by an earlier gather. Resetting between them would
        // leave the mask pointing at a table that had moved under it.
        // ⏱ Open a timing frame. `begin` refuses while the previous frame's
        // readback is still out, and `timing` carries that refusal to every mark
        // below — a frame is measured whole or not at all, because half a frame's
        // marks would report each pass against its neighbour's name.
        let timing = self.gpu_timing_open
            && gpu_timer.as_mut().map(|t| {
                t.poll();
                t.begin()
            }) == Some(true);
        macro_rules! gpu_mark {
            ($label:expr) => {
                if timing {
                    if let Some(t) = gpu_timer.as_mut() {
                        t.mark(gpu, $label);
                    }
                }
            };
        }


        // ---- build the egui UI (mutating the World) ----
        let mut raw_input = egui.state.take_egui_input(&window);
        // A focused game owns the keyboard (`floptle/0084`). egui hands Tab to
        // widget focus traversal before anything else sees it, which put every
        // press on the dock's tab bar and left `input.pressed("tab")` returning
        // false — the same as not being pressed, so a game bound to the most
        // conventional inventory key there is had no way to tell. Gated on a text
        // field not wanting input, so typing into the Console or the Inspector
        // during play still works; a click is how you go back to the editor.
        // `text_edit_focused`, not `egui_wants_keyboard_input` — the latter is
        // "any widget has focus", so clicking a Play-mode HUD button used to
        // hand Tab back to the dock for the rest of the session. See the same
        // fix at the `typing` gate in `main.rs`.
        if self.playing && game_focused && !egui.ctx.text_edit_focused() {
            crate::game_keys::claim_keys_for_game(&mut raw_input, &egui.ctx);
        }
        let ctx = egui.ctx.clone();
        // A package that shipped a typeface gets it registered here — after the
        // load pass, before anything draws with it. `set_fonts` rebuilds egui's
        // glyph atlas, so it is gated on the flag and not run per frame; a
        // project whose packages ship no fonts never reaches it at all.
        if self.ext.fonts_dirty {
            self.ext.fonts_dirty = false;
            ctx.set_fonts(crate::fonts::definitions(&self.ext.fonts));
        }
        // Apply the selected engine (chrome) theme, then a play-mode tint on top so you
        // never mistake play mode for edit mode (and lose edits on Stop). Reapplied each
        // frame so switching the theme in Preferences takes effect immediately.
        {
            let theme = ENGINE_THEMES[self.engine_theme.min(ENGINE_THEMES.len() - 1)];
            let mut vis = theme.visuals();
            if self.playing && self.play_tint_enabled {
                let [tr, tg, tb] = self.play_tint;
                let tint = |c: egui::Color32| {
                    egui::Color32::from_rgb(
                        (c.r() as u16 + tr as u16).min(255) as u8,
                        (c.g() as u16 + tg as u16).min(255) as u8,
                        (c.b() as u16 + tb as u16).min(255) as u8,
                    )
                };
                vis.panel_fill = tint(vis.panel_fill);
                vis.window_fill = tint(vis.window_fill);
                vis.extreme_bg_color = tint(vis.extreme_bg_color);
            }
            ctx.all_styles_mut(|s| {
                s.visuals = vis.clone();
                // **Leave the scrollbar its own gutter.**
                //
                // egui's scroll bars FLOAT by default: they are drawn over the
                // contents and allocate no width. So the last few pixels of
                // every scrolling panel are behind a bar — a slider's label
                // ellipsised down to its first letter, a `…` menu half over the
                // edge — and the panel looks a little bit cut off everywhere,
                // which is exactly what it is. The controls are laid out to the
                // panel's edge correctly; the edge is simply not where the
                // visible area ends.
                //
                // Allocating the bar's width moves that edge in to where things
                // can actually be seen, and every widget follows it — egui's own
                // truncation as much as `responsive::fit_here`. The bar still
                // floats and still looks the same.
                s.spacing.scroll.floating_allocated_width = s.spacing.scroll.bar_width;
            });
        }
        // Every named entity, Matter nodes and the Lighting node alike.
        let entity_names: Vec<(Entity, String)> =
            self.world.query::<Name>().map(|(e, n)| (e, n.0.clone())).collect();
        // Read before `self` is split into the panel context's borrows.
        let gi_status = crate::gi_bake::gi_status(
            &self.world,
            self.gi_bake.as_ref(),
            self.gi_baked.as_ref(),
            self.gi_show_only,
            self.gi_show_probes,
        );
        let nav_status = crate::nav_bake::nav_status(
            &self.world,
            crate::nav_bake::nav_node(&self.world).as_ref().map(|(_, m)| m),
            crate::nav_bake::NavHeld {
                mesh: self.nav_baked.as_ref(),
                seconds: self.nav_seconds,
                triangles: self.nav_triangles,
                file: self.nav_loaded_from.as_deref(),
                baking: self.nav_job.is_some(),
                coverage: self.nav_coverage.as_ref(),
            },
            &self.project_root,
        );
        let ppp = ctx.pixels_per_point();
        let dock_state = self.dock_state.get_or_insert_with(default_dock);
        // Bone names per rigged Mesh entity (name + parent index) — for the hierarchy's
        // expandable sub-objects and the inspector's bone-attach picker. Built read-only
        // before the borrow split so the UI never touches the mesh registry itself.
        let bone_names: HashMap<Entity, Vec<crate::RigNode>> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Mesh { asset_path } => self
                    .mesh_registry
                    .get(asset_path)
                    .and_then(|a| a.rig.as_ref())
                    .map(|rig| {
                        let nodes = rig
                            .skeleton
                            .nodes
                            .iter()
                            .enumerate()
                            .map(|(i, n)| crate::RigNode {
                                name: n.name.clone(),
                                parent: n.parent,
                                is_object: rig.node_is_object.get(i).copied().unwrap_or(true),
                            })
                            .collect();
                        (e, nodes)
                    }),
                _ => None,
            })
            .collect();
        // Prefill the export title from the project's title (Project Settings
        // ⏵ Game); the folder name is a poor fallback (the conventional root is
        // just `assets`, which also collides with the shipped assets folder).
        if self.export_title.is_empty() {
            self.export_title = self.project.title.clone().unwrap_or_else(|| {
                self.project_root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .filter(|n| n != "assets")
                    .unwrap_or_default()
            });
        }
        // The entry-scene picker's options (only scanned while the window is up).
        let fullscreen_tab = &mut self.fullscreen_tab;
        let world = &mut self.world;
        let maps = &self.maps;
        let map_sel = &self.map_sel;
        let map_mode = self.map_mode;
        let map_slot_name = &mut self.map_slot_name;
        let map_viz = &self.map_viz;
        let tile_viz = &self.tile_viz;
        let map_opts = &mut self.map_opts;
        let map_size_buf = &mut self.map_size_buf;
        let map_spec_buf = &mut self.map_spec_buf;
        let map_arm = self.map_arm;
        let map_knife_on = self.map_knife_on;
        let map_orient = &mut self.map_orient;
        let map_xform = &mut self.map_xform;
        let map_select_hidden = &mut self.map_select_hidden;
        let map_bevel = &mut self.map_bevel;
        let map_hud_open = &mut self.map_hud_open;
        let map_keys = &mut self.map_keys;
        let map_rebind = &mut self.map_rebind;
        let map_rebind_err = &mut self.map_rebind_err;
        let map_tool_on = self.tool == Tool::MapEdit;
        let map_playing = self.playing;
        // Copied, not borrowed: it is read by panels that also hold &mut borrows
        // of half the editor, and it does not change during the dock draw.
        let focused_tab = self.focused_tab;
        let has_selection = !self.selection.is_empty();
        // Copied out before the mutable borrow below: the panels read the lock,
        // and ask for the flip through `cmd`.
        let selection_locked = self.selection_locked;
        let selection = &mut self.selection;
        let bone_selection = &mut self.bone_selection;
        let pivot_edit = &mut self.pivot_edit;
        let collapsed = &mut self.collapsed;
        let hier_fold_pending = &mut self.hier_fold_pending;
        let hier_search = &mut self.hier_search;
        let hier_scope = &mut self.hier_scope;
        // Labels only — the parked documents themselves stay on the Editor, so
        // the tab strip can name them without the panel being able to reach into
        // another document's undo stack.
        let image_parked: Vec<String> =
            self.image_stash.iter().map(|s| s.tab_label()).collect();
        let console = &mut self.console;
        let preview_zoom = &mut self.preview_zoom;
        let preview_spin = &mut self.preview_spin;
        let preview_spinning = &mut self.preview_spinning;
        let preview_material = &mut self.preview_material;
        let map_asset_preview = &mut self.map_asset_preview;
        let project = &mut self.project;
        let layer_new = &mut self.layer_new;
        let show_project_mgr = &mut self.show_project_mgr;
        let project_path_buf = &mut self.project_path_buf;
        let grid = &mut self.grid;
        let show_grid_settings = &mut self.show_grid_settings;
        // ⏱ The frame-timing panel's open flag and the last frame it collected,
        // both taken out here for the same reason everything else on this line is
        // — the UI below runs while `self` is split apart.
        let show_gpu_timing = &mut self.gpu_timing_open;
        // Read from the borrowed timer rather than back out of `self`: the frame
        // took it mutably at the destructure. `poll` has already run this frame,
        // so these are the newest results that have actually landed.
        let gpu_spans: Vec<floptle_render::Span> =
            gpu_timer.as_deref().map(|t| t.spans().to_vec()).unwrap_or_default();
        let gpu_total = gpu_timer.as_deref().map(|t| t.total_ms()).unwrap_or(0.0);
        self.gpu_timing_frames = self.gpu_timing_frames.wrapping_add(1);
        let gpu_timing_supported = gpu_timer.is_some();
        if !gpu_spans.is_empty()
            && *show_gpu_timing
            && std::env::var("FLOPTLE_GPU_TIMING").is_ok()
            && self.gpu_timing_frames.is_multiple_of(120)
        {
            floptle_say::say!("--- GPU frame {gpu_total:.2} ms");
            for sp in &gpu_spans {
                floptle_say::say!("  {:>7.3} ms  {}", sp.ms, sp.label);
            }
        }
        let show_terrain_collider = &mut self.show_terrain_collider;
        let show_navmesh = &mut self.show_navmesh;
        let nav_cells = &mut self.nav_cells;
        let show_mesh_colliders = &mut self.show_mesh_colliders;
        let rename_target = &mut self.rename_target;
        let new_scene_buf = &mut self.new_scene_buf;
        let new_asset_prompt = &mut self.new_asset_prompt;
        let show_quit_confirm = &mut self.show_quit_confirm;
        let image_close_confirm = &mut self.image_close_confirm;
        let delete_confirm = &mut self.delete_confirm;
        let layer_children_confirm = &mut self.layer_children_confirm;
        let toast = &mut self.toast;
        // Dirty tilesets ride the scene's flag here. They are not scene state —
        // they are their own files — but every gate that asks "is there unsaved
        // work" wants one answer, and a tileset's collision shapes and autotile
        // groups are hours of work that used to leave with the window.
        let scene_dirty_now = self.scene_dirty || !self.tiles.dirty.is_empty();
        // The 🖼 tab keeps its own dirty flag — an unsaved image is unsaved
        // work, and quitting past it silently is the same loss as quitting past
        // a scene.
        let image_dirty_now = self.image.dirty && self.image.doc.is_some();
        // …and one that has never been written has no filename to save under,
        // so "Save & Quit" cannot silently do it: it has to ask first.
        let image_unnamed = image_dirty_now && self.image.path.is_none();
        let new_terrain_cfg = &mut self.new_terrain_cfg;
        let pending_open_scene = &mut self.pending_open_scene;
        let vertex_brush = &mut self.vertex_brush;
        let terrain_brush = &mut self.terrain_brush;
        let terrain_voxel = &mut self.terrain_voxel;
        let terrain_textures = &mut self.terrain_textures;
        let terrain_glow = &mut self.terrain_glow_mask;
        let terrain_tex_scale = &mut self.terrain_tex_scale;
        let terrain_present = !self.terrains.is_empty();
        // Terrain 2.0 stats: volumes, resident data chunks, resident bytes — the
        // honest sparse numbers (the dense field's O(n³) voxel count is gone).
        let terrain_stats = (!self.terrains.is_empty()).then(|| {
            let chunks: usize = self.terrains.values().map(|t| t.field.data_chunks()).sum();
            let bytes: usize = self.terrains.values().map(|t| t.field.memory_bytes()).sum();
            (self.terrains.len(), chunks, bytes)
        });
        let save_flash = &mut self.save_flash;
        // What the save-status chip names on hover: the real file being edited.
        let save_status_file = if self.scene_rel.is_empty() {
            format!("scenes/{}.ron", self.scene_name)
        } else {
            self.scene_rel.clone()
        };
        let external_editor = &mut self.external_editor;
        let prefer_external = &mut self.prefer_external_editor;
        let show_preferences = &mut self.show_preferences;
        let play_tint_enabled = &mut self.play_tint_enabled;
        let play_tint = &mut self.play_tint;
        // Current theme selections (changes are routed through `cmd`, then saved + applied).
        let engine_theme = self.engine_theme;
        let code_theme = self.code_theme;
        let asset_tree = &self.asset_tree;
        let texture_settings = &self.texture_settings;
        let assets_grid = &mut self.assets_grid;
        let assets_grid_dir = &mut self.assets_grid_dir;
        let project_root = self.project_root.as_path();
        let playing = self.playing;
        // Who owns the pointer this frame, for the Game-view hint. Read as plain
        // fields (not through `game_holds_cursor`) because the closure below only
        // ever holds disjoint field borrows and a `&self` method would collide.
        let cursor_held_by_game = self.game_trap || (self.script_mouse_lock && !self.cursor_freed);
        let cursor_held_by_editor = self.cursor_freed && self.script_mouse_lock;
        let paused = self.paused;
        let game_tick_no = self.game_tick_no;
        let has_active_camera = floptle_core::active_camera(world).is_some();
        // The selected camera's POV preview texture (only when a camera is selected).
        let cam_preview = selection
            .last()
            .copied()
            .filter(|&e| matches!(world.get::<Matter>(e), Some(Matter::Camera { .. })))
            .and(self.cam_preview.as_ref().map(|p| p.tex_id));
        let particles_active = crate::dock::tab_is_front(dock_state, EditorTab::Particles);
        let game_tex = self.game_vp.as_ref().map(|p| p.tex_id);
        let game_rect = &mut self.game_rect;
        let materials = &self.materials;
        let mat_name_buf = &mut self.mat_name_buf;
        let component_clip = &self.component_clip;
        let add_component_filter = &mut self.add_component_filter;
        let layer_names = project.build_layers().names;
        let sorting_names = project.sorting_order();
        let tag_edit = &mut self.tag_edit;
        let hier_scrolled = &mut self.hier_scrolled;
        let show_material_editor = &mut self.show_material_editor;
        // The package extensions and their window. `ext_host` is handed to the
        // dock (its Scene overlays draw in the viewport), and used again after
        // for the floating panels — sequentially, so one `&mut` covers both.
        // What the last load found, read off the host before it is borrowed
        // mutably for the tab viewer — the 📦 Packages tab draws from inside
        // that viewer and cannot hold a second borrow of the host itself.
        let pkg_load = crate::packages_ui::PkgLoad::of(&self.ext);
        let ext_host = &mut self.ext;
        let ext_painted = self.ext_painted.as_slice();
        let packages_state = &mut self.packages_ui;
        let ext_project_root = self.project_root.clone();
        let ext_account = self.account.as_ref();
        // Built before the closure: `ext_menu_tree` reads the whole editor, and
        // inside the UI pass only disjoint field borrows exist.
        let ext_menus = crate::ext_wire::menu_tree(ext_host);
        let ext_focus_window = self.ext_focus_window.take();
        let ext_message = &mut self.ext_message;
        // What the packages' menu and panels decided this frame, applied after
        // the UI pass — running a Lua callback while the host is drawing would
        // be re-entering it.
        let mut ext_menu_click: Option<usize> = None;
        let mut ext_shortcut_click: Option<usize> = None;
        let mut pkg_action = crate::packages_ui::PackagesAction::default();
        let ide = &mut self.ide;
        let learn = &mut self.learn;
        let script_errors = self.script_errors.as_slice();
        let ide_diag = self.ide_diag.as_ref();
        let selected_asset = &mut self.selected_asset;
        let asset_selection = &mut self.asset_selection;
        let aspect_mode = &mut self.aspect_mode;
        let viewport_zoom = &mut self.viewport_zoom;
        let scene_rect = &mut self.scene_rect;
        let scene_name = self.scene_name.clone();
        let gizmo = self.gizmo.as_ref();
        let terrain_viz = self.terrain_viz.as_ref();
        let paint_viz = self.paint_viz.as_ref();
        let camera_gizmos = self.camera_gizmos.as_slice();
        let light_gizmos = self.light_gizmos.as_slice();
        let volume_gizmos = self.volume_gizmos.as_slice();
        let rig_gizmos = self.rig_gizmos.as_slice();
        let gi_probe_dots = self.gi_probe_dots.as_slice();
        let body_gizmos = self.body_gizmos.as_slice();
        let contact_gizmos = self.contact_gizmos.as_slice();
        let script_gizmo_lines = self.script_gizmo_lines.as_slice();
        let game_gizmo_lines = self.game_gizmo_lines.as_slice();
        // The gizmo menu's checkbox writes this directly; remember it so the change can
        // be persisted after the dock UI runs.
        let game_gizmos_before = self.game_gizmos;
        let game_gizmos = &mut self.game_gizmos;
        let terrain_wire = self.terrain_wire_gizmo.as_slice();
        let nav_wire = self.nav_gizmo.as_slice();
        let mesh_wire = self.mesh_wire_gizmo.as_slice();
        let particle_gizmo = self.particle_gizmo.as_slice();
        let show_gizmos = &mut self.show_gizmos;
        let panels = &mut self.panels;
        let panels_saved = &mut self.panels_saved;
        let mut view_lock = self.camera.lock;
        let mut view_ortho = self.camera.ortho;
        let gizmo_filter = &mut self.gizmo_filter;
        let grabbed = self.grabbed;
        let tool = self.tool;
        let context_menu = self.context_menu;
        let anim_sys = &mut self.anim;
        let vfx_sys = &mut self.vfx;
        let vfx_ui_state = &mut self.vfx_ui;
        let audio_sys = &mut self.audio;
        let mixer_ui_state = &mut self.mixer_ui;
        let anim_ui_state = &mut self.anim_ui;
        let shader_graph_state = &mut self.shader_graph;
        let image_state = &mut self.image;
        let ui_design = &mut self.ui_design;
        let shader_preview_state = &mut self.shader_preview;
        let mesh_registry = &self.mesh_registry;
        // Multiplayer harness panel state: read-only status snapshot + live knobs.
        let net_hosting = self.net_server.is_some();
        let net_peer_count = self.net_server.as_ref().map(|s| s.peers().len()).unwrap_or(0);
        let net_has_client = self.net_client.is_some();
        let net_as_player = self.net_play_client.is_some();
        // The MEASURED round trip, falling back to the transport's own number
        // until the first probe comes back. Through a relay the transport can
        // only see its own leg, so it reports host↔relay and calls it the
        // player's ping — off by a whole hop, and always in the flattering
        // direction.
        let net_rtt = self
            .net_play_client
            .as_ref()
            .map(|c| {
                c.peer_rtt_ms(floptle_net::SERVER)
                    .unwrap_or_else(|| c.stats(floptle_net::SERVER).rtt_ms)
            })
            .unwrap_or(0.0);
        // Per-player pings, host side — what a relay could never report.
        let net_peer_rtts = self.net_server.as_ref().map(|s| s.peer_rtts()).unwrap_or_default();
        let net_predicted_name = self
            .net_predictor
            .as_ref()
            .and_then(|(e, _)| world.get::<Name>(*e).map(|n| n.0.clone()));
        let net_pred_stats = self
            .net_predictor
            .as_ref()
            .map(|(_, p)| (p.corrections, p.confirmations, p.last_error));
        let net_late_inputs = self
            .net_hidden
            .as_ref()
            .map(|h| h.session.late_inputs())
            .or_else(|| self.net_server.as_ref().map(|s| s.late_inputs()))
            .unwrap_or(0);
        // Client-side input timing, from the server's InputAck feedback —
        // the only place a JOINER can see whether its inputs run late.
        let net_input_ack = self.net_play_client.as_ref().and_then(|c| c.input_ack());
        // Rollback health (docs/multiplayer.md §7 P6): a fighting
        // game's connection quality is rollback depth and mispredict rate, not
        // ping — and the stall indicator is the one readout a player needs,
        // because a stalled sim looks like the game running slightly slow and
        // is otherwise indistinguishable from a bad frame rate.
        let net_rollback = self.net_rollback.as_ref().map(|d| {
            crate::rollback_session::RollbackStats::with_session(
                d,
                self.net_server.as_ref().or(self.net_play_client.as_ref()),
            )
        });
        // (referee tick, live tick) — how far behind the authoritative sim is.
        let referee = self
            .net_referee
            .as_ref()
            .map(|r| (r.tick(), self.net_rollback.as_ref().map(|d| d.net.current()).unwrap_or(0)));
        let replays = crate::shadow::list_replays(&self.project_root);
        // Interest management is the one feature whose job is to not send
        // things, so with no readout it is indistinguishable from a bug: set
        // the radius too tight and distant objects quietly stop moving, with
        // nothing anywhere saying why. `None` when it's off, which is a
        // different statement from "on, and culling nothing".
        let net_interest = self.net_server.as_ref().and_then(|s| {
            let cfg = s.interest();
            cfg.enabled.then(|| (cfg, s.interest_stats()))
        });
        // Voice chat (floptle/0180): `None` when nothing is captured or heard,
        // which is a different statement from "on, and silent". A voice that
        // is quiet because the jitter buffer is starving looks exactly like a
        // player who stopped talking, and these are the numbers that tell them
        // apart.
        let net_voice = self.voice.active().then(|| {
            (self.voice.diagnostics(), self.voice.mic_summary())
        });
        let voice_test_peer = self.voice.test_speaker();
        // Set by the panel below, acted on after the UI closure releases `self`.
        let (mut voice_test_pick, mut voice_test_stop) = (false, false);
        // A real session (QUIC) has no hub: the link is the actual network, so
        // the simulated latency/loss sliders and ghost worlds don't apply.
        let net_is_real = (self.net_server.is_some() || self.net_play_client.is_some())
            && self.net_hub.is_none();
        if self.net_host_port.is_empty() {
            self.net_host_port = "7777".into();
        }
        if self.net_join_addr.is_empty() {
            self.net_join_addr = "quic://127.0.0.1:7777".into();
        }
        if self.net_relay_addr.is_empty() {
            // The Floptle Cloud rendezvous relay: a DNS-only
            // record straight to the host — the name is the stable contract
            // even if the box moves. Self-hosters just type their own.
            self.net_relay_addr = "relay.fopull.com:7788".into();
        }
        let net_host_port = &mut self.net_host_port;
        let net_join_addr = &mut self.net_join_addr;
        let net_relay_addr = &mut self.net_relay_addr;
        let net_join_code = &mut self.net_join_code;
        let net_lobby_code = self.net_lobby_code.clone();
        // A snapshot of the profile, taken before the UI closure so the readout
        // never holds the `RefCell` across a frame that also writes it.
        let mut perf_snapshot = PerfSnapshot::take(&profile.borrow());
        perf_snapshot.pacing = Pacing {
            mean_ms: self.frame_ms,
            p99_ms: self.frame_low_ms,
            // `refresh_period` is in SECONDS (it is compared against `dt`).
            refresh_ms: self.refresh_period * 1000.0,
            snap_rate: self.dt_snap_rate,
            present_wait_ms: self.present_wait_ms,
            cost_ms: (self.frame_ms - self.present_wait_ms).max(0.0),
        };
        let show_net_panel = &mut self.show_net_panel;
        let show_perf_panel = &mut self.show_perf_panel;
        // Applied after the UI closure, because turning collection on or off
        // needs the profile and the closure has the fields split.
        let mut perf_toggle: Option<bool> = None;
        // Player mode (an exported build / --play): no editor chrome at all —
        // the Game view is the window. F1 (handled at the winit layer) toggles
        // the multiplayer window, which still works for LAN/relay sessions.
        let player_mode = self.player_mode;
        let play_t = self.play_t;
        let ui_overlay_snapshot = self.ui_overlay.clone();
        let ref_kinds = &self.ref_kinds;
        let script_meta = &mut self.script_meta;
        let ui_canvas_snapshot = self.ui_canvas.clone();
        let show_export = &mut self.show_export;
        // Relative export folders resolve against the project's parent (shown
        // live in the dialog) — never the process CWD, which depends on how
        // the editor was launched.
        let export_base =
            self.project_root.parent().unwrap_or(&self.project_root).to_path_buf();
        if self.export_dir.trim().is_empty() {
            self.export_dir = "builds".into();
        }
        let export_dir = &mut self.export_dir;
        let export_title = &mut self.export_title;
        let export_target = &mut self.export_target;
        let export_building = self.export_job.is_some();
        let export_status = &self.export_status;
        let export_done = self.export_done.clone();
        let autosave_prompt = self.autosave_prompt.clone();
        let crash_prompt = self.crash_prompt.clone();
        let project_trust = self.project_trust.clone();
        let scene_name_now = self.scene_name.clone();
        let net_latency_ticks = &mut self.net_latency_ticks;
        let net_loss = &mut self.net_loss;
        let net_ghosts = &mut self.net_ghosts;
        // ⚙ Settings tab inputs. Only gathered when the tab is actually open,
        // so a closed Settings tab costs nothing per frame.
        let settings_open = dock_state.find_tab(&crate::dock::EditorTab::Settings).is_some();
        // Accessibility is `Copy`, so the tab edits a copy and reports back
        // (`floptle/0079`) — no field borrow to thread through the tab viewer.
        let access = self.access;
        let settings_scene_files = if settings_open {
            crate::project::scene_files_in(&self.project_root)
        } else {
            Vec::new()
        };
        let settings_pad_names =
            if settings_open { self.pads.slot_names() } else { Vec::new() };
        let (settings_input_map, settings_input_pending) = {
            let sys = self.script_host.input_system().borrow();
            if settings_open {
                (sys.map().clone(), sys.pending_rebind().cloned())
            } else {
                (floptle_input::InputMap::default(), None)
            }
        };
        let settings_section = &mut self.settings_section;
        let settings_search = &mut self.settings_search;
        let input_scan = &self.input_scan;
        let input_new_action = &mut self.input_new_action;
        let input_test_state = &self.input_test_state;
        let mut cmd = EditorCmd::default();
        let mut want_save = false;
        let mut want_save_project = false;
        // Set inside the egui closure (which only holds field borrows), applied after it —
        // the same deferral `want_save` uses. `want_save_all` = full Ctrl+S save on quit;
        // `want_exit` = actually leave the app once the save has run.
        let mut want_save_all = false;
        let mut want_exit = false;
        let mut frame_pointer_down = false;
        let full_output = ctx.run_ui(raw_input, |ui| {
            let pointer_down = ui.input(|i| i.pointer.any_down());
            frame_pointer_down = pointer_down;
            // ---- top menu bar (never in a build) ----
            if !player_mode {
            // Above the menu bar, so it is the first thing seen: this
            // project's packages are running with no permissions until the
            // user says otherwise (`ext::trust`).
            if let (true, Some(answer)) = crate::ext::trust::banner(ui, &project_trust) {
                cmd.project_trust = Some(answer);
            }
            egui::Panel::top("menu_bar").show(ui, |ui| {
                egui::MenuBar::new().ui(ui, |ui| {
                    ui.menu_button("File", |ui| {
                        if ui.button("New / Open Project…").clicked() {
                            *show_project_mgr = true;
                            ui.close();
                        }
                        if ui.button("Close Project").clicked() {
                            cmd.project_action = Some(ProjectAction::Close);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Save Scene").clicked() {
                            want_save = true;
                            ui.close();
                        }
                        if ui.button("Save Project").clicked() {
                            want_save_project = true;
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .button("Open Project Folder")
                            .on_hover_text("show the project (assets, scenes, scripts) in your file manager")
                            .clicked()
                        {
                            cmd.open_folder = Some(std::path::PathBuf::new()); // empty = project root
                            ui.close();
                        }
                        if ui
                            .button("Export Game…")
                            .on_hover_text(
                                "stamp out a runnable build: the engine + your project, for \
                                 any platform — Windows, Linux or macOS, from whichever \
                                 one you're on",
                            )
                            .clicked()
                        {
                            *show_export = true;
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Exit").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                    ui.menu_button("Edit", |ui| {
                        if ui.button("Undo  (Ctrl+Z)").clicked() { cmd.undo = true; ui.close(); }
                        if ui.button("Redo  (Ctrl+Y)").clicked() { cmd.redo = true; ui.close(); }
                        ui.separator();
                        // Selection-dependent items grey out with nothing selected
                        // (Paste stays — it depends on the clipboard, not selection).
                        if ui.add_enabled(has_selection, egui::Button::new("Copy  (Ctrl+C)")).clicked() { cmd.copy = true; ui.close(); }
                        if ui.button("Paste  (Ctrl+V)").clicked() { cmd.paste = true; ui.close(); }
                        if ui.add_enabled(has_selection, egui::Button::new("Duplicate  (Ctrl+D)")).clicked() { cmd.duplicate = true; ui.close(); }
                        if ui.add_enabled(has_selection, egui::Button::new("Delete  (Del)")).clicked() { cmd.delete = true; ui.close(); }
                        ui.separator();
                        if ui.button("Project Settings").on_hover_text(
                            "Opens the ⚙ Settings tab — drag it wherever you like, or dock it beside the viewport.",
                        ).clicked() {
                            cmd.open_settings = true;
                            ui.close();
                        }
                        if ui.button("Preferences…").clicked() {
                            *show_preferences = true;
                            ui.close();
                        }
                    });
                    // The same catalog as the Hierarchy's ✚ New menu — one source of truth.
                    ui.menu_button("Add", |ui| node_new_menu(ui, &mut cmd, None));
                    ui.menu_button("View", |ui| {
                        ui.checkbox(&mut grid.show, "Grid");
                        ui.checkbox(&mut grid.snap, "Snap to grid");
                        if ui.button("Grid Settings…").clicked() {
                            *show_grid_settings = true;
                            ui.close();
                        }
                        ui.separator();
                        ui.checkbox(&mut *show_terrain_collider, "Terrain collider wireframe")
                            .on_hover_text("show the terrain's collision surface (what the player walks on)");
                        ui.checkbox(&mut *show_mesh_colliders, "Collider wireframes (mesh + shapes)")
                            .on_hover_text("show every static collider — walkable meshes and Collidable Cube/Sphere/Capsule shapes (the selected one always shows)");
                        ui.checkbox(&mut *show_navmesh, "Navmesh")
                            .on_hover_text(
                                "show where characters can walk as one filled surface, a colour \
                                 per connected area, with the joins between elevations drawn \
                                 where a character can actually take them (the Nav Mesh node \
                                 always shows its own when selected)",
                            );
                        ui.add_enabled_ui(*show_navmesh, |ui| {
                            ui.checkbox(&mut *nav_cells, "    ⊞ …and the rectangles it was cut into")
                                .on_hover_text(
                                    "the bake's working: every convex rectangle the walkable \
                                     surface was divided into. Useful for judging cell size; \
                                     it is not what the ground looks like",
                                );
                        });
                    });
                    // Tool windows + panels live under Window (View = viewport display).
                    // Every entry opens/focuses its window (close them from the
                    // window itself) — one consistent behavior.
                    ui.menu_button("Window", |ui| {
                        if ui.button("◑ Material Editor").clicked() {
                            *show_material_editor = true;
                            ui.close();
                        }
                        if ui.button("◎ Animation Controller").on_hover_text("the state-graph editor: states, transitions, fades, layers").clicked() {
                            cmd.focus_anim_graph = true;
                            ui.close();
                        }
                        if ui.button("⏱ Animating").on_hover_text("the animation timeline: preview, keys, events").clicked() {
                            cmd.focus_animating = true;
                            ui.close();
                        }
                        if ui
                            .checkbox(&mut *show_gpu_timing, "⏱ Frame timing")
                            .on_hover_text(
                                "where the frame's time actually goes, measured on the GPU pass \
                                 by pass. Nothing is measured while this is shut, so leaving it \
                                 off costs nothing",
                            )
                            .changed()
                        {
                            ui.close();
                        }
                        if ui.button("Δ Terrain tools").clicked() {
                            cmd.focus_terrain = true;
                            ui.close();
                        }
                        if ui.button("▦ Model tools").clicked() {
                            cmd.focus_map = true;
                            ui.close();
                        }
                        if ui
                            .button("🖼 Image editor")
                            .on_hover_text(
                                "draw a texture in the engine — pixels, paint and vectors, with the mesh updating as you paint",
                            )
                            .clicked()
                        {
                            cmd.focus_image = true;
                            ui.close();
                        }
                        if ui
                            .button("📦 Packages")
                            .on_hover_text(
                                "install, switch off or write a package — editor tools, \
                                 scripts and art anybody can make and share",
                            )
                            .clicked()
                        {
                            cmd.focus_packages = true;
                            ui.close();
                        }
                        ui.separator();
                        ui.label(
                            egui::RichText::new("your layout is saved when you close the editor")
                                .small()
                                .weak(),
                        );
                        if ui
                            .button("⟲ Reset layout")
                            .on_hover_text(
                                "put every panel back where it starts: Hierarchy + Map left, \
                                 viewports and graph editors centre, Inspector right, \
                                 project and timelines below — and forget the saved one, so \
                                 it stays reset",
                            )
                            .clicked()
                        {
                            cmd.reset_layout = true;
                            ui.close();
                        }
                        if ui
                            .button("⟲ Reset window size")
                            .on_hover_text(
                                "back to 1280×720 where it can be seen, and forget where this \
                                 window was — what to press if it opened somewhere awkward",
                            )
                            .clicked()
                        {
                            cmd.reset_window = true;
                            ui.close();
                        }
                    });
                    // HELP, and specifically somewhere to REPORT things. The tracker used
                    // to appear once, in the Hub's About tab, which is not where anybody
                    // is standing when something goes wrong.
                    ui.menu_button("Help", |ui| {
                        if ui
                            .button("🎓 Learn — follow-along tutorials")
                            .on_hover_text(
                                "build a platformer, a top-down RPG or Flappy step by \
                                 step, with each step ticking itself off as your project \
                                 comes to match it",
                            )
                            .clicked()
                        {
                            cmd.focus_learn = true;
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .button("🐛 Report a bug")
                            .on_hover_text(crate::ISSUES_URL)
                            .clicked()
                        {
                            crate::open_issue_tracker(None);
                            ui.close();
                        }
                        if ui.button("📖 Scripting docs").clicked() {
                            let _ = floptle_script::open_in_browser(crate::DOCS_URL);
                            ui.close();
                        }
                        if ui.button("🌐 fopull.com").clicked() {
                            let _ = floptle_script::open_in_browser("https://fopull.com/");
                            ui.close();
                        }
                        ui.separator();
                        ui.label(egui::RichText::new(format!("Floptle {}", env!("CARGO_PKG_VERSION"))).small());
                    });
                    // Whatever the project's packages registered, grouped by
                    // the first segment of each path — so two packages both
                    // filing under "Tools" build one menu, not two.
                    for group in &ext_menus {
                        ui.menu_button(&group.title, |ui| {
                            for (label, idx) in &group.items {
                                if ui.button(label).clicked() {
                                    ext_menu_click = Some(*idx);
                                    ui.close();
                                }
                            }
                        });
                    }
                    ui.separator();
                    let play_label = if playing { "⏹ Stop  (F1)" } else { "⏵ Play  (F1)" };
                    if ui.button(play_label).clicked() {
                        cmd.toggle_play = true;
                    }
                    if playing {
                        let pause_label = if paused { "⏵ Resume  (F2)" } else { "⏸ Pause  (F2)" };
                        if ui.button(pause_label).clicked() {
                            cmd.toggle_pause = true;
                        }
                        // Frame-step: only meaningful while frozen. One click = exactly
                        // one fixedUpdate tick (scripts, physics, animation), then stop
                        // again — how you find out whether a jab is 4 frames of startup
                        // or 5.
                        ui.add_enabled_ui(paused, |ui| {
                            // Backwards first, so the pair reads left-to-right as a
                            // scrubber rather than as two unrelated buttons.
                            if ui
                                .button("⏮ Back  (Shift+F3)")
                                .on_hover_text(
                                    "put the simulation back exactly one gameplay tick.\n\n                                     A simulation isn't invertible, so this reads the \
                                     ROLLBACK state ring rather than re-deriving anything: \
                                     it needs a rollback session running, and reaches back \
                                     as far as the ring keeps (about a fifth of a second).",
                                )
                                .clicked()
                            {
                                cmd.step_tick_back = true;
                            }
                            if ui
                                .button("⏭ Step  (F3)")
                                .on_hover_text(
                                    "advance exactly one gameplay tick — scripts, \
                                     physics and animation each move one frame",
                                )
                                .clicked()
                            {
                                cmd.step_tick = true;
                            }
                        });
                        // The tick counter, so an observed event has a frame NUMBER you
                        // can put in a frame-data table.
                        ui.label(
                            egui::RichText::new(format!("tick {game_tick_no}")).monospace().weak(),
                        )
                        .on_hover_text("gameplay ticks since Play started (60 Hz)");
                    }
                    if ui
                        .button(if net_hosting { "🌐 hosting" } else { "🌐" })
                        .on_hover_text("Multiplayer — host & join locally, latency/loss sliders (docs/multiplayer.md)")
                        .clicked()
                    {
                        *show_net_panel = !*show_net_panel;
                    }
                    // ⏱ Frame cost (`floptle/0077`). Opening it turns collection
                    // on; closing it turns collection off, so the profiler costs
                    // nothing when nobody is looking at it — which is the only
                    // way one stays switched on.
                    if ui
                        .button(if *show_perf_panel { "⏱ profiling" } else { "⏱" })
                        .on_hover_text(
                            "Frame cost — where the time goes, per subsystem and per \
                             script. Readable from Lua too (perf.*), so a game can \
                             assert its own budget in a smoke test.",
                        )
                        .clicked()
                    {
                        *show_perf_panel = !*show_perf_panel;
                        perf_toggle = Some(*show_perf_panel);
                    }
                    // The view is now chosen by the Scene / Game dock tabs (the editor
                    // free-fly view vs the active-camera gameplay view), not a toggle here.

                    // ---- save status (right end of the bar, always visible) ----
                    // Whatever tab you're docked in, this answers "are my changes
                    // on disk?": a quiet "✔ saved" at rest, an amber "● unsaved"
                    // the moment an edit lands, and a brief green glow when a
                    // save completes. Right-aligned so nothing else ever moves.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let dt = ui.input(|i| i.stable_dt).min(0.1);
                        *save_flash = (*save_flash - dt).max(0.0);
                        let quiet = ui.visuals().weak_text_color();
                        // The two signal colours, from the one place they live
                        // (`theme::signal`) — an unsaved change is a warn and a
                        // save that landed is a good, the same amber and green
                        // as everywhere else in the editor.
                        let (label, color, hover) = if scene_dirty_now {
                            (
                                "● unsaved",
                                crate::theme::signal::WARN,
                                format!("{save_status_file} has unsaved changes — click here (or Ctrl+S) to save"),
                            )
                        } else {
                            // Glow bright right after a save, settle to quiet
                            // (t = 0 is the resting state — one branch, one wording).
                            let t = (*save_flash / Editor::SAVE_FLASH_SECS).clamp(0.0, 1.0);
                            (
                                "✔ saved",
                                quiet.lerp_to_gamma(crate::theme::signal::GOOD, t),
                                format!("{save_status_file} is saved"),
                            )
                        };
                        let text = egui::RichText::new(label).color(color);
                        if playing {
                            // Saving is blocked during Play (Play changes aren't
                            // kept) — say so instead of failing quietly.
                            ui.add_enabled(false, egui::Button::new(text).frame(false))
                                .on_disabled_hover_text(
                                    "can't save during Play — press Stop first (Play changes aren't kept)",
                                );
                        } else if scene_dirty_now {
                            // Only a BUTTON when there is something to save. A
                            // chip that looks pressable and does nothing is the
                            // small dead interaction this is meant to replace.
                            if ui
                                .add(egui::Button::new(text).frame(false))
                                .on_hover_text(hover)
                                .clicked()
                            {
                                want_save = true;
                            }
                        } else {
                            ui.label(text).on_hover_text(hover);
                        }
                    });
                });
            });
            }

            // ---- ⏱ frame cost (`floptle/0077`) ----
            if *show_perf_panel {
                let mut open = true;
                egui::Window::new("⏱ Frame cost")
                    .open(&mut open)
                    .default_width(320.0)
                    .show(ui, |ui| {
                        perf_readout(ui, &perf_snapshot);
                    });
                if !open {
                    *show_perf_panel = false;
                    perf_toggle = Some(false);
                }
            }

            // ---- 🌐 multiplayer harness (Host & Join locally) ----
            if *show_net_panel {
                let mut open = true;
                egui::Window::new("🌐 Multiplayer")
                    .open(&mut open)
                    .default_width(280.0)
                    .show(ui, |ui| {
                        if !playing {
                            ui.label("Enter Play mode, then host or join a session here.");
                            ui.small(
                                "Test alone (a hidden ghost client over a simulated link), \
                                 or for real: host on a UDP port and a friend with this \
                                 project joins over the network.",
                            );
                            return;
                        }
                        if net_hosting && !net_peer_rtts.is_empty() {
                            ui.small(
                                net_peer_rtts
                                    .iter()
                                    .map(|(p, r)| format!("peer {p}: {r:.0} ms"))
                                    .collect::<Vec<_>>()
                                    .join(" · "),
                            )
                            .on_hover_text(
                                "measured host↔player round trip. Probed end to end rather \
                                 than read off the transport, because through a relay the \
                                 transport only sees its own leg — it would report host↔relay \
                                 and call it the player's ping.",
                            );
                        }
                        // ---- interest management, when the host turned it on ----
                        if let Some((cfg, stats)) = &net_interest {
                            ui.separator();
                            ui.label(format!(
                                "👁 interest · {:.0} m radius · {} KB/s per client{}",
                                cfg.radius,
                                cfg.budget_bytes_per_sec / 1024,
                                if cfg.occlusion { " · line of sight" } else { "" }
                            ))
                            .on_hover_text(
                                "each client is told about its own neighbourhood instead of the \
                                 whole world. Nothing is dropped for good — what doesn't fit \
                                 the budget accrues priority and goes in a later snapshot.\n\n\
                                 A radius is a BANDWIDTH boundary, not a security one: a client \
                                 told where everyone within it is standing knows where they \
                                 are, whatever it draws. Line of sight is the part that answers \
                                 that — net.host{ interestOcclusion = \"Level\" }.",
                            );
                            if stats.is_empty() {
                                ui.small("no clients yet — nothing to build a relevant set from");
                            }
                            for (peer, st) in stats {
                                let line = format!(
                                    "peer {peer}: {} of {} sent · {} B{}{}",
                                    st.sent,
                                    st.relevant,
                                    st.bytes,
                                    if st.deferred > 0 {
                                        format!(" · {} waiting", st.deferred)
                                    } else {
                                        String::new()
                                    },
                                    // What this client is not being told, and
                                    // by which rule. "Is my filter working"
                                    // has to be a number, or a project turns
                                    // one on and cannot tell it from a typo.
                                    if st.withheld() > 0 {
                                        format!(
                                            " · withheld {} ({} far{}{})",
                                            st.withheld(),
                                            st.withheld_radius,
                                            if st.withheld_occluded > 0 {
                                                format!(", {} unseen", st.withheld_occluded)
                                            } else {
                                                String::new()
                                            },
                                            if st.withheld_filter > 0 {
                                                format!(", {} by the game", st.withheld_filter)
                                            } else {
                                                String::new()
                                            },
                                        )
                                    } else {
                                        String::new()
                                    }
                                );
                                // A backlog that never clears is the one shape
                                // worth colouring: it means the budget cannot
                                // keep up with the scene, and distant things
                                // will visibly lag rather than merely update
                                // less often.
                                if st.deferred > st.sent && st.sent > 0 {
                                    ui.colored_label(egui::Color32::from_rgb(255, 170, 60), line)
                                        .on_hover_text(
                                            "more entities are waiting for a turn than got one. \
                                             They are not lost — they accrue priority — but if \
                                             this stays high, raise interestBudget or lower the \
                                             radius.",
                                        );
                                } else {
                                    ui.small(line).on_hover_text(
                                        "relevant = what this client may hear about at all; \
                                         sent = what fit in the last snapshot's budget; \
                                         withheld = replicable nodes it was told nothing \
                                         about, split by which rule decided — out of range, \
                                         out of sight, or net.setRelevant.",
                                    );
                                }
                            }
                        }
                        // ---- voice chat, when anything is speaking or listening ----
                        if let Some((rows, mic)) = &net_voice {
                            ui.separator();
                            ui.label(format!("🎤 voice · {mic}")).on_hover_text(
                                "the local microphone. The level meter is live whether or not \
                                 transmit is on, so a settings screen can prove the mic works \
                                 without joining a lobby.",
                            );
                            if rows.is_empty() {
                                ui.small("nobody else is speaking");
                            }
                            // The harness microphone. Voice normally needs two
                            // machines and two people to try at all; this makes
                            // a WAV stand in for the far end, through the real
                            // forwarding rules.
                            ui.horizontal(|ui| {
                                match voice_test_peer {
                                    Some(p) => {
                                        if ui.small_button("⏹ stop test voice").clicked() {
                                            voice_test_stop = true;
                                        }
                                        ui.small(format!("speaking as peer {p}"));
                                    }
                                    None => {
                                        if ui
                                            .small_button("🎤 test voice from a WAV…")
                                            .on_hover_text(
                                                "play an audio file in as though a remote \
                                                 player were speaking it — through the real \
                                                 forwarding rules, jitter buffer and spatial \
                                                 voice. Proves the routing without a second \
                                                 machine or a microphone.",
                                            )
                                            .clicked()
                                        {
                                            voice_test_pick = true;
                                        }
                                    }
                                }
                            });
                            for (peer, buffered, cushion, concealed, late) in rows {
                                let line = format!(
                                    "peer {peer}: {buffered:.0} ms buffered (target {cushion:.0})\
                                     {}{}",
                                    if *concealed > 0 {
                                        format!(" · {concealed} concealed")
                                    } else {
                                        String::new()
                                    },
                                    if *late > 0 { format!(" · {late} late") } else { String::new() }
                                );
                                // A cushion pinned at its ceiling means the link
                                // is the problem, and it is the one shape worth
                                // colouring: the voice still works, it is just
                                // permanently 60 ms behind and will stay there.
                                if *cushion >= 60.0 {
                                    ui.colored_label(egui::Color32::from_rgb(255, 170, 60), line)
                                        .on_hover_text(
                                            "the jitter buffer is as wide as it goes. Packets \
                                             keep arriving late, so this speaker is held at the \
                                             maximum delay to stop them dropping out.",
                                        );
                                } else {
                                    ui.small(line).on_hover_text(
                                        "buffered = audio waiting to play; target = the cushion \
                                         the jitter buffer is holding, which widens on lateness \
                                         and shrinks again when the link settles. concealed = \
                                         gaps Opus filled in for packets that never came.",
                                    );
                                }
                            }
                        }
                        if let Some(rb) = net_rollback.as_ref() {
                            ui.separator();
                            if rb.stalled {
                                ui.colored_label(
                                    egui::Color32::from_rgb(255, 170, 60),
                                    "⚔ ROLLBACK · waiting for input",
                                )
                                .on_hover_text(
                                    "past the depth cap the sim waits instead of guessing \
                                     further: the game runs slightly slow rather than \
                                     teleporting the opponent. It catches up on its own.",
                                );
                            } else {
                                // Delay and mispredict rate on one line, because
                                // neither means anything alone: a rollback
                                // implementation working perfectly and one badly
                                // misconfigured look identical from outside, and
                                // "delay 2 — 99% guessed" is the whole diagnosis
                                // (floptle/0049).
                                let line = format!(
                                    "⚔ ROLLBACK · {} fighter(s) · delay {} · {:.0}% guessed",
                                    rb.fighters,
                                    rb.input_delay,
                                    rb.mispredict_rate * 100.0,
                                );
                                // Only once there is enough of a match to judge:
                                // the opening ticks always guess.
                                let bad = rb.mispredict_rate > 0.5 && rb.current > 120;
                                if bad {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(255, 170, 60),
                                        line,
                                    )
                                    .on_hover_text(format!(
                                        "almost every tick is being guessed and re-simulated. \
                                         Nothing is broken — the fight is identical on both \
                                         machines — but this peer is doing several times the \
                                         work and it feels like it. The delay is too low for \
                                         this link: raise it between matches with \
                                         net.setInputDelay(n) (max {}), or set \
                                         net.host{{ inputDelay = n }}.",
                                        floptle_net::MAX_DELAY
                                    ));
                                } else {
                                    ui.label(line);
                                }
                            }
                            ui.small(format!(
                                "corrections {} · depth last {} / max {} / avg {:.1} · \
                                 ring {} ticks / {} KB",
                                rb.corrections,
                                rb.last_depth,
                                rb.max_depth_seen,
                                rb.average_depth,
                                rb.ring_ticks,
                                rb.ring_bytes / 1024,
                            ))
                            .on_hover_text(
                                "the delay is FIXED for the session — it never changes \
                                 mid-match, because how the game feels must not. These \
                                 numbers are the measurement you choose it from: a healthy \
                                 match sits at low average depth.",
                            );
                            // who is starved, and on what. A frozen match used
                            // to look identical from both screens; this names
                            // the side that stopped keeping up (floptle/0039).
                            ui.small(format!(
                                "frontier · confirmed {} of {} simulated ({} ahead)",
                                rb.confirmed,
                                rb.current,
                                rb.current.saturating_sub(rb.confirmed),
                            ))
                            .on_hover_text(
                                "\"confirmed\" is the newest tick every peer's REAL input is \
                                 known for. Everything past it was simulated from a guess and \
                                 can still be corrected. When the gap reaches the depth cap \
                                 the sim stalls — so a gap pinned at the cap means someone's \
                                 input has stopped arriving.",
                            );
                            for (peer, frontier, backlog) in &rb.peers {
                                let who = if *peer == floptle_net::SERVER {
                                    "host".to_string()
                                } else {
                                    format!("peer {peer}")
                                };
                                // A backlog past the fan-out window is a peer
                                // that has stopped confirming — the shape of a
                                // starved or departed player, not of a slow one.
                                let stuck = *backlog > 24;
                                let line =
                                    format!("   {who} · frontier {frontier} · {backlog} tick(s) held");
                                if stuck {
                                    ui.colored_label(egui::Color32::from_rgb(255, 170, 60), line)
                                        .on_hover_text(
                                            "this peer has stopped confirming ticks: the host \
                                             is holding its inputs and re-sending them, and \
                                             will keep doing so until they land. If it stays \
                                             here, that peer is the one that fell out of the \
                                             match.",
                                        );
                                } else {
                                    ui.small(line);
                                }
                            }
                            // Checksum status. "Never checked" and "checked and
                            // agreeing" are very different states to be in.
                            if rb.desynced {
                                ui.colored_label(
                                    egui::Color32::from_rgb(255, 90, 90),
                                    "⚠ DESYNCED — the peers no longer agree",
                                )
                                .on_hover_text(
                                    "from the reported tick on, the two machines are playing \
                                     different matches. The Console names the tick. Usual \
                                     causes: a gameplay value outside snapshot()/restore(), \
                                     an unseeded rng() (use net.random()), or reading node.x \
                                     inside fixedUpdate instead of node.tickPos.",
                                );
                            } else if rb.checksum_tick > 0 {
                                ui.small(format!(
                                    "✔ checksums agree through tick {}",
                                    rb.checksum_tick
                                ));
                            } else {
                                ui.small("checksums: none due yet (every 30 confirmed ticks)");
                            }
                            if let Some(rf) = referee {
                                ui.small(format!(
                                    "⚖ referee at tick {} ({} behind)",
                                    rf.0,
                                    rf.1.saturating_sub(rf.0)
                                ))
                                .on_hover_text(
                                    "a second simulation of this match on the host, advanced \
                                     only to ticks every peer's input has actually arrived \
                                     for. It never guesses and never rolls back, so it is \
                                     never wrong — only behind. Every peer's checksum is \
                                     judged against it, which is the difference between \
                                     \"someone is out of sync\" and \"THAT machine is\".",
                                );
                            }
                            ui.separator();
                        }
                        // Replays. A match's inputs and its seed are the match,
                        // so a replay is kilobytes and playing it back is
                        // re-simulation rather than re-enactment.
                        if !replays.is_empty() {
                            ui.small("🎞 replays");
                            for (name, path) in &replays {
                                if ui
                                    .button(name.as_str())
                                    .on_hover_text(
                                        "re-simulate this match in a headless second world. \
                                         Enter Play on its scene first — a replay is the match \
                                         run again, so it needs the world it was played in.",
                                    )
                                    .clicked()
                                {
                                    cmd.net_play_replay = Some(path.clone());
                                }
                            }
                            ui.separator();
                        }
                        // Dev-only rehearsal knob. The section only exists at
                        // all when FLOPTLE_NET_IMPAIR was set on the command
                        // line, so it cannot appear in front of someone who did
                        // not ask for it — the whole point is that a real
                        // session can never be silently degraded from the UI.
                        if let Some(knob) = Editor::net_impair() {
                            let mut imp = knob.get();
                            let before = imp;
                            let hot = imp.is_active();
                            ui.colored_label(
                                if hot {
                                    egui::Color32::from_rgb(255, 170, 60)
                                } else {
                                    egui::Color32::GRAY
                                },
                                "⚠ LINK IMPAIRMENT (dev build)",
                            )
                            .on_hover_text(
                                "adds latency and loss to THIS build's real transports (QUIC \
                                 and the relay) so a rollback match can be rehearsed at match \
                                 conditions between two instances on one desk. It is not a \
                                 network emulator — no jitter, no reordering — and it is not \
                                 a substitute for the two-machine acceptance run.",
                            );
                            let rtt = imp.rtt_ms();
                            ui.add(
                                egui::Slider::new(&mut imp.latency_ms, 0..=250)
                                    .text(format!("one-way ms  (≈{rtt} ms RTT)")),
                            );
                            let mut loss_pct = imp.loss * 100.0;
                            if ui
                                .add(egui::Slider::new(&mut loss_pct, 0.0..=25.0).text("% loss"))
                                .changed()
                            {
                                imp.loss = loss_pct / 100.0;
                            }
                            if hot && ui.button("off").clicked() {
                                imp = floptle_net::Impairment::default();
                            }
                            if imp != before {
                                knob.set(imp);
                            }
                            ui.small(
                                "reliable traffic is never dropped — a real reliable channel \
                                 retransmits, so dropping handshakes would only invent \
                                 failures the field can't produce.",
                            );
                            ui.separator();
                        }
                        if net_as_player {
                            ui.label(format!(
                                "🎮 you are a REMOTE PLAYER · rtt {net_rtt:.0} ms"
                            ));
                            match &net_predicted_name {
                                Some(n) => ui.small(format!(
                                    "predicting \"{n}\" locally — orange ghosts = the hidden server's truth. Raise latency/loss and feel it stay responsive."
                                )),
                                None => ui.small(
                                    "spectating (no Predicted node) — give your character a Networked component with mode 'Predicted (owner)'",
                                ),
                            };
                            if let Some((corr, conf, last)) = net_pred_stats {
                                let total = corr + conf;
                                let pct = if total > 0 {
                                    100.0 * corr as f64 / total as f64
                                } else {
                                    0.0
                                };
                                ui.small(format!(
                                    "reconciles: {conf} confirmed · {corr} corrected ({pct:.0}%) · last error {:.0} mm · late inputs {net_late_inputs}",
                                    last * 1000.0
                                ))
                                .on_hover_text("healthy prediction: corrections near 0%, late inputs near 0 (a brief burst right after dragging the latency slider is normal — the server pauses to refill the input pipeline). Constant growth = the sims disagree — report it");
                            }
                        } else {
                            match (net_hosting, net_has_client) {
                                (false, _) => {
                                    // The simulated-link harness is an editor
                                    // dev tool — a build's menu is just the
                                    // real hosting/joining flows.
                                    if !player_mode {
                                        ui.label("Test alone (simulated link)");
                                        if ui.button("⏵ Host + join a local client").clicked() {
                                            cmd.net_host_local = true;
                                            cmd.net_join_local = true;
                                        }
                                        if ui
                                            .button("🎮 Test as remote player (predicted)")
                                            .on_hover_text("the play world becomes a CLIENT predicting against a hidden authoritative server — your character stays responsive at any latency, the server keeps the truth")
                                            .clicked()
                                        {
                                            cmd.net_play_as_client = true;
                                        }
                                        ui.separator();
                                    }
                                    ui.label(if player_mode {
                                        "Host — friends join with a lobby code"
                                    } else {
                                        "Real network — via relay (lobby codes)"
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("relay");
                                        ui.add(
                                            egui::TextEdit::singleline(net_relay_addr)
                                                .desired_width(150.0)
                                                .hint_text("relay host:port"),
                                        );
                                    });
                                    ui.horizontal(|ui| {
                                        if ui
                                            .button("⏵ Host — get a lobby code")
                                            .on_hover_text("registers a lobby on the relay above and shows a five-letter CODE for friends. Nobody port-forwards; run `floptle-relay` anywhere both machines can reach.")
                                            .clicked()
                                        {
                                            cmd.net_host_relay = Some(net_relay_addr.clone());
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("code");
                                        let r = ui.add(
                                            egui::TextEdit::singleline(net_join_code)
                                                .desired_width(70.0)
                                                .hint_text("ABCDE"),
                                        );
                                        if r.changed() {
                                            *net_join_code = net_join_code.to_uppercase();
                                        }
                                        let ok = !net_join_code.trim().is_empty();
                                        if ui
                                            .add_enabled(ok, egui::Button::new("⏵ Join by code"))
                                            .on_hover_text("joins the lobby with this code, through the relay above")
                                            .clicked()
                                        {
                                            cmd.net_join_quic = Some(format!(
                                                "relay://{}/{}",
                                                net_relay_addr.trim(),
                                                net_join_code.trim()
                                            ));
                                        }
                                    });
                                    ui.separator();
                                    ui.label("Real network — direct (LAN / self-host)");
                                    ui.horizontal(|ui| {
                                        ui.label("port");
                                        ui.add(
                                            egui::TextEdit::singleline(net_host_port)
                                                .desired_width(60.0),
                                        );
                                        if ui.button("⏵ Host on LAN").clicked() {
                                            cmd.net_host_quic =
                                                Some(net_host_port.trim().parse().unwrap_or(7777));
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(net_join_addr)
                                                .desired_width(170.0)
                                                .hint_text("quic://ip:port"),
                                        );
                                        if ui.button("⏵ Join").clicked() {
                                            cmd.net_join_quic = Some(net_join_addr.clone());
                                        }
                                    });
                                    ui.small(
                                        "both machines run THIS project. Player slots = the \
                                         scene's Predicted nodes in order (#1 the host, #2+ \
                                         joiners) — or spawn one per joiner (player_spawner.lua). \
                                         Scripts: net.host{relay=\"…\"} / net.join(\"relay://…/CODE\")",
                                    );
                                }
                                (true, false) if !net_is_real => {
                                    ui.label("hosting · 0 ghost clients");
                                    if ui.button("➕ Join a local ghost client").clicked() {
                                        cmd.net_join_local = true;
                                    }
                                }
                                _ => {
                                    ui.label(format!(
                                        "hosting · {net_peer_count} client(s) connected"
                                    ));
                                    if let Some(code) = &net_lobby_code {
                                        ui.horizontal(|ui| {
                                            ui.label("lobby code:");
                                            ui.add(egui::Label::new(
                                                egui::RichText::new(code).strong().monospace(),
                                            ).selectable(true));
                                            if ui.small_button("copy").clicked() {
                                                ui.ctx().copy_text(code.clone());
                                            }
                                        });
                                    }
                                    if net_is_real && net_peer_count > 0 {
                                        ui.small(format!("late inputs {net_late_inputs} — near zero is healthy"));
                                    }
                                }
                            }
                        }
                        if net_hosting || net_as_player {
                            ui.separator();
                            if net_is_real {
                                ui.label("real link (QUIC)");
                                ui.small("latency and loss are whatever the network gives you — the sliders only shape the simulated harness");
                            } else {
                                ui.label("simulated link");
                                let mut lat = *net_latency_ticks as i32;
                                if ui
                                    .add(egui::Slider::new(&mut lat, 0..=30).text("latency (ticks)"))
                                    .on_hover_text("one-way, in gameplay ticks — 6 ticks ≈ 100 ms round trip")
                                    .changed()
                                {
                                    *net_latency_ticks = lat as u64;
                                }
                                ui.add(
                                    egui::Slider::new(net_loss, 0.0..=0.9)
                                        .text("packet loss")
                                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                                );
                                ui.checkbox(net_ghosts, "show client ghosts (cyan)")
                                    .on_hover_text("where the ghost client believes every networked node is — the gap to the real object is the interp delay");
                            }
                            ui.separator();
                            if ui.button("⏹ End session").clicked() {
                                cmd.net_stop_session = true;
                            }
                        }
                    });
                if !open {
                    *show_net_panel = false;
                }
            }

            // ---- net-stats overlay: one compact line while a session runs, so
            // connection health is visible without the 🌐 panel open ----
            if playing && (net_hosting || net_as_player) {
                egui::Area::new(egui::Id::new("net_stats_overlay"))
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::RIGHT_TOP, [-10.0, 40.0])
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            let kind = if net_is_real { "net" } else { "sim" };
                            let mut line = if net_as_player {
                                let timing = net_input_ack
                                    .map(|(margin, late)| {
                                        format!(" · input margin {margin:+} · late in {late}")
                                    })
                                    .unwrap_or_default();
                                format!("🌐 client ({kind}) · rtt {net_rtt:.0} ms{timing}")
                            } else {
                                format!(
                                    "🌐 host ({kind}) · {net_peer_count} peer(s) · late in {net_late_inputs}"
                                )
                            };
                            if let Some((corr, conf, last)) = net_pred_stats {
                                let total = corr + conf;
                                let clean =
                                    if total > 0 { 100.0 * conf as f64 / total as f64 } else { 100.0 };
                                line.push_str(&format!(
                                    " · predict {clean:.0}% clean · err {:.0} mm",
                                    last * 1000.0
                                ));
                            }
                            ui.small(line);
                        });
                    });
            }

            // ---- player-mode hint: the only chrome a build shows, and only
            // for the first seconds (until the UI system gives games real menus) ----
            if player_mode && play_t < 8.0 && !(net_hosting || net_as_player) {
                egui::Area::new(egui::Id::new("player_hint"))
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -14.0])
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.small("F1 — multiplayer");
                        });
                    });
            }

            // The 🌐 panel's test-voice buttons, acted on out here where the
            // borrow of the panel's destructured fields has ended.
            if voice_test_stop {
                self.voice.stop_test_speaker();
            }
            if voice_test_pick && self.voice_test_pick.is_none() {
                self.voice_test_pick = Some(crate::native_dialog::pick_files_filtered(
                    "Play a WAV as a remote player's microphone",
                    Some(("audio", &floptle_audio::AUDIO_EXTENSIONS
                        .iter()
                        .map(|e| (*e).to_string())
                        .collect::<Vec<_>>())),
                    false,
                ));
            }

            // ---- dockable panels: Hierarchy / Inspector / Assets / Scene + Scripting ----
            // The Scene tab is transparent so the 3D render shows through; the others
            // paint opaque over it. Users can drag/re-dock/tab these freely.
            //
            // Clear the Scene rect first: egui_dock only runs the ACTIVE tab's `ui`,
            // so if Scene is tabbed behind Scripting, scene_ui never runs and the rect
            // would otherwise stay pinned to the old viewport region — letting clicks,
            // context-menus and model-drops fall through onto whatever panel now
            // occupies that space. `scene_ui` re-arms it only on frames it draws.
            *scene_rect = None;
            let mut viewer = EditorTabViewer {
                world,
                selection,
                selection_locked,
                maps,
                map_sel,
                map_mode,
                map_slot_name,
                map_viz,
                tile_viz,
                map_opts,
                tiles: &mut self.tiles,
                tile_tools: &mut self.tile_tools,
                map_size_buf,
                map_spec_buf,
                map_arm,
                map_knife_on,
                map_orient,
                map_xform,
                map_select_hidden,
                map_bevel,
                map_tool_on,
                map_playing,
                light_counts: self.light_counts,
                map_hud_open,
                map_keys,
                map_rebind,
                map_rebind_err,
                gizmo_tool,
                ui_overlay: &ui_overlay_snapshot,
                ui_canvas: &ui_canvas_snapshot,
                ref_kinds,
                script_meta,
                bone_selection,
                pivot_edit,
                fullscreen_tab,
                focused_tab,
                hier_search,
                hier_scope,
                collapsed,
                hier_fold_pending,
                bone_names: &bone_names,
                console,
                preview: preview_view.clone(),
                preview_zoom,
                preview_spin,
                preview_spinning,
                preview_material,
                map_asset_preview,
                entity_names: &entity_names,
                gi: gi_status,
                nav: nav_status.clone(),
                materials,
                mat_name_buf,
                flsl_cache: &self.flsl_cache,
                ui_flsl_cache: &self.ui_flsl_cache,
                post_flsl_cache: &self.post_flsl_cache,
                ui_styles: &self.ui_styles,
                ui_tokens: &self.ui_tokens,
                ui_design,
                sdf_cache: &self.sdf_cache,
                sky_uniforms: self.sky_shader.as_ref().map_or(&[], |(_, _, u)| u.as_slice()),
                component_clip,
                add_component_filter,
                layer_names: &layer_names,
                sorting_names: &sorting_names,
                tag_edit,
                hier_scrolled,
                show_material_editor,
                asset_tree,
                texture_settings,
                cam_preview,
                has_active_camera,
                vertex_brush,
                terrain_brush,
                terrain_voxel,
                terrain_textures,
                terrain_glow,
                terrain_tex_scale,
                terrain_present,
                terrain_stats,
                assets_grid,
                assets_grid_dir,
                project_root,
                selected_asset,
                asset_selection,
                ide,
                learn,
                script_errors,
                ide_diag,
                gizmo,
                terrain_viz,
                paint_viz,
                camera_gizmos,
                light_gizmos,
                volume_gizmos,
                rig_gizmos,
                gi_probe_dots,
                body_gizmos,
                contact_gizmos,
                script_gizmo_lines,
                ext: ext_host,
                ext_painted,
                game_gizmo_lines,
                game_gizmos,
                terrain_wire,
                nav_wire,
                mesh_wire,
                particle_gizmo,
                show_gizmos,
                panels,
                view_lock: &mut view_lock,
                view_ortho: &mut view_ortho,
                gizmo_filter,
                grabbed,
                tool,
                scene_rect: &mut *scene_rect,
                game_rect,
                game_offscreen,
                game_tex,
                aspect: aspect_mode,
                zoom: viewport_zoom,
                scene_name: &scene_name,
                editing_prefab: self.editing_prefab.is_some(),
                ppp,
                code_theme,
                anim: anim_sys,
                vfx: vfx_sys,
                vfx_ui: vfx_ui_state,
                audio: audio_sys,
                mixer_ui: mixer_ui_state,
                project,
                particles_active,
                anim_ui: anim_ui_state,
                shader_graph: shader_graph_state,
                image: image_state,
                image_parked: &image_parked,
                shader_preview: shader_preview_state,
                mesh_registry,
                pointer_down,
                playing,
                player_mode,
                settings: crate::settings_ui::SettingsCtx {
                    scene_files: &settings_scene_files,
                    layer_new,
                    section: settings_section,
                    search: settings_search,
                    input_map: &settings_input_map,
                    input_pending: settings_input_pending.as_ref(),
                    input_scan,
                    input_test: input_test_state,
                    pad_names: &settings_pad_names,
                    input_new_action,
                    access,
                },
                packages: packages_state,
                packages_ctx: crate::packages_ui::PkgCtx {
                    project_root: &ext_project_root,
                    load: &pkg_load,
                    account: ext_account,
                },
                packages_action: &mut pkg_action,
                cmd: &mut cmd,
            };
            // Fullscreen: one tab maximized over the whole window (double-click a tab to
            // toggle). A slim header lets you restore (or press Esc); the dock layout is
            // untouched underneath and comes back exactly as it was.
            if let Some(ft) = *viewer.fullscreen_tab {
                let mut exit = false;
                // A build has nothing to restore to — no header, and Escape
                // belongs to the game (cursor release), not the layout.
                if !player_mode {
                    // A PANEL, not a bare `ui.horizontal`. A plain row paints no
                    // background of its own, so the strip it occupied stayed
                    // transparent and the 3D surface render showed through it —
                    // a band of scene along the top edge of every maximized tab,
                    // whichever tab it was. A panel fills itself, the same way
                    // the menu bar above it always has.
                    egui::Panel::top("fullscreen_header").show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .button(format!("⛶ Restore  ·  {}", ft.title()))
                                .on_hover_text(
                                    "double-click a tab to toggle fullscreen · Esc to restore",
                                )
                                .clicked()
                            {
                                exit = true;
                            }
                            ui.small("double-click a tab or press Esc to restore");
                        });
                    });
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        exit = true;
                    }
                }
                // Scene/Game are transparent (the 3D shows through); every other tab
                // needs an opaque fill so the surface render doesn't bleed behind it.
                // Everything from here down belongs to the tab — take the whole
                // remaining rect rather than letting a stray margin leave a seam.
                let body = ui.available_rect_before_wrap();
                if !matches!(ft, EditorTab::Scene | EditorTab::Game) {
                    let bg = ui.style().visuals.panel_fill;
                    ui.painter().rect_filled(body, 0.0, bg);
                }
                let mut t = ft;
                let mut body_ui = ui.new_child(
                    egui::UiBuilder::new().max_rect(body).layout(*ui.layout()),
                );
                egui_dock::TabViewer::ui(&mut viewer, &mut body_ui, &mut t);
                if exit {
                    *viewer.fullscreen_tab = None;
                }
            } else {
                egui_dock::DockArea::new(dock_state)
                    .style(egui_dock::Style::from_egui(ui.style()))
                    .show_inside(ui, &mut viewer);
            }

            if *viewer.game_gizmos != game_gizmos_before {
                crate::prefs::save_game_gizmos(*viewer.game_gizmos);
            }
            // Where the Scene view's floating panels ended up. Compared against
            // what is on disk rather than written on every change, because a
            // drag is a change per frame and that would be a file write per
            // frame of it.
            let panels_now = *viewer.panels;
            if panels_now != *panels_saved {
                crate::prefs::save_viewport_panels(&panels_now);
                *panels_saved = panels_now;
            }
            // The Scene view's plane lock, chosen in the viewport toolbar.
            // `set_lock` snaps the camera square without moving it.
            // Read both out in one go: each is a `&mut` into the local below, so
            // the borrow has to be finished with before either can be written.
            let (chosen_lock, chosen_ortho) = (*viewer.view_lock, *viewer.view_ortho);
            view_lock = chosen_lock;
            view_ortho = chosen_ortho;

            // ---- the packages' own panels ----
            // Floating windows, like every other tool window here: they can be
            // moved and resized, they remember where they were put, and a
            // package cannot take a docked slot away from the editor's own
            // panels. Drawn after the dock, so a panel is over the viewport it
            // is about.
            for i in 0..ext_host.windows.len() {
                if !ext_host.windows[i].open {
                    continue;
                }
                let title = ext_host.windows[i].title.clone();
                let id = ext_host.windows[i].id;
                let mut open = true;
                let win = egui::Window::new(&title)
                    .id(egui::Id::new(("ext_window", id)))
                    .open(&mut open)
                    .default_width(320.0)
                    .resizable(true);
                // `ed.window(...):focus()` brings the panel to the front. It
                // does not move it: a window that jumps to the middle of the
                // screen because a script mentioned it is a window somebody has
                // to put back.
                if ext_focus_window == Some(i) {
                    ui.ctx().move_to_top(egui::LayerId::new(
                        egui::Order::Middle,
                        egui::Id::new(("ext_window", id)),
                    ));
                }
                win.show(ui, |ui| ext_host.draw_window(i, ui));
                if !open {
                    ext_host.set_window_open(i, false);
                }
            }

            // 📦 Packages is a dock tab now, drawn with the other tabs — see
            // `EditorTab::Packages`. Nothing to draw here.

            // ---- what a package's `ed.message` asked to say ----
            if let Some((title, body)) = ext_message.clone() {
                let mut open = true;
                let mut dismissed = false;
                egui::Window::new(&title)
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ui, |ui| {
                        ui.label(&body);
                        dismissed = ui.button("OK").clicked();
                    });
                if !open || dismissed {
                    *ext_message = None;
                }
            }

            // ---- a package's keyboard shortcut ----
            // Read here rather than in the editor's own key handling so an
            // extension cannot fire while a text field has the keyboard.
            if !ext_host.shortcuts.is_empty() && !ui.ctx().egui_wants_keyboard_input() {
                let pressed = crate::ext_wire::pressed_shortcut(ui.ctx());
                if let Some(p) = pressed {
                    ext_shortcut_click = ext_host.shortcuts.iter().position(|s| s.keys == p);
                }
            }

            // Viewport drop: spawn a model when an asset is released over the Scene
            // tab (panel drops — script-on-node — are consumed by those tabs first).
            // No opaque region is allocated, so the viewport never greys mid-drag.
            if egui::DragAndDrop::has_payload_of_type::<AssetPayload>(ui.ctx())
                && ui.input(|i| i.pointer.any_released())
            {
                let pos = ui.input(|i| i.pointer.interact_pos());
                let over_scene = matches!((pos, *scene_rect), (Some(p), Some(r)) if r.contains(p));
                if over_scene
                    && let Some(p) = egui::DragAndDrop::take_payload::<AssetPayload>(ui.ctx()) {
                        cmd.drop_asset = Some(p.path.clone());
                    }
            }

            // ---- Export Game… (File menu): binary + assets + manifest ----
            if *show_export {
                let mut open = true;
                egui::Window::new("📦 Export Game")
                    .open(&mut open)
                    .resizable(false)
                    .default_width(340.0)
                    .show(ui, |ui| {
                        ui.label(
                            "A build = this engine binary + the project folder. It runs \
                             the game directly (no editor) — F1 in-game opens the \
                             multiplayer menu.",
                        );
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label("Title");
                            ui.text_edit_singleline(export_title);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Folder");
                            ui.text_edit_singleline(export_dir)
                                .on_hover_text("the build lands here (created if missing)");
                        });
                        // Exactly where that lands — no guessing at relative paths.
                        let resolved = {
                            let t = export_dir.trim();
                            let p = std::path::Path::new(t);
                            if p.is_absolute() { p.to_path_buf() } else { export_base.join(p) }
                        };
                        ui.small(format!("→  {}", resolved.display()));
                        ui.horizontal(|ui| {
                            ui.label("Target");
                            egui::ComboBox::from_id_salt("export_target")
                                .selected_text(EXPORT_TARGETS[*export_target].label)
                                .show_ui(ui, |ui| {
                                    for (i, t) in EXPORT_TARGETS.iter().enumerate() {
                                        ui.selectable_value(export_target, i, t.label);
                                    }
                                });
                        });
                        ui.small(
                            "any target exports from any machine: the engine binary for \
                             that platform is downloaded once (matched to this engine \
                             version, checksum-verified) and reused after that. No \
                             compiler or toolchain needed.",
                        );
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let can = !export_building && !export_dir.trim().is_empty();
                            if ui.add_enabled(can, egui::Button::new("📦 Export")).clicked() {
                                cmd.export_game =
                                    Some((export_dir.trim().to_string(), *export_target));
                            }
                            if export_building {
                                ui.spinner();
                            }
                        });
                        if let Some(status) = export_status {
                            ui.add_space(4.0);
                            ui.label(status.as_str());
                        }
                        if let Some(done) = &export_done
                            && ui.button("📂 Open build folder").clicked()
                        {
                            cmd.open_folder = Some(done.clone());
                        }
                    });
                if !open {
                    *show_export = false;
                }
            }

            // ---- last run crashed ----
            if let Some(note) = &crash_prompt {
                let first = note.lines().find(|l| l.starts_with("panic:")).unwrap_or("").to_string();
                egui::Window::new("⚠ Floptle crashed last time")
                    .resizable(false)
                    .collapsible(false)
                    .default_width(460.0)
                    .show(ui.ctx(), |ui| {
                        ui.label(
                            "The previous session ended in a crash. A report was saved — \
                             sending it is the single most useful thing you can do about it.",
                        );
                        if !first.is_empty() {
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new(&first).monospace().small());
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui
                                .button("🐛 Report it")
                                .on_hover_text(
                                    "opens the issue tracker with the version, platform and \
                                     backtrace already filled in — you can read and edit it \
                                     before posting. Nothing is sent automatically.",
                                )
                                .clicked()
                            {
                                cmd.crash_report = Some(true);
                            }
                            if ui.button("Not now").clicked() {
                                cmd.crash_report = Some(false);
                            }
                        });
                    });
            }

            // ---- autosave recovery (a newer autosave than the scene file) ----
            if let Some(auto) = &autosave_prompt {
                let age = floptle_vfs::modified(auto)
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| {
                        let s = d.as_secs();
                        if s < 120 { format!("{s} s ago") } else { format!("{} min ago", s / 60) }
                    })
                    .unwrap_or_else(|| "recently".into());
                egui::Window::new("💾 Recover unsaved work?")
                    .resizable(false)
                    .collapsible(false)
                    .default_width(360.0)
                    .show(ui.ctx(), |ui| {
                        ui.label(format!(
                            "'{scene_name_now}' has an AUTOSAVE newer than its saved file (written {age}) — usually the editor closed with unsaved changes. Restore it?"
                        ));
                        ui.small("Restoring loads the autosaved version (still unsaved — Ctrl+S to keep it). Discard deletes the autosave.");
                        ui.horizontal(|ui| {
                            if ui.button("♻ Restore autosave").clicked() {
                                cmd.autosave_action = Some(true);
                            }
                            if ui.button("🗑 Discard it").clicked() {
                                cmd.autosave_action = Some(false);
                            }
                        });
                    });
            }

            // Project Settings used to be a fixed-size modal window here. It's
            // now the ⚙ Settings DOCK TAB (see `settings_ui.rs`): draggable,
            // dockable beside the viewport, searchable, and closed by default.

            // ---- preferences window (user-wide editor settings) ----
            egui::Window::new("Preferences")
                .open(show_preferences)
                .resizable(false)
                .default_width(320.0)
                .show(ui.ctx(), |ui| {
                    ui.label("External editor — \"Open in IDE\"");
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(external_editor)
                                .desired_width(150.0)
                                .hint_text("code"),
                        );
                        if ui.button("Save").clicked() {
                            cmd.set_external_editor = Some(external_editor.clone());
                        }
                    });
                    ui.small("Binary name or path (e.g. code, codium, subl). VSCode-family editors open the project folder and jump to the file. Saved as a user preference.");
                    if ui
                        .checkbox(prefer_external, "Open scripts in my external editor")
                        .on_hover_text("When on, double-clicking a script (or its Edit button, or a console line) opens it here instead of the in-engine IDE.")
                        .changed()
                    {
                        cmd.set_prefer_external = Some(*prefer_external);
                    }

                    ui.add_space(12.0);
                    ui.label("Play-mode tint");
                    ui.separator();
                    let mut tint_changed = ui
                        .checkbox(play_tint_enabled, "Tint the editor while playing")
                        .on_hover_text("Tints the editor chrome while in play mode so you never mistake it for edit mode (and lose edits on Stop).")
                        .changed();
                    ui.add_enabled_ui(*play_tint_enabled, |ui| {
                        // The stored value is an additive RGB offset, so editing it as a color
                        // reads naturally: black = no tint, brighter = a stronger nudge.
                        let mut col =
                            egui::Color32::from_rgb(play_tint[0], play_tint[1], play_tint[2]);
                        ui.horizontal(|ui| {
                            ui.label("tint amount");
                            if ui.color_edit_button_srgba(&mut col).changed() {
                                *play_tint = [col.r(), col.g(), col.b()];
                                tint_changed = true;
                            }
                        });
                        ui.small("Color added to the editor background while playing (black = no tint).");
                        if ui.button("Reset to default").clicked() {
                            *play_tint = DEFAULT_PLAY_TINT;
                            tint_changed = true;
                        }
                    });
                    if tint_changed {
                        cmd.set_play_tint = Some((*play_tint_enabled, *play_tint));
                    }

                    ui.add_space(12.0);
                    ui.label("Themes");
                    ui.separator();
                    // Engine (chrome) theme.
                    ui.horizontal(|ui| {
                        ui.label("Engine theme");
                        let cur = engine_theme.min(ENGINE_THEMES.len() - 1);
                        egui::ComboBox::from_id_salt("engine_theme_combo")
                            .selected_text(ENGINE_THEMES[cur].name)
                            .show_ui(ui, |ui| {
                                for (i, t) in ENGINE_THEMES.iter().enumerate() {
                                    if ui.selectable_label(i == cur, t.name).clicked() {
                                        cmd.set_engine_theme = Some(i);
                                    }
                                }
                            });
                    });
                    ui.small("Recolors the editor windows, panels and menus.");
                    // Code-editor theme.
                    ui.horizontal(|ui| {
                        ui.label("Editor theme");
                        let cur = code_theme.min(CODE_THEMES.len() - 1);
                        egui::ComboBox::from_id_salt("code_theme_combo")
                            .selected_text(CODE_THEMES[cur].name)
                            .show_ui(ui, |ui| {
                                for (i, t) in CODE_THEMES.iter().enumerate() {
                                    if ui.selectable_label(i == cur, t.name).clicked() {
                                        cmd.set_code_theme = Some(i);
                                    }
                                }
                            });
                    });
                    ui.small("Syntax colors + background of the in-engine script editor.");
                });

            // ---- frame timing window ----
            //
            // **The number that answers "why is this slow".** Wall-clock frame
            // time says a frame was slow; this says which pass was, which is the
            // only version of the question anybody can act on. Measured with GPU
            // timestamps, so it is time the card spent rather than time the CPU
            // spent asking — those differ by orders of magnitude and it is
            // routinely the second one that looks fine.
            egui::Window::new("⏱ Frame timing")
                .open(show_gpu_timing)
                .resizable(false)
                .default_width(300.0)
                .show(ui.ctx(), |ui| {
                    if !gpu_timing_supported {
                        ui.label("This GPU does not offer timestamp queries.");
                        ui.small(
                            "Nothing can be measured per pass here. The frame cost in the title \
                             bar still applies.",
                        );
                        return;
                    }
                    if gpu_spans.is_empty() {
                        ui.label("measuring…");
                        ui.small("a frame's timings arrive a frame or two after it is drawn");
                        return;
                    }
                    ui.horizontal(|ui| {
                        ui.strong(format!("{gpu_total:.2} ms"));
                        ui.small("on the GPU, this frame");
                    });
                    ui.separator();
                    let worst = gpu_spans.iter().map(|s| s.ms).fold(0.0f32, f32::max).max(1e-4);
                    for s in &gpu_spans {
                        ui.horizontal(|ui| {
                            // A bar, because the ordering is the point: the eye
                            // finds the longest one without reading six numbers.
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(96.0, 12.0),
                                egui::Sense::hover(),
                            );
                            let vis = ui.visuals();
                            ui.painter().rect_filled(rect, 2.0, vis.extreme_bg_color);
                            let w = (s.ms / worst).clamp(0.0, 1.0) * rect.width();
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(rect.min, egui::vec2(w, rect.height())),
                                2.0,
                                vis.selection.bg_fill,
                            );
                            ui.label(format!("{:>6.2} ms", s.ms));
                            ui.label(&s.label);
                        });
                    }
                    ui.separator();
                    ui.small(
                        "GPU time per pass. A frame the display is pacing shows a small total \
                         here and a large one in the title bar — that is the display waiting, \
                         not the scene costing.",
                    );
                });

            // ---- grid settings window ----
            egui::Window::new("Grid Settings")
                .open(show_grid_settings)
                .resizable(false)
                .default_width(240.0)
                .show(ui.ctx(), |ui| {
                    let mut changed = false;
                    changed |= ui.checkbox(&mut grid.show, "show grid").changed();
                    changed |= ui.checkbox(&mut grid.snap, "snap objects to grid").changed();
                    changed |= ui.add(egui::Slider::new(&mut grid.size, 0.1..=10.0).text("cell size")).changed();
                    changed |= ui.add(egui::Slider::new(&mut grid.extent, 4..=120).text("extent (cells)")).changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut grid.y_offset, 0.0..=50.0)
                                .text("drop below camera")
                                .suffix(" m"),
                        )
                        .on_hover_text("How far below the camera the grid floor sits. Your value is saved between sessions.")
                        .changed();
                    changed |= ui.add(egui::Slider::new(&mut grid.alpha, 0.0..=1.0).text("opacity")).changed();
                    ui.horizontal(|ui| {
                        ui.label("color");
                        changed |= ui.color_edit_button_rgb(&mut grid.color).changed();
                    });
                    if ui.small_button("Reset to defaults").clicked() {
                        *grid = GridConfig::default();
                        changed = true;
                    }
                    // Persist the grid settings whenever a control changes (so they don't
                    // reset every launch).
                    if changed {
                        cmd.save_grid = true;
                    }
                });

            // ---- viewport context menu (RMB click on an object / empty space) ----
            if let Some((pos, hit)) = context_menu {
                egui::Area::new(egui::Id::new("ctx_menu"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(pos)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_max_width(190.0);
                            // ---- ▦ Model tool: the operations for what's selected ----
                            //
                            // Right-click is where people look for "what can I do to
                            // this?" — every one of these was previously a key you had
                            // to already know, or a button on a panel that may not even
                            // be open. Same `MapOp`s the panel emits, so there is one
                            // implementation and no stale subset.
                            if tool == Tool::MapEdit {
                                let sel = map_sel.as_ref();
                                let nv = sel.map_or(0, |s| s.verts.len());
                                let ne = sel.map_or(0, |s| s.edges.len());
                                let nf = sel.map_or(0, |s| s.faces.len());
                                let any = nv + ne + nf > 0;
                                // Chosen here, applied after the closures — `cmd` is
                                // borrowed by the outer menu.
                                let mut pick: Option<crate::map_edit::MapOp> = None;
                                let mut detach = false;
                                let mut mode_pick: Option<crate::map_edit::MapSubMode> = None;
                                ui.label(
                                    egui::RichText::new(match map_mode {
                                        crate::map_edit::MapSubMode::Vertex => format!("{nv} vertices"),
                                        crate::map_edit::MapSubMode::Edge => format!("{ne} edges"),
                                        crate::map_edit::MapSubMode::Face => format!("{nf} faces"),
                                    })
                                    .small()
                                    .weak(),
                                );
                                ui.separator();
                                let op = |ui: &mut egui::Ui,
                                              label: &str,
                                              tip: &str,
                                              on: bool,
                                              o: crate::map_edit::MapOp,
                                              pick: &mut Option<crate::map_edit::MapOp>| {
                                    if ui.add_enabled(on, egui::Button::new(label)).on_hover_text(tip).clicked() {
                                        *pick = Some(o);
                                    }
                                };
                                if map_mode == crate::map_edit::MapSubMode::Face {
                                    op(ui, "Extrude  (E)", "push the selected faces out along their average normal", nf > 0, crate::map_edit::MapOp::Extrude, &mut pick);
                                    op(ui, "Inset  (I)", "a smaller copy of each face inside itself", nf > 0, crate::map_edit::MapOp::Inset, &mut pick);
                                    op(ui, "Subdivide", "split each face into four", nf > 0, crate::map_edit::MapOp::Subdivide, &mut pick);
                                    op(ui, "Bridge", "join two face outlines with a tube", nf == 2, crate::map_edit::MapOp::Bridge, &mut pick);
                                    op(ui, "Flip", "reverse the winding — turn a face inside out", nf > 0, crate::map_edit::MapOp::FlipFaces, &mut pick);
                                    if ui.add_enabled(nf > 0, egui::Button::new("Detach")).on_hover_text("split the selected faces off into their own map node").clicked() {
                                        detach = true;
                                    }
                                    op(ui, "Delete faces  (Del)", "remove them, leaving a hole", nf > 0, crate::map_edit::MapOp::DeleteFaces, &mut pick);
                                } else {
                                    op(ui, "Weld selected", "merge vertices closer than the weld radius into one", nv > 1 || ne > 0, crate::map_edit::MapOp::WeldSelected, &mut pick);
                                    op(ui, "Snap to grid", "move the selection onto the grid", any, crate::map_edit::MapOp::SnapToGrid, &mut pick);
                                }
                                ui.separator();
                                ui.menu_button("Select", |ui| {
                                    op(ui, "All", "", true, crate::map_edit::MapOp::SelectAll, &mut pick);
                                    op(ui, "None", "", any, crate::map_edit::MapOp::SelectNone, &mut pick);
                                    op(ui, "Invert", "everything of this kind that isn't selected", true, crate::map_edit::MapOp::SelectInvert, &mut pick);
                                    ui.separator();
                                    op(ui, "Grow", "add the neighbouring ring", any, crate::map_edit::MapOp::Grow, &mut pick);
                                    op(ui, "Shrink", "drop the outermost ring", any, crate::map_edit::MapOp::Shrink, &mut pick);
                                    op(ui, "Linked", "everything connected to the selection", any, crate::map_edit::MapOp::SelectConnected, &mut pick);
                                    op(ui, "Coplanar", "faces lying in the same plane", nf > 0, crate::map_edit::MapOp::SelectCoplanar, &mut pick);
                                    op(ui, "Edge loop", "run along the quad loop", ne > 0, crate::map_edit::MapOp::SelectLoop, &mut pick);
                                    ui.separator();
                                    op(ui, "Warped faces", "faces whose corners no longer lie in one plane — the ones that look folded", true, crate::map_edit::MapOp::SelectNonPlanar, &mut pick);
                                });
                                ui.menu_button("Mode", |ui| {
                                    for m in [
                                        crate::map_edit::MapSubMode::Vertex,
                                        crate::map_edit::MapSubMode::Edge,
                                        crate::map_edit::MapSubMode::Face,
                                    ] {
                                        if ui.radio(map_mode == m, m.label()).clicked() {
                                            mode_pick = Some(m);
                                        }
                                    }
                                });
                                if let Some(o) = pick {
                                    cmd.map_op = Some(o);
                                    cmd.close_menu = true;
                                }
                                if detach {
                                    cmd.map_detach = true;
                                    cmd.close_menu = true;
                                }
                                if let Some(m) = mode_pick {
                                    cmd.set_map_mode = Some(m);
                                    cmd.close_menu = true;
                                }
                                ui.separator();
                            }
                            if hit.is_some() {
                                if ui.button("Duplicate  (Ctrl+D)").clicked() {
                                    cmd.duplicate = true;
                                    cmd.close_menu = true;
                                }
                                if ui.button("Copy  (Ctrl+C)").clicked() {
                                    cmd.copy = true;
                                    cmd.close_menu = true;
                                }
                                if ui.button("Delete  (Del)").clicked() {
                                    cmd.delete = true;
                                    cmd.close_menu = true;
                                }
                                ui.separator();
                            }
                            if ui.button("Paste  (Ctrl+V)").clicked() {
                                cmd.paste = true;
                                cmd.close_menu = true;
                            }
                            // The same node catalog as the Hierarchy's ✚ New and
                            // the menu-bar Add — one list, no stale subset.
                            ui.menu_button("Add", |ui| {
                                crate::hierarchy::node_new_menu(ui, &mut cmd, None);
                                cmd.close_menu |=
                                    cmd.add.is_some() || cmd.add_ui.is_some();
                            });
                        });
                    });
            }

            // ---- new / open project window (rfd unavailable ⏵ a text path) ----
            egui::Window::new("Project")
                .open(show_project_mgr)
                .resizable(false)
                .default_width(420.0)
                .show(ui.ctx(), |ui| {
                    ui.label("A project is a folder holding scenes/, models/, scripts/, …");
                    ui.horizontal(|ui| {
                        ui.label("path");
                        ui.add(
                            egui::TextEdit::singleline(project_path_buf)
                                .desired_width(290.0)
                                .hint_text("/path/to/project"),
                        );
                    });
                    ui.horizontal(|ui| {
                        let p = project_path_buf.trim().to_string();
                        if ui.add_enabled(!p.is_empty(), egui::Button::new("Open")).clicked() {
                            cmd.project_action = Some(ProjectAction::Open(p.clone()));
                        }
                        if ui.add_enabled(!p.is_empty(), egui::Button::new("Create New")).clicked() {
                            cmd.project_action = Some(ProjectAction::New(p));
                        }
                    });
                    ui.add_space(4.0);
                    ui.small("Open loads an existing folder; Create New scaffolds a fresh one.");
                });

            // ---- rename modal (for the asset browser) ----
            if let Some((path, buf)) = rename_target.as_mut() {
                let mut open = true;
                let mut close = false;
                // The fixed suffix = everything after the first dot, so compound
                // extensions (.prefab.ron, .vfx.ron) ride along whole. Folders
                // have no suffix.
                let ext = if floptle_vfs::is_dir(Path::new(path.as_str())) {
                    String::new()
                } else {
                    Path::new(path.as_str())
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.find('.').map(|i| n[i..].to_string()))
                        .unwrap_or_default()
                };
                egui::Window::new("Rename")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(320.0)
                    .show(ui.ctx(), |ui| {
                        ui.small(path.as_str());
                        // Edit just the base name; the extension rides along as a suffix.
                        let edit = ui
                            .horizontal(|ui| {
                                let e = ui.add(
                                    egui::TextEdit::singleline(buf)
                                        .desired_width(240.0)
                                        .hint_text("name"),
                                );
                                if !ext.is_empty() {
                                    ui.monospace(&ext);
                                }
                                e
                            })
                            .inner;
                        edit.request_focus();
                        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.horizontal(|ui| {
                            let valid = !buf.trim().is_empty();
                            if ui.add_enabled(valid, egui::Button::new("Rename")).clicked() || (enter && valid) {
                                cmd.do_rename = Some((path.clone(), buf.clone()));
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *rename_target = None;
                }
            }

            // ---- new scene modal ----
            if let Some(buf) = new_scene_buf.as_mut() {
                let mut open = true;
                let mut close = false;
                egui::Window::new("New scene")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(300.0)
                    .show(ui.ctx(), |ui| {
                        ui.label("Name your new blank scene:");
                        let edit = ui.add(
                            egui::TextEdit::singleline(buf).desired_width(260.0).hint_text("scene name"),
                        );
                        edit.request_focus();
                        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.horizontal(|ui| {
                            let valid = !buf.trim().is_empty();
                            if ui.add_enabled(valid, egui::Button::new("Create")).clicked() || (enter && valid) {
                                cmd.new_scene = Some(buf.clone());
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *new_scene_buf = None;
                }
            }

            // ---- name a new asset ----
            //
            // One modal for every "✚ New <thing>" that writes a file, so the
            // rule is the same everywhere: you name it, then it exists. The
            // words come from the kind; the mechanics (Enter to accept, Escape
            // or ✖ to cancel, empty is refused) do not vary.
            if let Some((kind, buf)) = new_asset_prompt.as_mut() {
                let (title, prompt, hint) = kind.words();
                let kind = *kind;
                let mut open = true;
                let mut close = false;
                egui::Window::new(title)
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(300.0)
                    .show(ui.ctx(), |ui| {
                        ui.label(prompt);
                        let edit = ui.add(
                            egui::TextEdit::singleline(buf).desired_width(260.0).hint_text(hint),
                        );
                        edit.request_focus();
                        let enter =
                            edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.horizontal(|ui| {
                            let valid = !buf.trim().is_empty();
                            if ui.add_enabled(valid, egui::Button::new("Create")).clicked()
                                || (enter && valid)
                            {
                                match kind {
                                    crate::NewAsset::Effect(e) => {
                                        cmd.do_new_particles = Some((e, buf.clone()));
                                    }
                                }
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *new_asset_prompt = None;
                }
            }

            // ---- quit with unsaved changes ----
            if *show_quit_confirm {
                let mut open = true;
                let mut close = false;
                egui::Window::new("Unsaved changes")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(320.0)
                    .show(ui.ctx(), |ui| {
                        match (scene_dirty_now, image_dirty_now) {
                            (true, true) => ui.label("The scene and the open image have unsaved changes."),
                            (true, false) => ui.label("The scene has unsaved changes."),
                            (false, true) => ui.label("The open image has unsaved changes."),
                            (false, false) => ui.label("Quit Floptle?"),
                        };
                        ui.horizontal(|ui| {
                            // Save & Quit: save everything, then close (the save runs after
                            // this closure, then `about_to_wait` exits — a real close, not the
                            // no-op ViewportCommand this app used to send).
                            let save_label =
                                if image_unnamed { "💾 Save…" } else { "💾 Save & Quit" };
                            if (scene_dirty_now || image_dirty_now)
                                && ui.button(save_label)
                                    .on_hover_text(if image_unnamed {
                                        "the image has never been saved — it needs a name, so this \
                                         stays open"
                                    } else {
                                        "save everything, then close"
                                    })
                                    .clicked()
                            {
                                want_save_all = true;
                                want_exit = !image_unnamed;
                                close = true;
                            }
                            // Discard: leave without saving.
                            if ui.button("Discard & Quit").clicked() {
                                want_exit = true;
                                close = true;
                            }
                            // Cancel: just dismiss — no save, no exit.
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *show_quit_confirm = false;
                }
            }

            // ---- closing an image with unsaved changes ----
            //
            // Three answers, because there are three things a person means.
            // The old code offered one — "save first" — and a document that has
            // never been named cannot be saved without a name, so that answer
            // was sometimes not available and the close simply never happened.
            // Discard is the arm that was missing, and it is the arm that turns
            // "I'm stuck editing this image" back into an ordinary decision.
            if let Some(which) = *image_close_confirm {
                let mut decided = None;
                let mut open = true;
                egui::Window::new("Close this image?")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(340.0)
                    .show(ui.ctx(), |ui| {
                        ui.label("This image has unsaved changes.");
                        ui.small(
                            "Saving writes the layered .flimg and the flat .png beside it.",
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            // Only offered for the live document: saving a parked
                            // one would mean making it live first, and a button
                            // that silently switches which image you are looking
                            // at is worse than not being there.
                            if which.is_none() && ui.button("💾  Save & close").clicked() {
                                decided = Some(1);
                            }
                            if ui
                                .button("🗑  Discard")
                                .on_hover_text("close it and lose the changes")
                                .clicked()
                            {
                                decided = Some(2);
                            }
                            if ui.button("Cancel").clicked() {
                                decided = Some(0);
                            }
                        });
                    });
                if !open {
                    decided = Some(0);
                }
                match decided {
                    Some(0) => *image_close_confirm = None,
                    Some(1) => *image_close_confirm = Some(which), // saved below, then closed
                    Some(2) => {
                        *image_close_confirm = None;
                        cmd.image_discard = Some(which);
                    }
                    _ => {}
                }
                if decided == Some(1) {
                    *image_close_confirm = None;
                    cmd.image_save_then_close = true;
                }
            }

            // ---- transient toast (save confirmation etc.) — top-center, fades out ----
            if let Some((msg, secs)) = toast.as_mut() {
                *secs -= ui.input(|i| i.stable_dt).min(0.1);
                if *secs <= 0.0 {
                    *toast = None;
                } else {
                    let a = (*secs).clamp(0.0, 1.0); // fade over the last second
                    egui::Area::new(egui::Id::new("save-toast"))
                        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 48.0))
                        .interactable(false)
                        .show(ui.ctx(), |ui| {
                            egui::Frame::popup(ui.style())
                                .fill(egui::Color32::from_rgba_unmultiplied(30, 120, 60, (220.0 * a) as u8))
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new(msg.as_str())
                                            .color(egui::Color32::from_white_alpha((255.0 * a) as u8))
                                            .strong(),
                                    );
                                });
                        });
                }
            }

            // ---- delete asset confirmation (deletion is irreversible) ----
            if let Some(paths) = delete_confirm.clone() {
                let mut open = true;
                let mut close = false;
                let name = |p: &String| {
                    Path::new(p)
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.clone())
                };
                egui::Window::new("Delete asset")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(340.0)
                    .show(ui.ctx(), |ui| {
                        match paths.as_slice() {
                            [p] if floptle_vfs::is_dir(Path::new(p)) => {
                                ui.label(format!(
                                    "Delete the folder \"{}\" and everything in it?",
                                    name(p)
                                ));
                            }
                            [p] => {
                                ui.label(format!("Delete \"{}\"?", name(p)));
                            }
                            many => {
                                ui.label(format!("Delete these {} files?", many.len()));
                                for p in many.iter().take(8) {
                                    ui.small(format!("  {}", name(p)));
                                }
                                if many.len() > 8 {
                                    ui.small(format!("  …and {} more", many.len() - 8));
                                }
                            }
                        }
                        ui.small("This can't be undone.");
                        ui.horizontal(|ui| {
                            if ui.button("🗑 Delete").clicked() {
                                cmd.do_delete_asset = Some(paths.clone());
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *delete_confirm = None;
                }
            }

            // ---- collision layer: do the children come too? ----
            //
            // See `LayerChildrenPrompt` for why this is a question and not a
            // default. Both answers are offered as buttons that say what they
            // will do, with the counts in them — "Yes/No" on a dialog nobody
            // reads carefully is how the wrong one gets clicked every time.
            if let Some(pending) = layer_children_confirm.clone() {
                let mut open = true;
                let mut close = false;
                let n_targets = pending.targets.len();
                let n_kids = pending.children.len();
                egui::Window::new("Collision layer")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(380.0)
                    .show(ui.ctx(), |ui| {
                        ui.label(format!(
                            "{} has {} child node{} under it.",
                            if n_targets == 1 {
                                "This node".to_string()
                            } else {
                                format!("These {n_targets} nodes have")
                            },
                            n_kids,
                            if n_kids == 1 { "" } else { "s" },
                        ));
                        ui.small(format!(
                            "Put them on \"{}\" as well? A collider usually hangs under the \
                             node you just changed, so leaving the children behind is often \
                             why a layer change looks like it did nothing.",
                            pending.layer
                        ));
                        ui.add_space(6.0);
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .button(format!("⬇  Include the {n_kids} children"))
                                .clicked()
                            {
                                let mut all = pending.targets.clone();
                                all.extend_from_slice(&pending.children);
                                cmd.do_set_layer = Some(crate::SetLayer {
                                    targets: all,
                                    layer: pending.layer.clone(),
                                });
                                close = true;
                            }
                            if ui
                                .button(if n_targets == 1 {
                                    "Just this node".to_string()
                                } else {
                                    format!("Just these {n_targets}")
                                })
                                .clicked()
                            {
                                cmd.do_set_layer = Some(crate::SetLayer {
                                    targets: pending.targets.clone(),
                                    layer: pending.layer.clone(),
                                });
                                close = true;
                            }
                            // Closing the window is Cancel, and cancel changes
                            // nothing — the layer has not been written yet.
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *layer_children_confirm = None;
                }
            }

            // ---- new terrain dialog ----
            // Lets a fresh terrain arrive already the size/look you want (a tiny
            // rock-grey patch or a massive grass field) instead of always starting as
            // the same small default slab you'd otherwise have to sculpt/fill out by
            // hand — see NewTerrainCfg.
            if let Some(cfg) = new_terrain_cfg.as_mut() {
                let mut open = true;
                let mut close = false;
                egui::Window::new("New terrain")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(320.0)
                    .show(ui.ctx(), |ui| {
                        ui.label("Footprint (X/Z) and thickness (Y), world units:");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut cfg.size_xz)
                                    .range(0.5..=4000.0)
                                    .speed(1.0)
                                    .prefix("size ")
                                    .suffix(" (x/z)"),
                            );
                            ui.add(
                                egui::DragValue::new(&mut cfg.thickness)
                                    .range(0.2..=500.0)
                                    .speed(0.5)
                                    .prefix("thick ")
                                    .suffix(" (y)"),
                            );
                        });
                        // The size/detail pair silently decides quality, and the old
                        // copy here ("set detail higher before sculpting a large one")
                        // Terrain 2.0: the field is sparse and unbounded — the dialog
                        // sizes a STARTING slab, and memory scales with the surface,
                        // not the volume. Show the honest estimate live.
                        let (chunks, mb) = crate::terrain_ui::new_terrain_preview(
                            cfg.size_xz,
                            cfg.thickness,
                            *terrain_voxel,
                        );
                        ui.small(format!(
                            "→ voxel {:.2} units · ~{chunks} chunks · ~{mb:.1} MB (sparse — grows as you sculpt)",
                            *terrain_voxel,
                        ));
                        ui.horizontal(|ui| {
                            ui.label("color");
                            ui.color_edit_button_rgb(&mut cfg.color);
                        });
                        ui.label("texture (optional — paints the whole slab)");
                        let mut tex_list = Vec::new();
                        collect_texture_paths(asset_tree, &mut tex_list);
                        let cur_label = if cfg.texture.is_empty() {
                            "(none — flat color)".to_string()
                        } else {
                            Path::new(&cfg.texture)
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_default()
                        };
                        egui::ComboBox::from_id_salt("new_terrain_tex")
                            .selected_text(cur_label)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(cfg.texture.is_empty(), "(none — flat color)")
                                    .clicked()
                                {
                                    cfg.texture.clear();
                                }
                                for p in &tex_list {
                                    let n = Path::new(p)
                                        .file_name()
                                        .map(|s| s.to_string_lossy().to_string())
                                        .unwrap_or_default();
                                    if ui.selectable_label(&cfg.texture == p, n).clicked() {
                                        cfg.texture = p.clone();
                                    }
                                }
                            });
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Create").clicked() {
                                cmd.create_terrain = Some(cfg.clone());
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *new_terrain_cfg = None;
                }
            }

            // ---- open-scene unsaved-changes confirm ----
            if let Some(path) = pending_open_scene.clone() {
                let name = Path::new(&path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                // One gate, both directions: a prefab replaces the world exactly
                // as thoroughly as a scene does, so it comes through here too and
                // only the wording differs (`floptle/0090`).
                let kind = if crate::assets::is_prefab(&path) { "prefab" } else { "scene" };
                let name = name.trim_end_matches(".prefab").to_string();
                let mut keep = true;
                egui::Window::new("Unsaved changes")
                    .open(&mut keep)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(320.0)
                    .show(ui.ctx(), |ui| {
                        ui.label(format!("Open {kind} \"{name}\"?"));
                        ui.label("The current scene has unsaved changes.");
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Save & open").clicked() {
                                cmd.do_open_scene = Some((path.clone(), true));
                                *pending_open_scene = None;
                            }
                            if ui.button("Discard & open").clicked() {
                                cmd.do_open_scene = Some((path.clone(), false));
                                *pending_open_scene = None;
                            }
                            if ui.button("Cancel").clicked() {
                                *pending_open_scene = None;
                            }
                        });
                    });
                if !keep {
                    *pending_open_scene = None;
                }
            }

            // ---- who has the pointer ------------------------------------------
            // A grabbed cursor is invisible by definition, so the one thing that
            // says how to get it back cannot itself be the cursor. Without this
            // the way out (Escape) was findable only by reading the source, and
            // what people did instead was alt-tab out of the whole application
            // to reach the Inspector.
            //
            // Only while playing, only over the Game view, and only when the
            // pointer is actually contested — a game that never grabs never
            // sees it.
            // Never in a shipped build: `player_mode` has no editor to hand the
            // pointer back to, so the hint names a negotiation that does not
            // exist there. The build's own way out is the player-mode hint
            // above, which says Escape once and then goes away.
            if playing
                && !player_mode
                && (cursor_held_by_game || cursor_held_by_editor)
                && let Some(r) = *game_rect
            {
                let (msg, fg) = if cursor_held_by_editor {
                    ("Click the game to give the mouse back", egui::Color32::from_rgb(150, 210, 255))
                } else {
                    ("Esc — free the mouse", egui::Color32::from_rgb(215, 220, 230))
                };
                egui::Area::new(egui::Id::new("pointer_owner_hint"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(r.center().x, r.max.y - 34.0))
                    .pivot(egui::Align2::CENTER_CENTER)
                    // Purely a label: it must never eat the click that hands
                    // the pointer back, which lands in this very corner.
                    .interactable(false)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::new()
                            .fill(egui::Color32::from_black_alpha(150))
                            .corner_radius(9.0)
                            .inner_margin(egui::Margin::symmetric(10, 5))
                            .show(ui, |ui| ui.colored_label(fg, msg));
                    });
            }

            // (Terrain tools live in the dockable Terrain tab now; the gizmo paints
            // inside the Scene tab, clipped to its rect.)
        });
        if view_lock != self.camera.lock {
            self.camera.set_lock(view_lock);
        }
        if view_ortho != self.camera.ortho {
            self.camera.set_ortho(view_ortho);
        }
        egui.state.handle_platform_output(&window, full_output.platform_output);
        // egui-winit's cursor-icon handling calls set_cursor_visible(true) whenever
        // the hover icon changes — un-hiding a cursor the game grabbed. Re-assert
        // the hide while any lock is held so the pointer can't flicker back.
        // A script lock only hides the cursor while it's actually over the game
        // view: where the grab is only a Confine (X11), the pointer can reach
        // the Inspector mid-play — it must be visible there to tweak values.
        // Where the grab is a real Lock it cannot travel at all, which is what
        // Escape (`cursor_freed`) is for.
        // (cursor_over_game and game_holds_cursor are inlined as plain field
        // reads — this scope holds a mutable gpu borrow, so a `&self` method
        // here would borrow the whole editor)
        let over_game = scene_hit(&egui.ctx, self.cursor, self.game_rect);
        let game_has_it = self.script_mouse_lock && !self.cursor_freed;
        if self.game_trap || (game_has_it && over_game) {
            window.set_cursor_visible(false);
        } else if self.script_mouse_lock {
            // Off the game view with the lock still wanted — or held back by
            // Escape — force the show. egui only un-hides on an icon change,
            // which may never fire, and a cursor you freed but cannot see is
            // the same bug as one you never freed.
            window.set_cursor_visible(true);
        }
        // **Against the dims last APPLIED, not against what they were at the top
        // of this frame.** The old check captured the value before the UI pass
        // and compared after it, which caught exactly one source of change:
        // Project Settings. A script setting `app.setRetroHeight` runs before
        // that capture, so its new value was already there to be captured as the
        // "old" one and the target was never resized — the setting appeared to
        // do nothing, for ever (`floptle/0175`). Comparing against what the
        // target actually is has no such blind spot, whoever moved the number.
        let want_retro =
            self.project.retro_size(gpu.config.width as f32 / gpu.config.height.max(1) as f32);
        if want_retro != self.retro_applied {
            retro.resize_to(gpu, want_retro.0, want_retro.1);
            self.retro_applied = want_retro;
        }

        // Post-processing (SSAO/bloom/vignette, from the scene's PostProcess node —
        // gathered above) runs at the resolution the scene was composited at: the
        // retro internal res in retro mode (before the nearest-neighbor upscale, so
        // AO/bloom/vignette land on the same chunky pixel grid as the scene), else
        // full frame res. The stack lazily re-sizes when retro toggles/resizes.
        let post_size =
            if self.project.retro { retro.resolution() } else { (gpu.config.width, gpu.config.height) };
        post.configure(gpu, post_size.0, post_size.1, self.project.retro);

        // Screen-space reflections need somewhere to keep last frame's picture.
        // Allocated the first frame a scene asks for them and dropped again when
        // it stops: this is a full-frame mip chain, and much the largest thing
        // the renderer holds, so a project that never turns reflections on must
        // not carry one. It follows the COMPOSITED size, which in retro mode is
        // the internal resolution — reflecting a full-res picture into a 320×240
        // scene would be sharper than anything else in the frame.
        let ssr_on = light_node.reflections;
        // Glass needs the same stored picture, for the opposite reason: not to
        // reflect the scene but to see through it. So the texture is allocated
        // when either asks, and a scene with a single window in it gets one
        // without having to switch reflections on as well.
        let glass = raster.any_transmissive(&instances);
        {
            let fmt = gpu.scene_format();
            let rebuilt = if ssr_on || glass {
                match scene_history.as_mut() {
                    Some(h) => h.resize_to(&gpu.device, post_size.0, post_size.1, fmt),
                    None => {
                        *scene_history = Some(floptle_render::SceneHistory::new(
                            &gpu.device,
                            post_size.0,
                            post_size.1,
                            fmt,
                        ));
                        true
                    }
                }
            } else {
                scene_history.take().is_some()
            };
            // A bind group is immutable, so it is rebuilt only when the texture
            // behind it actually changed — not every frame, which would allocate
            // a bind group per frame for as long as the editor was open.
            if rebuilt {
                let bind = scene_history.as_ref().map(|h| (h.view(), h.sampler()));
                raymarch.set_scene_history(gpu, bind);
            }
        }

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
                // any effect is switched on.
                //
                // It used to go straight to the swapchain when the chain was
                // empty, which was a real saving when both were the same 8-bit
                // sRGB texture. They are not any more: the scene renders in the
                // floating-point scene format, the window takes 8-bit sRGB, and
                // the chain's terminal pass is the only thing that knows how to
                // get from one to the other. So "no effects" is now a chain of
                // exactly one pass rather than a different route.
                let depth =
                    if self.project.retro { retro.depth_view() } else { gpu.depth_view() };
                let color = post.input_view();
                // `rm_draw` already accounts for the matter toggle + terrain presence;
                // with nothing to raymarch the globals still upload so the raster
                // pass's field group (shadows/AO/proxies) sees this frame's data.
                // …and RENDER, second half: the passes themselves.
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
                // the setting quantizes; the light is a multiplier on top of it
                // (`floptle/0127`).
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
                // Script-drawn FILLED triangles (draw.tri/cone/disc — solid gizmos).
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
                // outlines) is deliberately on the far side of this line too: a
                // mirror must not show the reference grid.
                if let Some(h) = scene_history.as_mut() {
                    h.capture(gpu, color, view_proj, cam.world_position);
                }
                // The reference grid is an editor aid — Scene view only, and
                // deliberately after the capture above: it is not part of the
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
                    for (_, placed, origin, right, down, design_vp) in &ui_world {
                        let rel = floptle_core::math::Vec3::new(
                            (origin[0] - cam.world_position.x) as f32,
                            (origin[1] - cam.world_position.y) as f32,
                            (origin[2] - cam.world_position.z) as f32,
                        );
                        let r3 = floptle_core::math::Vec3::from(*right);
                        let d3 = floptle_core::math::Vec3::from(*down);
                        // Project element rects → Scene-tab overlay entries
                        // (gizmos — the master Gizmos toggle hides them, the
                        // canvas CONTENT stays since it's your actual UI).
                        if !ui_gizmos {
                            continue;
                        }
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
                            self.ui_overlay.push((pl.id, [sx, sy, sw, sh], scale.max(0.001)));
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
                            self.ui_canvas.push(quad);
                        }
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
                    // Motion blur is a GAME-view effect. The Scene view is a
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
                let ppp = full_output.pixels_per_point;
                let tris = ctx.tessellate(full_output.shapes, ppp);
                let screen = egui_wgpu::ScreenDescriptor {
                    size_in_pixels: [gpu.config.width, gpu.config.height],
                    pixels_per_point: ppp,
                };
                for (id, delta) in &full_output.textures_delta.set {
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
                for id in &full_output.textures_delta.free {
                    egui.renderer.free_texture(id);
                }
                // `FLOPTLE_FRAME_DUMP=<dir>`: photograph every presented frame
                // into that directory, out of the swapchain image itself. A
                // glitch that lasts one frame while the camera moves cannot be
                // caught by a screenshot key or a headless render; a stream of
                // the frames the player actually saw can be scanned for it
                // afterwards. A diagnostic, not a feature — a readback per frame.
                if let Some(dir) = std::env::var_os("FLOPTLE_FRAME_DUMP")
                    && let Some(px) = read_back_frame(gpu, &frame.surface.texture)
                {
                    let (w, h) = (frame.surface.texture.width(), frame.surface.texture.height());
                    let path = std::path::PathBuf::from(dir)
                        .join(format!("frame-{:05}.png", self.frame_no));
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
                frame.present();
            }
            None => {
                let size = window.inner_size();
                gpu.resize(size.width, size.height);
            }
        }

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

        // The frame is over: fold every bucket into its history (`floptle/0077`).
        // Once, at the very end, so a subsystem that reported in several pieces —
        // physics per tick, scripts per pass — contributes one figure per frame.
        // A no-op while collection is off.
        self.script_host.profile().borrow_mut().end_frame();
    }
}

