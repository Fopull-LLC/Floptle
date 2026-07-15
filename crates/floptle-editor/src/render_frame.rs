//! The editor's per-frame render: step the sim + scripts, gather the World
//! into renderer uniforms, build the egui UI, and draw (raymarch -> raster ->
//! overlays -> post). `render()` is the frame loop's single entry point.

use floptle_core::Entity;
use floptle_core::Light;
use floptle_core::Material;
use floptle_core::Matter;
use floptle_core::Name;
use floptle_core::math::DVec3;
use floptle_core::math::Mat4;
use floptle_core::math::Vec3;
use floptle_core::transform::Transform;
use floptle_render::Globals;
use floptle_render::InstanceRaw;
use floptle_render::MaterialParams;
use floptle_render::MeshId;
use floptle_render::Projection;
use floptle_render::RaymarchGlobals;
use floptle_render::RenderCamera;
use floptle_render::TexId;
use floptle_render::instance_of;
use floptle_render::instance_of_mat;
use floptle_scene::MatterDoc;
use floptle_scene::ShapeDoc;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;
use crate::assets::{AssetPayload, build_assets, collect_texture_paths, is_model};
use crate::dock::{EditorTab, default_dock, focus_scripting_tab, game_tab_active};
use crate::gizmo::build_gizmo;
use crate::hierarchy::{node_new_menu};
use crate::prefs::{DEFAULT_PLAY_TINT, GridConfig, code_theme_path, engine_theme_path, open_external_editor, save_external_editor, save_grid, save_play_tint, save_prefer_external, save_theme_index};
use crate::shading::{blob_default_material, blob_mat_arrays, collect_point_lights, collect_shadow_proxies, fog_uniforms, material_params, post_process_uniforms, shadow_uniforms, skybox_uniforms};
use crate::terrain_ui::{NewTerrainCfg, TerrainFill};
use crate::theme::{CODE_THEMES, ENGINE_THEMES};
use crate::viz::{CameraGizmo, EmitterViz, ForceViz, box_lines, camera_frustum_lines, cursor_ground, gravity_volume_lines, light_dir_lines, mesh_collider_wire_local, oriented_box_lines, particle_gizmo_lines, point_light_lines, project, rigidbody_lines, terrain_collider_wire};
use crate::{Editor, EditorCmd, EditorTabViewer, EXPORT_TARGETS, FOCUS_SECS, MeshAsset, ProjectAction, Snapshot, anim, anim_ui, grab_cursor, scene_hit};

impl Editor {
    pub(crate) fn render(&mut self) {
        // Terrain brush telegraph + throttled stroke (before the destructure, so it
        // can freely borrow `self`).
        self.terrain_frame_update();

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
        // Terrain volumes render PER-VOLUME, each at native resolution: moving a
        // terrain needs NO GPU work — only structural changes re-upload into the
        // shared 3D atlas (where shadow-only mesh occluders also live).
        self.sync_terrain_gpu();
        self.sync_sky_texture();
        // Keep the Inspector's script param list in sync with each script's `defaults`
        // (cheap: cached by file mtime, selected node only) so editing a script surfaces
        // new tunables and drops removed ones live.
        self.sync_selected_script_params();
        // Whether the Game viewport is focused (precomputed before the GPU borrow): game
        // input only feeds scripts here. `game_view()` is pointer-aware in split view, so
        // when both tabs show, input goes to whichever viewport the mouse is over and the
        // Scene view stays fully interactive.
        let game_focused = self.game_view() || self.game_trap;

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

        let (dt, elapsed) = self.advance_clock(game_focused);
        // Capture this frame's pre-edit scene, so an inspector/gizmo edit can push it
        // as a single undo step (see `begin_edit`). Inlined (not via `self.snapshot()`)
        // so it only touches disjoint fields while gpu/egui are borrowed. Not while
        // playing — script-driven transforms must not enter the undo history — and
        // not while recording (the world carries previewed clip values then; edits
        // go to the CLIP as keys, not to scene undo).
        if !self.playing && !self.anim_ui.record {
            self.frame_snapshot =
                Some(floptle_scene::to_doc(self.scene_name.clone(), &self.world));
        }

        self.play_step(dt, game_focused);
        self.finish_input_frame();
        // Register every texture + import every mesh the particle system needs
        // BEFORE the gather that resolves them (full &mut self here — no borrow
        // race, no frame lag on the open effect).
        self.ensure_vfx_assets();
        // Compile/hot-reload `.flsl` shader materials + refresh their group(3)
        // bindings — the gathers below (main, Game viewport, camera preview)
        // all read `flsl_binds`, so this must run before any of them. Field
        // Shapes follow: their sdf shaders splice into both passes on change.
        self.ensure_flsl_materials();
        self.sync_field_shapes();

        // Edit-mode animation preview (Animating tab): pose the bound node at the
        // playhead. This must run BEFORE anything gathers draw data — the UI
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
                    // Record first: capture the user's pose edits as keys BEFORE
                    // the preview re-applies the clip (which then includes them).
                    if self.anim_ui.record
                        && anim_ui::record_scan(&self.world, &mut self.anim_ui, target) {
                            self.anim_ui.clip_dirty = true;
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
                        // frame's diff sees only NEW user edits.
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
        // AFTER the animation preview, so scrubbing shows live in every view.
        let ui_view = self.game_view() || self.player_mode;
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
        // World canvases: in the Scene (authoring) view, EVERY layer renders as
        // a movable hologram at its node's transform; in game/player view, only
        // the layers whose `space` is World (screen-space ones are the overlay
        // above). Either way outlines project onto the canvas and drags come
        // back through cmd.ui_move (in design units).
        let aspect = self
            .gpu
            .as_ref()
            .map(|g| g.config.width as f32 / g.config.height.max(1) as f32)
            .unwrap_or(16.0 / 9.0);
        let ui_world = self.gather_ui_world(aspect, !ui_view);

        // Offscreen previews render LAST (after play_step advanced this frame's poses
        // and particles, and after ensure_vfx_assets registered their textures/meshes):
        // otherwise a docked/split Game view or the Inspector camera POV showed frozen
        // animation and missing effects — it was drawing a frame before the sim, with
        // VFX assets not yet resolved. Reuses `elapsed` so it costs no extra clock read.
        // Both take &mut self and must live outside the main GPU destructure below, so
        // this is the last safe point before it.
        self.update_camera_preview(elapsed);
        self.update_game_viewport(elapsed);
        // The ◈ Shaders tab's per-node preview atlas (only while it's visible).
        self.update_shader_graph_preview(elapsed);

        let (
            Some(gpu),
            Some(raster),
            Some(raymarch),
            Some(retro),
            Some(outline),
            Some(grid_render),
            Some(particles),
            Some(post),
            Some(egui),
            Some(window),
        ) = (
            self.gpu.as_mut(),
            self.raster.as_mut(),
            self.raymarch.as_mut(),
            self.retro.as_mut(),
            self.outline.as_ref(),
            self.grid_render.as_mut(),
            self.particles.as_mut(),
            self.post.as_mut(),
            self.egui.as_mut(),
            self.window.as_ref(),
        ) else {
            return;
        };
        let window = window.clone();

        // ---- gather the scene from the World ----
        let aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
        // The Game dock tab being front = render from the active camera node; otherwise
        // (Scene tab) use the editor's free-fly camera. Works whether or not we're
        // playing, so you can frame the active camera's shot without entering play.
        // (Inlined — self methods can't be called while gpu/egui are borrowed.) A
        // fullscreened tab overrides which view is front. A DOCKED (non-fullscreen)
        // Game tab renders through its own offscreen target sized to the tab rect
        // (update_game_viewport + the tab's Image blit), so the SURFACE renders the
        // editor view underneath — this keeps the game framed to its panel instead of
        // spilling the full-window render behind the other tabs. (Cost: a docked Game
        // tab draws the scene once for the offscreen game view and once for the hidden
        // editor surface; double-click the Game tab to fullscreen it for a single
        // full-window render.) Only a FULLSCREEN Game tab renders the active camera
        // straight to the surface (it fills the whole window, so that framing is right).
        let game_view = matches!(self.fullscreen_tab, Some(EditorTab::Game));
        let cam = {
            let active = if game_view {
                self.world.query::<Matter>().find_map(|(e, m)| {
                    matches!(m, Matter::Camera { active: true, .. }).then_some(e)
                })
            } else {
                None
            };
            match active {
                Some(e) => {
                    let fov_y = match self.world.get::<Matter>(e) {
                        Some(Matter::Camera { fov_y, .. }) => *fov_y,
                        _ => 60f32.to_radians(),
                    };
                    let wt = floptle_core::world_transform(&self.world, e);
                    RenderCamera::new(
                        wt.translation,
                        wt.rotation,
                        Projection::Perspective { fov_y, near: 0.05, far: 4000.0 },
                    )
                }
                None => self.camera.render_camera(),
            }
        };
        let view_proj = cam.view_proj(aspect);

        // Camera frustum + point-light gizmos so they're visible/placeable (hidden in
        // the game view, where you're seeing the game, not the editor overlays).
        self.camera_gizmos.clear();
        self.light_gizmos.clear();
        self.body_gizmos.clear();
        self.contact_gizmos.clear();
        self.terrain_wire_gizmo.clear();
        self.mesh_wire_gizmo.clear();
        self.particle_gizmo.clear();
        // Script debug gizmos (`gizmo.*` from Lua). Unlike the editor overlays these
        // draw in the GAME view too — they're the developer's own telegraphs — but
        // the viewport gizmos toggle still hides them. (Projected for the SURFACE
        // camera; in split view the tab viewer paints them on the Scene side only.)
        self.script_gizmo_lines.clear();
        if self.show_gizmos && self.gizmo_filter.script && !self.script_gizmos.is_empty() {
            let (gw, gh) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
            let cmds = &self.script_gizmos;
            let out = &mut self.script_gizmo_lines;
            let cam_pos = cam.world_position;
            let mut seg = |a: DVec3, b: DVec3, color: [f32; 3]| {
                if let (Some(pa), Some(pb)) =
                    (project(a, cam_pos, view_proj, gw, gh), project(b, cam_pos, view_proj, gw, gh))
                {
                    out.push((pa, pb, color));
                }
            };
            let v3 = |p: [f32; 3]| DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64);
            for cmd in cmds {
                match *cmd {
                    floptle_script::GizmoCmd::Line { a, b, color } => seg(v3(a), v3(b), color),
                    floptle_script::GizmoCmd::Sphere { center, radius, color } => {
                        // Three axis-aligned rings read as a sphere from any angle.
                        let c = v3(center);
                        let r = radius as f64;
                        const N: usize = 20;
                        for (u, v) in [(DVec3::X, DVec3::Y), (DVec3::Y, DVec3::Z), (DVec3::X, DVec3::Z)] {
                            let mut prev = c + u * r;
                            for k in 1..=N {
                                let t = k as f64 / N as f64 * std::f64::consts::TAU;
                                let p = c + u * (r * t.cos()) + v * (r * t.sin());
                                seg(prev, p, color);
                                prev = p;
                            }
                        }
                    }
                    floptle_script::GizmoCmd::Point { pos, size, color } => {
                        let p = v3(pos);
                        let h = size as f64 * 0.5;
                        for off in [DVec3::X, DVec3::Y, DVec3::Z] {
                            seg(p - off * h, p + off * h, color);
                        }
                    }
                }
            }
        }
        if !game_view && self.show_gizmos {
            let (gw, gh) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
            // Only cameras and point lights get gizmos — gather the few Copy fields we
            // need (no per-frame Matter clone over the whole world).
            enum Giz {
                Cam(f32, bool),
                Light(f32),
                Gravity(bool, f32), // radial?, radius
            }
            let filter = self.gizmo_filter;
            let gizmos: Vec<(Entity, Giz)> = self
                .world
                .query::<Matter>()
                .filter_map(|(e, m)| match m {
                    Matter::Camera { fov_y, active } if filter.cameras => {
                        Some((e, Giz::Cam(*fov_y, *active)))
                    }
                    Matter::PointLight { range, .. } if filter.lights => {
                        Some((e, Giz::Light(*range)))
                    }
                    Matter::GravityVolume { mode, radius, .. } if filter.lights => {
                        Some((e, Giz::Gravity(*mode == floptle_core::GravityMode::Radial, *radius)))
                    }
                    _ => None,
                })
                .collect();
            for (e, g) in gizmos {
                let wt = floptle_core::world_transform(&self.world, e);
                match g {
                    Giz::Cam(fov_y, active) => {
                        let lines = camera_frustum_lines(
                            wt.translation, wt.rotation, fov_y, aspect, cam.world_position, view_proj, gw, gh,
                        );
                        if !lines.is_empty() {
                            self.camera_gizmos.push(CameraGizmo { lines, active });
                        }
                    }
                    Giz::Light(range) => {
                        let lines =
                            point_light_lines(wt.translation, range, cam.world_position, view_proj, gw, gh);
                        if !lines.is_empty() {
                            self.light_gizmos.push(lines);
                        }
                    }
                    Giz::Gravity(radial, radius) => {
                        let lines = gravity_volume_lines(
                            wt.translation, radial, radius, cam.world_position, view_proj, gw, gh,
                        );
                        if !lines.is_empty() {
                            self.light_gizmos.push(lines);
                        }
                    }
                }
            }
            // The directional "sun" Light has no world position, so its direction gizmo
            // only shows when the Lighting node is selected — anchored in front of the
            // editor camera so it's always framed, pointing along the light direction.
            if filter.lights
                && self.selection.iter().any(|&e| self.world.get::<Light>(e).is_some())
            {
                let fwd = (self.camera.rotation() * Vec3::NEG_Z).as_dvec3();
                let anchor = cam.world_position + fwd * 6.0;
                let dir = self
                    .world
                    .query::<Light>()
                    .next()
                    .map(|(_, l)| Vec3::from(l.direction))
                    .unwrap_or(Vec3::Y);
                let lines = light_dir_lines(anchor, dir, cam.world_position, view_proj, gw, gh);
                if !lines.is_empty() {
                    self.light_gizmos.push(lines);
                }
            }
            // Rigidbody collider outlines, so physics bodies are visible/placeable.
            let bodies: Vec<(Entity, floptle_core::RigidBody)> = if filter.physics {
                self.world.query::<floptle_core::RigidBody>().map(|(e, rb)| (e, *rb)).collect()
            } else {
                Vec::new()
            };
            for (e, rb) in bodies {
                let wt = floptle_core::world_transform(&self.world, e);
                let p = wt.translation;
                let lines = if rb.kind == floptle_core::BodyKind::Box {
                    let s = wt.scale;
                    let half = Vec3::new(
                        rb.half_extents[0] * s.x,
                        rb.half_extents[1] * s.y,
                        rb.half_extents[2] * s.z,
                    );
                    box_lines(p, half, cam.world_position, view_proj, gw, gh)
                } else {
                    rigidbody_lines(
                        p,
                        rb.kind == floptle_core::BodyKind::Capsule,
                        rb.radius,
                        rb.height,
                        cam.world_position,
                        view_proj,
                        gw,
                        gh,
                    )
                };
                if !lines.is_empty() {
                    self.body_gizmos.push(lines);
                }
            }
            // Collision telegraph: a small cross at each contact resolved this step.
            // (Contacts are sim-frame — origin-relative — so convert to world here.)
            if let Some(sim) = self.sim.as_ref().filter(|_| filter.physics) {
                let cs = 0.15;
                for c in &sim.world.contacts {
                    let cp = sim.world.origin
                        + DVec3::new(c.point.x as f64, c.point.y as f64, c.point.z as f64);
                    for off in [DVec3::X, DVec3::Y, DVec3::Z] {
                        if let (Some(a), Some(b)) = (
                            project(cp - off * cs, cam.world_position, view_proj, gw, gh),
                            project(cp + off * cs, cam.world_position, view_proj, gw, gh),
                        ) {
                            self.contact_gizmos.push((a, b));
                        }
                    }
                }
            }
            // Terrain collider wireframes (the SDF surfaces you walk on). Cached per
            // terrain in NODE-LOCAL coords at native resolution + rebuilt only when
            // that terrain's shape changes; here we add each node's f64 anchor and
            // re-project — so a moved terrain's wireframe follows for free.
            // Coarseness scales with each grid so the line count stays sane.
            if self.show_terrain_collider && filter.colliders {
                for (&e, t) in &self.terrains {
                    if !self.terrain_wire_world.iter().any(|(we, _)| *we == e) {
                        let stride = (t.baked.dims.into_iter().max().unwrap_or(64) / 48).max(2);
                        self.terrain_wire_world.push((e, terrain_collider_wire(t, stride)));
                    }
                }
                self.terrain_wire_world.retain(|(we, _)| self.terrains.contains_key(we));
                for (e, segs) in &self.terrain_wire_world {
                    let anchor = floptle_core::world_transform(&self.world, *e).translation;
                    for &(a, b) in segs {
                        let wa = anchor + DVec3::new(a.x as f64, a.y as f64, a.z as f64);
                        let wb = anchor + DVec3::new(b.x as f64, b.y as f64, b.z as f64);
                        if let (Some(pa), Some(pb)) = (
                            project(wa, cam.world_position, view_proj, gw, gh),
                            project(wb, cam.world_position, view_proj, gw, gh),
                        ) {
                            self.terrain_wire_gizmo.push((pa, pb));
                        }
                    }
                }
            }
            // Mesh collider wireframes. Every Mesh node flagged Collidable OR (legacy)
            // MeshCollider when the global toggle is on, plus the SELECTED one always (so
            // you can verify it). Both markers build a static triangle-mesh collider, so
            // both must draw the wireframe (union; dedup a node flagged both).
            let mut collider_ents: Vec<Entity> =
                self.world.query::<floptle_core::Collidable>().map(|(e, _)| e).collect();
            for (e, _) in self.world.query::<floptle_core::MeshCollider>() {
                if !collider_ents.contains(&e) {
                    collider_ents.push(e);
                }
            }
            let mesh_colliders: Vec<(Entity, String)> = collider_ents
                .into_iter()
                .filter_map(|e| match self.world.get::<Matter>(e) {
                    Some(Matter::Mesh { asset_path }) => Some((e, asset_path.clone())),
                    _ => None,
                })
                .collect();
            for (e, path) in mesh_colliders {
                if !filter.colliders
                    || (!self.show_mesh_colliders && !self.selection.contains(&e))
                {
                    continue;
                }
                if !self.mesh_wire_cache.contains_key(&path) {
                    let edges = floptle_assets::gltf_import::import(std::path::Path::new(&path))
                        .map(|m| mesh_collider_wire_local(&m))
                        .unwrap_or_default();
                    self.mesh_wire_cache.insert(path.clone(), edges);
                }
                let edges = &self.mesh_wire_cache[&path];
                let wt = floptle_core::world_transform(&self.world, e);
                let m = Mat4::from_scale_rotation_translation(wt.scale, wt.rotation, wt.translation.as_vec3());
                for &(a, b) in edges {
                    let wa = m.transform_point3(a).as_dvec3();
                    let wb = m.transform_point3(b).as_dvec3();
                    if let (Some(pa), Some(pb)) = (
                        project(wa, cam.world_position, view_proj, gw, gh),
                        project(wb, cam.world_position, view_proj, gw, gh),
                    ) {
                        self.mesh_wire_gizmo.push((pa, pb));
                    }
                }
            }
            // Static PRIMITIVE collider wireframes (the "Collidable" switch on a Cube /
            // Sphere / Capsule) — drawn with the same toggle as mesh colliders, plus the
            // selected one always. Each matches the static collider built at Play.
            let shape_colliders: Vec<(Entity, floptle_core::Shape)> = self
                .world
                .query::<floptle_core::Collidable>()
                .filter_map(|(e, _)| match self.world.get::<Matter>(e) {
                    Some(Matter::Primitive { shape, .. }) => Some((e, *shape)),
                    _ => None,
                })
                .collect();
            for (e, shape) in shape_colliders {
                if !filter.colliders
                    || (!self.show_mesh_colliders && !self.selection.contains(&e))
                {
                    continue;
                }
                let wt = floptle_core::world_transform(&self.world, e);
                let s = wt.scale;
                let lines = match shape {
                    floptle_core::Shape::Cube => {
                        let m = Mat4::from_scale_rotation_translation(s, wt.rotation, wt.translation.as_vec3());
                        oriented_box_lines(m, 0.7, cam.world_position, view_proj, gw, gh)
                    }
                    floptle_core::Shape::Plane => {
                        // Flat in Z: outline the thin-box collider proxy.
                        let thin = Vec3::new(s.x, s.y, 0.02 * s.z.max(1.0));
                        let m = Mat4::from_scale_rotation_translation(thin, wt.rotation, wt.translation.as_vec3());
                        oriented_box_lines(m, 0.7, cam.world_position, view_proj, gw, gh)
                    }
                    floptle_core::Shape::Sphere => rigidbody_lines(
                        wt.translation, false, 0.85 * s.max_element(), 0.0,
                        cam.world_position, view_proj, gw, gh,
                    ),
                    floptle_core::Shape::Capsule => {
                        let r = 0.5 * s.x.max(s.z);
                        rigidbody_lines(
                            wt.translation, true, r, s.y + 2.0 * r,
                            cam.world_position, view_proj, gw, gh,
                        )
                    }
                };
                self.mesh_wire_gizmo.extend(lines);
            }

            // Selected particle track: draw its emitter birth shape + emit direction +
            // force arrows, so authoring a VFX has spatial feedback. The node is the
            // Particles-tab preview anchor, or a selected ParticleSystem node; the edited
            // effect is `vfx_ui.doc`. sel_track only (less clutter) else every track.
            let particle_node = self
                .vfx
                .preview
                .as_ref()
                .and_then(|p| p.anchor)
                .or_else(|| {
                    self.selection
                        .last()
                        .copied()
                        .filter(|&e| self.world.get::<floptle_core::ParticleSystem>(e).is_some())
                });
            if let (Some(node), Some(doc)) =
                (particle_node.filter(|_| filter.particles), self.vfx_ui.doc.as_ref())
            {
                use floptle_scene::{VfxForceDoc, VfxShapeDoc, VfxSpaceDoc};
                let wt = floptle_core::world_transform(&self.world, node);
                let m_shape = Mat4::from_scale_rotation_translation(
                    wt.scale,
                    wt.rotation,
                    wt.translation.as_vec3(),
                );
                let m_anchor = Mat4::from_translation(wt.translation.as_vec3());
                let tracks: Vec<usize> = match self.vfx_ui.sel_track {
                    Some(i) if i < doc.tracks.len() => vec![i],
                    _ => (0..doc.tracks.len()).collect(),
                };
                for ti in tracks {
                    let t = &doc.tracks[ti];
                    let shape = match t.shape {
                        VfxShapeDoc::Point => EmitterViz::Point,
                        VfxShapeDoc::Cone { angle, radius } => EmitterViz::Cone { angle, radius },
                        VfxShapeDoc::Sphere { radius, .. } => EmitterViz::Sphere { radius },
                        VfxShapeDoc::Edge { length } => EmitterViz::Edge { length },
                        VfxShapeDoc::Ring { radius } => EmitterViz::Ring { radius },
                    };
                    let forces: Vec<ForceViz> = t
                        .forces
                        .iter()
                        .filter_map(|f| match *f {
                            VfxForceDoc::Directional { dir, .. } => {
                                Some(ForceViz::Directional { dir: Vec3::from(dir) })
                            }
                            VfxForceDoc::Point { center, strength } => Some(ForceViz::Point {
                                center: Vec3::from(center),
                                attract: strength >= 0.0,
                            }),
                            VfxForceDoc::Vortex { center, axis, .. } => Some(ForceViz::Vortex {
                                center: Vec3::from(center),
                                axis: Vec3::from(axis),
                            }),
                            VfxForceDoc::Turbulence { .. } => None,
                        })
                        .collect();
                    // World-space forces act in world/anchor space (translation only);
                    // Local-space forces (and every birth shape) ride the emitter frame.
                    let m_force =
                        if t.space == VfxSpaceDoc::World { m_anchor } else { m_shape };
                    self.particle_gizmo.extend(particle_gizmo_lines(
                        &shape, &forces, m_shape, m_force, cam.world_position, view_proj, gw, gh,
                    ));
                }
            }
        }

        // Rebuild the overlay gizmo for the selected object (projects + hit-tests).
        // The Rect tool needs the object's local bounds (None = unsupported matter,
        // e.g. a UI element — those get 2D handles in the Scene tab instead).
        let rect_half = self
            .selection
            .last()
            .copied()
            .and_then(|e| crate::selection::rect_base_half(&self.world, &self.mesh_registry, e));
        self.gizmo = build_gizmo(
            self.tool,
            self.selection.last().copied(),
            &self.world,
            self.cursor,
            cam.world_position,
            view_proj,
            gpu.config.width as f32,
            gpu.config.height.max(1) as f32,
            rect_half,
        );

        // Lighting comes from the scene's mandatory Lighting node (a Light component).
        let light_node = self.world.query::<Light>().next().map(|(_, l)| *l).unwrap_or_default();
        let light = Vec3::from(light_node.direction).normalize_or_zero();
        let li = light_node.intensity;
        let (pl_count, pl_pos, pl_col) = collect_point_lights(&self.world, cam.world_position);
        // Sun shadows (Lighting node knobs) + the collider-proxy occluders that let
        // raster meshes cast — both ride the raymarch globals, which the raster pass
        // reads too through the shared field bind group.
        let (sh_params, sh_tint, sh_extra) = shadow_uniforms(&light_node);
        let (fog_color, fog_params) = fog_uniforms(&light_node);
        let (prox_count, prox_a, prox_b, prox_rot) =
            collect_shadow_proxies(&self.world, cam.world_position, light_node.shadows);
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [light_node.color[0] * li, light_node.color[1] * li, light_node.color[2] * li, 0.0],
            ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
            point_count: pl_count,
            point_pos: pl_pos,
            point_color: pl_col,
        };

