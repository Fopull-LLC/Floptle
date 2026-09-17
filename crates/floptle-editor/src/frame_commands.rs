//! Commands the UI queued during the frame, applied once egui is done with the world.

use floptle_core::Entity;
use floptle_core::Material;
use floptle_core::Matter;
use floptle_core::math::Vec3;
use floptle_core::transform::Transform;
use floptle_scene::MatterDoc;
use floptle_scene::ShapeDoc;
use std::path::Path;
use std::path::PathBuf;
use crate::assets::{AssetPayload, build_assets, is_model};
#[cfg(feature = "editor-ui")]
use crate::dock::{EditorTab, focus_scripting_tab};
#[cfg(feature = "editor-ui")]
use crate::gizmo::Tool;
use crate::prefs::{code_theme_path, engine_theme_path, open_external_editor, save_external_editor, save_grid, save_play_tint, save_prefer_external, save_theme_index};
#[cfg(feature = "editor-ui")]
use crate::terrain_ui::{NewTerrainCfg, TerrainFill};
use crate::{Editor, ProjectAction, Snapshot, anim};
#[cfg(feature = "editor-ui")]
use crate::EditorCmd;


impl Editor {
    /// Apply the frame's deferred [`EditorCmd`] intents — runs after every
    /// gpu/egui borrow has ended, so `self` is fully free again.
    #[cfg(feature = "editor-ui")]
    pub(crate) fn apply_frame_commands(&mut self, mut cmd: EditorCmd, frame_pointer_down: bool) {
        // In this order: a later group may read a flag an earlier one set.
        self.apply_editing_commands(&mut cmd);
        self.apply_add_commands(&mut cmd);
        self.apply_ui_layout_commands(&mut cmd);
        self.apply_lighting_commands(&mut cmd, frame_pointer_down);
        self.apply_session_commands(&mut cmd);
        self.apply_component_commands(&mut cmd);
        self.apply_layer_commands(&mut cmd);
        self.apply_model_commands(&mut cmd);
        self.apply_tab_commands(&mut cmd);
        self.apply_terrain_commands(&mut cmd);
        self.apply_dock_commands(&mut cmd);
        self.apply_file_commands(&mut cmd);
    }