        // A model being dragged from Assets shows a live ghost at the cursor's
        // ground point, so you see it follow the cursor and land where you drop.
        // Only while the cursor is actually over the viewport (not over an opaque
        // panel), matching where the drop is accepted.
        let ghost_over_scene = scene_hit(&egui.ctx, self.cursor, self.scene_rect);
        let drag_ghost: Option<(String, DVec3)> = egui::DragAndDrop::payload::<AssetPayload>(&egui.ctx)
            .filter(|p| is_model(&p.path) && ghost_over_scene)
            .map(
                |p| {
                    let pos = cursor_ground(
                        cam.world_position,
                        cam.rotation,
                        view_proj.inverse(),
                        gpu.config.width as f32,
                        gpu.config.height.max(1) as f32,
                        self.cursor,
                    );
                    (p.path.clone(), pos)
                },
            );

        // Bone attachments follow their mesh's bones while authoring too (uses the
        // preview pose if the Animating tab is scrubbing, else the rig's rest pose).
        anim::resolve_attachments(&self.anim, &mut self.world, &self.mesh_registry);

        let ents: Vec<(Entity, Matter)> =
            self.world.query::<Matter>().map(|(e, m)| (e, m.clone())).collect();
        let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
        // Custom-shader draws (a Material with a compiled `.flsl`): same
        // instance data, drawn through the shader's own pipeline + group(3).
        let mut flsl_draws: Vec<floptle_render::FlslDraw> = Vec::new();
        let mut blobs: Vec<(DVec3, f32, MaterialParams)> = Vec::new();
        // Reused scratch for CPU vertex skinning (deformed vertices, re-uploaded per part).
        let mut skin_scratch: Vec<floptle_render::Vertex> = Vec::new();
        if let Some((path, pos)) = &drag_ghost
            && let Some(asset) = self.mesh_registry.get(path) {
                let ghost = Transform { translation: *pos, ..Transform::default() };
                let model = ghost.render_matrix(cam.world_position);
                for (i, &mid) in asset.parts.iter().enumerate() {
                    let local = asset
                        .rig
                        .as_ref()
                        .and_then(|r| r.rest_world.get(*r.part_nodes.get(i)?).copied())
                        .unwrap_or(Mat4::IDENTITY);
                    instances.push((mid, None, instance_of(model * local, [0.7, 0.85, 1.0])));
                }
            }
        for (e, matter) in &ents {
            // Hidden nodes (Visible(false)) don't draw their geometry (a script or the
            // Inspector can toggle this); they still keep transforms, physics, children.
            if matches!(self.world.get::<floptle_core::Visible>(*e), Some(floptle_core::Visible(false))) {
                continue;
            }
            // World transform (composes any parent chain) — a parent carries children.
            let t = floptle_core::world_transform(&self.world, *e);
            // A node's Material (if any) overrides the look; else fall back to the
            // primitive's color (meshes default to white = untinted texture). A
            // material texture (resolved to a registered handle) re-textures the shape.
            let mat = self.world.get::<Material>(*e).cloned();
            let tex = mat
                .as_ref()
                .and_then(|m| m.texture.as_deref())
                .and_then(|p| self.texture_registry.get(p).copied());
            let flsl = self.flsl_binds.get(e).map(|b| b.binding);
            match matter {
                Matter::Primitive { shape, color } => {
                    if let Some(&mesh) = self.mesh_ids.get(*shape as usize) {
                        let model = t.render_matrix(cam.world_position);
                        let mp = mat.as_ref().map(material_params).unwrap_or_else(|| MaterialParams::flat(*color));
                        let raw = instance_of_mat(model, &mp);
                        match flsl {
                            Some(b) => flsl_draws.push((mesh, tex, b, raw)),
                            None => instances.push((mesh, tex, raw)),
                        }
                    }
                }
                Matter::Blob { scale } => {
                    // Blobs render in the raymarch pass — a custom fragment
                    // shader doesn't apply (the Sdf stage is their world).
                    let mp = mat.as_ref().map(material_params).unwrap_or_else(blob_default_material);
                    blobs.push((t.translation, scale * t.scale.x, mp));
                }
                Matter::Mesh { asset_path } => {
                    if let Some(asset) = self.mesh_registry.get(asset_path) {
                        let model = t.render_matrix(cam.world_position);
                        let mp = mat.as_ref().map(material_params).unwrap_or_else(|| MaterialParams::flat([1.0, 1.0, 1.0]));
                        let pose = self.anim.poses.get(e).map(|v| v.as_slice());
                        push_mesh_instances(gpu, raster, asset, pose, model, tex, &mp, &mut skin_scratch, &mut instances, flsl, &mut flsl_draws);
                    }
                }
                // group / terrain / camera / light / gravity / skybox / post render
                // elsewhere; Field Shapes are raymarched (globals filled below).
                Matter::Empty
                | Matter::Terrain { .. }
                | Matter::Camera { .. }
                | Matter::PointLight { .. }
                | Matter::GravityVolume { .. }
                | Matter::FieldShape { .. }
                | Matter::Skybox { .. }
                | Matter::PostProcess { .. } => {}
            }
        }

        // Undo any transient scene-binding animation preview now that the draw list
        // is built — the ECS goes back to authored transforms before UI/undo/save.
        // NOT while recording: record keeps the previewed values live so the
        // Inspector shows what's under the playhead (edit it → it's keyed) and a
        // scrub can't diff a stale pose into spurious keys. The pre-record scene is
        // restored by stop_record_ui when ● Record turns off.
        if !self.anim_ui.record {
            self.anim.restore_preview(&mut self.world);
        }

        // Live particle effects (play mode): pack every instance's billboards for
        // this frame. Owned data — drawn after the grid, before post, so particles
        // depth-test against the scene and inherit retro/post like everything else.
        // The tab's preview draws only while the Particles tab is actually up
        // (front of its dock leaf) and we're not in Play.
        let vfx_preview_on = !self.playing
            && self
                .dock_state
                .as_ref()
                .is_some_and(|d| crate::dock::tab_is_front(d, EditorTab::Particles));
        let mut vfx_instances: Vec<floptle_render::ParticleInstance> = Vec::new();
        let mut vfx_batches: Vec<floptle_render::ParticleBatch> = Vec::new();
        self.vfx.collect(
            &self.world,
            &cam,
            &self.texture_registry,
            vfx_preview_on,
            &mut vfx_instances,
            &mut vfx_batches,
        );
        // Mesh-render particle tracks ride the raster instance list (lit + shadowed
        // like scene meshes), so append them to `instances` built above.
        let vfx_mesh_draws = self.vfx.collect_mesh_draws(&self.world, &cam, vfx_preview_on);
        resolve_mesh_particles(&self.mesh_registry, &vfx_mesh_draws, &mut instances);

        // Skybox: a Skybox node drives the environment background — a solid color, or an
        // equirect texture × tint, rotated by the node so a script can spin the sky.
        let (sky_params, sky_tint, sky_rot, sky_solid) = skybox_uniforms(&self.world);
        let clear = [sky_solid[0], sky_solid[1], sky_solid[2], 1.0];
        // The terrain's surface Material (active terrain's, or any terrain that has one)
        // so terrain shades like the rest of the scene. Neutral default = plain matte.
        // (Inlined via disjoint field access — a `&self` method can't be called here
        // while gpu/raster/etc. are mutably borrowed for the render.)
        let terrain_mat = {
            let pick = self
                .active_terrain
                .filter(|e| self.world.get::<Material>(*e).is_some())
                .or_else(|| {
                    self.terrains
                        .keys()
                        .copied()
                        .find(|&e| self.world.get::<Material>(e).is_some())
                });
            pick.and_then(|e| self.world.get::<Material>(e))
                .map(material_params)
                .unwrap_or_else(|| MaterialParams::flat([1.0, 1.0, 1.0]))
        };
        // The scene's PostProcess node drives the whole post chain (per scene, not
        // per project): PostStack settings + the raymarch SDF-AO params.
        let (post_settings, rm_ao_params) = post_process_uniforms(&self.world);
        // Build raymarch globals for a set of blobs (all of them, or just one for the
        // selection mask). Up to 16 blobs are folded together in one march.
        let make_rm = |set: &[(DVec3, f32, MaterialParams)]| -> RaymarchGlobals {
            let mut arr = [[0.0f32; 4]; 16];
            let n = set.len().min(16);
            for (i, (center, scale, _)) in set.iter().take(16).enumerate() {
                let c = (*center - cam.world_position).as_vec3();
                arr[i] = [c.x, c.y, c.z, scale.max(0.05)];
            }
            let (blob_tint, blob_emissive, blob_specular, blob_params, blob_rim) = blob_mat_arrays(set);
            let tm = &terrain_mat;
            RaymarchGlobals {
                view_proj: view_proj.to_cols_array_2d(),
                inv_view_proj: view_proj.inverse().to_cols_array_2d(),
                light_dir: [light.x, light.y, light.z, 0.0],
                light_color: [light_node.color[0] * li, light_node.color[1] * li, light_node.color[2] * li, 0.0],
                ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
                bg: [clear[0], clear[1], clear[2], 1.0],
                center: [0.0; 4],
                params: [elapsed, n as f32, 0.0, 0.0],
                vol_center: [[0.0; 4]; 16],
                vol_half: [[1.0, 1.0, 1.0, 0.5]; 16],
                vol_atlas: [[0.0; 4]; 16],
                vol_dims: [[1.0, 1.0, 1.0, 0.0]; 16],
                terrain_tint: [tm.color[0], tm.color[1], tm.color[2], 1.0],
                terrain_emissive: [tm.emissive[0], tm.emissive[1], tm.emissive[2], tm.emissive_strength],
                terrain_specular: [tm.specular[0], tm.specular[1], tm.specular[2], tm.specular_strength],
                terrain_params: [tm.shininess, tm.rim_strength, if tm.unlit { 1.0 } else { 0.0 }, tm.ambient],
                terrain_rim: [tm.rim[0], tm.rim[1], tm.rim[2], 0.0],
                blobs: arr,
                point_count: pl_count,
                point_pos: pl_pos,
                point_color: pl_col,
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
                // vol_tight_* are renderer-patched at draw time from the uploaded
                // volumes; the default is "unbounded" (behaves like the full brick).
                ..Default::default()
            }
        };