    /// The project menu, the tool, opening scripts, the edit verbs and enabling nodes.
    #[cfg(feature = "editor-ui")]
    fn apply_editing_commands(&mut self, cmd: &mut EditorCmd) {
        if let Some(action) = cmd.project_action.take() {
            match action {
                ProjectAction::New(p) => self.new_project(PathBuf::from(p)),
                ProjectAction::Open(p) => {
                    let path = PathBuf::from(p);
                    if floptle_vfs::is_dir(&path) {
                        self.open_project(path);
                    } else {
                        floptle_say::say_err!("  open project: not a folder: {}", path.display());
                    }
                }
                ProjectAction::Close => self.close_project(),
            }
        }
        if let Some(tool) = cmd.set_tool.take() {
            self.set_tool(tool);
        }
        if let Some(path) = cmd.open_script.take() {
            self.ide.open_file(&path);
        }
        if let Some(path) = cmd.open_script_pref.take() {
            self.open_script_preferred(&path);
        }
        if let Some((name, line)) = cmd.open_log_source.take() {
            self.open_source_at(&name, line);
        }
        if cmd.focus_learn
            && let Some(dock) = self.dock_state.as_mut()
        {
            crate::dock::focus_learn_tab(dock);
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
        if let Some((ents, on)) = cmd.set_enabled.take() {
            self.record();
            for e in ents {
                if on {
                    self.world.remove::<floptle_core::Disabled>(e);
                } else {
                    self.world.insert(e, floptle_core::Disabled);
                }
            }
            self.scene_dirty = true;
            // Physics is built from the world at Play; a mid-Play toggle has to rebuild
            // or the switched-off node keeps colliding with nothing on screen.
            self.rebuild_sim();
        }
    }

    /// Adding nodes and UI elements, and the Map tool's operations.
    #[cfg(feature = "editor-ui")]
    fn apply_add_commands(&mut self, cmd: &mut EditorCmd) {
        if let Some(m) = cmd.add.take() {
            let name = match &m {
                MatterDoc::Primitive { shape: ShapeDoc::Sphere, .. } => "Sphere",
                MatterDoc::Primitive { shape: ShapeDoc::Cube, .. } => "Cube",
                MatterDoc::Primitive { shape: ShapeDoc::Capsule, .. } => "Capsule",
                MatterDoc::Primitive { shape: ShapeDoc::Plane, .. } => "Plane",
                MatterDoc::Blob { .. } => "Blob",
                MatterDoc::Mesh { .. } => "Mesh",
                MatterDoc::Empty => "Group",
                MatterDoc::MapMesh { .. } => "Model Mesh",
                MatterDoc::Terrain { .. } => "Terrain",
                MatterDoc::NavMesh { .. } => "Nav Mesh",
                MatterDoc::NavLink { .. } => "Nav Link",
                MatterDoc::NavArea { .. } => "Nav Area",
                MatterDoc::Camera { .. } => "Camera",
                MatterDoc::PointLight { .. } => "Point Light",
                MatterDoc::GravityVolume { .. } => "Gravity Volume",
                MatterDoc::WaterVolume { .. } => "Water Volume",
                MatterDoc::FieldShape { .. } => "Field Shape",
                MatterDoc::Tilemap { .. } => "Tilemap",
                MatterDoc::SpriteBatch { .. } => "Sprite Batch",
                MatterDoc::Sprite { .. } => "Sprite",
                MatterDoc::Skybox { .. } => "Skybox",
                MatterDoc::PostProcess { .. } => "Post Processing",
                MatterDoc::LightProbes { .. } => "Light Probes",
                MatterDoc::ReflectionProbe { .. } => "Reflection Probe",
            };
            // A navmesh's id keys its baked file, so a second one in the same
            // scene must not arrive holding the first one's. The menu cannot
            // know what is already here, so the id is assigned on the way in.
            let m = if let MatterDoc::NavMesh { .. } = &m {
                let next = self
                    .world
                    .query::<floptle_core::Matter>()
                    .filter_map(|(_, m)| match m {
                        floptle_core::Matter::NavMesh { id, .. } => Some(*id),
                        _ => None,
                    })
                    .max()
                    .map_or(1, |n| n + 1);
                MatterDoc::from(&floptle_core::Matter::default_nav_mesh(next))
            } else if let MatterDoc::NavLink { .. } = &m {
                // A link's id is how a script names it and how a bake matches it
                // back, so two links sharing one is two links a game cannot tell
                // apart.
                let next = self
                    .world
                    .query::<floptle_core::Matter>()
                    .filter_map(|(_, m)| match m {
                        floptle_core::Matter::NavLink { id, .. } => Some(*id),
                        _ => None,
                    })
                    .max()
                    .map_or(1, |n| n + 1);
                MatterDoc::from(&floptle_core::Matter::default_nav_link(next))
            } else {
                m
            };
            self.add_node(name, m);
        }
        if let Some(what) = cmd.add_ui.take() {
            self.add_ui_node(what);
            // Bring the ◫ UI tab up: the thing you just added is a flat screen
            // element, and hunting for the tab that shows it is the kind of
            // friction that keeps people typing coordinates instead.
            if let Some(dock) = self.dock_state.as_mut() {
                crate::dock::focus_ui_tab(dock);
            }
        }
        if let Some(shape) = cmd.add_map_shape.take() {
            self.add_map_shape(shape);
        }
        if let Some(op) = cmd.map_op.take() {
            self.apply_map_op(op);
        }
        // ◫ Tiles intents, in the order they were pressed.
        if !cmd.tile_cmds.is_empty() {
            let cmds = std::mem::take(&mut cmd.tile_cmds);
            self.apply_tile_cmds(cmds);
        }
        if let Some(mode) = cmd.set_map_mode.take() {
            // Converts rather than clears — see `set_map_mode`.
            self.set_map_mode(mode);
        }
        if let Some(on) = cmd.set_map_knife.take() {
            self.set_map_knife(on);
            // Cutting needs the tool, same as drawing does.
            if on && self.tool != Tool::MapEdit {
                self.set_tool(Tool::MapEdit);
                self.set_map_knife(true); // set_tool clears it on the way in
            }
        }
        if let Some(arm) = cmd.set_map_arm.take() {
            self.map_draw = None;
            self.set_map_knife(false); // drawing and cutting both own the click
            self.map_arm = arm;
            // Drawing needs the tool: arming from the tab turns it on rather
            // than leaving a button that visibly does nothing.
            if arm.is_some() && self.tool != Tool::MapEdit {
                self.set_tool(Tool::MapEdit);
                self.map_arm = arm; // set_tool clears the arm on the way in
            }
        }
        if cmd.map_detach {
            self.map_detach_selection();
        }
        if let Some(q) = cmd.map_turn.take() {
            self.map_turn(q);
        }
        if cmd.map_prune {
            let n = self.prune_map_orphans();
            self.map_note(
                floptle_script::LogLevel::Debug,
                if n == 0 {
                    "no unused map geometry to clean".to_string()
                } else {
                    format!("cleaned {n} unused map mesh(es) from this scene's sidecar")
                },
            );
        }
        // Latch "pointer on a UI overlay interact" for the raw LMB handler (which
        // runs between frames): while set, presses belong to egui, not pick/gizmo.
        self.ui_overlay_hot = cmd.ui_hot;
        // A UI move/resize drag is one coalesced undo step (banked on the first
        // frame of the gesture via the pre-edit frame_snapshot; closed when the
        // pointer releases and `editing` resets). Without this, dragging/resizing
        // a UI element in the Scene view left no undo point.
    }

    /// The UI designer's moves, resizes and reordering.
    #[cfg(feature = "editor-ui")]
    fn apply_ui_layout_commands(&mut self, cmd: &mut EditorCmd) {
        if !cmd.ui_move.is_empty() || cmd.ui_resize.is_some() {
            self.begin_edit();
        }
        for (idx, d) in &cmd.ui_move {
            let ent = self.world.entity_with::<Transform>(*idx);
            if let Some(e) = ent
                && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
            {
                crate::ui_game::nudge_place(&mut spec.place, *d);
                self.world.insert(e, spec);
            }
        }
        // Rect-tool resize: grow/shrink toward the dragged side, keeping the
        // OPPOSITE edge visually fixed — Free positions and Pin offsets get the
        // exact compensation for their placement mode.
        if let Some((idx, dsize, from_min, cur)) = cmd.ui_resize.take() {
            let ent = self.world.entity_with::<Transform>(idx);
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
                        // Dragging an edge shrinks that side's margin so the box
                        // grows toward the drag (margin is [L, T, R, B]).
                        floptle_ui::Place::Stretch { margin, .. } => {
                            let side = if from_min[a] { a } else { a + 2 };
                            margin[side] -= d;
                        }
                    }
                }
                self.world.insert(e, spec);
            }
        }
        // ◫ UI tab writes. Each is an ordinary component edit, banked as one
        // undo step like any Inspector change.
        if !cmd.ui_order.is_empty()
            || !cmd.ui_set_visible.is_empty()
            || cmd.ui_set_text.is_some()
            || !cmd.ui_set_style.is_empty()
            || !cmd.ui_paste_look.is_empty()
        {
            self.begin_edit();
            let ent = |world: &floptle_core::World, idx: u32| {
                world.entity_with::<Transform>(idx)
            };
            for (idx, order) in &cmd.ui_order {
                if let Some(e) = ent(&self.world, *idx)
                    && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
                {
                    spec.order = *order;
                    self.world.insert(e, spec);
                }
            }
            for (idx, vis) in &cmd.ui_set_visible {
                if let Some(e) = ent(&self.world, *idx)
                    && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
                {
                    spec.visible = *vis;
                    self.world.insert(e, spec);
                }
            }
            if let Some((idx, text)) = &cmd.ui_set_text
                && let Some(e) = ent(&self.world, *idx)
                && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
                && let Some(t) = spec.text.as_mut()
            {
                t.text = text.clone();
                self.world.insert(e, spec);
            }
            for (idx, name) in &cmd.ui_set_style {
                if let Some(e) = ent(&self.world, *idx)
                    && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
                {
                    spec.style = name.clone();
                    self.world.insert(e, spec);
                }
            }
            // Pasting a look copies the visual properties only: placement, size
            // and the element's children are what make it that element, and
            // nothing about "make this look like that" should move it.
            for (idx, src) in &cmd.ui_paste_look {
                if let Some(e) = ent(&self.world, *idx)
                    && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
                {
                    spec.shape = src.shape.clone();
                    spec.opacity = src.opacity;
                    spec.tint = src.tint;
                    spec.rotation = src.rotation;
                    spec.scale = src.scale;
                    spec.pivot = src.pivot;
                    spec.style = src.style.clone();
                    if let (Some(dst), Some(s)) = (spec.text.as_mut(), src.text.as_ref()) {
                        let keep = std::mem::take(&mut dst.text);
                        *dst = s.clone();
                        dst.text = keep;
                    }
                    if let (Some(dst), Some(s)) = (spec.stack.as_mut(), src.stack.as_ref()) {
                        dst.pad = s.pad;
                        dst.gap = s.gap;
                    }
                    self.world.insert(e, spec);
                }
            }
            self.scene_dirty = true;
        }
        if cmd.ui_reload_styles {
            self.reload_ui_styles();
        }
    }

    /// The Inspector's lighting, GI, probe and navmesh requests.
    #[cfg(feature = "editor-ui")]
    fn apply_lighting_commands(&mut self, cmd: &mut EditorCmd, frame_pointer_down: bool) {
        if cmd.inspector_changed {
            self.begin_edit();
        }
        // ---- baked GI ------------------------------------------------------
        if cmd.gi_changed {
            self.gi_dirty = true;
        }
        if let Some(v) = cmd.gi_show_only.take() {
            self.gi_show_only = v;
            self.gi_dirty = true;
        }
        if let Some(v) = cmd.gi_show_probes.take() {
            self.gi_show_probes = v;
        }
        if cmd.recapture_probes {
            self.recapture_reflection_probes();
        }
        if cmd.gi_bake && !self.start_gi_bake() {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "nothing to bake: the scene has no enabled Light Probes node".into(),
                None,
            );
        }
        if cmd.nav_bake {
            self.bake_nav();
        }
        if cmd.nav_clear {
            if let Some((_, floptle_core::Matter::NavMesh { id, .. })) =
                crate::nav_bake::nav_node(&self.world)
            {
                let _ = floptle_vfs::remove_file(self.nav_path(id));
            }
            self.nav_baked = None;
            self.nav_overlay = None;
            self.nav_seconds = 0.0;
            self.nav_triangles = 0;
            self.publish_nav_mesh();
        }
        if cmd.gi_cancel {
            self.cancel_gi_bake();
        }
        if cmd.gi_clear {
            self.gi_baked = None;
            self.gi_dirty = true;
            let _ = floptle_vfs::remove_file(self.gi_path());
        }
        // Close the undo-coalescing session whenever the pointer isn't held. A drag
        // (gizmo, DragValue, UI move) keeps the button down across frames, so it stays
        // one step; but a discrete edit (checkbox, combo pick, typed value) releases
        // the button, so this frees `editing` and the next edit banks its own pre-edit
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
    }

    /// Play, the network session, exports, stepping, drops and preferences.
    #[cfg(feature = "editor-ui")]
    fn apply_session_commands(&mut self, cmd: &mut EditorCmd) {
        if cmd.toggle_selection_lock {
            self.toggle_selection_lock();
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
        if let Some(port) = cmd.net_host_quic.take() {
            self.net_host_quic(port);
        }
        if let Some(p) = cmd.net_play_replay.take() {
            self.net_play_replay(&p);
        }
        if let Some(addr) = cmd.net_join_quic.take() {
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
        if let Some(addr) = cmd.net_host_relay.take() {
            self.net_host_relay(addr.trim());
        }
        if let Some((dir, target)) = cmd.export_game.take() {
            self.begin_export(dir, target);
        }
        if cmd.step_tick {
            self.step_tick(1);
        }
        if cmd.step_tick_back {
            self.step_tick_back();
        }
        if cmd.toggle_pause {
            self.toggle_pause();
        }
        if let Some(path) = cmd.drop_asset.take() {
            self.drop_asset(&path);
        }
        if let Some(path) = cmd.convert_model.take() {
            self.start_model_conversion(&path);
        }
        // Free until one is running: a `try_recv` on nothing is a branch.
        self.poll_model_conversion();
        if let Some(path) = cmd.import_map.take() {
            // The Assets browser's "Add to scene": no drop point, so the group
            // lands in front of the camera (the `add_node_at` convention).
            self.import_map_file(&path, None);
        }
        if let Some((path, e)) = cmd.drop_script_on.take() {
            self.attach_script_file(&path, Some(e));
        }
        if let Some((script_path, e)) = cmd.attach_named.take() {
            let path = self.project_root.join(&script_path);
            self.attach_script_file(&path.to_string_lossy(), Some(e));
        }
        if let Some(file) = cmd.open_in_editor.take() {
            open_external_editor(&self.external_editor, &self.project_root, &file, 1);
        }
        if let Some(c) = cmd.set_external_editor.take() {
            save_external_editor(&c);
            self.external_editor = c;
        }
        if let Some(v) = cmd.set_prefer_external.take() {
            save_prefer_external(v);
            self.prefer_external_editor = v;
        }
        if let Some((en, tint)) = cmd.set_play_tint.take() {
            save_play_tint(en, tint);
            self.play_tint_enabled = en;
            self.play_tint = tint;
        }
        if cmd.save_grid {
            save_grid(&self.grid);
        }
        if let Some(i) = cmd.set_engine_theme.take() {
            self.engine_theme = i;
            save_theme_index(engine_theme_path(), i);
        }
        if let Some(i) = cmd.set_code_theme.take() {
            self.code_theme = i;
            save_theme_index(code_theme_path(), i);
        }
    }

    /// Materials, textures and the components a node gains or loses, physics included.
    #[cfg(feature = "editor-ui")]
    fn apply_component_commands(&mut self, cmd: &mut EditorCmd) {
        if let Some((name, doc)) = cmd.save_material.take() {
            let dir = self.materials_dir();
            let _ = floptle_scene::save_material(&name, &doc, &dir);
            self.materials = self.load_materials();
            self.mat_name_buf.clear();
            self.asset_tree = build_assets(&self.project_root);
        }
        if let Some(e) = cmd.add_material.take() {
            self.record();
            for e in self.selected_group(e) {
                // Seed from each node's own primitive color (else white), so a
                // multi-selection keeps twelve colours instead of taking one.
                let base = match self.world.get::<Matter>(e) {
                    Some(Matter::Primitive { color, .. }) => *color,
                    _ => [1.0, 1.0, 1.0],
                };
                self.world.insert(e, Material::tinted(base));
            }
        }
        if let Some(e) = cmd.reset_transform.take() {
            self.record();
            // The whole selection, like every other component action here: a
            // reset that only reached the node you happened to click would be a
            // surprise the first time it matters.
            for e in self.selected_group(e) {
                if let Some(t) = self.world.get_mut::<Transform>(e) {
                    *t = Transform::IDENTITY;
                }
            }
        }
        // **Get a model's own pictures out of it.** See `model_textures` —
        // everything a dev wants to do next with a model's art (layer over it,
        // recolour it, point one part at a different copy) starts with the file
        // existing.
        if let Some(path) = cmd.extract_model_textures.take() {
            let abs = self.resolve_asset_path(&path);
            match crate::model_textures::extract_model_textures(&abs, &path, &self.project_root) {
                Ok(written) => {
                    self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!(
                            "extracted {} texture(s) from {path}: {}",
                            written.len(),
                            written.iter().map(|e| e.path.as_str()).collect::<Vec<_>>().join(", ")
                        ),
                        None,
                    );
                    // They are real project assets now — the Assets panel and
                    // every texture picker have to see them without a restart.
                    self.asset_tree = build_assets(&self.project_root);
                }
                Err(e) => self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("could not extract {path}'s textures: {e}"),
                    None,
                ),
            }
        }
        // Override one sub-object's material, seeded with what that part already
        // looks like — its imported colour and, if the model brought one, its
        // texture (extracted on the spot, because an override that names no
        // texture draws untextured and "override" must not mean "go blank").
        if let Some((e, key, model)) = cmd.override_object_material.take() {
            self.record();
            let (base, textured, material) = self
                .mesh_registry
                .get(&model)
                .and_then(|a| {
                    a.part_meta.iter().enumerate().find(|(i, pm)| {
                        a.override_key(*i) == Some(key.as_str()) || pm.material == key
                    })
                })
                .map(|(_, pm)| (pm.base_color, pm.textured, pm.material.clone()))
                .unwrap_or(([1.0; 3], false, key.clone()));
            let mut mat = Material::tinted(base);
            if textured {
                let abs = self.resolve_asset_path(&model);
                let existing =
                    crate::model_textures::extracted_file(&self.project_root, &model, &material);
                mat.texture = match existing {
                    Some(p) => Some(p),
                    None => match crate::model_textures::extract_model_textures(
                        &abs,
                        &model,
                        &self.project_root,
                    ) {
                        Ok(written) => {
                            self.asset_tree = build_assets(&self.project_root);
                            written
                                .iter()
                                .find(|x| x.material == material)
                                .map(|x| x.path.clone())
                        }
                        Err(err) => {
                            self.console.push(
                                floptle_script::LogLevel::Warn,
                                format!(
                                    "{key}: could not extract this part's texture ({err}) — the \
                                     override starts untextured"
                                ),
                                None,
                            );
                            None
                        }
                    },
                };
            }
            let mut om =
                self.world.get::<floptle_core::ObjectMaterials>(e).cloned().unwrap_or_default();
            om.0.insert(key, mat);
            self.world.insert(e, om);
        }
        if let Some(e) = cmd.remove_material.take() {
            self.record();
            for e in self.selected_group(e) {
                self.world.remove::<Material>(e);
            }
        }
        if let Some(e) = cmd.add_rigidbody.take() {
            self.record();
            for e in self.selected_group(e) {
                self.world.insert(e, floptle_core::RigidBody::default());
            }
            self.rebuild_sim();
        }
        if let Some(e) = cmd.remove_rigidbody.take() {
            self.record();
            for e in self.selected_group(e) {
                self.world.remove::<floptle_core::RigidBody>(e);
            }
            self.rebuild_sim();
        }
        if let Some(e) = cmd.add_celestial.take() {
            self.record();
            for e in self.selected_group(e) {
                self.world.insert(e, floptle_core::CelestialBody::default());
            }
            self.rebuild_sim(); // they're gravity sources now
        }
        if let Some(e) = cmd.remove_celestial.take() {
            self.record();
            for e in self.selected_group(e) {
                self.world.remove::<floptle_core::CelestialBody>(e);
            }
            self.rebuild_sim();
        }
        if let Some(e) = cmd.add_networked.take() {
            self.record();
            for e in self.selected_group(e) {
                self.world.insert(e, floptle_core::Replicated::default());
            }
        }
        if let Some((e, key)) = cmd.add_particles.take() {
            self.record();
            for e in self.selected_group(e) {
                self.world.insert(
                    e,
                    floptle_core::ParticleSystem { asset: key.clone(), play_on_start: true },
                );
                // Attached mid-play: start emitting right away (live-tweak discipline).
                if self.playing {
                    self.vfx.spawn(e, &key);
                }
            }
        }
        // ✚ Effect asks for a name before it writes anything.
        //
        // It used to invent `NewEffect`, `NewEffect1`, `NewEffect2` and hand you
        // the timeline — so naming your own effect meant renaming a file that a
        // node already pointed at, which is the moment nobody does it. Asking
        // first is also the only order that is safe: a `.vfx.ron` renamed after
        // the fact leaves the `ParticleSystem.asset` on the node pointing at the
        // old key.
        if let Some(e) = cmd.new_particles.take() {
            self.new_asset_prompt = Some((crate::NewAsset::Effect(e), String::new()));
        }
        if let Some((e, name)) = cmd.do_new_particles.take() {
            // Sanitised into a filename here rather than refused in the modal: a
            // space in an effect name is a reasonable thing to type.
            let stem = crate::assets::sanitize_asset_name(&name);
            let mut n = 0;
            let (key, path) = loop {
                let key =
                    if n == 0 { format!("vfx/{stem}") } else { format!("vfx/{stem}{n}") };
                let path = self.project_root.join(format!("{key}{}", floptle_scene::VFX_EXT));
                if !floptle_vfs::exists(&path) {
                    break (key, path);
                }
                n += 1;
            };
            let doc = crate::vfx::starter_effect_doc(key.rsplit('/').next().unwrap_or(&key));
            if let Err(err) = floptle_scene::save_vfx_effect(&doc, &path) {
                floptle_say::say_err!("  new effect {key} failed: {err}");
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
        if let Some(e) = cmd.remove_particles.take() {
            self.record();
            for e in self.selected_group(e) {
                self.world.remove::<floptle_core::ParticleSystem>(e);
            }
        }
        if let Some(e) = cmd.add_audio.take() {
            self.record();
            for e in self.selected_group(e) {
                self.world.insert(e, floptle_audio::AudioSource::default());
            }
        }
        if let Some(e) = cmd.remove_audio.take() {
            self.record();
            for e in self.selected_group(e) {
                self.world.remove::<floptle_audio::AudioSource>(e);
            }
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
        if let Some((e, on)) = cmd.set_mesh_collider.take() {
            self.record();
            for e in self.selected_group(e) {
                if on {
                    self.world.insert(e, floptle_core::MeshCollider);
                } else {
                    self.world.remove::<floptle_core::MeshCollider>(e);
                }
            }
            self.rebuild_sim();
        }
        if let Some((e, on)) = cmd.set_collidable.take() {
            self.record();
            for e in self.selected_group(e) {
                if on {
                    self.world.insert(e, floptle_core::Collidable);
                } else {
                    // Clear both the new marker and any legacy mesh-collider marker.
                    self.world.remove::<floptle_core::Collidable>(e);
                    self.world.remove::<floptle_core::MeshCollider>(e);
                }
            }
            self.rebuild_sim();
        }
        if let Some((e, on)) = cmd.set_nav_exclude.take() {
            self.record();
            for e in self.selected_group(e) {
                if on {
                    self.world.insert(e, floptle_core::NavMeshExclude);
                } else {
                    self.world.remove::<floptle_core::NavMeshExclude>(e);
                }
            }
        }
        if cmd.rebuild_physics {
            self.rebuild_sim();
        }
        if let Some((e, on)) = cmd.set_trigger.take() {
            self.record();
            for e in self.selected_group(e) {
                if on {
                    self.world.insert(e, floptle_core::Trigger);
                } else {
                    self.world.remove::<floptle_core::Trigger>(e);
                }
            }
            self.rebuild_sim(); // the sensor flag bakes into the static collider
        }
    }

    /// 2D sorting and lighting, collision layers, matter, visibility and presets.
    #[cfg(feature = "editor-ui")]
    fn apply_layer_commands(&mut self, cmd: &mut EditorCmd) {
        if let Some((e, layer, order)) = cmd.set_sorting.take() {
            self.record();
            // Default-at-0 is the absence of the component, so a node put back
            // to the default stops carrying one and its scene stops mentioning
            // sorting at all.
            // The mode is not this command's to change — it has its own control
            // — so it is carried over rather than reset. And it joins the
            // default test: a Y-sorted node on the Default layer at order 0 is
            // not the default, and dropping its component would silently turn
            // Y-sorting off the first time somebody touched the layer picker.
            let mode = self
                .world
                .get::<floptle_core::Sorting>(e)
                .map(|s| s.mode)
                .unwrap_or_default();
            if layer == floptle_core::DEFAULT_SORTING_LAYER
                && order == 0
                && mode == floptle_core::SortMode::default()
            {
                self.world.remove::<floptle_core::Sorting>(e);
            } else {
                self.world.insert(e, floptle_core::Sorting { layer, order, mode });
            }
        }
        if let Some((e, mode)) = cmd.set_sort_mode.take() {
            self.record();
            let cur = self.world.get::<floptle_core::Sorting>(e).cloned().unwrap_or_default();
            // Same default test as the layer/order path above, with the mode in
            // it: back to `order` on the Default layer at 0 = no component, so
            // the scene stops mentioning sorting entirely.
            if mode == floptle_core::SortMode::default()
                && cur.order == 0
                && (cur.layer.is_empty() || cur.layer == floptle_core::DEFAULT_SORTING_LAYER)
            {
                self.world.remove::<floptle_core::Sorting>(e);
            } else {
                self.world.insert(e, floptle_core::Sorting { mode, ..cur });
            }
            self.scene_dirty = true;
        }
        if let Some((e, p)) = cmd.set_parallax.take() {
            self.record();
            // Identity is the absence of the component, the same rule sorting
            // and 2D lighting follow — so a layer put back to 1,1 stops carrying
            // one and its scene stops mentioning parallax.
            if p.is_identity() {
                self.world.remove::<floptle_core::Parallax>(e);
            } else {
                self.world.insert(e, p);
            }
            self.scene_dirty = true;
        }
        if let Some((e, lit)) = cmd.set_lighting_2d.take() {
            self.record();
            // Auto with no layer list is the absence of the component, exactly
            // as with sorting above — so a node put back to the default stops
            // carrying one and its scene stops mentioning 2D lighting.
            if lit == floptle_core::Lighting2D::default() {
                self.world.remove::<floptle_core::Lighting2D>(e);
            } else {
                self.world.insert(e, lit);
            }
        }
        if let Some((e, cast)) = cmd.set_shadow_2d.take() {
            self.record();
            if cast == floptle_core::Cast2D::Auto {
                self.world.remove::<floptle_core::Shadow2D>(e);
            } else {
                self.world.insert(e, floptle_core::Shadow2D(cast));
            }
        }
        if let Some((e, c)) = cmd.set_camera_2d.take() {
            self.record();
            match c {
                // The live half (where the follow has got to, any shake running)
                // is deliberately left at its default here: this is an EDIT to
                // the rule, and inheriting a play session's position into an
                // authored camera is how a camera moves when you change its
                // dead zone.
                Some(c) => self.world.insert(e, c),
                None => {
                    self.world.remove::<floptle_core::camera2d::Camera2D>(e);
                }
            }
        }
        if let Some(req) = cmd.do_set_layer.clone() {
            // Already answered — the modal put the final target list here.
            self.apply_layer(&req.targets, &req.layer);
        }
        if let Some(req) = cmd.set_layer.clone() {
            // Children make the scope of this edit a real question rather than a
            // detail — see `LayerChildrenPrompt`. Ask once, covering the whole
            // selection, and only when there is actually something to ask about.
            let kids = self.descendants_of(&req.targets);
            if kids.is_empty() {
                self.apply_layer(&req.targets, &req.layer);
            } else {
                self.layer_children_confirm = Some(crate::LayerChildrenPrompt {
                    targets: req.targets,
                    children: kids,
                    layer: req.layer,
                });
            }
        }
        if let Some(a) = cmd.access.take() {
            // One set of values, two ways in: this pane and a game's own options
            // menu (`access.*`). Pushed into the host so Lua reads back what the
            // editor just set, rather than the two disagreeing.
            self.access = a;
            self.script_host.set_access(a);
        }
        if let Some((old, new)) = cmd.rename_layer.take() {
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
        if let Some((e, mt)) = cmd.set_matter.take() {
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
        if let Some(path) = cmd.import_model.take() {
            self.import_model(&path);
        }
        if let Some((e, vis)) = cmd.set_visible.take() {
            self.record();
            self.world.insert(e, floptle_core::Visible(vis));
        }
        if let Some(clip) = cmd.copy_component.take() {
            self.component_clip = Some(clip);
        }
        if let Some(e) = cmd.paste_component.take() {
            self.paste_onto(e);
        }
        if let Some((e, name)) = cmd.apply_preset.take()
            && let Some((_, doc)) = self.materials.iter().find(|(n, _)| n == &name) {
                let mat = doc.to_material();
                self.record();
                self.world.insert(e, mat);
            }
        if let Some(path) = cmd.extract_textures.take() {
            self.extract_textures(&path);
        }
    }

    /// Bones, pivots, parents, mirrors, hair rigs, clips and controllers of a model.
    #[cfg(feature = "editor-ui")]
    fn apply_model_commands(&mut self, cmd: &mut EditorCmd) {
        if let Some((mesh, idx)) = cmd.select_bone.take() {
            // Select a model object/bone from the Inspector's Objects & Rig lists —
            // the same rule as the Hierarchy tree and the viewport rig.
            self.select_bone(mesh, idx);
        }
        if let Some((mesh, name, p)) = cmd.set_object_pivot.take() {
            self.apply_object_pivot(mesh, &name, Vec3::from(p));
        }
        if let Some((child, mesh, bone)) = cmd.attach_to_bone.take() {
            // A BoneAttach's local Transform is in the target model's space, so it
            // must be a direct child of that Mesh.  Preserve the scene-world pose as
            // its bone-local offset before normalizing the hierarchy; this supports
            // meshes nested under sockets/Empties as well as direct children.
            let child_world = floptle_core::world_transform(&self.world, child);
            let offset = crate::anim::bone_world_transform(
                &self.anim,
                &self.world,
                &self.mesh_registry,
                mesh,
                &bone,
            )
            .map(|bone_world| {
                // Componentwise TRS inverse (matches resolve_attachments) — a
                // mirrored mesh keeps its negative scale on the right axis.
                let local = bone_world.inv_mul(&child_world);
                if local.translation.is_finite() && local.scale.is_finite() {
                    local
                } else {
                    floptle_core::Transform::IDENTITY
                }
            })
            .unwrap_or(floptle_core::Transform::IDENTITY);
            self.world.insert(child, floptle_core::Parent(mesh));
            self.world.insert(child, floptle_core::BoneAttach { target: mesh, bone, offset });
        }
        if let Some((mesh, child, parent)) = cmd.set_object_parent.take() {
            // Persist an object re-parent to the model's `.rig.ron` sidecar, then
            // re-import the model so the new hierarchy takes effect live and every
            // instance rebinds against the reordered skeleton.
            if let Some(Matter::Mesh { asset_path }) = self.world.get::<Matter>(mesh).cloned() {
                let abs = self.resolve_asset_path(&asset_path);
                let mut ov = crate::rig_overrides::RigOverrides::load(&abs);
                // "" = model root (an explicit reparent-to-root, distinct from absent).
                ov.reparent.insert(child, parent.unwrap_or_default());
                if let Err(e) = ov.save(&abs) {
                    self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("save rig override failed: {e}"),
                        None,
                    );
                }
                self.mesh_registry.remove(&asset_path);
                self.import_model(&asset_path);
                self.anim.revision += 1; // force every instance to rebind
                self.bone_selection = None; // node indices changed after the re-sort
            }
        }
        if let Some((path, filter)) = cmd.set_model_filter.take() {
            // Persist the embedded-texture filter to the model's sidecar, then drop
            // the registration — the ensure sweep re-imports it next frame with the
            // new sampling (skin variants self-heal on the new MeshIds).
            let abs = self.resolve_asset_path(&path);
            let mut ov = crate::rig_overrides::RigOverrides::load(&abs);
            ov.texture_filter = filter;
            if let Err(e) = ov.save(&abs) {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("saving {}: {e}", abs.display()),
                    None,
                );
            }
            self.mesh_registry.remove(&path);
        }
        if let Some(mesh) = cmd.mirror_model.take()
            && let Some(Matter::Mesh { asset_path }) = self.world.get::<Matter>(mesh).cloned()
        {
            let abs = self.resolve_asset_path(&asset_path);
            match floptle_assets::mirror_apply(&abs) {
                Ok(r) => {
                    // Carry any object re-parenting onto the mirrored model (same node
                    // names), so the sidecar keeps working after the bake.
                    let src_side = crate::rig_overrides::RigOverrides::sidecar_path(&abs);
                    if floptle_vfs::exists(&src_side) {
                        let _ = floptle_vfs::copy(
                            &src_side,
                            crate::rig_overrides::RigOverrides::sidecar_path(&r.output),
                        );
                    }
                    let rel = r
                        .output
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let split: Vec<String> =
                        r.split.iter().map(|(l, _)| l.trim_end_matches(".L").trim_end_matches(".R").to_string()).collect();
                    self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!(
                            "Mirror-apply → {rel}  ·  welded {:?}  ·  split L/R {:?}  ·  kept {:?}  \
                             (assign the new model in the Inspector to use it)",
                            r.welded, split, r.kept
                        ),
                        None,
                    );
                    self.asset_tree = build_assets(&self.project_root);
                }
                Err(e) => self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("Mirror-apply failed: {e}"),
                    None,
                ),
            }
        }
        if let Some((mesh, object)) = cmd.add_hair_rig.take()
            && let Some(Matter::Mesh { asset_path }) = self.world.get::<Matter>(mesh).cloned()
        {
            let abs = self.resolve_asset_path(&asset_path);
            match floptle_assets::add_flow_rig(&abs, &object, 5) {
                Ok(r) => {
                    // Carry any object re-parenting/pivots onto the rigged model
                    // (same node names), so the sidecar keeps working after the bake.
                    let src_side = crate::rig_overrides::RigOverrides::sidecar_path(&abs);
                    if floptle_vfs::exists(&src_side) {
                        let _ = floptle_vfs::copy(
                            &src_side,
                            crate::rig_overrides::RigOverrides::sidecar_path(&r.output),
                        );
                    }
                    let rel = r
                        .output
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!(
                            "Flow-rig → {rel}  ·  {} got a {}-bone chain  \
                             (assign the new model, then pose the {}_root chain to make it flow)",
                            r.object, r.bones, r.object
                        ),
                        None,
                    );
                    self.asset_tree = build_assets(&self.project_root);
                }
                Err(e) => self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("Flow-rig failed: {e}"),
                    None,
                ),
            }
        }
        if let Some(path) = cmd.extract_anims.take() {
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
        if let Some((e, key)) = cmd.set_anim_controller.take() {
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
    }

    /// The graph, image and particle editors, and which tab comes to the front.
    #[cfg(feature = "editor-ui")]
    fn apply_tab_commands(&mut self, cmd: &mut EditorCmd) {
        if let Some(key) = cmd.open_anim_graph.take() {
            cmd.focus_anim_graph = true;
            self.anim_ui.graph_key = Some(key);
            self.anim_ui.graph_doc = None; // reload the working copy
            self.anim_ui.graph_dirty = false;
            self.anim_ui.sel_state = None;
            self.anim_ui.sel_trans = None;
        }
        if let Some(attach) = cmd.new_anim_controller.take() {
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
        if let Some(path) = cmd.open_shader_graph.take() {
            self.open_shader_in_graph(&path);
        }
        if let Some(path) = cmd.import_aseprite.take() {
            self.import_aseprite_sheet(&path);
        }
        if let Some((path, cols, rows)) = cmd.new_sprite_anim.take() {
            self.write_sprite_anim(&path, cols, rows);
        }
        if let Some(path) = cmd.open_image.take() {
            self.open_image_doc(&path);
        }
        if let Some(form) = cmd.image_new.take() {
            self.new_image_doc(&form);
        }
        if cmd.image_save {
            self.save_image_doc();
        }
        if let Some(name) = cmd.image_save_as.take() {
            self.save_image_doc_as(&name);
        }
        if let Some(what) = cmd.image_export.take() {
            self.export_image(what);
        }
        if cmd.image_save_palette {
            self.save_image_palette();
        }
        // Closing goes through `close_image_doc`, which asks about unsaved work
        // and takes DISCARD for an answer. It used to refuse outright and say
        // "save first" — which is not a thing you can do to a document that has
        // never had a name, so the only exit was closing the project.
        if cmd.image_close {
            self.close_image_doc(None);
        }
        if let Some(i) = cmd.image_close_tab.take() {
            self.close_image_doc(Some(i));
        }
        if let Some(i) = cmd.image_activate.take() {
            self.activate_image_doc(i);
        }
        if cmd.image_new_from_clipboard {
            self.new_image_from_clipboard();
        }
        if let Some(which) = cmd.image_discard.take() {
            self.discard_image_doc(which);
        }
        if cmd.image_save_then_close {
            // An unnamed document routes to Save As, which is a dialog, so the
            // close waits for it rather than happening behind it.
            if self.image.path.is_some() {
                self.save_image_doc();
                if !self.image.dirty {
                    self.discard_image_doc(None);
                }
            } else {
                self.image.save_name = Some(String::new());
                self.image.toast("give it a name first — then close it");
            }
        }
        if let Some(key) = cmd.open_particle_editor.take() {
            cmd.focus_particles = true;
            self.vfx_ui.open(key);
        }
        if let Some(at) = cmd.look_at.take() {
            self.focus_point(at, 6.0);
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
    }

    /// Reparenting, painting, terrain and cameras.
    #[cfg(feature = "editor-ui")]
    fn apply_terrain_commands(&mut self, cmd: &mut EditorCmd) {
        if let Some((children, parent)) = cmd.reparent.take() {
            self.reparent_many(&children, parent);
        }
        if let Some((matter, parent)) = cmd.add_parented.take() {
            self.add_parented(matter, parent);
        }
        if cmd.paint_fill {
            // Same target routing as Clear: filling vertex blocks while the UI says
            // ▦ Texture would silently stomp vertex work.
            if self.vertex_brush.target == crate::paint_ui::PaintTarget::Texture {
                self.tex_fill_selected();
            } else {
                self.paint_fill_selected();
            }
        }
        if cmd.paint_clear {
            // Texture target → drop the painted texture (back to the original material tex);
            // Vertex target → clear the per-vertex colors.
            if self.vertex_brush.target == crate::paint_ui::PaintTarget::Texture {
                for e in self.selection.clone() {
                    self.clear_texture_paint(e);
                }
            } else {
                self.paint_clear_selected();
            }
        }
        if cmd.open_new_terrain {
            self.new_terrain_cfg = Some(NewTerrainCfg::default());
        }
        if let Some(cfg) = cmd.create_terrain.take() {
            self.create_terrain(&cfg);
            self.focus_terrain();
        }
        if let Some(parent) = cmd.add_camera.take() {
            self.add_camera_node(parent);
        }
        if let Some((path, setting)) = cmd.set_texture_setting.take() {
            self.apply_texture_setting(&path, setting);
        }
        if let Some(e) = cmd.set_active_camera.take() {
            self.set_active_camera(e);
        }
        if let Some(e) = cmd.camera_from_view.take() {
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
        if let Some(fill) = cmd.fill_terrain.take()
            && let Some(e) = self.target_terrain() {
                // Snapshot for undo (one step), then fill the whole field. Fills only
                // modify EXISTING chunks, so the stored set is the exact undo cover.
                let id = match self.world.get::<Matter>(e) {
                    Some(Matter::Terrain { id, .. }) => *id,
                    _ => 0,
                };
                if let Some(t) = self.terrains.get(&e) {
                    let undo = t.field.snapshot_chunks(&t.field.all_chunk_coords());
                    self.push_history(Snapshot::Terrain(id, undo));
                }
                if let Some(t) = self.terrains.get_mut(&e) {
                    match fill {
                        TerrainFill::Color(c) => t.field.fill_color(c),
                        TerrainFill::Texture(slot) => t.field.fill_texture(slot),
                    }
                    t.rebuild_shadow();
                    self.terrain_gpu_dirty = true;
                }
            }
        if cmd.fill_bounds
            && let Some(e) = self.target_terrain() {
                let id = match self.world.get::<Matter>(e) {
                    Some(Matter::Terrain { id, .. }) => *id,
                    _ => 0,
                };
                if let Some(t) = self.terrains.get(&e) {
                    // Fill-bounds may create chunks inside the bounds box — cover the
                    // stored set plus that box so undo can also remove them.
                    let mut cand = t.field.all_chunk_coords();
                    if let Some((lo, hi)) = t.field.bounds() {
                        let pad = t.field.band() + 2.0 * t.field.voxel();
                        cand.extend(t.field.chunks_in_world_box(
                            lo - Vec3::splat(pad),
                            hi + Vec3::splat(pad),
                        ));
                        cand.sort_unstable();
                        cand.dedup();
                    }
                    let undo = t.field.snapshot_chunks(&cand);
                    self.push_history(Snapshot::Terrain(id, undo));
                }
                let (top, floor, inset, color) = (
                    self.terrain_brush.fill_top,
                    self.terrain_brush.fill_floor,
                    self.terrain_brush.fill_inset,
                    self.terrain_brush.color,
                );
                if let Some(t) = self.terrains.get_mut(&e) {
                    // Mirror cover = chunks present before ∪ after the fill, so
                    // chunks the fill removed clear from the sim copy too.
                    let mut coords = t.field.all_chunk_coords();
                    t.field.fill_bounds(top, floor, inset, color);
                    t.rebuild_shadow();
                    self.terrain_gpu_dirty = true;
                    coords.extend(t.field.all_chunk_coords());
                    coords.sort_unstable();
                    coords.dedup();
                    self.mirror_terrain_chunks_to_sim(e, &coords);
                }
            }
    }

    /// Bringing a tab to the front, and resetting the layout or the window.
    #[cfg(feature = "editor-ui")]
    fn apply_dock_commands(&mut self, cmd: &mut EditorCmd) {
        if cmd.focus_terrain {
            self.focus_terrain();
        }
        if cmd.focus_tiles {
            // The tab and the tool: reaching the Tiles tab and finding the pointer
            // still on Select is the "why is nothing painting" moment, and it is
            // avoidable with one line.
            if let Some(dock) = self.dock_state.as_mut() {
                crate::dock::focus_tiles_tab(dock);
            }
            self.tool = Tool::Tiles;
            // …and make the node you came from the layer, since that is the one you
            // were looking at when you pressed the button.
            if let Some(e) = self.primary()
                && matches!(self.world.get::<Matter>(e), Some(Matter::Tilemap { .. }))
            {
                self.tile_tools.layer = Some(e);
            }
        }
        if cmd.focus_image {
            self.focus_image_tab();
        }
        if cmd.focus_map {
            self.focus_map();
        }
        if cmd.focus_packages
            && let Some(dock) = self.dock_state.as_mut()
        {
            crate::dock::focus_packages_tab(dock);
        }
        if cmd.reset_layout {
            self.dock_state = Some(crate::dock::default_dock());
            // Throw the saved file away too, not just this session's state. A
            // reset that only lasts until the next crash is not a reset — and
            // "reset the layout" is the thing somebody reaches for precisely
            // when the editor is misbehaving.
            crate::layout::forget_dock();
        }
        if cmd.reset_window {
            crate::layout::forget_window();
            if let Some(window) = self.window.as_ref() {
                let d = crate::layout::WindowPlace::default();
                window.set_maximized(false);
                let _ = window.request_inner_size(winit::dpi::LogicalSize::new(d.width, d.height));
            }
        }
    }

    /// Scenes, prefabs, the asset tree, imports, crash reports and trust.
    #[cfg(feature = "editor-ui")]
    fn apply_file_commands(&mut self, cmd: &mut EditorCmd) {
        if let Some(path) = cmd.open_scene.take() {
            // Opening a scene ends any play session first — Stop restores the
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
        if let Some(path) = cmd.open_prefab.take() {
            // Same shape as opening a scene, for the same reason: this replaces
            // the world.
            if self.playing {
                self.toggle_play();
            }
            if self.scene_dirty {
                self.pending_open_scene = Some(path);
            } else {
                self.open_prefab_file(&path);
            }
        }
        if let Some((path, save_first)) = cmd.do_open_scene.take() {
            if save_first {
                self.save_all();
            }
            if crate::assets::is_prefab(&path) {
                self.open_prefab_file(&path);
            } else {
                self.open_scene_file(&path);
            }
        }
        if cmd.open_new_scene {
            self.new_scene_buf = Some(String::new());
        }
        if let Some((e, kind, func)) = cmd.run_editor_action.take() {
            self.run_editor_action(e, &kind, &func);
        }
        // Adopt any finished background planet generations. (The runtime queue
        // DRAINS earlier in the frame — before residency, see render_frame's
        // ordering comment; editor actions drain inside `run_editor_action`.)
        self.poll_terrain_generates();
        if let Some(name) = cmd.new_scene.take() {
            self.new_scene(&name);
        }
        if cmd.refresh_assets {
            self.asset_tree = build_assets(&self.project_root);
            self.anim.rescan(&self.project_root);
            self.vfx.rescan(&self.project_root);
            self.anim_ui.probes.clear(); // re-probe model animation lists
        }
        if let Some(dir) = cmd.new_folder_in.take() {
            self.new_folder(&dir);
        }
        if let Some(dir) = cmd.new_script_in.take() {
            self.new_script(&dir);
        }
        if let Some(dir) = cmd.new_shader_in.take() {
            self.new_shader(&dir);
            // The graph tab's ✚ New: show the fresh shader on the canvas too
            // (the naming modal from new_shader stays up over it).
            if cmd.new_shader_to_graph
                && let Some((p, _)) = self.rename_target.clone()
            {
                self.open_shader_in_graph(&p);
            }
        }
        if let Some(path) = cmd.rename_asset.take() {
            // Seed the rename modal with the current base name (the extension is shown as a
            // fixed suffix in the modal, so you edit just the name).
            let p = Path::new(&path);
            // Seed with the BASE name (up to the first dot) — the modal shows
            // the rest as a fixed suffix, compound extensions included.
            let full = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let name = if floptle_vfs::is_dir(p) {
                full
            } else {
                full.split('.').next().unwrap_or_default().to_string()
            };
            self.rename_target = Some((path, name));
        }
        if let Some((from, to)) = cmd.do_rename.take() {
            self.rename_asset(&from, &to);
        }
        if let Some(paths) = cmd.delete_asset.take() {
            // Deleting files/folders is irreversible — always confirm first.
            self.delete_confirm = Some(paths);
        }
        if let Some(paths) = cmd.do_delete_asset.take() {
            self.delete_assets(&paths);
        }
        if let Some((sources, dest)) = cmd.move_assets.take() {
            self.move_assets(&sources, &dest);
        }
        if let Some((sources, dest)) = cmd.import_files.take() {
            self.import_files(&sources, &dest);
        }
        if let Some(dir) = cmd.pick_import_dir.take() {
            self.open_import_dialog(dir);
        }
        // Drain a completed native import dialog (see open_import_dialog).
        if let Some((rx, dir)) = &self.import_rx {
            match crate::native_dialog::poll(rx) {
                crate::native_dialog::Answer::Waiting => {}
                crate::native_dialog::Answer::Chose(files) => {
                    let dir = dir.clone();
                    self.import_rx = None;
                    self.import_files(&files, &dir);
                }
                crate::native_dialog::Answer::Closed => self.import_rx = None,
            }
        }
        if let Some((roots, dir)) = cmd.save_prefab.take() {
            self.save_prefab(&roots, &dir);
        }
        if let Some((path, parent)) = cmd.instantiate_prefab.take() {
            // No parent = place in front of the camera (like Add-menu nodes);
            // with a parent, the authored root transform is the local offset.
            let at = parent.is_none().then(|| {
                let cam = self.camera.render_camera();
                cam.world_position + (cam.rotation * Vec3::NEG_Z * 5.0).as_dvec3()
            });
            self.instantiate_prefab(&path, at, parent);
        }
        if let Some(dir) = cmd.open_folder.take() {
            // Empty path = "the project root" (the File-menu shortcut).
            let target = if dir.as_os_str().is_empty() { self.project_root.clone() } else { dir };
            crate::project::open_in_file_manager(&target);
        }
        if let Some(send) = cmd.crash_report.take() {
            if let Some(note) = self.crash_prompt.take()
                && send
            {
                crate::open_issue_tracker(Some(&note));
            }
            self.crash_prompt = None;
        }
        if let Some(answer) = cmd.project_trust.take() {
            self.answer_project_trust(answer);
        }
        if let Some(restore) = cmd.autosave_action.take() {
            if restore {
                self.restore_autosave();
            } else if let Some(auto) = self.autosave_prompt.take() {
                let _ = floptle_vfs::remove_file(auto);
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
    }
}