        // Selection outline source: the selected object's silhouette into the mask —
        // a mesh instance, or (for a blob) a one-blob raymarch so the outline hugs
        // only the selected blob.
        let mut mask_mesh: Vec<(MeshId, InstanceRaw)> = Vec::new();
        let mut mask_blob: Option<RaymarchGlobals> = None;
        // The Game view plays like a build — no selection outline there.
        if let Some(e) = self.selection.last().copied().filter(|_| !game_view)
            && let Some(m) = self.world.get::<Matter>(e) {
                let t = floptle_core::world_transform(&self.world, e);
                match m {
                    Matter::Primitive { shape, .. } => {
                        if let Some(&mesh) = self.mesh_ids.get(*shape as usize) {
                            let model = t.render_matrix(cam.world_position);
                            mask_mesh.push((mesh, instance_of(model, [1.0, 1.0, 1.0])));
                        }
                    }
                    Matter::Mesh { asset_path } => {
                        if let Some(asset) = self.mesh_registry.get(asset_path) {
                            let model = t.render_matrix(cam.world_position);
                            if let Some(rig) = asset.rig.as_ref() {
                                // Match the posed draw so the outline hugs the pose.
                                let node_world =
                                    self.anim.poses.get(&e).unwrap_or(&rig.rest_world);
                                for (i, &mid) in asset.parts.iter().enumerate() {
                                    let local = rig
                                        .part_nodes
                                        .get(i)
                                        .and_then(|&n| node_world.get(n))
                                        .copied()
                                        .unwrap_or(Mat4::IDENTITY);
                                    mask_mesh
                                        .push((mid, instance_of(model * local, [1.0, 1.0, 1.0])));
                                }
                            } else {
                                for &mid in &asset.parts {
                                    mask_mesh.push((mid, instance_of(model, [1.0, 1.0, 1.0])));
                                }
                            }
                        }
                    }
                    Matter::Blob { scale } => {
                        let mp = self
                            .world
                            .get::<Material>(e)
                            .map(material_params)
                            .unwrap_or_else(blob_default_material);
                        mask_blob = Some(make_rm(&[(t.translation, scale * t.scale.x, mp)]));
                    }
                    Matter::FieldShape { .. } => {
                        // Mask through the raymarch with ONLY this shape active,
                        // so the outline hugs the authored SDF silhouette.
                        let mut g = make_rm(&[]);
                        crate::shaders::apply_field_shapes(&self.world, &self.flsl_shape_slots, &self.sdf_cache, &mut g, cam.world_position, Some(e));
                        mask_blob = Some(g);
                    }
                    Matter::Empty
                    | Matter::Terrain { .. }
                    | Matter::Camera { .. }
                    | Matter::PointLight { .. }
                    | Matter::GravityVolume { .. }
                    | Matter::Skybox { .. }
                    | Matter::PostProcess { .. } => {}
                }
            }

        // The raymarch pass renders the blob matter (gated by the SDF-matter toggle)
        // and/or the combined terrain volume — and it's ALSO what draws a textured
        // skybox (rays that miss every bound sample the sky, zero march steps), so a
        // scene with no terrain/blobs still runs it when the sky has a texture; a
        // solid-color sky is just the raster clear. The globals are built either way
        // — on frames with nothing to raymarch they're still uploaded (not drawn) so
        // the raster pass's field bind group has this frame's shadow/proxy data.
        let show_blobs = self.project.matter && !blobs.is_empty();
        let rm_draw = show_blobs
            || !self.terrains.is_empty()
            || sky_params[0] >= 0.5
            || !self.flsl_shape_slots.is_empty();
        let rm = {
            let mut g = make_rm(if show_blobs { &blobs } else { &[] });
            Self::fill_terrain_volumes(&self.terrains, &self.terrain_slots, &self.mesh_occluders, &self.occluder_slots, &self.world, &mut g, cam.world_position);
            crate::shaders::apply_field_shapes(&self.world, &self.flsl_shape_slots, &self.sdf_cache, &mut g, cam.world_position, None);
            g
        };

        // ---- build the egui UI (mutating the World) ----
        let raw_input = egui.state.take_egui_input(&window);
        let ctx = egui.ctx.clone();
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
            ctx.all_styles_mut(|s| s.visuals = vis.clone());
        }
        // Every named entity, Matter nodes and the Lighting node alike.
        let entity_names: Vec<(Entity, String)> =
            self.world.query::<Name>().map(|(e, n)| (e, n.0.clone())).collect();
        let old_retro_h = self.project.retro_height;
        let ppp = ctx.pixels_per_point();
        let dock_state = self.dock_state.get_or_insert_with(default_dock);
        // Bone names per rigged Mesh entity (name + parent index) — for the hierarchy's
        // expandable sub-objects and the inspector's bone-attach picker. Built read-only
        // before the borrow split so the UI never touches the mesh registry itself.
        let bone_names: HashMap<Entity, Vec<(String, Option<usize>)>> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Mesh { asset_path } => self
                    .mesh_registry
                    .get(asset_path)
                    .and_then(|a| a.rig.as_ref())
                    .map(|rig| {
                        (e, rig.skeleton.nodes.iter().map(|n| (n.name.clone(), n.parent)).collect())
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
        let scene_files = if self.show_project_settings {
            crate::project::scene_files_in(&self.project_root)
        } else {
            Vec::new()
        };
        let fullscreen_tab = &mut self.fullscreen_tab;
        let world = &mut self.world;
        let has_selection = !self.selection.is_empty();
        let selection = &mut self.selection;
        let bone_selection = &mut self.bone_selection;
        let collapsed = &mut self.collapsed;
        let console = &mut self.console;
        let preview_zoom = &mut self.preview_zoom;
        let preview_spin = &mut self.preview_spin;
        let preview_spinning = &mut self.preview_spinning;
        let preview_material = &mut self.preview_material;
        let project = &mut self.project;
        let show_project_settings = &mut self.show_project_settings;
        let layer_new = &mut self.layer_new;
        let show_project_mgr = &mut self.show_project_mgr;
        let project_path_buf = &mut self.project_path_buf;
        let grid = &mut self.grid;
        let show_grid_settings = &mut self.show_grid_settings;
        let show_terrain_collider = &mut self.show_terrain_collider;
        let show_mesh_colliders = &mut self.show_mesh_colliders;
        let rename_target = &mut self.rename_target;
        let new_scene_buf = &mut self.new_scene_buf;
        let show_quit_confirm = &mut self.show_quit_confirm;
        let quit_confirmed = &mut self.quit_confirmed;
        let delete_confirm = &mut self.delete_confirm;
        let scene_dirty_now = self.scene_dirty;
        let new_terrain_cfg = &mut self.new_terrain_cfg;
        let pending_open_scene = &mut self.pending_open_scene;
        let terrain_brush = &mut self.terrain_brush;
        let terrain_detail = &mut self.terrain_detail;
        let terrain_textures = &mut self.terrain_textures;
        let terrain_present = !self.terrains.is_empty();
        let terrain_voxels = (!self.terrains.is_empty()).then(|| {
            let total: u64 = self
                .terrains
                .values()
                .map(|t| t.baked.dims.iter().map(|&d| d as u64).product::<u64>())
                .sum();
            (self.terrains.len(), total)
        });
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
        let paused = self.paused;
        let has_active_camera =
            world.query::<Matter>().any(|(_, m)| matches!(m, Matter::Camera { active: true, .. }));
        // The selected camera's POV preview texture (only when a camera is selected).
        let cam_preview = selection
            .last()
            .copied()
            .filter(|&e| matches!(world.get::<Matter>(e), Some(Matter::Camera { .. })))
            .and(self.cam_preview.as_ref().map(|p| p.tex_id));
        // A docked (non-fullscreen) Game tab paints its own offscreen render this frame,
        // sized+blit to its rect (single-view or split) so it never spills behind panels.
        let game_offscreen = fullscreen_tab.is_none() && game_tab_active(dock_state);
        let particles_active = crate::dock::tab_is_front(dock_state, EditorTab::Particles);
        let game_tex = self.game_vp.as_ref().map(|p| p.tex_id);
        let game_rect = &mut self.game_rect;
        let materials = &self.materials;
        let mat_name_buf = &mut self.mat_name_buf;
        let component_clip = &self.component_clip;
        let add_component_filter = &mut self.add_component_filter;
        let layer_names = project.build_layers().names;
        let tag_edit = &mut self.tag_edit;
        let hier_scrolled = &mut self.hier_scrolled;
        let show_material_editor = &mut self.show_material_editor;
        let ide = &mut self.ide;
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
        let camera_gizmos = self.camera_gizmos.as_slice();
        let light_gizmos = self.light_gizmos.as_slice();
        let body_gizmos = self.body_gizmos.as_slice();
        let contact_gizmos = self.contact_gizmos.as_slice();
        let script_gizmo_lines = self.script_gizmo_lines.as_slice();
        let terrain_wire = self.terrain_wire_gizmo.as_slice();
        let mesh_wire = self.mesh_wire_gizmo.as_slice();
        let particle_gizmo = self.particle_gizmo.as_slice();
        let show_gizmos = &mut self.show_gizmos;
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
        let shader_preview_state = &mut self.shader_preview;
        let mesh_registry = &self.mesh_registry;
        // Multiplayer harness panel state: read-only status snapshot + live knobs.
        let net_hosting = self.net_server.is_some();
        let net_peer_count = self.net_server.as_ref().map(|s| s.peers().len()).unwrap_or(0);
        let net_has_client = self.net_client.is_some();
        let net_as_player = self.net_play_client.is_some();
        let net_rtt = self
            .net_play_client
            .as_ref()
            .map(|c| c.stats(floptle_net::SERVER).rtt_ms)
            .unwrap_or(0.0);
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
        // A REAL session (QUIC) has no hub: the link is the actual network, so
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
            // The Floptle Cloud rendezvous relay (task 0005): a DNS-only
            // record straight to the host — the name is the stable contract
            // even if the box moves. Self-hosters just type their own.
            self.net_relay_addr = "relay.fopull.com:7788".into();
        }
        let net_host_port = &mut self.net_host_port;
        let net_join_addr = &mut self.net_join_addr;
        let net_relay_addr = &mut self.net_relay_addr;
        let net_join_code = &mut self.net_join_code;
        let net_lobby_code = self.net_lobby_code.clone();
        let show_net_panel = &mut self.show_net_panel;
        // Player mode (an exported build / --play): no editor chrome at all —
        // the Game view IS the window. F1 (handled at the winit layer) toggles
        // the multiplayer window, which still works for LAN/relay sessions.
        let player_mode = self.player_mode;
        let play_t = self.play_t;
        let ui_overlay_snapshot = self.ui_overlay.clone();
        let ref_kinds = &self.ref_kinds;
        let ui_canvas_snapshot = self.ui_canvas.clone();
        let show_export = &mut self.show_export;
        // Relative export folders resolve against the project's PARENT (shown
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
        let export_building = self.export_build.is_some();
        let export_status = &self.export_status;
        let export_done = self.export_done.clone();
        let autosave_prompt = self.autosave_prompt.clone();
        let scene_name_now = self.scene_name.clone();
        let net_latency_ticks = &mut self.net_latency_ticks;
        let net_loss = &mut self.net_loss;
        let net_ghosts = &mut self.net_ghosts;
        let mut cmd = EditorCmd::default();
        let mut want_save = false;
        let mut want_save_project = false;
        let mut frame_pointer_down = false;
        let full_output = ctx.run_ui(raw_input, |ui| {
            let pointer_down = ui.input(|i| i.pointer.any_down());
            frame_pointer_down = pointer_down;
            // ---- top menu bar (never in a build) ----
            if !player_mode {
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
                                "stamp out a runnable build: this binary + the project \
                                 (for THIS platform — export on each OS you target)",
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
                        if ui.button("Project Settings…").clicked() {
                            *show_project_settings = true;
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
                        if ui.button("✏ Animating").on_hover_text("the animation timeline: preview, keys, events").clicked() {
                            cmd.focus_animating = true;
                            ui.close();
                        }
                        if ui.button("Δ Terrain tools").clicked() {
                            cmd.focus_terrain = true;
                            ui.close();
                        }
                    });
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
                    }
                    if ui
                        .button(if net_hosting { "🌐 hosting" } else { "🌐" })
                        .on_hover_text("Multiplayer — host & join locally, latency/loss sliders (docs/netcode-design.md)")
                        .clicked()
                    {
                        *show_net_panel = !*show_net_panel;
                    }
                    // The view is now chosen by the Scene / Game dock tabs (the editor
                    // free-fly view vs the active-camera gameplay view), not a toggle here.
                });
            });
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
                                    // dev tool — a BUILD's menu is just the
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
                ui_overlay: &ui_overlay_snapshot,
                ui_canvas: &ui_canvas_snapshot,
                ref_kinds,
                bone_selection,
                fullscreen_tab,
                collapsed,
                bone_names: &bone_names,
                console,
                preview: preview_view.clone(),
                preview_zoom,
                preview_spin,
                preview_spinning,
                preview_material,
                entity_names: &entity_names,
                materials,
                mat_name_buf,
                flsl_cache: &self.flsl_cache,
                sdf_cache: &self.sdf_cache,
                component_clip,
                add_component_filter,
                layer_names: &layer_names,
                tag_edit,
                hier_scrolled,
                show_material_editor,
                asset_tree,
                texture_settings,
                cam_preview,
                has_active_camera,
                terrain_brush,
                terrain_detail,
                terrain_textures,
                terrain_present,
                terrain_voxels,
                assets_grid,
                assets_grid_dir,
                project_root,
                selected_asset,
                asset_selection,
                ide,
                script_errors,
                ide_diag,
                gizmo,
                terrain_viz,
                camera_gizmos,
                light_gizmos,
                body_gizmos,
                contact_gizmos,
                script_gizmo_lines,
                terrain_wire,
                mesh_wire,
                particle_gizmo,
                show_gizmos,
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
                ppp,
                code_theme,
                anim: anim_sys,
                vfx: vfx_sys,
                vfx_ui: vfx_ui_state,
                audio: audio_sys,
                mixer_ui: mixer_ui_state,
                mixer: &mut project.mixer,
                particles_active,
                anim_ui: anim_ui_state,
                shader_graph: shader_graph_state,
                shader_preview: shader_preview_state,
                mesh_registry,
                pointer_down,
                playing,
                cmd: &mut cmd,
            };
            // Fullscreen: one tab maximized over the whole window (double-click a tab to
            // toggle). A slim header lets you restore (or press Esc); the dock layout is
            // untouched underneath and comes back exactly as it was.
            if let Some(ft) = *viewer.fullscreen_tab {
                let mut exit = false;
                // A build has nothing to restore TO — no header, and Escape
                // belongs to the game (cursor release), not the layout.
                if !player_mode {
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
                        ui.small("fullscreen — double-click a tab or press Esc to restore");
                    });
                    ui.separator();
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        exit = true;
                    }
                }
                // Scene/Game are transparent (the 3D shows through); every other tab
                // needs an opaque fill so the surface render doesn't bleed behind it.
                if !matches!(ft, EditorTab::Scene | EditorTab::Game) {
                    let bg = ui.style().visuals.panel_fill;
                    ui.painter().rect_filled(ui.available_rect_before_wrap(), 0.0, bg);
                }
                let mut t = ft;
                egui_dock::TabViewer::ui(&mut viewer, ui, &mut t);
                if exit {
                    *viewer.fullscreen_tab = None;
                }
            } else {
                egui_dock::DockArea::new(dock_state)
                    .style(egui_dock::Style::from_egui(ui.style()))
                    .show_inside(ui, &mut viewer);
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
                            "cross-target builds compile the engine for that platform in \
                             the background (first time takes minutes). macOS can't be \
                             cross-built — export from an editor on a Mac.",
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

            // ---- autosave recovery (a newer autosave than the scene file) ----
            if let Some(auto) = &autosave_prompt {
                let age = std::fs::metadata(auto)
                    .and_then(|m| m.modified())
                    .ok()
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
                            "'{scene_name_now}' has an AUTOSAVE newer than its saved file                              (written {age}) — usually the editor closed with unsaved                              changes. Restore it?"
                        ));
                        ui.small("Restoring loads the autosaved version (still unsaved —                                   Ctrl+S to keep it). Discard deletes the autosave.");
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

            // ---- project settings window (project-wide rendering) ----
            egui::Window::new("Project Settings")
                .open(show_project_settings)
                .resizable(false)
                .default_width(280.0)
                .show(ui.ctx(), |ui| {
                    ui.label("Game — what a build ships as");
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("title");
                        let mut t = project.title.clone().unwrap_or_default();
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut t)
                                    .desired_width(170.0)
                                    .hint_text("My Game"),
                            )
                            .changed()
                        {
                            project.title = (!t.trim().is_empty()).then_some(t);
                            want_save_project = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("entry scene");
                        let current = project
                            .entry_scene
                            .clone()
                            .unwrap_or_else(|| "scenes/first.ron".into());
                        let stem = |s: &str| {
                            std::path::Path::new(s)
                                .file_stem()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| s.to_string())
                        };
                        egui::ComboBox::from_id_salt("entry_scene_pick")
                            .width(170.0)
                            .selected_text(stem(&current))
                            .show_ui(ui, |ui| {
                                for s in &scene_files {
                                    if ui
                                        .selectable_label(current == *s, stem(s))
                                        .on_hover_text(s)
                                        .clicked()
                                    {
                                        project.entry_scene = Some(s.clone());
                                        want_save_project = true;
                                    }
                                }
                            });
                    });
                    ui.small("the scene a build boots into — the editor opens it on project load too");

                    ui.add_space(8.0);
                    ui.label("Rendering — applies to every scene");
                    ui.separator();
                    if ui.checkbox(&mut project.retro, "retro pixelization").changed() {
                        want_save_project = true;
                    }
                    if ui
                        .add(egui::Slider::new(&mut project.retro_height, 80u32..=1080).text("pixel rows"))
                        .changed()
                    {
                        want_save_project = true;
                    }
                    if ui.checkbox(&mut project.matter, "SDF matter").changed() {
                        want_save_project = true;
                    }

                    ui.add_space(8.0);
                    ui.small("Post-processing (bloom, vignette, ambient occlusion) moved to each scene's ✨ Post Processing node — select it in the Hierarchy.");

                    ui.add_space(8.0);
                    ui.label("Layers — collision & query groups");
                    ui.separator();
                    ui.small("nodes pick a layer in the Inspector; scripts read/write node.layer and filter raycasts by name");
                    // "Default" is implicit (always bit 0, can't be removed) —
                    // the rows below edit the project's CUSTOM layers.
                    let mut remove_idx: Option<usize> = None;
                    for i in 0..project.layers.len() {
                        ui.horizontal(|ui| {
                            let before = project.layers[i].clone();
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut project.layers[i])
                                    .desired_width(150.0),
                            );
                            if resp.changed() {
                                let after = project.layers[i].clone();
                                // The rename follows through: exception pairs here,
                                // the open scene's nodes via cmd (per keystroke, so
                                // they never detach mid-edit). Other scene FILES
                                // keep the old name — they warn at Play.
                                for (a, b) in project.no_collide.iter_mut() {
                                    if *a == before {
                                        *a = after.clone();
                                    }
                                    if *b == before {
                                        *b = after.clone();
                                    }
                                }
                                cmd.rename_layer = Some((before, after));
                                want_save_project = true;
                            }
                            // Removal is destructive and NOT undoable, so it's a
                            // two-step click: 🗑 arms this row, ✔ commits, ✖ (or
                            // clicking 🗑 on another row) disarms.
                            let arm_id = egui::Id::new("layer-delete-armed");
                            let armed: Option<usize> =
                                ui.ctx().data(|d| d.get_temp(arm_id)).flatten();
                            if armed == Some(i) {
                                ui.small("delete?");
                                if ui.small_button("✔").on_hover_text("yes, remove it — nodes still naming it act as Default (and warn at Play)").clicked() {
                                    remove_idx = Some(i);
                                    ui.ctx().data_mut(|d| d.insert_temp(arm_id, None::<usize>));
                                }
                                if ui.small_button("✖").clicked() {
                                    ui.ctx().data_mut(|d| d.insert_temp(arm_id, None::<usize>));
                                }
                            } else if ui
                                .small_button("🗑")
                                .on_hover_text("remove this layer (asks to confirm)")
                                .clicked()
                            {
                                ui.ctx().data_mut(|d| d.insert_temp(arm_id, Some(i)));
                            }
                        });
                    }
                    if let Some(i) = remove_idx {
                        let name = project.layers.remove(i);
                        project.no_collide.retain(|(a, b)| *a != name && *b != name);
                        want_save_project = true;
                    }
                    let resolved = project.build_layers();
                    ui.horizontal(|ui| {
                        let full = resolved.names.len() >= floptle_core::layers::MAX_LAYERS;
                        let resp = ui.add(
                            egui::TextEdit::singleline(layer_new)
                                .hint_text("new layer…")
                                .desired_width(150.0),
                        );
                        let commit = (resp.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                            || ui.small_button("➕").clicked();
                        if commit && !layer_new.trim().is_empty() {
                            let name = layer_new.trim().to_string();
                            if !full && resolved.index_of(&name).is_none() {
                                project.layers.push(name);
                                want_save_project = true;
                            }
                            layer_new.clear();
                        }
                        if full {
                            ui.small("32-layer max");
                        }
                    });
                    // The collision matrix: an unchecked pair passes through each
                    // other (bodies vs colliders AND unfiltered rays still hit
                    // everything — rays only filter when a script asks).
                    let resolved = project.build_layers();
                    if resolved.names.len() > 1 {
                        ui.add_space(4.0);
                        ui.small("collision matrix — unchecked pairs pass through each other");
                        egui::Grid::new("layer_matrix").spacing([4.0, 2.0]).show(ui, |ui| {
                            ui.label("");
                            for (j, name) in resolved.names.iter().enumerate() {
                                ui.small(format!("{j}")).on_hover_text(name);
                            }
                            ui.end_row();
                            for (i, a) in resolved.names.iter().enumerate() {
                                ui.small(format!("{i} {a}"));
                                for (j, b) in resolved.names.iter().enumerate() {
                                    if j < i {
                                        ui.label("");
                                        continue;
                                    }
                                    let mut on = resolved.collides(i as u8, j as u8);
                                    if ui
                                        .checkbox(&mut on, "")
                                        .on_hover_text(format!("{a} × {b}"))
                                        .changed()
                                    {
                                        if on {
                                            project.no_collide.retain(|(x, y)| {
                                                !((x == a && y == b) || (x == b && y == a))
                                            });
                                        } else {
                                            project.no_collide.push((a.clone(), b.clone()));
                                        }
                                        want_save_project = true;
                                    }
                                }
                                ui.end_row();
                            }
                        });
                    }

                    ui.add_space(6.0);
                    ui.small("saved to assets/project.ron");
                });

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
                            ui.set_max_width(150.0);
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
                            // The SAME node catalog as the Hierarchy's ✚ New and
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
                // The fixed suffix = everything after the FIRST dot, so compound
                // extensions (.prefab.ron, .vfx.ron) ride along whole. Folders
                // have no suffix.
                let ext = if Path::new(path.as_str()).is_dir() {
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
                        if scene_dirty_now {
                            ui.label("The scene has unsaved changes.");
                        } else {
                            ui.label("Quit Floptle?");
                        }
                        ui.horizontal(|ui| {
                            if scene_dirty_now && ui.button("💾 Save & Quit").clicked() {
                                want_save = true;
                                *quit_confirmed = true;
                                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                                close = true;
                            }
                            if ui.button("Quit without saving").clicked() {
                                *quit_confirmed = true;
                                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *show_quit_confirm = false;
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
                            [p] if Path::new(p).is_dir() => {
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
                        ui.small("a flat slab renders perfectly smooth at any size — set \"detail\" in the Terrain tab higher before sculpting bumps into a large one.");
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
                let mut keep = true;
                egui::Window::new("Unsaved changes")
                    .open(&mut keep)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(320.0)
                    .show(ui.ctx(), |ui| {
                        ui.label(format!("Open scene \"{name}\"?"));
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

            // (Terrain tools live in the dockable Terrain tab now; the gizmo paints
            // inside the Scene tab, clipped to its rect.)
        });
        egui.state.handle_platform_output(&window, full_output.platform_output);
        if self.project.retro_height != old_retro_h {
            retro.resize(gpu, self.project.retro_height.max(80));
        }

        // Post-processing (SSAO/bloom/vignette, from the scene's PostProcess node —
        // gathered above) runs at the resolution the scene was composited at: the
        // retro internal res in retro mode (BEFORE the nearest-neighbor upscale, so
        // AO/bloom/vignette land on the same chunky pixel grid as the scene), else
        // full frame res. The stack lazily re-sizes when retro toggles/resizes.
        let post_on = post_settings.any();
        let post_size =
            if self.project.retro { retro.resolution() } else { (gpu.config.width, gpu.config.height) };
        post.configure(gpu, post_size.0, post_size.1, self.project.retro);

        // ---- draw: scene into the retro target, blit, then egui on top ----
        match gpu.acquire() {
            Some(frame) => {
                let (color, depth) = if self.project.retro {
                    if post_on {
                        // Retro + post: scene renders into the (retro-sized) post
                        // input; the chain later writes the retro color target.
                        (post.input_view(), retro.depth_view())
                    } else {
                        (retro.color_view(), retro.depth_view())
                    }
                } else if post_on {
                    // Non-retro + post: render the scene into the post input target.
                    (post.input_view(), gpu.depth_view())
                } else {
                    (&frame.view, gpu.depth_view())
                };
                // `rm_draw` already accounts for the matter toggle + terrain presence;
                // with nothing to raymarch the globals still upload so the raster
                // pass's field group (shadows/AO/proxies) sees this frame's data.
                let raster_clear = if rm_draw {
                    // Opaque depth prepass: primes the depth buffer (early-z kills
                    // hidden raster fragments before their shadow-marching shader
                    // runs) and caps the raymarch at the nearest mesh per pixel.
                    let depth_tex =
                        if self.project.retro { retro.depth_texture() } else { gpu.depth_texture() };
                    if raster.depth_prepass_with(gpu, globals, &instances, &flsl_draws, depth_tex) {
                        raymarch.set_depth_prime(gpu, raster.prepass_view());
                    }
                    raymarch.draw_into_primed(gpu, color, depth, rm);
                    None
                } else {
                    raymarch.upload_globals(gpu, rm);
                    Some(clear.map(|c| c as f64))
                };
                raster.draw_scene_with(
                    gpu, color, depth, globals, &instances, &flsl_draws, raster_clear,
                    Some(raymarch.field_bind()),
                );
                // The reference grid is an editor aid — Scene view only.
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
                // Live particles: after all opaque work (they depth-test against
                // meshes AND raymarched matter), before post/retro — so they're
                // AO'd/bloomed and pixelate with the scene.
                if !vfx_batches.is_empty() {
                    particles.draw(
                        gpu,
                        color,
                        depth,
                        crate::vfx::particle_globals(&cam, aspect, fog_color, fog_params),
                        &vfx_instances,
                        &vfx_batches,
                        raster,
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
                    for (dl, placed, origin, right, down, design_vp) in &ui_world {
                        let rel = floptle_core::math::Vec3::new(
                            (origin[0] - cam.world_position.x) as f32,
                            (origin[1] - cam.world_position.y) as f32,
                            (origin[2] - cam.world_position.z) as f32,
                        );
                        let r3 = floptle_core::math::Vec3::from(*right);
                        let d3 = floptle_core::math::Vec3::from(*down);
                        let mut ui_instances = Vec::new();
                        let mut ui_batches = Vec::new();
                        {
                            let reg = &self.texture_registry;
                            uir.pack(gpu, dl, [0.0, 0.0], 1.0, &mut |p| reg.get(p).copied(), &mut ui_instances, &mut ui_batches);
                        }
                        uir.draw_world(
                            gpu,
                            color,
                            depth,
                            vp_mat.to_cols_array_2d(),
                            floptle_render::UiPlane {
                                origin: [rel.x, rel.y, rel.z],
                                right: *right,
                                down: *down,
                            },
                            &ui_instances,
                            &ui_batches,
                            raster,
                        );
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

                // Post runs BEFORE any retro upscale, at the scene's composited
                // resolution. SSAO reads whichever depth the scene rendered with;
                // in retro mode the chain outputs into the retro color target so
                // the nearest-neighbor blit carries the finished effects up with
                // the same chunky pixels as the scene.
                if post_on {
                    let proj = cam.proj_matrix(aspect);
                    let ssao_frame = floptle_render::SsaoFrame {
                        depth: if self.project.retro { retro.depth_view() } else { gpu.depth_view() },
                        proj: proj.to_cols_array_2d(),
                        inv_proj: proj.inverse().to_cols_array_2d(),
                    };
                    let out = if self.project.retro { retro.color_view() } else { &frame.view };
                    post.run(gpu, &post_settings, Some(&ssao_frame), out);
                }
                if self.project.retro {
                    retro.blit(gpu, &frame);
                }

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
                        uir.pack(
                            gpu,
                            dl,
                            [0.0, 0.0],
                            *scale,
                            &mut |p| reg.get(p).copied(),
                            &mut ui_instances,
                            &mut ui_batches,
                        );
                    }
                    uir.draw(gpu, &frame.view, vp, &ui_instances, &ui_batches, raster);
                }

                // Selection outline: mask the selected object's silhouette (full
                // frame res, so it stays crisp over the retro scene) then edge-detect
                // it onto the frame. Works for meshes and the SDF blob alike.
                let masked = if !mask_mesh.is_empty() {
                    raster.draw_mask(gpu, outline.mask_view(), globals, &mask_mesh);
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
                for id in &full_output.textures_delta.free {
                    egui.renderer.free_texture(id);
                }
                frame.present();
            }
            None => {
                let size = window.inner_size();
                gpu.resize(size.width, size.height);
            }
        }

        if want_save || cmd.save_scene {
            self.save_scene();
        }
        if want_save_project
            && let Err(e) = floptle_scene::save_project(&self.project, &self.project_cfg_path()) {
                eprintln!("  save project failed: {e}");
            }

        self.apply_frame_commands(cmd, frame_pointer_down);
    }

    /// Live syntax check for the active IDE file (drives the red squiggle):
    /// Lua through the script host, `.flsl` through the shader compiler.
    fn check_active_script_syntax(&mut self) {
        self.ide_diag = self.ide.active.and_then(|i| self.ide.open.get(i)).and_then(|f| {
            if f.path.ends_with(".lua") {
                self.script_host.check_syntax(&f.text)
            } else if crate::assets::is_shader(&f.path) {
                Editor::check_flsl_syntax(&f.text)
            } else {
                None
            }
        });
    }

    /// Per-frame GPU sync for SDF matter: upload structurally-changed terrain
    /// volumes + shadow-occluder bakes into the shared 3D atlas (or just the
    /// dabbed region on the fast sculpt path), and refresh the texture palette.
    fn sync_terrain_gpu(&mut self) {
        // Terrain volumes render PER-VOLUME, each at native resolution: moving a
        // terrain needs NO GPU work at all — its f64 anchor is read fresh every frame
        // when the globals are built. Only structural changes (add/edit/delete/resize)
        // re-upload the volume set into the shared 3D atlas. Static collider MESHES
        // join the same atlas as shadow-only occluder volumes (they cast, never draw).
        let occluders_changed = self.refresh_mesh_occluders();
        if self.terrain_gpu_dirty || occluders_changed {
            if let (Some(gpu), Some(raymarch)) = (self.gpu.as_ref(), self.raymarch.as_mut()) {
                // Deterministic slot order (by Matter::Terrain id) so the globals'
                // per-frame fill always matches the atlas layout.
                let mut items: Vec<(u32, Entity)> = self
                    .terrains
                    .keys()
                    .map(|&e| {
                        let id = match self.world.get::<Matter>(e) {
                            Some(Matter::Terrain { id }) => *id,
                            _ => 0,
                        };
                        (id, e)
                    })
                    .collect();
                items.sort_by_key(|(id, _)| *id);
                let entities: Vec<Entity> = items.iter().map(|&(_, e)| e).collect();
                // Occluders upload AFTER the terrains (stable order by asset + name,
                // so identical content always lays out identically).
                let mut occ_items: Vec<(String, Entity)> = self
                    .mesh_occluders
                    .iter()
                    .map(|(&e, (key, _))| {
                        let name =
                            self.world.get::<Name>(e).map(|n| n.0.clone()).unwrap_or_default();
                        (format!("{}\u{1}{name}", key.0), e)
                    })
                    .collect();
                occ_items.sort_by(|a, b| a.0.cmp(&b.0));
                let occ_entities: Vec<Entity> = occ_items.iter().map(|(_, e)| *e).collect();
                let mut baked: Vec<&floptle_field::BakedSdf> =
                    entities.iter().map(|e| &self.terrains[e].baked).collect();
                baked.extend(occ_entities.iter().map(|e| &*self.mesh_occluders[e].1));
                let accepted = raymarch.set_volumes(gpu, &baked);
                let total = entities.len() + occ_entities.len();
                if accepted < total {
                    // Never drop content silently: colliders still work, but say so.
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!(
                            "{} volume(s) (terrain / mesh shadow occluders) exceed the GPU volume budget and won't render or cast (collision is unaffected)",
                            total - accepted
                        ),
                        None,
                    );
                }
                let t_kept = accepted.min(entities.len());
                self.terrain_slots = entities[..t_kept].to_vec();
                self.occluder_slots = occ_entities[..accepted - t_kept].to_vec();
                self.terrain_gpu_dirty = false;
                self.terrain_region_dirty = None; // the full upload supersedes any region
                self.terrain_wire_world.clear(); // terrain changed → rebuild the wireframe
            }
        } else if let Some((e, mn, mx, geom)) = self.terrain_region_dirty.take() {
            // Fast paint/sculpt path: upload only the dabbed voxel box into this
            // terrain's atlas slot — its field maps 1:1 at native resolution.
            if let (Some(gpu), Some(raymarch), Some(t), Some(slot)) = (
                self.gpu.as_ref(),
                self.raymarch.as_mut(),
                self.terrains.get(&e),
                self.terrain_slots.iter().position(|&se| se == e),
            ) {
                raymarch.set_volume_region(gpu, slot, &t.baked, mn, mx);
            }
            if geom {
                // Sculpt moved this terrain's surface — rebuild just its wireframe.
                self.terrain_wire_world.retain(|(we, _)| *we != e);
            }
        }
        // Re-upload the terrain texture palette when it changes. Each slot resolves
        // to a 256² layer (empty / unreadable slots become white so indices align).
        if self.terrain_textures_dirty {
            let layers: Vec<floptle_render::TextureData> = self
                .terrain_textures
                .iter()
                .map(|p| {
                    if !p.is_empty()
                        && let Some(t) = floptle_assets::load_texture_sized(Path::new(p), 256, 256) {
                            return t;
                        }
                    floptle_render::TextureData { pixels: vec![255; 256 * 256 * 4], width: 256, height: 256 }
                })
                .collect();
            if let (Some(gpu), Some(raymarch)) = (self.gpu.as_ref(), self.raymarch.as_mut()) {
                raymarch.set_terrain_textures(gpu, &layers);
            }
            self.terrain_textures_dirty = false;
        }
    }

    /// (Re)upload the skybox equirect when the Skybox node's texture changes.
    fn sync_sky_texture(&mut self) {
        // Re-upload the skybox texture when the skybox node's texture path changes.
        let sky_tex_path = self.world.query::<Matter>().find_map(|(_, m)| match m {
            Matter::Skybox { texture, .. } => texture.clone(),
            _ => None,
        });
        if sky_tex_path != self.sky_texture_loaded {
            let data = sky_tex_path.as_ref().and_then(|p| floptle_assets::load_texture(Path::new(p)));
            if let (Some(gpu), Some(raymarch)) = (self.gpu.as_ref(), self.raymarch.as_mut()) {
                raymarch.set_sky_texture(gpu, data.as_ref());
            }
            self.sky_texture_loaded = sky_tex_path;
        }
    }

    /// Frame-time smoothing: SNAP the measured dt to the nearest whole multiple
    /// of the display's refresh period when it's close. Under vsync (Fifo) a
    /// frame's true screen time IS a whole number of refresh periods — the
    /// CPU-side measurement just adds 1–3 ms of scheduler noise on top, and
    /// feeding that noise into the fixed-step accumulator moves everything the
    /// interpolation renders by `velocity × noise` every frame (the moving-
    /// jitter that came and went with window mode / load). The residual error
    /// is banked and folded back a hair per frame, so long-term time stays
    /// wall-clock exact (a fast clip on the bank absorbs one-off stalls).
    fn smooth_dt(&mut self, raw: f32) -> f32 {
        // (Re)read the monitor's refresh rate occasionally — cheap, and the
        // window can move between monitors.
        if self.refresh_poll == 0 {
            self.refresh_period = self
                .window
                .as_ref()
                .and_then(|w| w.current_monitor())
                .and_then(|m| m.refresh_rate_millihertz())
                .map(|mhz| 1000.0 / mhz as f32)
                .unwrap_or(0.0);
            self.refresh_poll = 240;
        }
        self.refresh_poll -= 1;
        let period = self.refresh_period;
        if period <= 0.0 || raw <= 0.0 {
            return raw;
        }
        let n = (raw / period).round();
        // Snap only when the measurement is close to a whole vsync count (and
        // at least one) — a giant hitch or an uncapped frame passes through.
        if n < 1.0 || (raw - n * period).abs() > period * 0.12 {
            self.dt_snap_error = self.dt_snap_error.clamp(-period, period);
            return raw;
        }
        self.dt_snap_error += raw - n * period;
        // Fold the banked truth back in slowly (≤0.25 ms/frame): time stays
        // exact without re-introducing per-frame noise.
        let give = self.dt_snap_error.clamp(-0.00025, 0.00025);
        self.dt_snap_error -= give;
        n * period + give
    }

    /// Advance the frame clock: `dt`/`elapsed`, the editor fly-camera (unless
    /// the Game view owns input), the smoothed FPS title, and the F-key focus
    /// glide. Returns `(dt, elapsed)`.
    fn advance_clock(&mut self, game_focused: bool) -> (f32, f32) {
        let now = Instant::now();
        let raw_dt = self.last.map(|l| (now - l).as_secs_f32()).unwrap_or(0.0);
        self.last = Some(now);
        let dt = self.smooth_dt(raw_dt);
        let elapsed = self.started.map(|s| (now - s).as_secs_f32()).unwrap_or(0.0);
        // Don't drive the editor (Scene) camera while the Game viewport is focused — that
        // input belongs to the game (e.g. the mouse is over the Game view in split mode).
        if !game_focused {
            self.camera.update(&self.input, dt);
            // Mouse wheel dollies the editor camera when hovering the Scene viewport.
            // Consumed here (before scripts / finish_input_frame): scripts only read
            // scroll while the Game view is focused, so this never steals game input.
            if self.input_scroll != 0.0 && self.cursor_over_scene() {
                self.camera.dolly(self.input_scroll);
                self.input_scroll = 0.0;
            }
        }

        // FPS in the window title (smoothed, refreshed a few times a second).
        if dt > 0.0 {
            let inst = 1.0 / dt;
            self.fps = if self.fps > 0.0 { self.fps * 0.9 + inst * 0.1 } else { inst };
            self.fps_timer += dt;
            if self.fps_timer >= 0.4 {
                self.fps_timer = 0.0;
                if let Some(window) = self.window.as_ref() {
                    window.set_title(&format!(
                        "Floptle Editor — {}{} — {:.0} fps",
                        self.scene_name,
                        if self.scene_dirty { " •" } else { "" },
                        self.fps
                    ));
                }
            }
        }

        // Glide an in-progress focus (F). Any WASD/Space/C input hands control back
        // to the user immediately. Only the camera position eases; the view angle is
        // left to mouse-look, so you can look around mid-glide.
        if self.focus_anim.is_some() {
            let moving = self.input.forward
                || self.input.back
                || self.input.left
                || self.input.right
                || self.input.up
                || self.input.down;
            if moving {
                self.focus_anim = None;
            } else {
                let (from, to, t) = {
                    let a = self.focus_anim.as_mut().unwrap();
                    a.t += dt;
                    (a.from, a.to, a.t)
                };
                let k = (t / FOCUS_SECS).clamp(0.0, 1.0);
                let eased = 1.0 - (1.0 - k).powi(3); // ease-out cubic
                self.camera.position = from.lerp(to, eased as f64);
                if k >= 1.0 {
                    self.focus_anim = None;
                }
            }
        }
        (dt, elapsed)
    }

    /// One play-mode step (ordering: scripts → animation → physics): feed body
    /// state / input / assets / animator info to the script host, run the Lua
    /// scripts, apply their writes (models, mouse lock, velocities, heights),
    /// advance the animators, then step the sim. Clears stale script errors
    /// when not playing.
    fn play_step(&mut self, dt: f32, game_focused: bool) {
        // Play mode: advance the (pausable) script clock and run the Lua scripts
        // attached to nodes (ADR-0003). Scripts hot-reload as their files change.
        if self.playing {
            // A scene transition a script queued LAST frame happens first —
            // at a frame boundary, never mid-frame under the scripts that
            // asked for it (offline/host = switch; joined client = refused).
            if let Some(req) = self.pending_scene.take() {
                self.perform_scene_request(&req);
            }
            // Pausing freezes the clock AND the frame delta scripts see, so
            // dt-driven motion stops too (not just `time`-driven motion).
            let sdt = if self.paused { 0.0 } else { dt };
            self.play_t += sdt;
            // Direct field access (not the `scripts_dir()` method) so we don't take
            // a whole-`self` borrow while gpu/egui are mutably borrowed here.
            let dir = self.project_root.join("scripts");
            // Feed the physics body state to scripts so they can read node.grounded and
            // read/write node.vx/vy/vz (a script sets velocity, physics then integrates).
            if let Some(sim) = self.sim.as_ref() {
                let mut states = HashMap::new();
                for (e, vel, up, grounded, height) in sim.body_states() {
                    states.insert(
                        e.index(),
                        floptle_script::BodyState {
                            vel: [vel.x, vel.y, vel.z],
                            up: [up.x, up.y, up.z],
                            grounded,
                            height,
                        },
                    );
                }
                self.script_host.set_bodies(states);
            }
            // The active camera's view angles ride every input snapshot
            // (`input.aimYaw()`): camera-relative movement stays deterministic
            // under prediction because the aim IS part of the input command.
            let aim = self.world.query::<Matter>().find_map(|(e, m)| {
                matches!(m, Matter::Camera { active: true, .. }).then(|| {
                    let wt = floptle_core::world_transform(&self.world, e);
                    let (yaw, pitch, _) =
                        wt.rotation.to_euler(floptle_core::math::EulerRot::YXZ);
                    [yaw, pitch]
                })
            });
            // Game-UI interaction (buttons + draggable sliders): detect hover/press/
            // click against this frame's layout BEFORE scripts run, so a dragged
            // slider's value is already in the ECS when `update` reads it. The hook
            // events dispatch to Lua right after the run.
            self.ui_interact();
            // Feed the player input to scripts (the Lua `input` API) — but ONLY while the
            // Game view is focused. In the Scene view you're editing, not playing, so the
            // game gets neutral input (the character stops moving) even though physics
            // keeps simulating.
            let frame_input = if game_focused {
                floptle_script::InputSnapshot {
                    keys_down: self.input_keys.clone(),
                    keys_pressed: self.input_keys_pressed.clone(),
                    keys_released: self.input_keys_released.clone(),
                    mouse: self.cursor.map(|c| (c.x, c.y)).unwrap_or((0.0, 0.0)),
                    mouse_delta: self.input_mouse_delta,
                    scroll: self.input_scroll,
                    buttons_down: self.input_buttons,
                    buttons_pressed: self.input_buttons_pressed,
                    aim,
                }
            } else {
                floptle_script::InputSnapshot { aim, ..Default::default() }
            };
            self.script_host.set_input(frame_input.clone());
            // Lend the sim's colliders to scripts so `raycast(...)` works this frame
            // (physics doesn't step until after scripts, so this is safe). The sim
            // origin rides along so ray coordinates convert world ↔ sim frame.
            if let Some(sim) = self.sim.as_mut() {
                self.script_host
                    .set_colliders(std::mem::take(&mut sim.world.colliders), sim.world.origin);
            }
            // …and the dynamic bodies' hulls (copies), so rays can hit
            // players/crates and identify the node (`hit.node`).
            if let Some(sim) = self.sim.as_ref() {
                self.script_host.set_hulls(sim.body_hulls(&self.world));
            }
            // Lend the asset root (for `assets.getFile/getContents`) and the material
            // presets (so `node.material = "Gold"` resolves) for this frame's scripts.
            self.script_host.set_project_root(self.project_root.clone());
            // The running scene's name, for `scene.current()`.
            self.script_host.set_scene_name(&self.scene_name);
            self.script_host.set_materials(
                self.materials.iter().map(|(n, d)| (n.clone(), d.to_material())).collect(),
            );
            // Feed each animator's state (layers/current/time) so scripts can read
            // anim:state()/:time()/:clips() this frame.
            self.script_host.set_anim_info(anim::build_info(&self.anim));
            // Feed each particle node's state so scripts can read
            // node:particles():isPlaying()/:alive() this frame.
            self.script_host.set_vfx_info(self.vfx.script_info(&self.world));
            // Feed sound playback state so scripts can read sound:isPlaying()/
            // :position() this frame.
            self.script_host.set_audio_info(self.audio.script_info());
            self.script_host.run(&mut self.world, &dir, sdt, self.play_t);
            // UI hook events (clicked / hoverStart / …) fire against the run's
            // fresh scene mirror, with their own write flush.
            let ui_events = std::mem::take(&mut self.ui_events);
            self.script_host.run_ui_hooks(&mut self.world, &ui_events);
            self.script_errors = self.script_host.errors().to_vec();
            // Apply any mouse lock/unlock a script requested this frame (grab + hide the
            // cursor for free-look, or release it). The state persists until changed/Stop.
            if let Some(want) = self.script_host.take_mouse_lock() {
                self.script_mouse_lock = want;
                if let Some(window) = self.window.as_ref() {
                    self.cursor_lock_soft = grab_cursor(window, want);
                }
            }
            // A `scene.load(...)` from this frame's scripts: queued, performed
            // at the top of the next frame (see above).
            if let Some(req) = self.script_host.take_scene_request() {
                self.pending_scene = Some(req);
            }
            // GPU-load any models a script swapped via `node.model` (the Matter is
            // already updated by run; re-importing here means the new mesh renders
            // THIS frame).
            self.load_script_swapped_models();
            // Animation: bind + apply queued Lua animator commands + advance every
            // controller (ordering: scripts → animation → physics), then dispatch
            // fired clip events back into the node's scripts.
            let anim_cmds = self.script_host.take_anim_commands();
            let fired = anim::advance_animators(
                &mut self.anim,
                &mut self.world,
                &self.mesh_registry,
                sdt,
                anim_cmds,
            );
            for (eid, func) in fired {
                self.script_host.call_function(&mut self.world, eid, &func);
            }
            // Animator warnings (e.g. play() on a state name the controller
            // doesn't have) surface in the Console, once per name.
            for msg in self.anim.warnings.drain(..) {
                self.console.push(floptle_script::LogLevel::Warn, msg, None);
            }
            // Event handlers can log/raise — surface those in the Scripting tab
            // (run() cleared + snapshotted errors before the dispatch above).
            if !self.script_host.errors().is_empty() {
                self.script_errors = self.script_host.errors().to_vec();
            }
            // Apply script velocity writes, then run the GAMEPLAY TICK loop (docs/
            // netcode-design.md §3): each banked 60 Hz tick runs `fixedUpdate` with a
            // per-tick input snapshot, applies its writes, and steps physics exactly one
            // tick — the deterministic unit netcode snapshots/prediction share. Rendered
            // transforms interpolate across the current tick (anti-stutter). Gravity is
            // rebuilt from the scene's GravityVolume node(s) every frame (cheap scan) so
            // tweaking mode/strength/radius takes effect immediately. The active camera
            // is the floating-origin focus: drift far enough and the sim recenters on it.
            let focus = self.world.query::<Matter>().find_map(|(e, m)| {
                matches!(m, Matter::Camera { active: true, .. })
                    .then(|| floptle_core::world_transform(&self.world, e).translation)
            });
            if let Some(sim) = self.sim.as_mut() {
                sim.world.gravity = Self::build_gravity_field(&self.world, sim.world.origin);
                sim.world.colliders = self.script_host.take_colliders(); // reclaim before stepping
                // Live Inspector edits: re-read RigidBody tunables (shape/size, friction,
                // restitution, gravity, pos/rot locks) into the running bodies each frame —
                // no teleport.
                sim.sync_dynamic_params(&self.world);
                // `update`'s velocity/height writes apply before the first tick, so
                // frame-pass controllers (the pre-fixedUpdate style) behave as before.
                for (eid, v) in self.script_host.take_body_changes() {
                    sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
                }
                for (eid, h) in self.script_host.take_body_height_changes() {
                    sim.set_body_height(eid, h);
                }
            }
            if self.sim.is_some() {
                self.game_tick.accumulate(sdt);
                while self.game_tick.tick() {
                    self.game_tick_no += 1;
                    // Per-tick input: consume the tick accumulators (edges bank between
                    // ticks so a between-tick press is never lost). Neutral when the
                    // Game view isn't focused — but still consumed, so stale edges
                    // don't fire on refocus.
                    let snap = if game_focused {
                        floptle_script::InputSnapshot {
                            keys_down: self.input_keys.clone(),
                            keys_pressed: std::mem::take(&mut self.tick_keys_pressed),
                            keys_released: std::mem::take(&mut self.tick_keys_released),
                            mouse: self.cursor.map(|c| (c.x, c.y)).unwrap_or((0.0, 0.0)),
                            mouse_delta: std::mem::take(&mut self.tick_mouse_delta),
                            scroll: std::mem::take(&mut self.tick_scroll),
                            buttons_down: self.input_buttons,
                            buttons_pressed: std::mem::take(&mut self.tick_buttons_pressed),
                            aim,
                        }
                    } else {
                        self.tick_keys_pressed.clear();
                        self.tick_keys_released.clear();
                        self.tick_mouse_delta = (0.0, 0.0);
                        self.tick_scroll = 0.0;
                        self.tick_buttons_pressed = [false; 3];
                        floptle_script::InputSnapshot { aim, ..Default::default() }
                    };
                    // Keep what the scripts saw: prediction records + ships it.
                    self.last_tick_input = snap.clone();
                    self.script_host.set_input(snap);
                    if let Some(sim) = self.sim.as_mut() {
                        // Fresh body state for THIS tick (post previous tick's physics).
                        let mut states = HashMap::new();
                        for (e, vel, up, grounded, height) in sim.body_states() {
                            states.insert(
                                e.index(),
                                floptle_script::BodyState {
                                    vel: [vel.x, vel.y, vel.z],
                                    up: [up.x, up.y, up.z],
                                    grounded,
                                    height,
                                },
                            );
                        }
                        self.script_host.set_bodies(states);
                        // Lend colliders so `raycast(...)` works inside `fixedUpdate` too.
                        self.script_host.set_colliders(
                            std::mem::take(&mut sim.world.colliders),
                            sim.world.origin,
                        );
                        self.script_host.set_hulls(sim.body_hulls(&self.world));
                    }
                    // `time` on the fixed pass is the deterministic tick clock.
                    let tick_time = self.game_tick_no as f32 * self.game_tick.step;
                    // Real hosting: each REMOTE player's Predicted node runs
                    // with ITS OWNER's replayed input for this tick — the
                    // one-script model (§6), server side. Those nodes are
                    // filtered out of the global passes; run_*_for bypasses
                    // the filters. The host's own input is restored after.
                    if !self.net_remote_predicted.is_empty() && self.net_server.is_some() {
                        if let Some(s) = self.net_server.as_mut() {
                            // Tick-start pump so this tick's freshest client
                            // inputs are in the buffer before scripts consume.
                            s.pump_server(&self.world, self.game_tick_no);
                        }
                        for (e, owner) in self.net_remote_predicted.clone() {
                            let Some(s) = self.net_server.as_mut() else { break };
                            let inp = s.input_for(owner, self.game_tick_no);
                            self.script_host.set_input(floptle_script::net_to_input(&inp));
                            self.script_host.run_frame_for(
                                &mut self.world,
                                e.index(),
                                self.game_tick.step,
                                tick_time,
                            );
                            self.script_host.run_fixed_for(
                                &mut self.world,
                                e.index(),
                                self.game_tick.step,
                                tick_time,
                            );
                        }
                        self.script_host.set_input(self.last_tick_input.clone());
                    }
                    // A predicted node's `update` rides the tick clock (its
                    // frame pass is filtered) so client + server integrate the
                    // same controller identically — see net.rs.
                    if let Some((pe, _)) = &self.net_predictor {
                        let pe = pe.index();
                        self.script_host.run_frame_for(
                            &mut self.world,
                            pe,
                            self.game_tick.step,
                            tick_time,
                        );
                    }
                    self.script_host.run_fixed(&mut self.world, self.game_tick.step, tick_time);
                    if let Some(sim) = self.sim.as_mut() {
                        sim.world.colliders = self.script_host.take_colliders(); // reclaim
                        // Apply the tick's writes, then step physics exactly one tick.
                        sim.sync_dynamic_params(&self.world);
                        for (eid, v) in self.script_host.take_body_changes() {
                            sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
                        }
                        for (eid, h) in self.script_host.take_body_height_changes() {
                            sim.set_body_height(eid, h);
                        }
                        sim.step_tick(self.game_tick.step, focus);
                    }
                    // Collision / trigger events from THIS tick, dispatched to
                    // BOTH nodes' scripts: `onCollisionEnter/Stay/Exit(node,
                    // other, hit)` for solid contacts (incl. body-vs-body),
                    // `onTriggerEnter/Stay/Exit` when a Trigger collider is
                    // involved. Events fire where physics runs (offline, the
                    // server, a predicted owner) — never during replays.
                    let touches =
                        self.sim.as_mut().map(|s| s.take_touch_events()).unwrap_or_default();
                    for ev in touches {
                        use floptle_physics::TouchPhase;
                        let func = match (ev.sensor, ev.phase) {
                            (true, TouchPhase::Enter) => "onTriggerEnter",
                            (true, TouchPhase::Stay) => "onTriggerStay",
                            (true, TouchPhase::Exit) => "onTriggerExit",
                            (false, TouchPhase::Enter) => "onCollisionEnter",
                            (false, TouchPhase::Stay) => "onCollisionStay",
                            (false, TouchPhase::Exit) => "onCollisionExit",
                        };
                        let p = [ev.point.x, ev.point.y, ev.point.z];
                        let n = [ev.normal.x, ev.normal.y, ev.normal.z];
                        self.script_host.call_touch(&mut self.world, ev.a, func, ev.b, p, n);
                        self.script_host.call_touch(&mut self.world, ev.b, func, ev.a, p, n);
                    }
                    // A handler's body writes (knockback, bounce) land THIS
                    // tick, not the next one.
                    if let Some(sim) = self.sim.as_mut() {
                        for (eid, v) in self.script_host.take_body_changes() {
                            sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
                        }
                    }
                    // Netcode rides the tick (docs/netcode-design.md §9): session
                    // commands, server snapshot send, ghost-client apply, RPC/event
                    // dispatch — all after physics, all on the deterministic clock.
                    self.net_tick(self.game_tick_no);
                }
                if let Some(sim) = self.sim.as_mut() {
                    // Render this frame partway into the current tick: smooth at any fps.
                    sim.writeback_interpolated(&mut self.world, self.game_tick.alpha());
                }
                // Prediction corrections render as a decaying nudge, not a snap:
                // the rendered transform carries the (shrinking) error offset.
                if let Some((pe, pred)) = &self.net_predictor
                    && pred.error_offset != [0.0; 3]
                    && let Some(tr) = self.world.get_mut::<Transform>(*pe)
                {
                    tr.translation +=
                        floptle_core::math::DVec3::from_array(pred.error_offset);
                }
            }
            // `lateUpdate` — the CAMERA pass: after physics and the interpolated
            // writeback, so followers sample this frame's FINAL poses. (A camera
            // positioned in `update` reads LAST frame's pose — a follow error of
            // velocity × dt that turns frame-time noise into visible jitter.)
            // The tick loop overwrote the input snapshot with per-tick state —
            // restore the FRAME snapshot first, so mouse/scroll reads in
            // lateUpdate see this frame's input, not the last tick's leftovers.
            self.script_host.set_input(frame_input);
            // Re-lend the sim's state for the late pass: the tick loop reclaimed
            // the colliders before stepping, so without this an orbit camera's
            // wall raycast would see NO static geometry. Hulls and body state are
            // refreshed too — post-step, so `raycast` hits bodies where they
            // rendered and `node.vx/grounded` reads this frame's final values.
            if let Some(sim) = self.sim.as_mut() {
                self.script_host
                    .set_colliders(std::mem::take(&mut sim.world.colliders), sim.world.origin);
                let mut states = HashMap::new();
                for (e, vel, up, grounded, height) in sim.body_states() {
                    states.insert(
                        e.index(),
                        floptle_script::BodyState {
                            vel: [vel.x, vel.y, vel.z],
                            up: [up.x, up.y, up.z],
                            grounded,
                            height,
                        },
                    );
                }
                self.script_host.set_bodies(states);
            }
            if let Some(sim) = self.sim.as_ref() {
                self.script_host.set_hulls(sim.body_hulls(&self.world));
            }
            self.script_host.run_late(&mut self.world, sdt, self.play_t);
            if let Some(sim) = self.sim.as_mut() {
                sim.world.colliders = self.script_host.take_colliders(); // reclaim
                // A velocity write from lateUpdate still lands (applied next
                // step) — but the camera pass shouldn't steer bodies; drain so
                // nothing double-applies with next frame's `update` writes.
                for (eid, v) in self.script_host.take_body_changes() {
                    sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
                }
            }
            // Surface fixedUpdate errors alongside the frame pass's.
            if !self.script_host.errors().is_empty() {
                self.script_errors = self.script_host.errors().to_vec();
            }
            // Script debug gizmos queued this frame — by `update` AND `fixedUpdate` —
            // drained once here (drawn by the viewport overlay), plus the multiplayer
            // harness's ghost-client markers.
            self.script_gizmos = self.script_host.take_gizmos();
            self.net_ghost_gizmos();
            // Prefab spawns + node destroys scripts queued this frame — applied
            // before attachments/particles so a spawned node is complete (body,
            // meshes, callback-configured) within this same frame.
            self.apply_script_spawns();
            // Bone attachments resolve AFTER physics: physics moves the mesh ROOT (a
            // character body), while animation only bent the bones — so a weapon on a
            // bone must read the POST-physics mesh world or it swims a frame behind.
            anim::resolve_attachments(&self.anim, &mut self.world, &self.mesh_registry);
            // Particles tick last: emitter node transforms are final for the frame
            // (scripts → animation → physics → attachments → particles). Apply any
            // play/stop/restart a script queued this frame first, so it lands now.
            let vfx_cmds = self.script_host.take_vfx_commands();
            self.vfx.apply_script_commands(&self.world, vfx_cmds);
            // Fire-and-forget one-shots a script requested this frame (spawnEffect).
            for (key, p) in self.script_host.take_spawn_effects() {
                self.vfx.spawn_detached(&key, floptle_core::math::DVec3::from_array(p));
            }
            self.vfx.advance(&self.world, sdt);
            // Audio: apply queued Lua commands, then tick voices against the
            // final node transforms (same ordering rationale as particles).
            let audio_cmds = self.script_host.take_audio_commands();
            let root = self.project_root.clone();
            if !audio_cmds.is_empty() {
                // `node:sound():setClip(...)` mutates the component (a string —
                // outside the numeric mirror); the diff in advance() restarts
                // the voice on the new clip.
                for cmd in &audio_cmds {
                    if let floptle_script::AudioCmd::SourceSetClip { ent, clip } = cmd {
                        let target = self
                            .world
                            .query::<floptle_audio::AudioSource>()
                            .find(|(e, _)| e.index() == *ent)
                            .map(|(e, _)| e);
                        if let Some(e) = target
                            && let Some(src) = self.world.get_mut::<floptle_audio::AudioSource>(e)
                        {
                            src.clip = clip.clone();
                        }
                    }
                }
                self.audio.apply_script_commands(&self.world, &root, audio_cmds);
            }
            // Listener = the active camera's ears.
            let listener = self
                .world
                .query::<Matter>()
                .find(|(_, m)| matches!(m, Matter::Camera { active: true, .. }))
                .map(|(e, _)| {
                    let wt = floptle_core::world_transform(&self.world, e);
                    floptle_audio::Listener {
                        position: wt.translation,
                        forward: (wt.rotation * floptle_core::math::Vec3::NEG_Z).as_dvec3(),
                        right: (wt.rotation * floptle_core::math::Vec3::X).as_dvec3(),
                    }
                })
                .unwrap_or_default();
            for e in self.audio.advance(&self.world, &root, listener) {
                // EndBehavior::Destroy — the sound finished, its node goes too.
                self.world.despawn(e);
                self.selection.retain(|s| *s != e);
            }
        } else if !self.script_errors.is_empty() {
            self.script_errors.clear();
        }
    }

    /// GPU-load models a script swapped via `node.model` so they render this
    /// frame (rigged import first, static fallback).
    pub(crate) fn load_script_swapped_models(&mut self) {
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return;
        };
        for (_eid, path) in self.script_host.take_model_changes() {
            if !self.mesh_registry.contains_key(&path) {
                // Rigged first (animated glTF keeps its node tree + clips).
                match floptle_assets::import_rigged(std::path::Path::new(&path)) {
                    Ok(Some(model)) => {
                        let parts = model
                            .parts
                            .iter()
                            .map(|p| raster.register(gpu, &p.mesh, p.texture.map(|i| &model.textures[i])))
                            .collect();
                        let rig = anim::rig_from_model(&model);
                        self.mesh_registry.insert(
                            path.clone(),
                            MeshAsset { parts, size: model.size, rig: Some(rig) },
                        );
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("  rig swap-import {path} failed ({e}); trying static"),
                }
                match floptle_assets::gltf_import::import(std::path::Path::new(&path)) {
                    Ok(model) => {
                        let parts = model
                            .parts
                            .iter()
                            .map(|p| raster.register(gpu, &p.mesh, p.texture.map(|i| &model.textures[i])))
                            .collect();
                        self.mesh_registry
                            .insert(path.clone(), MeshAsset { parts, size: model.size, rig: None });
                    }
                    Err(e) => eprintln!("  swap-import {path} failed: {e}"),
                }
            }
        }
    }

    /// End-of-input bookkeeping: clear the per-frame key/button edges, re-pin a
    /// CONFINE-only cursor grab, and drain script logs into the Console.
    fn finish_input_frame(&mut self) {
        // Clear per-frame input edges after scripts consumed them.
        self.input_keys_pressed.clear();
        self.input_keys_released.clear();
        self.input_buttons_pressed = [false; 3];
        self.input_mouse_delta = (0.0, 0.0);
        self.input_scroll = 0.0;
        // The per-TICK accumulators are consumed by the gameplay-tick loop while
        // playing; outside play they'd grow unbounded, so drain them here instead.
        if !self.playing {
            self.tick_keys_pressed.clear();
            self.tick_keys_released.clear();
            self.tick_buttons_pressed = [false; 3];
            self.tick_mouse_delta = (0.0, 0.0);
            self.tick_scroll = 0.0;
        }
        // A CONFINE-only grab (X11 has no OS cursor lock) still lets the pointer
        // wander inside the window — pin it ourselves while a look/pan/lock/trap is
        // active. Look/pan read RAW device motion, so re-centering never pollutes
        // the deltas. A trapped Game cursor re-centers to the GAME rect (not the
        // window) so a Confined pointer stays inside the viewport it's playing in.
        if self.cursor_lock_soft
            && (self.script_mouse_lock || self.input.looking || self.panning || self.game_trap)
            && let Some(window) = self.window.as_ref()
        {
            let sz = window.inner_size();
            let (cx, cy) = match (self.game_trap, self.game_rect) {
                (true, Some(r)) => {
                    let ppp = self.egui.as_ref().map(|e| e.ctx.pixels_per_point()).unwrap_or(1.0);
                    ((r.center().x * ppp) as u32, (r.center().y * ppp) as u32)
                }
                _ => (sz.width / 2, sz.height / 2),
            };
            let _ = window.set_cursor_position(winit::dpi::PhysicalPosition::new(cx, cy));
        }
        // Safety: never stay trapped once play stops (e.g. Stop while trapped, or a
        // layout change hid the Game tab). Escape/Stop already handle the common path.
        if self.game_trap && !self.playing {
            self.game_trap = false;
            if let Some(window) = self.window.as_ref() {
                self.cursor_lock_soft = grab_cursor(window, false);
            }
        }
        // Drain any script logs/errors into the Console (consecutive dups merge).
        for l in self.script_host.drain_logs() {
            self.console.push(l.level, l.msg, l.source);
        }
    }

    /// Apply the frame's deferred [`EditorCmd`] intents — runs after every
    /// gpu/egui borrow has ended, so `self` is fully free again.
    fn apply_frame_commands(&mut self, mut cmd: EditorCmd, frame_pointer_down: bool) {
        // ---- apply UI commands (gpu/egui borrows have ended; `self` is free) ----
        if let Some(action) = cmd.project_action {
            match action {
                ProjectAction::New(p) => self.new_project(PathBuf::from(p)),
                ProjectAction::Open(p) => {
                    let path = PathBuf::from(p);
                    if path.is_dir() {
                        self.open_project(path);
                    } else {
                        eprintln!("  open project: not a folder: {}", path.display());
                    }
                }
                ProjectAction::Close => self.close_project(),
            }
        }
        if let Some(tool) = cmd.set_tool {
            self.set_tool(tool);
        }
        if let Some(path) = cmd.open_script {
            self.ide.open_file(&path);
        }
        if let Some(path) = cmd.open_script_pref {
            self.open_script_preferred(&path);
        }
        if let Some((name, line)) = cmd.open_log_source {
            self.open_source_at(&name, line);
        }
        if cmd.focus_scripting
            && let Some(dock) = self.dock_state.as_mut() {
                focus_scripting_tab(dock);
            }
        if cmd.close_menu {
            self.context_menu = None;
        }
        if cmd.undo {
            self.undo();
        }
        if cmd.redo {
            self.redo();
        }
        if cmd.copy {
            self.copy_selected();
        }
        if cmd.paste {
            self.paste();
        }
        if cmd.duplicate {
            self.duplicate_selected();
        }
        if cmd.delete {
            self.delete_selected();
        }
        if let Some(m) = cmd.add {
            let name = match &m {
                MatterDoc::Primitive { shape: ShapeDoc::Sphere, .. } => "Sphere",
                MatterDoc::Primitive { shape: ShapeDoc::Cube, .. } => "Cube",
                MatterDoc::Primitive { shape: ShapeDoc::Capsule, .. } => "Capsule",
                MatterDoc::Primitive { shape: ShapeDoc::Plane, .. } => "Plane",
                MatterDoc::Blob { .. } => "Blob",
                MatterDoc::Mesh { .. } => "Mesh",
                MatterDoc::Empty => "Group",
                MatterDoc::Terrain { .. } => "Terrain",
                MatterDoc::Camera { .. } => "Camera",
                MatterDoc::PointLight { .. } => "Point Light",
                MatterDoc::GravityVolume { .. } => "Gravity Volume",
                MatterDoc::FieldShape { .. } => "Field Shape",
                MatterDoc::Skybox { .. } => "Skybox",
                MatterDoc::PostProcess { .. } => "Post Processing",
            };
            self.add_node(name, m);
        }
        if let Some(what) = cmd.add_ui {
            self.add_ui_node(what);
        }
        // Latch "pointer on a UI overlay interact" for the raw LMB handler (which
        // runs between frames): while set, presses belong to egui, not pick/gizmo.
        self.ui_overlay_hot = cmd.ui_hot;
        // A UI move/resize drag is one coalesced undo step (banked on the first
        // frame of the gesture via the pre-edit frame_snapshot; closed when the
        // pointer releases and `editing` resets). Without this, dragging/resizing
        // a UI element in the Scene view left no undo point.
        if cmd.ui_move.is_some() || cmd.ui_resize.is_some() {
            self.begin_edit();
        }
        if let Some((idx, d)) = cmd.ui_move {
            let ent = self.world.query::<Transform>().map(|(e, _)| e).find(|e| e.index() == idx);
            if let Some(e) = ent
                && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
            {
                match &mut spec.place {
                    floptle_ui::Place::Free { pos } => {
                        pos[0] += d[0];
                        pos[1] += d[1];
                    }
                    floptle_ui::Place::Pin { offset, .. } => {
                        offset[0] += d[0];
                        offset[1] += d[1];
                    }
                }
                self.world.insert(e, spec);
            }
        }
        // Rect-tool resize: grow/shrink toward the dragged side, keeping the
        // OPPOSITE edge visually fixed — Free positions and Pin offsets get the
        // exact compensation for their placement mode.
        if let Some((idx, dsize, from_min, cur)) = cmd.ui_resize {
            let ent = self.world.query::<Transform>().map(|(e, _)| e).find(|e| e.index() == idx);
            if let Some(e) = ent
                && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
            {
                for a in 0..2 {
                    if dsize[a] == 0.0 {
                        continue;
                    }
                    let old = cur[a].max(1.0);
                    let new = (old + dsize[a]).max(1.0);
                    let d = new - old;
                    spec.size[a] = match spec.size[a] {
                        // % keeps tracking the parent (scaled proportionally);
                        // px adjusts; fit/grow become concrete px on first drag.
                        floptle_ui::Size::Pct(p) => floptle_ui::Size::Pct(p * new / old),
                        floptle_ui::Size::Fixed(v) => floptle_ui::Size::Fixed((v + d).max(1.0)),
                        _ => floptle_ui::Size::Fixed(new),
                    };
                    match &mut spec.place {
                        floptle_ui::Place::Free { pos } => {
                            if from_min[a] {
                                pos[a] -= d;
                            }
                        }
                        floptle_ui::Place::Pin { anchor, offset } => {
                            let f = anchor.factors()[a];
                            offset[a] += d * if from_min[a] { f - 1.0 } else { f };
                        }
                    }
                }
                self.world.insert(e, spec);
            }
        }
        if cmd.inspector_changed {
            self.begin_edit();
        }
        // Close the undo-coalescing session whenever the pointer isn't held. A drag
        // (gizmo, DragValue, UI move) keeps the button down across frames, so it stays
        // ONE step; but a discrete edit (checkbox, combo pick, typed value) releases
        // the button, so this frees `editing` and the NEXT edit banks its own pre-edit
        // snapshot. Without it, `editing` stuck true after any non-drag edit and every
        // following edit silently coalesced into it — the "undo doesn't work on
        // property edits" bug. (The raw LMB-release handler also clears it; this is the
        // reliable backstop for keyboard/scroll/click edits that skip that path.)
        if !frame_pointer_down {
            self.editing = false;
        }
        // Persist pending animation-asset edits even when their tab is hidden
        // (the tabs flush on draw; this covers edits left behind a tab switch).
        if !frame_pointer_down {
            if self.anim_ui.graph_dirty {
                if let (Some(k), Some(doc)) =
                    (self.anim_ui.graph_key.clone(), self.anim_ui.graph_doc.clone())
                {
                    self.anim.save_controller(&self.project_root, &k, &doc);
                }
                self.anim_ui.graph_dirty = false;
            }
            if self.anim_ui.clip_dirty {
                if let Some((k, d)) = self.anim_ui.clip_doc.clone() {
                    self.anim.save_clip(&self.project_root, &k, &d);
                }
                self.anim_ui.clip_dirty = false;
            }
        }
        if cmd.toggle_play {
            self.toggle_play();
        }
        if cmd.net_host_local {
            self.net_start_hosting();
        }
        if cmd.net_join_local {
            self.net_join_local();
        }
        if cmd.net_play_as_client {
            self.net_play_as_client();
        }
        if cmd.net_stop_session {
            self.net_stop("panel");
        }
        if let Some(port) = cmd.net_host_quic {
            self.net_host_quic(port);
        }
        if let Some(addr) = cmd.net_join_quic {
            let a = addr.trim().to_string();
            if let Some(rest) = a.strip_prefix("relay://") {
                match rest.rsplit_once('/') {
                    Some((raddr, code)) => self.net_join_relay(raddr, code),
                    None => self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!("join \"{a}\": expected relay://host:port/CODE"),
                        None,
                    ),
                }
            } else {
                self.net_join_quic(a.trim_start_matches("quic://"));
            }
        }
        if let Some(addr) = cmd.net_host_relay {
            self.net_host_relay(addr.trim());
        }
        if let Some((dir, target)) = cmd.export_game {
            self.begin_export(dir, target);
        }
        if cmd.toggle_pause {
            self.toggle_pause();
        }
        if let Some(path) = cmd.drop_asset {
            self.drop_asset(&path);
        }
        if let Some((path, e)) = cmd.drop_script_on {
            self.attach_script_file(&path, Some(e));
        }
        if let Some((name, e)) = cmd.attach_named {
            let path = self.scripts_dir().join(format!("{name}.lua"));
            self.attach_script_file(&path.to_string_lossy(), Some(e));
        }
        if let Some(file) = cmd.open_in_editor {
            open_external_editor(&self.external_editor, &self.project_root, &file, 1);
        }
        if let Some(c) = cmd.set_external_editor {
            save_external_editor(&c);
            self.external_editor = c;
        }
        if let Some(v) = cmd.set_prefer_external {
            save_prefer_external(v);
            self.prefer_external_editor = v;
        }
        if let Some((en, tint)) = cmd.set_play_tint {
            save_play_tint(en, tint);
            self.play_tint_enabled = en;
            self.play_tint = tint;
        }
        if cmd.save_grid {
            save_grid(&self.grid);
        }
        if let Some(i) = cmd.set_engine_theme {
            self.engine_theme = i;
            save_theme_index(engine_theme_path(), i);
        }
        if let Some(i) = cmd.set_code_theme {
            self.code_theme = i;
            save_theme_index(code_theme_path(), i);
        }
        if let Some((name, doc)) = cmd.save_material {
            let dir = self.materials_dir();
            let _ = floptle_scene::save_material(&name, &doc, &dir);
            self.materials = self.load_materials();
            self.mat_name_buf.clear();
            self.asset_tree = build_assets(&self.project_root);
        }
        if let Some(e) = cmd.add_material {
            // Seed from the primitive's current color (else white), then customize.
            let base = match self.world.get::<Matter>(e) {
                Some(Matter::Primitive { color, .. }) => *color,
                _ => [1.0, 1.0, 1.0],
            };
            self.record();
            self.world.insert(e, Material::tinted(base));
        }
        if let Some(e) = cmd.remove_material {
            self.record();
            self.world.remove::<Material>(e);
        }
        if let Some(e) = cmd.add_rigidbody {
            self.record();
            self.world.insert(e, floptle_core::RigidBody::default());
            self.rebuild_sim();
        }
        if let Some(e) = cmd.remove_rigidbody {
            self.record();
            self.world.remove::<floptle_core::RigidBody>(e);
            self.rebuild_sim();
        }
        if let Some(e) = cmd.add_networked {
            self.record();
            self.world.insert(e, floptle_core::Replicated::default());
        }
        if let Some((e, key)) = cmd.add_particles {
            self.record();
            self.world.insert(
                e,
                floptle_core::ParticleSystem { asset: key.clone(), play_on_start: true },
            );
            // Attached mid-play: start emitting right away (live-tweak discipline).
            if self.playing {
                self.vfx.spawn(e, &key);
            }
        }
        if let Some(e) = cmd.new_particles {
            // Write a starter effect asset (unique name), attach it, refresh assets.
            let mut n = 0;
            let (key, path) = loop {
                let key = if n == 0 {
                    "vfx/NewEffect".to_string()
                } else {
                    format!("vfx/NewEffect{n}")
                };
                let path = self.project_root.join(format!("{key}{}", floptle_scene::VFX_EXT));
                if !path.exists() {
                    break (key, path);
                }
                n += 1;
            };
            let doc = crate::vfx::starter_effect_doc(key.rsplit('/').next().unwrap_or(&key));
            if let Err(err) = floptle_scene::save_vfx_effect(&doc, &path) {
                eprintln!("  new effect {key} failed: {err}");
            } else {
                self.vfx.rescan(&self.project_root);
                self.asset_tree = build_assets(&self.project_root);
                self.record();
                self.world.insert(
                    e,
                    floptle_core::ParticleSystem { asset: key.clone(), play_on_start: true },
                );
                if self.playing {
                    self.vfx.spawn(e, &key);
                }
                // Fresh effect → straight into the timeline editor.
                cmd.open_particle_editor = Some(key);
            }
        }
        if let Some(e) = cmd.remove_particles {
            self.record();
            self.world.remove::<floptle_core::ParticleSystem>(e);
        }
        if let Some(e) = cmd.add_audio {
            self.record();
            self.world.insert(e, floptle_audio::AudioSource::default());
        }
        if let Some(e) = cmd.remove_audio {
            self.record();
            self.world.remove::<floptle_audio::AudioSource>(e);
        }
        if let Some(key) = cmd.preview_audio.take() {
            let rel = crate::assets::asset_rel_path(&key, &self.project_root).replace('\\', "/");
            let root = self.project_root.clone();
            self.audio.preview(&root, &rel);
        }
        if cmd.mixer_changed {
            // Live-apply: the running play session tracks the edit too (its
            // runtime overlay restarts from the edited graph).
            if self.playing {
                self.audio.runtime_mixer = Some(self.project.mixer.clone());
            }
            let mixer = self.project.mixer.clone();
            self.audio.apply_mixer(&mixer);
        }
        if let Some((e, on)) = cmd.set_mesh_collider {
            self.record();
            if on {
                self.world.insert(e, floptle_core::MeshCollider);
            } else {
                self.world.remove::<floptle_core::MeshCollider>(e);
            }
            self.rebuild_sim();
        }
        if let Some((e, on)) = cmd.set_collidable {
            self.record();
            if on {
                self.world.insert(e, floptle_core::Collidable);
            } else {
                // Clear both the new marker and any legacy mesh-collider marker.
                self.world.remove::<floptle_core::Collidable>(e);
                self.world.remove::<floptle_core::MeshCollider>(e);
            }
            self.rebuild_sim();
        }
        if cmd.rebuild_physics {
            self.rebuild_sim();
        }
        if let Some((e, on)) = cmd.set_trigger {
            self.record();
            if on {
                self.world.insert(e, floptle_core::Trigger);
            } else {
                self.world.remove::<floptle_core::Trigger>(e);
            }
            self.rebuild_sim(); // the sensor flag bakes into the static collider
        }
        if let Some((e, layer)) = cmd.set_layer {
            self.record();
            if layer == floptle_core::layers::DEFAULT_LAYER {
                self.world.remove::<floptle_core::Layer>(e);
            } else {
                self.world.insert(e, floptle_core::Layer(layer));
            }
            // Re-layer the live sim: bodies re-resolve via sync_dynamic_params,
            // but static colliders bake their bit at build — so rebuild.
            self.rebuild_sim();
        }
        if let Some((old, new)) = cmd.rename_layer {
            // The open scene's nodes follow a Project-Settings layer rename
            // (fires per keystroke, so they never detach mid-edit). "Default"
            // as the new name = the component becomes redundant — drop it.
            let on_old: Vec<Entity> = self
                .world
                .query::<floptle_core::Layer>()
                .filter(|(_, l)| l.0 == old)
                .map(|(e, _)| e)
                .collect();
            for e in on_old {
                if new == floptle_core::layers::DEFAULT_LAYER {
                    self.world.remove::<floptle_core::Layer>(e);
                } else {
                    self.world.insert(e, floptle_core::Layer(new.clone()));
                }
            }
            self.rebuild_sim();
        }
        if let Some((e, mt)) = cmd.set_matter {
            // Switch the node's "type" (mutually-exclusive components). Terrain owns an
            // out-of-ECS SDF field, so never morph one through here — and the mandatory
            // PostProcess node keeps its type (nothing else may become one either).
            if !matches!(
                self.world.get::<Matter>(e),
                Some(Matter::Terrain { .. } | Matter::PostProcess { .. })
            ) && !matches!(mt, Matter::PostProcess { .. })
            {
                // Becoming a Mesh: GPU-load the model so it renders this frame.
                if let Matter::Mesh { asset_path } = &mt {
                    self.import_model(&asset_path.clone());
                }
                self.record();
                self.world.insert(e, mt);
                self.rebuild_sim();
            }
        }
        if let Some(path) = cmd.import_model {
            self.import_model(&path);
        }
        if let Some((e, vis)) = cmd.set_visible {
            self.record();
            self.world.insert(e, floptle_core::Visible(vis));
        }
        if let Some(clip) = cmd.copy_component {
            self.component_clip = Some(clip);
        }
        if let Some(e) = cmd.paste_component {
            self.paste_onto(e);
        }
        if let Some((e, name)) = cmd.apply_preset
            && let Some((_, doc)) = self.materials.iter().find(|(n, _)| n == &name) {
                let mat = doc.to_material();
                self.record();
                self.world.insert(e, mat);
            }
        if let Some(path) = cmd.extract_textures {
            self.extract_textures(&path);
        }
        if let Some(path) = cmd.extract_anims {
            self.anim_ui.probes.remove(&path); // refresh the model's clip list
            match anim::extract_clips(&mut self.anim, &self.project_root, &path) {
                Ok(keys) => {
                    self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!(
                            "extracted {} animation clip(s) → assets/animations/",
                            keys.len()
                        ),
                        None,
                    );
                    self.asset_tree = build_assets(&self.project_root);
                }
                Err(e) => self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("extract animations failed: {e}"),
                    None,
                ),
            }
        }
        if let Some((e, key)) = cmd.set_anim_controller {
            self.record();
            match key {
                Some(k) => {
                    self.world.insert(e, floptle_core::AnimController { asset: k });
                }
                None => {
                    self.world.remove::<floptle_core::AnimController>(e);
                }
            }
            // Live in Play: the runtime rebinds lazily on the next animator advance.
        }
        if let Some(key) = cmd.open_anim_graph {
            cmd.focus_anim_graph = true;
            self.anim_ui.graph_key = Some(key);
            self.anim_ui.graph_doc = None; // reload the working copy
            self.anim_ui.graph_dirty = false;
            self.anim_ui.sel_state = None;
            self.anim_ui.sel_trans = None;
        }
        if let Some(attach) = cmd.new_anim_controller {
            cmd.focus_anim_graph = true;
            self.anim_ui.new_ctl_buf = Some(String::new());
            self.anim_ui.focus_prompt = true;
            self.anim_ui.new_ctl_attach = attach;
            self.anim_ui.new_ctl_dir = cmd.new_anim_controller_dir.take().and_then(|d| {
                Path::new(&d)
                    .strip_prefix(&self.project_root)
                    .ok()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            });
        }
        if let Some(path) = cmd.open_shader_graph {
            self.open_shader_in_graph(&path);
        }
        if let Some(key) = cmd.open_particle_editor {
            cmd.focus_particles = true;
            self.vfx_ui.open(key);
        }
        if cmd.focus_particles
            && let Some(dock) = self.dock_state.as_mut() {
                if let Some(path) = dock.find_tab(&EditorTab::Particles) {
                    let _ = dock.set_active_tab(path);
                } else {
                    dock.push_to_focused_leaf(EditorTab::Particles);
                }
            }
        if cmd.focus_animating
            && let Some(dock) = self.dock_state.as_mut() {
                if let Some(path) = dock.find_tab(&EditorTab::Animation) {
                    let _ = dock.set_active_tab(path);
                } else {
                    dock.push_to_focused_leaf(EditorTab::Animation);
                }
            }
        if cmd.focus_anim_graph
            && let Some(dock) = self.dock_state.as_mut() {
                if let Some(path) = dock.find_tab(&EditorTab::AnimGraph) {
                    let _ = dock.set_active_tab(path);
                } else {
                    dock.push_to_focused_leaf(EditorTab::AnimGraph);
                }
            }
        if let Some((child, parent)) = cmd.reparent {
            self.reparent(child, parent);
        }
        if let Some((matter, parent)) = cmd.add_parented {
            self.add_parented(matter, parent);
        }
        if cmd.open_new_terrain {
            self.new_terrain_cfg = Some(NewTerrainCfg::default());
        }
        if let Some(cfg) = cmd.create_terrain {
            self.create_terrain(&cfg);
            self.focus_terrain();
        }
        if let Some(parent) = cmd.add_camera {
            self.add_camera_node(parent);
        }
        if let Some((path, setting)) = cmd.set_texture_setting.take() {
            self.texture_settings.insert(path.clone(), setting);
            // Drop the cached registration so the texture re-uploads with the new
            // sampler (and mips) on next use, and persist the change.
            self.texture_registry.remove(&path);
            self.texture_registry_setting.remove(&path);
            self.save_texture_settings();
        }
        if let Some(e) = cmd.set_active_camera {
            self.set_active_camera(e);
        }
        if let Some(e) = cmd.camera_from_view {
            self.camera_to_view(e);
        }
        if cmd.clear_terrain {
            let nodes: Vec<Entity> = self.terrains.keys().copied().collect();
            if !nodes.is_empty() {
                self.record();
                for e in nodes {
                    self.world.despawn(e);
                }
                self.terrains.clear();
                self.active_terrain = None;
                self.terrain_gpu_dirty = true;
            }
        }
        if cmd.terrain_palette_changed {
            self.terrain_textures_dirty = true;
        }
        if let Some(fill) = cmd.fill_terrain
            && let Some(e) = self.target_terrain() {
                // Snapshot for undo (one step), then fill the whole field.
                let id = match self.world.get::<Matter>(e) {
                    Some(Matter::Terrain { id }) => *id,
                    _ => 0,
                };
                if let Some(t) = self.terrains.get(&e) {
                    self.push_history(Snapshot::Terrain(id, t.to_bytes()));
                }
                if let Some(t) = self.terrains.get_mut(&e) {
                    match fill {
                        TerrainFill::Color(c) => t.fill_color(c),
                        TerrainFill::Texture(slot) => t.fill_texture(slot),
                    }
                    self.terrain_gpu_dirty = true;
                }
            }
        if cmd.fill_bounds
            && let Some(e) = self.target_terrain() {
                let id = match self.world.get::<Matter>(e) {
                    Some(Matter::Terrain { id }) => *id,
                    _ => 0,
                };
                if let Some(t) = self.terrains.get(&e) {
                    self.push_history(Snapshot::Terrain(id, t.to_bytes()));
                }
                let (top, floor, inset, color) = (
                    self.terrain_brush.fill_top,
                    self.terrain_brush.fill_floor,
                    self.terrain_brush.fill_inset,
                    self.terrain_brush.color,
                );
                if let Some(t) = self.terrains.get_mut(&e) {
                    t.fill_bounds(top, floor, inset, color);
                    self.terrain_gpu_dirty = true;
                }
            }
        if cmd.focus_terrain {
            self.focus_terrain();
        }
        if let Some(path) = cmd.open_scene {
            // Opening a scene ends any play session FIRST — Stop restores the
            // pre-Play scene (name, world, terrain), so the unsaved-changes
            // prompt and its save below operate on real edit state, never on
            // play-simulation state or a mid-play `scene.load(...)`'s scene.
            if self.playing {
                self.toggle_play();
            }
            // Opening a scene replaces the world — prompt first if there are unsaved
            // edits, otherwise switch immediately.
            if self.scene_dirty {
                self.pending_open_scene = Some(path);
            } else {
                self.open_scene_file(&path);
            }
        }
        if let Some((path, save_first)) = cmd.do_open_scene {
            if save_first {
                self.save_all();
            }
            self.open_scene_file(&path);
        }
        if cmd.open_new_scene {
            self.new_scene_buf = Some(String::new());
        }
        if let Some(name) = cmd.new_scene {
            self.new_scene(&name);
        }
        if cmd.refresh_assets {
            self.asset_tree = build_assets(&self.project_root);
            self.anim.rescan(&self.project_root);
            self.vfx.rescan(&self.project_root);
            self.anim_ui.probes.clear(); // re-probe model animation lists
        }
        if let Some(dir) = cmd.new_folder_in {
            self.new_folder(&dir);
        }
        if let Some(dir) = cmd.new_script_in {
            self.new_script(&dir);
        }
        if let Some(dir) = cmd.new_shader_in {
            self.new_shader(&dir);
            // The graph tab's ✚ New: show the fresh shader on the canvas too
            // (the naming modal from new_shader stays up over it).
            if cmd.new_shader_to_graph
                && let Some((p, _)) = self.rename_target.clone()
            {
                self.open_shader_in_graph(&p);
            }
        }
        if let Some(path) = cmd.rename_asset {
            // Seed the rename modal with the current base name (the extension is shown as a
            // fixed suffix in the modal, so you edit just the name).
            let p = Path::new(&path);
            // Seed with the BASE name (up to the first dot) — the modal shows
            // the rest as a fixed suffix, compound extensions included.
            let full = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let name = if p.is_dir() {
                full
            } else {
                full.split('.').next().unwrap_or_default().to_string()
            };
            self.rename_target = Some((path, name));
        }
        if let Some((from, to)) = cmd.do_rename {
            self.rename_asset(&from, &to);
        }
        if let Some(paths) = cmd.delete_asset {
            // Deleting files/folders is irreversible — always confirm first.
            self.delete_confirm = Some(paths);
        }
        if let Some(paths) = cmd.do_delete_asset {
            self.delete_assets(&paths);
        }
        if let Some((sources, dest)) = cmd.move_assets {
            self.move_assets(&sources, &dest);
        }
        if let Some((roots, dir)) = cmd.save_prefab {
            self.save_prefab(&roots, &dir);
        }
        if let Some((path, parent)) = cmd.instantiate_prefab {
            // No parent = place in front of the camera (like Add-menu nodes);
            // with a parent, the authored root transform is the local offset.
            let at = parent.is_none().then(|| {
                let cam = self.camera.render_camera();
                cam.world_position + (cam.rotation * Vec3::NEG_Z * 5.0).as_dvec3()
            });
            self.instantiate_prefab(&path, at, parent);
        }
        if let Some(dir) = cmd.open_folder {
            // Empty path = "the project root" (the File-menu shortcut).
            let target = if dir.as_os_str().is_empty() { self.project_root.clone() } else { dir };
            crate::project::open_in_file_manager(&target);
        }
        if let Some(restore) = cmd.autosave_action {
            if restore {
                self.restore_autosave();
            } else if let Some(auto) = self.autosave_prompt.take() {
                let _ = std::fs::remove_file(auto);
            }
        }
        // Pre-warm a model being dragged so its live ghost can render next frame
        // (the gather can't import — gpu/raster are borrowed there).
        if let Some(p) =
            self.egui.as_ref().and_then(|e| egui::DragAndDrop::payload::<AssetPayload>(&e.ctx))
            && is_model(&p.path) && !self.mesh_registry.contains_key(&p.path) {
                let path = p.path.clone();
                self.import_model(&path);
            }
        // Pre-warm material textures so the gather can resolve them next frame.
        let tex_paths: Vec<String> = self
            .world
            .query::<Material>()
            .filter_map(|(_, m)| m.texture.clone())
            .filter(|p| !self.texture_registry.contains_key(p))
            .collect();
        for p in tex_paths {
            self.ensure_texture(&p);
        }
    }

    /// Register (GPU-upload) every texture and import every mesh the particle
    /// system references this frame: the effect open in the Particles tab (its
    /// live working doc — so a just-picked asset resolves next frame
    /// deterministically), every saved effect, every live play instance, and the
    /// tab preview. Idempotent. Called at the top of `render()`, before the gather
    /// resolves batch textures / mesh handles.
    fn ensure_vfx_assets(&mut self) {
        let mut tex: Vec<String> = Vec::new();
        let mut meshes: Vec<String> = Vec::new();
        let push = |v: &mut Vec<String>, p: &str| {
            if !p.is_empty() && !v.iter().any(|q| q == p) {
                v.push(p.to_string());
            }
        };
        // The open working doc first (it holds edits not yet in the registry).
        if let Some(doc) = &self.vfx_ui.doc {
            for t in &doc.tracks {
                match &t.render {
                    floptle_scene::VfxRenderDoc::Billboard { texture: Some(p) } => push(&mut tex, p),
                    floptle_scene::VfxRenderDoc::Mesh { asset_path } => push(&mut meshes, asset_path),
                    _ => {}
                }
            }
        }
        for p in self.vfx.texture_paths() {
            push(&mut tex, &p);
        }
        for p in self.vfx.mesh_paths() {
            push(&mut meshes, &p);
        }
        for p in tex {
            if !self.texture_registry.contains_key(&p) {
                self.ensure_texture(&p);
            }
        }
        for p in meshes {
            if !self.mesh_registry.contains_key(&p) {
                self.import_model(&p);
            }
        }
    }



    /// Render the whole scene from `cam` (at `aspect`) into offscreen color+depth views —
    /// the shared body behind the Inspector camera preview and the split-view Game render.
    pub(crate) fn render_world_into(
        &mut self,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        cam: &RenderCamera,
        aspect: f32,
        elapsed: f32,
    ) {
        let view_proj = cam.view_proj(aspect);

        let light_node = self.world.query::<Light>().next().map(|(_, l)| *l).unwrap_or_default();
        let light = Vec3::from(light_node.direction).normalize_or_zero();
        let li = light_node.intensity;
        let (pl_count, pl_pos, pl_col) = collect_point_lights(&self.world, cam.world_position);
        let (sh_params, sh_tint, sh_extra) = shadow_uniforms(&light_node);
        let (fog_color, fog_params) = fog_uniforms(&light_node);
        let (prox_count, prox_a, prox_b, prox_rot) =
            collect_shadow_proxies(&self.world, cam.world_position, light_node.shadows);
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [light_node.color[0] * li, light_node.color[1] * li, light_node.color[2] * li, 0.0],
            ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
            point_count: pl_count,
            point_pos: pl_pos,
            point_color: pl_col,
        };

        // Camera-relative instances + blobs, exactly like the main gather.
        let ents: Vec<(Entity, Matter)> =
            self.world.query::<Matter>().map(|(e, m)| (e, m.clone())).collect();
        let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
        // Custom `.flsl` materials draw offscreen too (bindings were refreshed
        // by ensure_flsl_materials before any gather this frame).
        let mut flsl_draws: Vec<floptle_render::FlslDraw> = Vec::new();
        let mut blobs: Vec<(DVec3, f32, MaterialParams)> = Vec::new();
        // Reused scratch for CPU vertex skinning (deformed vertices, re-uploaded per part),
        // exactly like the main gather — so offscreen views animate skinned meshes too.
        let mut skin_scratch: Vec<floptle_render::Vertex> = Vec::new();
        for (ent, matter) in &ents {
            if matches!(self.world.get::<floptle_core::Visible>(*ent), Some(floptle_core::Visible(false))) {
                continue;
            }
            let t = floptle_core::world_transform(&self.world, *ent);
            let mat = self.world.get::<Material>(*ent).cloned();
            let tex = mat
                .as_ref()
                .and_then(|m| m.texture.as_deref())
                .and_then(|p| self.texture_registry.get(p).copied());
            let flsl = self.flsl_binds.get(ent).map(|b| b.binding);
            match matter {
                Matter::Primitive { shape, color } => {
                    if let Some(&mesh) = self.mesh_ids.get(*shape as usize) {
                        let model = t.render_matrix(cam.world_position);
                        let mp =
                            mat.as_ref().map(material_params).unwrap_or_else(|| MaterialParams::flat(*color));
                        let raw = instance_of_mat(model, &mp);
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
                        let mp = mat
                            .as_ref()
                            .map(material_params)
                            .unwrap_or_else(|| MaterialParams::flat([1.0, 1.0, 1.0]));
                        let pose = self.anim.poses.get(ent).map(|v| v.as_slice());
                        push_mesh_instances(
                            gpu, raster, asset, pose, model, tex, &mp, &mut skin_scratch,
                            &mut instances, flsl, &mut flsl_draws,
                        );
                    }
                }
                _ => {}
            }
        }

        let (sky_params, sky_tint, sky_rot, sky_solid) = skybox_uniforms(&self.world);
        let clear = [sky_solid[0], sky_solid[1], sky_solid[2], 1.0];
        // SDF AO from the scene's PostProcess node shades SDF matter in offscreen
        // views too (previews + the split Game viewport).
        let (_, rm_ao_params) = post_process_uniforms(&self.world);
        let terrain_mat = self.terrain_material();
        let show_blobs = self.project.matter && !blobs.is_empty();
        // A textured skybox is DRAWN by the raymarch pass (missed rays sample the
        // sky) — keep it running even with no terrain/blobs in the scene.
        let rm_draw = show_blobs
            || !self.terrains.is_empty()
            || sky_params[0] >= 0.5
            || !self.flsl_shape_slots.is_empty();
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
            let mut g = RaymarchGlobals {
                view_proj: view_proj.to_cols_array_2d(),
                inv_view_proj: view_proj.inverse().to_cols_array_2d(),
                light_dir: [light.x, light.y, light.z, 0.0],
                light_color: [light_node.color[0] * li, light_node.color[1] * li, light_node.color[2] * li, 0.0],
                ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
                bg: [clear[0], clear[1], clear[2], 1.0],
                center: [0.0; 4],
                params: [elapsed, if show_blobs { n as f32 } else { 0.0 }, 0.0, 0.0],
                vol_center: [[0.0; 4]; 16],
                vol_half: [[1.0, 1.0, 1.0, 0.5]; 16],
                vol_atlas: [[0.0; 4]; 16],
                vol_dims: [[1.0, 1.0, 1.0, 0.0]; 16],
                terrain_tint: [tm.color[0], tm.color[1], tm.color[2], 1.0],
                terrain_emissive: [tm.emissive[0], tm.emissive[1], tm.emissive[2], tm.emissive_strength],
                terrain_specular: [tm.specular[0], tm.specular[1], tm.specular[2], tm.specular_strength],
                terrain_params: [tm.shininess, tm.rim_strength, if tm.unlit { 1.0 } else { 0.0 }, tm.ambient],
                terrain_rim: [tm.rim[0], tm.rim[1], tm.rim[2], 0.0],
                blobs: arr,
                point_count: pl_count,
                point_pos: pl_pos,
                point_color: pl_col,
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
                // vol_tight_* are renderer-patched at draw time (default: unbounded).
                ..Default::default()
            };
            Self::fill_terrain_volumes(&self.terrains, &self.terrain_slots, &self.mesh_occluders, &self.occluder_slots, &self.world, &mut g, cam.world_position);
            crate::shaders::apply_field_shapes(&self.world, &self.flsl_shape_slots, &self.sdf_cache, &mut g, cam.world_position, None);
            g
        };

        // Live particles render in offscreen views too (the split Game viewport
        // must show what the game shows).
        let vfx_preview_on = !self.playing
            && self
                .dock_state
                .as_ref()
                .is_some_and(|d| crate::dock::tab_is_front(d, EditorTab::Particles));
        let mut vfx_instances: Vec<floptle_render::ParticleInstance> = Vec::new();
        let mut vfx_batches: Vec<floptle_render::ParticleBatch> = Vec::new();
        self.vfx.collect(
            &self.world,
            cam,
            &self.texture_registry,
            vfx_preview_on,
            &mut vfx_instances,
            &mut vfx_batches,
        );
        let vfx_mesh_draws = self.vfx.collect_mesh_draws(&self.world, cam, vfx_preview_on);
        resolve_mesh_particles(&self.mesh_registry, &vfx_mesh_draws, &mut instances);

        if let (Some(gpu), Some(raster), Some(raymarch), Some(particles)) = (
            self.gpu.as_ref(),
            self.raster.as_mut(),
            self.raymarch.as_mut(),
            self.particles.as_mut(),
        ) {
            let raster_clear = if rm_draw {
                raymarch.draw_into(gpu, color, depth, rm);
                None
            } else {
                // Nothing to raymarch, but the raster field group still needs this
                // frame's shadow/proxy data (mesh-only scenes cast via proxies).
                raymarch.upload_globals(gpu, rm);
                Some(clear.map(|c| c as f64))
            };
            raster.draw_scene_with(
                gpu, color, depth, globals, &instances, &flsl_draws, raster_clear,
                Some(raymarch.field_bind()),
            );
            if !vfx_batches.is_empty() {
                particles.draw(
                    gpu,
                    color,
                    depth,
                    crate::vfx::particle_globals(cam, aspect, fog_color, fog_params),
                    &vfx_instances,
                    &vfx_batches,
                    raster,
                );
            }
        }
    }
}

/// Gather one `Matter::Mesh`'s draw instances. Rigged meshes animate: each part
/// either rides its (possibly animated) node rigidly (R6-style), or — for a TRUE
/// vertex-skinned part (Ty) — is CPU-deformed by this frame's bone palette, its
/// vertices re-uploaded, and drawn at the mesh matrix. `pose` is the node's animated
/// world matrices (falls back to the rig rest pose). Static (unrigged) meshes just
/// draw every part at `model`.
///
/// Shared by the main surface gather AND the offscreen `render_world_into` so the
/// fullscreen, docked, split, and camera-preview views all animate identically —
/// previously the offscreen path drew every mesh rigidly at its root, so a character
/// looked frozen whenever the Game view wasn't the fullscreen/focused one.
#[allow(clippy::too_many_arguments)]
fn push_mesh_instances(
    gpu: &floptle_render::Gpu,
    raster: &mut floptle_render::Raster,
    asset: &MeshAsset,
    pose: Option<&[Mat4]>,
    model: Mat4,
    tex: Option<TexId>,
    mp: &MaterialParams,
    skin_scratch: &mut Vec<floptle_render::Vertex>,
    instances: &mut Vec<(MeshId, Option<TexId>, InstanceRaw)>,
    flsl: Option<floptle_render::FlslBindingId>,
    flsl_out: &mut Vec<floptle_render::FlslDraw>,
) {
    // A node's custom `.flsl` material routes every part through the shader's
    // pipeline instead of the built-in one — same instance data either way.
    let mut push = |mid: MeshId, raw: InstanceRaw| match flsl {
        Some(b) => flsl_out.push((mid, tex, b, raw)),
        None => instances.push((mid, tex, raw)),
    };
    let Some(rig) = asset.rig.as_ref() else {
        for &mid in &asset.parts {
            push(mid, instance_of_mat(model, mp));
        }
        return;
    };
    let node_world = pose.unwrap_or(rig.rest_world.as_slice());
    for (i, &mid) in asset.parts.iter().enumerate() {
        let part_node = rig.part_nodes.get(i).copied().unwrap_or(0);
        if let Some(Some(skin)) = rig.skins.get(i) {
            anim::cpu_skin_part(skin, part_node, node_world, skin_scratch);
            raster.update_mesh_vertices(gpu, mid, skin_scratch);
            push(mid, instance_of_mat(model, mp));
        } else {
            let local = node_world.get(part_node).copied().unwrap_or(Mat4::IDENTITY);
            push(mid, instance_of_mat(model * local, mp));
        }
    }
}

/// Resolve mesh-particle draws to raster instances (camera-relative model matrix
/// plus alpha-aware tinted material) and append them to `instances`. Free function
/// so callers pass just `&mesh_registry`, a disjoint field borrow, while `gpu` and
/// `raster` are held by the main render's destructure.
fn resolve_mesh_particles(
    mesh_registry: &HashMap<String, MeshAsset>,
    draws: &[floptle_vfx::MeshDraw],
    instances: &mut Vec<(MeshId, Option<TexId>, InstanceRaw)>,
) {
    for md in draws {
        let Some(asset) = mesh_registry.get(&md.asset_path) else { continue };
        for (model, color) in &md.instances {
            let mut mp = MaterialParams::flat([color[0], color[1], color[2]]);
            mp.alpha = color[3];
            let raw = instance_of_mat(*model, &mp);
            for &mid in &asset.parts {
                instances.push((mid, None, raw));
            }
        }
    }
}
