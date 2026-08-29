//! Project + asset IO: open/new/close project, scene load/save (with legacy
//! migrations), asset file management, textures, and save-all.

use floptle_core::Matter;
use floptle_core::World;
use floptle_core::math::Mat3;
use floptle_core::math::Quat;
use floptle_core::math::Vec3;
use floptle_render::{MeshId, TexId};
use floptle_scene::MaterialDoc;
use floptle_scene::MatterDoc;
use floptle_scene::SceneDoc;
use std::path::Path;
use std::path::PathBuf;
use crate::assets::{build_assets, script_name_of, unique_path};
use crate::dock::{focus_scripting_tab};
use crate::ide::{IdeState, script_template};
use crate::lua_support::{seed_default_scripts, write_lua_support};
use crate::prefs::{open_external_editor};
use crate::{anim, Editor, History, MeshAsset};

impl Editor {
    /// Decode a model's embedded textures and write them to `<project>/textures/`
    /// as PNGs (so they can be reused as material textures — e.g. a grass material
    /// from the retro map). Refreshes the asset tree.
    pub(crate) fn extract_textures(&mut self, model_path: &str) {
        let Ok(model) = floptle_assets::import(Path::new(model_path)) else {
            eprintln!("  extract: failed to read {model_path}");
            return;
        };
        if model.textures.is_empty() {
            eprintln!("  extract: {model_path} has no embedded textures");
            return;
        }
        let stem = Path::new(model_path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "model".into());
        let dir = self.project_root.join("textures");
        let mut wrote = 0;
        for (i, tex) in model.textures.iter().enumerate() {
            let path = dir.join(format!("{stem}_{i}.png"));
            if floptle_assets::save_texture_png(tex, &path).is_ok() {
                wrote += 1;
            }
        }
        println!("  extracted {wrote} texture(s) from {stem} to textures/");
        self.asset_tree = build_assets(&self.project_root);
    }

    /// Open a script in the user's preferred editor — the external one (ADR-0011) if
    /// they prefer it, otherwise the in-engine IDE (focusing the Scripting tab).
    pub(crate) fn open_script_preferred(&mut self, path: &str) {
        if self.prefer_external_editor {
            open_external_editor(&self.external_editor, &self.project_root, path, 1);
        } else {
            self.ide.open_file(path);
            if let Some(dock) = self.dock_state.as_mut() {
                focus_scripting_tab(dock);
            }
        }
    }

    /// Open a script by its chunk `name` (as captured in a Console line) at `line`,
    /// in the preferred editor — the Console's double-click-to-source.
    pub(crate) fn open_source_at(&mut self, name: &str, line: u32) {
        let line = line.max(1) as usize;
        let path = if name.ends_with(".lua") {
            let p = self.project_root.join(name);
            if p.exists() { p } else { self.scripts_dir().join(name) }
        } else {
            self.scripts_dir().join(format!("{name}.lua"))
        };
        let path_str = path.to_string_lossy().to_string();
        if self.prefer_external_editor {
            open_external_editor(&self.external_editor, &self.project_root, &path_str, line);
        } else {
            if self.ide.open_file(&path_str) {
                self.ide.goto = Some(line);
            }
            if let Some(dock) = self.dock_state.as_mut() {
                focus_scripting_tab(dock);
            }
        }
    }

    /// Load + register a material texture (cached by path + its sampling settings),
    /// returning its handle. Re-registers if the texture's filter/wrap was changed.
    pub(crate) fn ensure_texture(&mut self, path: &str) -> Option<TexId> {
        // Live render targets ("rt:<name>") are registered by the camera
        // target pass (update_render_targets), never loaded from disk — the
        // lookup misses until the named camera has rendered once.
        if path.starts_with("rt:") {
            return self.texture_registry.get(path).copied();
        }
        let want =
            crate::assets::tex_setting(&self.texture_settings, &self.project_root, path);
        if let (Some(id), Some(prev)) =
            (self.texture_registry.get(path), self.texture_registry_setting.get(path))
            && *prev == want {
                return Some(*id);
            }
        // A texture bigger than the GPU's max 2D dimension would panic
        // `create_texture` — common with spritesheets. Downscale to fit and warn
        // rather than crash (UVs are normalized, so it still samples correctly).
        let max = self.gpu.as_ref()?.device.limits().max_texture_dimension_2d;
        // The registry stays keyed by the ref as WRITTEN; only the fs read resolves.
        let file = self.resolve_asset_path(path);
        let mut data = floptle_assets::load_texture(&file)?;
        if data.width > max || data.height > max {
            let s = max as f32 / data.width.max(data.height) as f32;
            let w = ((data.width as f32 * s).floor() as u32).max(1);
            let h = ((data.height as f32 * s).floor() as u32).max(1);
            log::warn!(
                "texture {path} is {}×{} — larger than the GPU limit {max}; downscaled to {w}×{h}",
                data.width, data.height
            );
            data = floptle_assets::load_texture_sized(&file, w, h)?;
        }
        let (gpu, raster) = (self.gpu.as_ref()?, self.raster.as_mut()?);
        let id = raster.register_texture(gpu, &data, want.to_sampling());
        self.texture_registry.insert(path.to_string(), id);
        self.texture_registry_setting.insert(path.to_string(), want);
        Some(id)
    }

    /// Persist the per-texture sampling settings to `.floptle/textures.ron`.
    /// Store one texture's import settings and make everything holding the old
    /// ones let go.
    ///
    /// Its own function because there are two callers now — the Assets panel
    /// and the Aseprite import, which slices a sheet on the person's behalf —
    /// and the four things that have to happen after the write are exactly the
    /// four somebody would forget.
    pub(crate) fn apply_texture_setting(
        &mut self,
        path: &str,
        setting: crate::assets::TexSetting,
    ) {
        // Stored under the PROJECT-RELATIVE key, which is how scenes and
        // materials reference a texture; callers hand over absolute paths.
        let path = crate::assets::asset_rel_path(path, &self.project_root);
        self.texture_settings.insert(path.clone(), setting);
        // Drop the cached registration so the texture re-uploads with the new
        // sampler (and mips) on next use. The registry is keyed by the ref AS
        // WRITTEN, so drop every spelling of this texture.
        let root = self.project_root.clone();
        let same = |k: &String| crate::assets::asset_rel_path(k, &root) == path;
        self.texture_registry.retain(|k, _| !same(k));
        self.texture_registry_setting.retain(|k, _| !same(k));
        // The terrain palette bakes its own 256² copy at load, so a filter
        // change has to re-RESAMPLE it — a sampler swap alone cannot un-blur a
        // bilinear resize. Only re-upload if this texture is in the palette.
        if self.terrain_textures.contains(&path) {
            self.terrain_textures_dirty = true;
        }
        // **Re-slicing a texture re-slices every material using it.** That is
        // what the sheet grid living on the TEXTURE is for, and it was only true
        // of the one material whose Inspector happened to be open — see
        // `assets::reslice_materials`.
        crate::assets::reslice_materials(&mut self.world, &root, &path, setting);
        self.save_texture_settings();
    }

    pub(crate) fn save_texture_settings(&self) {
        let dir = self.project_root.join(".floptle");
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(s) = ron::ser::to_string_pretty(&self.texture_settings, Default::default()) {
            let _ = std::fs::write(dir.join("textures.ron"), s);
        }
    }

    /// Load the per-texture sampling settings from `.floptle/textures.ron` (if present).
    ///
    /// Keys are normalised to the project-relative form scenes and materials reference
    /// textures by. Older files stored the Assets browser's ABSOLUTE paths, which no
    /// renderer ever looked up — those migrate here, and are written back relative by
    /// the next save (floptle/0026).
    pub(crate) fn load_texture_settings(&mut self) {
        let path = self.project_root.join(".floptle").join("textures.ron");
        let raw: std::collections::HashMap<String, crate::assets::TexSetting> =
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| ron::from_str(&s).ok())
                .unwrap_or_default();
        self.texture_settings = raw
            .into_iter()
            .map(|(k, v)| (crate::assets::asset_rel_path(&k, &self.project_root), v))
            .collect();
    }

    /// One-time migration: a scene from before the PostProcess node inherits the
    /// legacy project-wide bloom/vignette settings (old `project.ron` fields) onto
    /// its self-healed node, so an old project keeps the look it was tuned for.
    /// Scenes that already carry a PostProcess node are left alone, as are legacy
    /// projects that never enabled an effect (the healed default — AO on — stands).
    pub(crate) fn migrate_legacy_post(&mut self, doc: &SceneDoc) {
        if doc.nodes.iter().any(|n| matches!(n.matter, MatterDoc::PostProcess { .. })) {
            return;
        }
        let p = self.project.clone();
        if !(p.bloom || p.vignette) {
            return;
        }
        let node = self
            .world
            .query::<Matter>()
            .find_map(|(e, m)| matches!(m, Matter::PostProcess { .. }).then_some(e));
        if let Some(e) = node
            && let Some(Matter::PostProcess {
                bloom,
                bloom_threshold,
                bloom_intensity,
                vignette,
                vignette_strength,
                vignette_radius,
                ..
            }) = self.world.get_mut::<Matter>(e)
            {
                *bloom = p.bloom;
                *bloom_threshold = p.bloom_threshold;
                *bloom_intensity = p.bloom_intensity;
                *vignette = p.vignette;
                *vignette_strength = p.vignette_strength;
                *vignette_radius = p.vignette_radius;
            }
    }

    /// Import + register a glTF model (cached by path). Returns true on success.
    /// Every imported model's material slots, keyed by asset path — what the
    /// script host is lent so `node:materials()` can answer.
    ///
    /// Both names per slot, because a part answers to both and neither is
    /// enough on its own: the OBJECT name is precise but is rewritten by import
    /// when a model repeats a name (`Torso` → `Torso#2`), and the MATERIAL name
    /// is the one on the model's own list and usually the group somebody means.
    pub(crate) fn model_slots(
        &self,
    ) -> std::collections::HashMap<String, Vec<floptle_script::ModelSlot>> {
        self.mesh_registry
            .iter()
            .map(|(path, asset)| {
                let slots = asset
                    .part_meta
                    .iter()
                    .enumerate()
                    .map(|(i, pm)| floptle_script::ModelSlot {
                        object: asset.override_key(i).unwrap_or(&pm.material).to_string(),
                        material: pm.material.clone(),
                        textured: pm.textured,
                    })
                    .collect();
                (path.clone(), slots)
            })
            .collect()
    }

    pub(crate) fn import_model(&mut self, path: &str) -> bool {
        if self.mesh_registry.contains_key(path) {
            return true;
        }
        // The registry stays keyed by the ref as WRITTEN; only the fs read resolves.
        let file = resolve_asset_path(&self.project_root, path);
        // A missing file (e.g. a model deleted while still referenced by a VFX effect or
        // a scene node) must NOT be re-attempted + error-logged every frame — bail on the
        // cheap existence check. It re-imports for free if the file comes back.
        if !file.exists() {
            return false;
        }
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return false;
        };
        // Rigged path first: any glTF with animations keeps its node tree +
        // clips (parts stay node-local and get posed each frame).
        match floptle_assets::import_rigged(&file) {
            Ok(Some(model)) => {
                let parts: Vec<MeshId> = model
                    .parts
                    .iter()
                    .map(|p| raster.register(gpu, &p.mesh, p.texture.map(|i| &model.textures[i])))
                    .collect();
                let part_meta = model
                    .parts
                    .iter()
                    .map(|p| crate::PartMeta {
                        material: p.material.clone(),
                        base_color: p.base_color,
                        textured: p.texture.is_some(),
                    })
                    .collect();
                let overrides = crate::rig_overrides::RigOverrides::load(&file);
                if let Some(f) = overrides.texture_filter {
                    let s = crate::assets::TexSetting { filter: f, ..Default::default() };
                    for &mid in &parts {
                        raster.set_mesh_sampling(gpu, mid, s.to_sampling());
                    }
                }
                let mut rig = anim::rig_from_model(&model, &overrides);
                // Bind pose + per-vertex weights to the GPU once, here: from now on
                // this model's characters are deformed in the vertex shader
                // (`floptle/0080`).
                anim::upload_skins(gpu, raster, &mut rig);
                let skinned = model.parts.iter().filter(|p| p.skin.is_some()).count();
                let verts: usize = model.parts.iter().map(|p| p.mesh.vertices.len()).sum();
                self.mesh_registry.insert(
                    path.to_string(),
                    MeshAsset {
                        parts,
                        part_meta,
                        tex_filter: overrides.texture_filter,
                        size: model.size,
                        rig: Some(rig),
                    },
                );
                // Surface the import stats to the Console so an incomplete import (e.g. a
                // Blender Mirror modifier that wasn't applied at export, which drops half
                // the geometry) is visible — the missing half lives in the .glb, not here.
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!(
                        "imported {path} — rigged: {} part(s) ({skinned} skinned), {verts} verts, {} clip(s)",
                        model.parts.len(),
                        model.clips.len()
                    ),
                    None,
                );
                eprintln!("  imported {path} (rigged, {} clip(s))", model.clips.len());
                return true;
            }
            Ok(None) => {} // no animations — fall through to the static bake
            Err(e) => eprintln!("  rig import {path} failed ({e}); trying static"),
        }
        match floptle_assets::gltf_import::import(&file) {
            Ok(model) => {
                let parts: Vec<MeshId> = model
                    .parts
                    .iter()
                    .map(|p| raster.register(gpu, &p.mesh, p.texture.map(|i| &model.textures[i])))
                    .collect();
                let part_meta = model
                    .parts
                    .iter()
                    .map(|p| crate::PartMeta {
                        material: p.material.clone(),
                        base_color: p.base_color,
                        textured: p.texture.is_some(),
                    })
                    .collect();
                let overrides = crate::rig_overrides::RigOverrides::load(&file);
                if let Some(f) = overrides.texture_filter {
                    let s = crate::assets::TexSetting { filter: f, ..Default::default() };
                    for &mid in &parts {
                        raster.set_mesh_sampling(gpu, mid, s.to_sampling());
                    }
                }
                self.mesh_registry.insert(
                    path.to_string(),
                    MeshAsset {
                        parts,
                        part_meta,
                        tex_filter: overrides.texture_filter,
                        size: model.size,
                        rig: None,
                    },
                );
                eprintln!("  imported {path}");
                // A model imported after Play started — a script spawning a
                // prefab, a scatter prototype baking — has to reach
                // `node:materials()` too, or a runtime-spawned character has no
                // parts a script can name.
                self.script_host.set_model_slots(self.model_slots());
                true
            }
            Err(e) => {
                eprintln!("  import {path} failed: {e}");
                false
            }
        }
    }

    /// Create a new blank scene `<name>.ron`, save it, and switch the editor to it.
    pub(crate) fn new_scene(&mut self, name: &str) {
        self.reset_anim_bindings();
        self.editing_prefab = None; // a new scene is a scene (`floptle/0090`)
        let name = {
            let n = name.trim();
            if n.is_empty() { "untitled".to_string() } else { n.to_string() }
        };
        let _ = std::fs::create_dir_all(self.project_root.join("scenes"));
        let path = self.project_root.join("scenes").join(format!("{name}.ron"));
        // A starter Down gravity node so bodies fall without setup — part of
        // the NEW-scene template only (never healed back in on load): gravity
        // volumes are optional, and deleting this one sticks. Space scenes
        // with celestial bodies simply don't want it. (Parsed from RON so
        // every serde field default — visible, cast_shadow… — applies.)
        let gravity: floptle_scene::NodeDoc = ron::from_str(
            r#"(
                name: "Gravity",
                transform: (
                    translation: (0.0, 0.0, 0.0),
                    rotation: (0.0, 0.0, 0.0, 1.0),
                    scale: (1.0, 1.0, 1.0),
                ),
                matter: GravityVolume(radial: false, strength: 10.0, radius: 20.0),
                scripts: [],
            )"#,
        )
        .expect("gravity node template");
        let doc = floptle_scene::SceneDoc {
            name: name.clone(),
            lighting: floptle_scene::LightDoc::default(),
            nodes: vec![default_camera_node(), gravity],
        };
        if let Err(e) = floptle_scene::save(&doc, &path) {
            eprintln!("  new scene failed: {e}");
            return;
        }
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.set_scene_file(&path);
        self.adopt_terrain();
        self.adopt_tilesets();
        // Maps FIRST: a blockout node's paint is keyed to its triangulation,
        // and the triangulation comes out of the map store — loading paint
        // before the geometry it belongs to would find nothing to attach to
        // and quietly drop it.
        self.adopt_maps();
        self.adopt_paint();
        self.adopt_tex_paint();
        // A brand-new scene has no bake; clear whatever the last one had.
        self.adopt_scene_bakes();
        self.selection.clear();
        self.history = History::default();
        self.mesh_registry.clear();
        self.paint_meshes.clear(); // stale CPU geometry would paint the wrong vertices
        self.mesh_wire_cache.clear(); // keep the collider-wire cache in lockstep
        self.scene_dirty = false;
        self.asset_tree = build_assets(&self.project_root);
        eprintln!("  new scene: {}", path.display()); // progress, so: stderr
    }

    /// Open an existing scene `.ron` (double-clicked in Assets). Resets the world to
    /// it, loads its terrain + meshes. The caller handles unsaved-changes prompting.
    pub(crate) fn open_scene_file(&mut self, path: &str) {
        self.reset_anim_bindings();
        // Opening a scene is the way OUT of prefab editing, and the only one —
        // which is what keeps "am I editing a prefab" a question with one answer
        // (`floptle/0090`).
        self.editing_prefab = None;
        let p = Path::new(path);
        let doc = match floptle_scene::load(p) {
            Ok(d) => d,
            Err(e) => {
                // **Say so.** This was an `eprintln!`, which in a windowed build
                // goes to a terminal that is usually not there — so a scene the
                // loader refused looked exactly like a double-click that did not
                // register, and the reasonable conclusion was that opening scenes
                // is flaky. It is not: the file did not parse, and the parser
                // says where.
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("could not open {} — {e}", p.display()),
                    None,
                );
                self.toast = Some((format!("⚠  {} did not load — see the Console", p.display()), 6.0));
                eprintln!("  open scene failed: {e}");
                return;
            }
        };
        self.playing = false;
        self.paused = false;
        self.play_snapshot = None;
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.report_scene_wiring(&doc);
        self.migrate_legacy_post(&doc);
        self.set_scene_file(p);
        self.adopt_terrain();
        self.adopt_tilesets();
        // Maps FIRST: a blockout node's paint is keyed to its triangulation,
        // and the triangulation comes out of the map store — loading paint
        // before the geometry it belongs to would find nothing to attach to
        // and quietly drop it.
        self.adopt_maps();
        self.adopt_paint();
        self.adopt_tex_paint();
        // The scene's baked GI (its `.fgi`), if it has one. Absent = no bounce,
        // which is exactly how every scene rendered before v0.45.
        self.adopt_scene_bakes();
        self.register_scene_meshes();
        // A scene saved before its textures were sliced — or whose materials a
        // script built — carries a sheet grid that disagrees with the project's
        // import settings, and that disagreement draws as a sprite showing its
        // whole sheet stretched across itself.
        let corrected = crate::assets::sync_sheet_grids(
            &mut self.world,
            &self.texture_settings,
            &self.project_root.clone(),
        );
        self.selection.clear();
        self.selected_asset = None;
        self.history = History::default();
        self.scene_dirty = false;
        // …and if it DID correct something, say so and leave the scene dirty. A
        // correction the person is not told about is one they cannot save, and
        // an exported build ships the scene FILE — so a grid fixed only in
        // memory is a grid the build does not get.
        if corrected {
            self.scene_dirty = true;
            self.console.push(
                floptle_script::LogLevel::Debug,
                "some materials disagreed with their texture's spritesheet grid and were put \
                 back in step — save the scene to keep it"
                    .to_string(),
                None,
            );
        }
        self.check_autosave(); // offer crash recovery if an autosave is newer
        eprintln!("  opened scene: {}", p.display()); // progress, so: stderr
    }

    /// Register the GPU meshes named by a set of paths, importing each once.
    ///
    /// Prefer this to [`Self::register_scene_meshes`] whenever the caller knows
    /// which models arrived. Registering "everything in the scene" to account for
    /// one new prop is `floptle/0138`: spawning a desk into a room holding two
    /// thousand props cloned two thousand asset paths and re-imported all of
    /// them, per desk.
    pub(crate) fn register_meshes<'a>(&mut self, paths: impl IntoIterator<Item = &'a str>) {
        let mut seen = std::collections::HashSet::new();
        let wanted: Vec<String> =
            paths.into_iter().filter(|p| seen.insert(*p)).map(str::to_string).collect();
        for p in wanted {
            self.import_model(&p);
        }
    }

    /// Register the GPU meshes for every imported model the current scene
    /// references. For opening a scene, where "everything" is the honest answer.
    pub(crate) fn register_scene_meshes(&mut self) {
        let mesh_paths: Vec<String> = self
            .world
            .query::<Matter>()
            .filter_map(|(_, m)| match m {
                Matter::Mesh { asset_path } => Some(asset_path.clone()),
                _ => None,
            })
            .collect();
        for p in mesh_paths {
            self.import_model(&p);
        }
    }

    // ---- project paths (everything resolves against `project_root`) ----

    /// The file the open scene loads from and saves to.
    ///
    /// **This is `scene_rel`, not `scenes/{scene_name}.ron`** (`floptle/0111`).
    /// `scene_name` is only the file STEM — it is what the hierarchy header and
    /// the window title show, and what `scene.current()` hands a script. Building
    /// a save path out of it threw the subfolder away, so
    /// `scenes/cutscenes/Opening.ron` was loaded and `scenes/Opening.ron` was
    /// written: a different file, at the project root, reported as a success.
    /// Reopening loaded the original, so every edit looked reverted, while a
    /// stray file quietly accumulated the real work. Hours of it, in one report.
    ///
    /// `scene_rel` has recorded the true relative path all along — multiplayer
    /// names scenes by it on the wire. It simply was not the thing the save used.
    pub(crate) fn scene_path(&self) -> PathBuf {
        self.project_root.join(self.scene_rel_or_default())
    }

    /// Load this scene's sidecar bakes — the baked GI (`.fgi`) and the navmesh
    /// (`.fnav`) — from beside the scene file.
    ///
    /// **Every path that opens a scene has to run this**, the same way each of
    /// them runs `adopt_terrain` / `adopt_maps` / `adopt_paint`: a bake is a
    /// file keyed to the scene, not a part of its `.ron`. Two of the five ways
    /// a scene gets opened skipped it — the boot path and a `scene.load` during
    /// Play — and the symptom was a level that came up with no navmesh overlay
    /// and no bounce light while both files sat right there on disk, with
    /// nothing said. `tick_nav_autobake` re-checks `bakes_loaded_scene` every
    /// frame so a path that forgets is corrected rather than believed.
    pub(crate) fn adopt_scene_bakes(&mut self) {
        self.bakes_loaded_scene = Some(self.scene_path());
        self.load_gi();
        self.load_nav();
    }

    pub(crate) fn project_cfg_path(&self) -> PathBuf {
        self.project_root.join("project.ron")
    }

    pub(crate) fn materials_dir(&self) -> PathBuf {
        self.project_root.join("materials")
    }

    pub(crate) fn scripts_dir(&self) -> PathBuf {
        self.project_root.join("scripts")
    }

    /// Resolve an asset path the way the rest of the editor does (`ensure_texture`,
    /// the IDE): the asset tree stores paths as walked from `project_root` — which
    /// may itself be relative (the default is plain `assets`) — so a stored path is
    /// usually already resolvable AS-IS. Only a bare project-relative path (e.g. a
    /// hand-edited `shaders/foo.flsl` in a scene file) needs the root joined on.
    /// Joining unconditionally double-prefixes the root: `assets/assets/…` (ENOENT).
    pub(crate) fn resolve_asset_path(&self, path: &str) -> PathBuf {
        resolve_asset_path(&self.project_root, path)
    }

    // ---- asset file operations (the in-engine create / rename / delete) --------
    /// Create a new folder inside `dir` (auto-numbered if `new_folder` is taken),
    /// then rescan so it appears in the browser.
    pub(crate) fn new_folder(&mut self, dir: &str) {
        let target = unique_path(Path::new(dir), "new_folder", None);
        if let Err(e) = std::fs::create_dir_all(&target) {
            eprintln!("  new folder failed: {e}");
            return;
        }
        self.asset_tree = build_assets(&self.project_root);
        self.selected_asset = Some(target.to_string_lossy().to_string());
    }

    /// Create a new blank `.lua` script (seeded with a skeleton) and open it in the
    /// IDE. Scripts must live under a `scripts/` path to be recognised, so a `dir`
    /// that isn't already inside one falls back to the project `scripts/`.
    pub(crate) fn new_script(&mut self, dir: &str) {
        let dirp = PathBuf::from(dir);
        let target_dir = if dir.replace('\\', "/").contains("/scripts") {
            dirp
        } else {
            self.scripts_dir()
        };
        if let Err(e) = std::fs::create_dir_all(&target_dir) {
            eprintln!("  new script failed: {e}");
            return;
        }
        let path = unique_path(&target_dir, "script", Some("lua"));
        let name = script_name_of(&path.to_string_lossy());
        if let Err(e) = std::fs::write(&path, script_template(&name)) {
            eprintln!("  new script failed: {e}");
            return;
        }
        self.asset_tree = build_assets(&self.project_root);
        let p = path.to_string_lossy().to_string();
        self.ide.open_file(&p);
        if let Some(dock) = self.dock_state.as_mut() {
            focus_scripting_tab(dock);
        }
        self.selected_asset = Some(p.clone());
        // Immediately prompt for the name: open the naming modal with an empty field (the
        // ".lua" suffix is fixed), so you just type a name and press Enter. Cancel keeps the
        // default "script.lua".
        self.rename_target = Some((p, String::new()));
    }

    /// Create a new `.flsl` shader in `dir` (or the project's `shaders/` folder when the
    /// target isn't shader-ish), seeded from the worked-example template, opened in the
    /// IDE with the naming modal up — the same flow as a new Lua script.
    pub(crate) fn new_shader(&mut self, dir: &str) {
        let dirp = PathBuf::from(dir);
        let target_dir = if dir.replace('\\', "/").contains("/shaders") {
            dirp
        } else {
            self.project_root.join("shaders")
        };
        if let Err(e) = std::fs::create_dir_all(&target_dir) {
            eprintln!("  new shader failed: {e}");
            return;
        }
        let path = unique_path(&target_dir, "shader", Some("flsl"));
        if let Err(e) = std::fs::write(&path, floptle_shader::NEW_SHADER_TEMPLATE) {
            eprintln!("  new shader failed: {e}");
            return;
        }
        self.asset_tree = build_assets(&self.project_root);
        let p = path.to_string_lossy().to_string();
        self.ide.open_file(&p);
        if let Some(dock) = self.dock_state.as_mut() {
            focus_scripting_tab(dock);
        }
        self.selected_asset = Some(p.clone());
        self.rename_target = Some((p, String::new()));
    }

    /// Rename a file/folder to `new_name` within its current parent directory. If the
    /// typed name has no extension, the original file's extension is kept (so naming a new
    /// `.lua` script "player" yields "player.lua", and a rename can't drop the extension).
    /// The directories holding files keyed by a scene's STEM rather than by its
    /// path — terrain fields, the blockout map, vertex paint, autosaves.
    const SCENE_SIDECAR_DIRS: [&'static str; 4] = ["terrain", "maps", "paint", ".floptle/autosave"];

    /// Every sidecar that belongs to `old_stem`, paired with where it has to go
    /// for `new_stem` to find it.
    ///
    /// Matched by the `<stem>.` PREFIX rather than by a list of extensions, so
    /// this keeps working when a new per-scene file is invented — the failure
    /// mode of an extension list is that the file nobody remembered is the one
    /// that goes missing, which is exactly the bug this exists to fix.
    /// `Terrain 1.ron` and `Terrain 10.ron` do not collide, because the dot is
    /// part of the prefix.
    pub(crate) fn scene_sidecar_moves(&self, old_stem: &str, new_stem: &str) -> Vec<(PathBuf, PathBuf)> {
        let mut out = Vec::new();
        if old_stem.is_empty() || new_stem.is_empty() || old_stem == new_stem {
            return out;
        }
        let prefix = format!("{old_stem}.");
        for dir in Self::SCENE_SIDECAR_DIRS {
            let d = self.project_root.join(dir);
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            for entry in entries.flatten() {
                if !entry.file_type().is_ok_and(|t| t.is_file()) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(rest) = name.strip_prefix(&prefix) else { continue };
                out.push((d.join(&name), d.join(format!("{new_stem}.{rest}"))));
            }
        }
        out
    }

    pub(crate) fn rename_asset(&mut self, from: &str, new_name: &str) {
        let typed = new_name.trim();
        if typed.is_empty() {
            return;
        }
        let src = PathBuf::from(from);
        // The fixed suffix is everything after the FIRST dot — so compound
        // extensions (.prefab.ron, .vfx.ron, .anim.ron) survive a rename that
        // types just the base name.
        let src_name = src.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let suffix = src_name.find('.').map(|i| &src_name[i..]).unwrap_or("");
        let final_name = if !src.is_dir() && !typed.contains('.') && !suffix.is_empty() {
            format!("{typed}{suffix}")
        } else {
            typed.to_string()
        };
        let dst = src.parent().unwrap_or(Path::new(".")).join(&final_name);
        if dst == src {
            return;
        }
        if dst.exists() {
            eprintln!("  rename: {} already exists", dst.display());
            return;
        }
        // A SCENE carries files that are keyed by its stem — terrain fields, the
        // map, vertex paint, autosaves. Renaming the `.ron` alone orphans every
        // one of them, and the symptom is an empty terrain that looks exactly
        // like work that was never done.
        let is_scene = src.extension().is_some_and(|e| e == "ron")
            && src.starts_with(self.project_root.join("scenes"));
        let sidecars = if is_scene {
            let old_stem = Self::scene_name_of(&src);
            let new_stem = Self::scene_name_of(&dst);
            self.scene_sidecar_moves(&old_stem, &new_stem)
        } else {
            Vec::new()
        };
        // Decided while `src` still exists — after the rename there is nothing
        // left to compare against.
        let was_open = is_scene && {
            let open_abs = self.project_root.join(self.scene_rel_or_default());
            open_abs == src
                || open_abs.canonicalize().ok().zip(src.canonicalize().ok()).is_some_and(|(a, b)| a == b)
        };
        // Refuse the WHOLE rename if any sidecar would land on a file that is
        // already there. Half a rename leaves a scene pointing at another
        // scene's terrain, which is worse than not renaming at all.
        if let Some((_, taken)) = sidecars.iter().find(|(_, to)| to.exists()) {
            let msg = format!(
                "rename refused: {} already exists — rename or move it first",
                taken.display()
            );
            eprintln!("  {msg}");
            self.console.push(floptle_script::LogLevel::Error, msg.clone(), None);
            self.toast = Some((format!("⚠  {msg}"), 8.0));
            return;
        }
        if let Err(e) = std::fs::rename(&src, &dst) {
            eprintln!("  rename failed: {e}");
            return;
        }
        // The scene file has moved; bring its data with it. A sidecar that fails
        // to move is reported rather than swallowed — the user needs to know
        // which file to go and find.
        let mut moved = 0usize;
        for (from_p, to_p) in &sidecars {
            match std::fs::rename(from_p, to_p) {
                Ok(()) => moved += 1,
                Err(e) => {
                    let msg =
                        format!("scene renamed, but {} did not follow: {e}", from_p.display());
                    self.console.push(floptle_script::LogLevel::Error, msg, None);
                }
            }
        }
        if moved > 0 {
            self.console.push(
                floptle_script::LogLevel::Debug,
                format!("renamed {moved} file(s) that belong to this scene"),
                None,
            );
        }
        // If the OPEN scene was the one renamed, follow it — otherwise the next
        // save writes its terrain back under the old name and orphans it again.
        if was_open {
            self.set_scene_file(&dst);
        }
        let dst_str = dst.to_string_lossy().to_string();
        // Follow the file in any open IDE tab, the graph tab and the selection.
        for f in &mut self.ide.open {
            if f.path == from {
                f.path = dst_str.clone();
                f.name = final_name.clone();
            }
        }
        if self.shader_graph.path.as_deref() == Some(from) {
            self.shader_graph.path = Some(dst_str.clone());
        }
        if self.selected_asset.as_deref() == Some(from) {
            self.selected_asset = Some(dst_str.clone());
        }
        for s in self.asset_selection.iter_mut() {
            if s == from {
                *s = dst_str.clone();
            }
        }
        self.asset_tree = build_assets(&self.project_root);
    }

    /// Move asset files/folders into `dest_dir` (both absolute). Skips a source
    /// that is already there, that would overwrite an existing entry, or that is
    /// the destination itself / an ancestor of it. Rebuilds the tree once and
    /// follows the moved paths in the selection + open IDE tabs.
    pub(crate) fn move_assets(&mut self, sources: &[String], dest_dir: &Path) {
        if !dest_dir.is_dir() {
            return;
        }
        let mut moved: Vec<(String, String)> = Vec::new();
        for src in sources {
            let sp = PathBuf::from(src);
            let Some(name) = sp.file_name() else { continue };
            let dst = dest_dir.join(name);
            // No-op if already in dest; refuse to move a folder into itself/descendant.
            if sp.parent() == Some(dest_dir) || dst == sp || dest_dir.starts_with(&sp) {
                continue;
            }
            if dst.exists() {
                eprintln!("  move: {} already exists", dst.display());
                continue;
            }
            if let Err(e) = std::fs::rename(&sp, &dst) {
                eprintln!("  move failed: {e}");
                continue;
            }
            moved.push((src.clone(), dst.to_string_lossy().to_string()));
        }
        if moved.is_empty() {
            return;
        }
        // Follow moved paths in open IDE tabs, the graph tab + the selection.
        for (from, to) in &moved {
            for f in &mut self.ide.open {
                if &f.path == from {
                    f.path = to.clone();
                }
            }
            if self.shader_graph.path.as_deref() == Some(from.as_str()) {
                self.shader_graph.path = Some(to.clone());
            }
            if self.selected_asset.as_deref() == Some(from.as_str()) {
                self.selected_asset = Some(to.clone());
            }
            for s in &mut self.asset_selection {
                if s == from {
                    *s = to.clone();
                }
            }
        }
        self.asset_tree = build_assets(&self.project_root);
    }

    /// Import OS files by COPYING them into a project folder — the native
    /// file-explorer drag-and-drop. Sources are absolute paths from the OS; each
    /// lands in `dest_dir` (auto-suffixed on name collision so nothing is
    /// clobbered). Directories are copied recursively. Dropped models are
    /// registered so they're usable immediately without a reload.
    pub(crate) fn import_files(&mut self, sources: &[PathBuf], dest_dir: &Path) {
        // Guard the destination: it must be a folder inside this project (a drop
        // that resolved to nothing falls back to the project root).
        let dest = if dest_dir.is_dir() && dest_dir.starts_with(&self.project_root) {
            dest_dir.to_path_buf()
        } else {
            self.project_root.clone()
        };
        let mut imported = 0usize;
        let mut model_refs: Vec<String> = Vec::new();
        for src in sources {
            if !src.exists() {
                continue;
            }
            // Refuse to copy a folder into itself/a descendant of it.
            if src.is_dir() && dest.starts_with(src) {
                continue;
            }
            let Some(stem) = src.file_stem().map(|s| s.to_string_lossy().to_string()) else {
                continue;
            };
            let ext = src.extension().map(|e| e.to_string_lossy().to_string());
            let dst = unique_path(&dest, &stem, ext.as_deref());
            let ok = if src.is_dir() {
                copy_dir_recursive(src, &dst).is_ok()
            } else {
                std::fs::copy(src, &dst).is_ok()
            };
            if !ok {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("import: failed to copy {}", src.display()),
                    None,
                );
                continue;
            }
            imported += 1;
            // A model dropped in is ready to use immediately: register it under its
            // project-relative ref (how scenes/pickers reference meshes).
            if src.is_file()
                && crate::assets::is_model(&dst.to_string_lossy())
                && let Ok(rel) = dst.strip_prefix(&self.project_root)
            {
                model_refs.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
        if imported == 0 {
            return;
        }
        self.asset_tree = build_assets(&self.project_root);
        for r in &model_refs {
            self.import_model(r);
        }
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!(
                "imported {imported} file(s) into {}",
                dest.strip_prefix(&self.project_root)
                    .map(|p| format!("assets/{}", p.display()))
                    .unwrap_or_else(|_| dest.display().to_string())
            ),
            None,
        );
    }

    /// Open the OS's native file picker (multi-select) and import the chosen
    /// files into `dir` when the user confirms. This is the reliable
    /// cross-platform import path: rfd's XDG-desktop-portal backend works on
    /// Wayland — where winit delivers no drag-and-drop — as well as on X11,
    /// Windows and macOS. The picker runs off the UI thread (see
    /// [`crate::native_dialog`], which is also where the reason it *must* is
    /// written down), so the editor never freezes while it's open; the result is
    /// drained each frame in `apply_frame_commands`.
    pub(crate) fn open_import_dialog(&mut self, dir: std::path::PathBuf) {
        if self.import_rx.is_some() {
            return; // one dialog at a time
        }
        self.import_rx =
            Some((crate::native_dialog::pick_files("Import assets into the project"), dir));
    }

    /// Delete files/folders (recursively) and drop any references to them —
    /// IDE tabs, the asset selection, the preview. One tree rebuild at the end.
    pub(crate) fn delete_assets(&mut self, paths: &[String]) {
        for path in paths {
            let p = Path::new(path);
            let res =
                if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
            if let Err(e) = res {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("delete {path} failed: {e}"),
                    None,
                );
                continue;
            }
            self.ide.open.retain(|f| f.path != *path);
            self.ide.active = self.ide.active.filter(|&i| i < self.ide.open.len());
            if self.selected_asset.as_deref() == Some(path.as_str()) {
                self.selected_asset = None;
            }
            self.asset_selection.retain(|s| s != path);

            // Drop editor state that referenced the deleted file, so nothing re-imports
            // it, re-saves it (resurrecting the file on disk), or keeps animating against
            // a now-missing asset — the delete-while-animating hang/crash. Registry keys
            // and clip keys are project-relative, so relativize the (absolute) path.
            if let Ok(rel) = p.strip_prefix(&self.project_root)
                && let Some(rel) = rel.to_str()
            {
                self.mesh_registry.remove(rel); // model asset (a Matter::Mesh ref)
                // A `.anim.ron` clip — or a `.spriteanim.ron`, which lands in
                // the same registry and so has to leave it the same way.
                // Case-insensitively, matching the scan that put the key
                // there: stripping exactly would leave the extension on and
                // the deleted clip would stay in the registry forever.
                let clip_key = crate::anim::strip_ext(rel, floptle_scene::ANIM_CLIP_EXT)
                    .or_else(|| crate::anim::strip_ext(rel, floptle_scene::SPRITE_ANIM_EXT));
                if let Some(ck) = clip_key {
                    self.anim.clips.retain(|(k, _)| k != ck);
                    // …and it stops being a SPRITE key. Left behind, it refuses
                    // every future save to that key — forever, against a file
                    // that is no longer there.
                    self.anim.sprite_keys.remove(ck);
                    // Stop the Animating tab from re-saving (and resurrecting) this clip.
                    if self.anim_ui.clip_doc.as_ref().map(|(k, _)| k.as_str()) == Some(ck) {
                        self.anim_ui.clip_doc = None;
                        self.anim_ui.clip_dirty = false;
                        self.anim_ui.sel_anim = None;
                    }
                }
                // A deleted MODEL: clear a bone/object selection or anim target riding it,
                // then rebind everything fresh (orphaned instances simply drop).
                let rides_deleted = |e| {
                    matches!(self.world.get::<Matter>(e), Some(Matter::Mesh { asset_path }) if asset_path == rel)
                };
                if self.bone_selection.map(|(m, _)| rides_deleted(m)).unwrap_or(false) {
                    self.bone_selection = None;
                    self.pivot_edit = false;
                }
                if self.anim_ui.target.map(rides_deleted).unwrap_or(false) {
                    self.anim_ui.target = None;
                    self.anim_ui.clip_doc = None;
                    self.anim_ui.clip_dirty = false;
                }
                self.anim.clear_instances();
            }
        }
        self.asset_tree = build_assets(&self.project_root);
    }

    /// Create the standard project subfolders + seed default materials (no-op if
    /// they already exist).
    pub(crate) fn seed_project_dirs(&self) {
        for d in ["scenes", "textures", "models", "materials", "audio", "scripts"] {
            let _ = std::fs::create_dir_all(self.project_root.join(d));
        }
        let mat_dir = self.materials_dir();
        for (n, c) in [
            ("white", [1.0, 1.0, 1.0]),
            ("orange", [0.9, 0.45, 0.35]),
            ("blue", [0.4, 0.7, 0.95]),
            ("green", [0.5, 0.85, 0.45]),
            ("gray", [0.6, 0.6, 0.62]),
        ] {
            if !mat_dir.join(format!("{n}.ron")).exists() {
                let _ =
                    floptle_scene::save_material(n, &MaterialDoc { color: c, ..Default::default() }, &mat_dir);
            }
        }
        seed_default_scripts(&self.scripts_dir());
        // The action map those scripts are written against — every entry bound
        // on BOTH keyboard and gamepad. Seeded HERE rather than in `new_project`
        // so the headless `--new` path (what the Hub uses) gets it too:
        // otherwise a Hub-created project ships the converted default scripts
        // with no map for them to resolve against.
        self.seed_input_map();
        seed_example_shaders(&self.project_root);
        seed_default_effects(&self.project_root);
        crate::ui_shader_lib::seed_ui_effects(&self.project_root);
        write_lua_support(&self.project_root);
    }

    /// Report anything wrong with a freshly-loaded scene's wiring.
    ///
    /// A stale positional parent link is always *valid* — the index exists, it
    /// is just not the node the author meant — so nothing could ever catch it
    /// from the file. What CAN be caught is reported here, loudly, because the
    /// symptom otherwise reaches you as a UI bug and sends you reading UI
    /// scripts that are correct. floptle/0046.
    pub(crate) fn report_scene_wiring(&mut self, doc: &floptle_scene::SceneDoc) {
        for line in floptle_scene::validate_parents(&doc.nodes) {
            self.console.push(floptle_script::LogLevel::Error, format!("🔗 {line}"), None);
        }
        for line in floptle_scene::validate_ui_visibility(&doc.nodes) {
            self.console.push(floptle_script::LogLevel::Warn, format!("👁 {line}"), None);
        }
    }

    /// Write `input.ron` if absent, and top up an existing one with any starter
    /// entry it has no NAME for. Never overwrites and never re-adds: a project
    /// that deleted or re-scoped a binding keeps that decision across every
    /// version bump, and anything that IS added is printed. floptle/0044.
    pub(crate) fn seed_input_map(&self) {
        let mut had_map = true;
        let mut map = match floptle_input::load_map(&self.project_root) {
            Ok(Some(m)) => m,
            Ok(None) => {
                had_map = false;
                floptle_input::InputMap::default()
            }
            // Don't touch a file we couldn't parse — the editor reports it and
            // keeps the previous map; clobbering it here would destroy work.
            Err(_) => return,
        };
        // A project with NO map gets the whole starter set — that is the
        // feature. A project that HAS one has an opinion, and only gets entries
        // it has no name for at all.
        //
        // It used to top up at BINDING granularity, which could not tell "never
        // had this" from "deleted this on purpose" or "kept it but scoped it to
        // one player". So every version bump silently re-added unscoped
        // bindings a two-player game had deliberately removed — an unscoped
        // binding serves every local slot, so the re-seeded Space jumped both
        // fighters — and the rewrite took the file's explanatory comments with
        // it. That shipped into two builds before anyone re-read the file.
        // floptle/0044.
        let added = if had_map {
            map.top_up_missing(&floptle_input::InputMap::starter())
        } else {
            map = floptle_input::InputMap::starter();
            vec!["the starter set".to_string()]
        };
        if added.is_empty() {
            return;
        }
        if let Err(e) = floptle_input::save_map(&map, &self.project_root) {
            eprintln!("  could not seed input.ron: {e}");
            return;
        }
        // Never silent: this rewrites input.ron, comments and all.
        println!("  input.ron: added {}", added.join(", "));
    }

    pub(crate) fn load_materials(&self) -> Vec<(String, floptle_scene::MaterialDoc)> {
        floptle_scene::load_materials(&self.materials_dir())
    }

    /// Load the project's active scene + the file it came from: the project's
    /// chosen ENTRY scene (project.ron `entry_scene` — the same scene a build
    /// boots into, so what you open is what ships), else `scenes/first.ron`,
    /// else the first `.ron` in `scenes/`, else a tiny built-in default.
    /// The returned path's stem becomes `scene_name`, so edits save back to the same
    /// file even if the scene's internal name differs.
    pub(crate) fn load_active_scene(&self) -> (PathBuf, floptle_scene::SceneDoc) {
        let cfg = floptle_scene::load_project(&self.project_cfg_path());
        if let Some(entry) = cfg.entry_scene.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            // Resolved the way `scene.load` resolves names, so a path AND a bare
            // scene name both work. They used to disagree — this field demanded
            // `scenes/menu.ron` while `scene.load` took `menu` — and a plausible
            // `"menu"` fell through to `scenes/first.ron`, so the scene you
            // playtested was not the one that shipped.
            match crate::export::resolve_entry_scene(&self.project_root, entry) {
                Some(p) => match floptle_scene::load(&p) {
                    Ok(doc) => return (p, doc),
                    Err(e) => eprintln!("  entry scene {entry} failed to load ({e}); falling back"),
                },
                None => eprintln!("  entry scene {entry} doesn't exist; falling back"),
            }
        }
        let first = self.project_root.join("scenes/first.ron");
        if let Ok(doc) = floptle_scene::load(&first) {
            return (first, doc);
        }
        let scenes = self.project_root.join("scenes");
        let mut rons: Vec<PathBuf> = std::fs::read_dir(&scenes)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "ron"))
            .collect();
        rons.sort();
        for p in &rons {
            if let Ok(doc) = floptle_scene::load(p) {
                return (p.clone(), doc);
            }
        }
        (first, default_scene())
    }

    /// Track the open scene file: its stem (the name edits save under) plus its
    /// project-root-relative path (what multiplayer sessions name scenes by on
    /// the wire — `scene_rel`).
    pub(crate) fn set_scene_file(&mut self, path: &Path) {
        self.scene_name = Self::scene_name_of(path);
        self.scene_rel = path
            .strip_prefix(&self.project_root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| format!("scenes/{}.ron", self.scene_name));
        // A new scene's tree starts FOLDED. Every path that replaces the world comes
        // through here, so this is the one place that has to say so — the alternative was
        // six call sites and a seventh added later that forgot.
        //
        // `collapsed` is opt-in, so an empty set means "everything open", and a scene of
        // any size opened as a fully-expanded wall of rows. Seeding happens in the
        // Hierarchy itself, where the parent⏵children map already exists.
        self.hier_fold_pending = true;
        self.collapsed.clear();
    }

    /// Say so when this project is already split by `floptle/0111`.
    ///
    /// The fix stops NEW saves going astray; it cannot know that the stray file
    /// is there, and the user has no reason to look. Left unsaid, they reopen
    /// the project, see the old values again, and conclude the fix did not work
    /// — while their real edits sit in a file nothing loads.
    ///
    /// Names files and stops. Merging is the user's call: two files, both
    /// plausibly wanted, and an editor quietly picking one is how this started.
    fn warn_about_shadowed_scenes(&mut self) {
        let split = scenes_shadowed_by_a_root_copy(&self.project_root);
        if split.is_empty() {
            return;
        }
        for rel in &split {
            let stem = rel.rsplit('/').next().unwrap_or(rel);
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "⚠ {rel} is shadowed by scenes/{stem} — a version of Floptle before v0.37.1 \
                     saved subfolder scenes to the project root, so scenes/{stem} probably holds \
                     edits you made and never saw. Compare the two; the root one is usually the \
                     newer. Nothing has been moved."
                ),
                None,
            );
        }
        self.toast = Some((
            format!("⚠  {} scene(s) have a stray copy at scenes/ — see the Console", split.len()),
            8.0,
        ));
    }

    /// `scene_rel`, or the `scenes/<name>.ron` convention if it was never set.
    pub(crate) fn scene_rel_or_default(&self) -> String {
        if self.scene_rel.is_empty() {
            format!("scenes/{}.ron", self.scene_name)
        } else {
            self.scene_rel.clone()
        }
    }

    /// The scene-file stem (the name edits save under).
    pub(crate) fn scene_name_of(path: &std::path::Path) -> String {
        path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "untitled".into())
    }

    /// Switch the editor to the project rooted at `root`, reloading everything.
    pub(crate) fn open_project(&mut self, root: PathBuf) {
        self.reset_anim_bindings();
        self.project_root = root;
        self.seed_project_dirs();
        let (path, doc) = self.load_active_scene();
        self.set_scene_file(&path);
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.report_scene_wiring(&doc);
        self.adopt_terrain();
        self.adopt_tilesets();
        // Maps FIRST: a blockout node's paint is keyed to its triangulation,
        // and the triangulation comes out of the map store — loading paint
        // before the geometry it belongs to would find nothing to attach to
        // and quietly drop it.
        self.adopt_maps();
        self.adopt_paint();
        self.adopt_tex_paint();
        // **The bakes, exactly as opening a scene loads them.** Opening a
        // project restores its active scene, which makes this a scene load in
        // every way that matters — and it was the one scene load that skipped
        // these two. So the navmesh and the baked GI came back empty every time
        // the editor started or a project was switched to, and the only symptom
        // was a level with no bake in it: no navmesh overlay, no bounce light,
        // nothing said. Both files were sitting right there beside the scene.
        // The reasonable conclusion is that baking does not stick, and the
        // reasonable response is to bake again, every single session.
        self.adopt_scene_bakes();
        self.project = floptle_scene::load_project(&self.project_cfg_path());
        // The action map belongs to the project, so it reloads with it —
        // otherwise the new project's scripts would resolve against the old
        // project's actions.
        self.load_input_map();
        self.migrate_legacy_post(&doc);
        self.check_autosave(); // offer crash recovery if an autosave is newer
        self.warn_about_shadowed_scenes();
        self.materials = self.load_materials();
        // Packages last: an extension's first line may read the project's
        // settings, its scenes or its assets, and all of those have to be
        // loaded before it does.
        self.ext_reload();
        // Re-scan the animation + particle registries against the NEW project
        // root. Without this they kept pointing at whatever was scanned at editor
        // startup (e.g. the workspace's `assets/`), so opening another project
        // found none of ITS controllers or effects: characters T-posed (the
        // controller key never resolved) and every spawnEffect / plume silently
        // no-op'd (the effect key never resolved). Project-scoped assets MUST
        // follow the project. (Meshes below + flsl materials each frame already
        // resolve against project_root; these two registries were the gap.)
        self.anim.rescan(&self.project_root);
        self.vfx.rescan(&self.project_root);
        self.asset_tree = build_assets(&self.project_root);
        self.load_texture_settings();
        self.texture_registry.clear();
        self.texture_registry_setting.clear();
        // Shader pipelines/bindings live in the (kept) raster pass but their
        // TexIds and paths belong to the old project — recompile fresh.
        self.clear_flsl_state();
        self.selection.clear();
        self.selected_asset = None;
        self.ide = IdeState::default();
        self.history = History::default();
        self.playing = false;
        self.paused = false;
        // A different project's models live behind the same path strings, so drop the
        // old GPU-mesh cache before re-importing (else import_model early-returns).
        self.mesh_registry.clear();
        self.paint_meshes.clear(); // stale CPU geometry would paint the wrong vertices
        self.mesh_wire_cache.clear(); // keep the collider-wire cache in lockstep
        // Re-register any meshes the new scene references.
        let mesh_paths: Vec<String> = self
            .world
            .query::<Matter>()
            .filter_map(|(_, m)| match m {
                Matter::Mesh { asset_path } => Some(asset_path.clone()),
                _ => None,
            })
            .collect();
        for p in mesh_paths {
            self.import_model(&p);
        }
        // stderr, not stdout. This is progress chatter, and stdout belongs to
        // whatever the caller asked for — a command-line verb's `--json`
        // document is on it, and one stray line of prose makes that document
        // unparseable. Nothing reads this line; the Hub parses `--list-templates`
        // and nothing else.
        eprintln!("  opened project {}", self.project_root.display());
    }

    /// Create a fresh project at `root` (folders + a starter scene + example
    /// scripts), then open it.
    pub(crate) fn new_project(&mut self, root: PathBuf) {
        let _ = std::fs::create_dir_all(root.join("scenes"));
        let _ = std::fs::create_dir_all(root.join("scripts"));
        // A starter scene if none exists yet.
        let first = root.join("scenes/first.ron");
        let fresh = !first.exists();
        if fresh {
            let _ = floptle_scene::save(&default_scene(), &first);
        }
        // Ship the default Lua scripts so the IDE/docs have something to show.
        // (`input.ron`, the action map they're written against, is seeded by
        // `seed_project_dirs` — shared with the headless `--new` path.)
        seed_default_scripts(&root.join("scripts"));
        seed_example_shaders(&root);
        crate::ui_shader_lib::seed_ui_effects(&root);
        self.open_project(root);
        if fresh {
            // The polished blank slate: full-resolution rendering (retro is one
            // click away in the toolbar, but a new project shouldn't start
            // pixelated), and a sculpt-ready grassy ground slab under the
            // starter physics shapes. Terrain lives in sidecar field files, so
            // it can't ship inside the scene template — generate + save it here.
            self.project.retro = false;
            let _ = floptle_scene::save_project(&self.project, &self.project_cfg_path());
            self.create_terrain(&crate::terrain_ui::NewTerrainCfg {
                size_xz: 48.0,
                thickness: 12.0,
                color: [0.35, 0.6, 0.28],
                texture: String::new(),
            });
            if let Some(e) = self.active_terrain
                && let Some(t) = self.world.get_mut::<floptle_core::Transform>(e)
            {
                t.translation = floptle_core::math::DVec3::ZERO;
            }
            self.selection.clear();
            self.history = History::default(); // a template isn't an undoable edit
            self.save_scene();
            self.scene_dirty = false;
        }
    }

    /// Close the current project: empty world, no selection, clean history.
    pub(crate) fn close_project(&mut self) {
        // Before anything else is torn down: an extension's `onUnload` may want
        // to save what it was holding, and its stores are written here.
        self.ext.save_prefs();
        self.ext.teardown();
        self.ext_painted.clear();
        self.reset_anim_bindings();
        self.world = World::new();
        floptle_scene::spawn_into(&empty_scene(), &mut self.world);
        self.scene_name = "untitled".into();
        self.scene_rel = String::new();
        self.terrains.clear();
        self.active_terrain = None;
        self.terrain_slots.clear();
        self.selection.clear();
        self.selected_asset = None;
        self.ide = IdeState::default();
        self.history = History::default();
        self.playing = false;
        self.paused = false;
        self.mesh_registry.clear();
        self.paint_meshes.clear(); // stale CPU geometry would paint the wrong vertices
        self.mesh_wire_cache.clear(); // keep the collider-wire cache in lockstep
    }

    /// Save the open scene (+ its terrain fields/palette). Success clears the
    /// dirty flag and the crash-recovery autosave; FAILURE keeps both and lands
    /// in the Console loudly — a failed save must never look like a saved one
    /// (the old path printed to stderr and callers cleared `scene_dirty`
    /// unconditionally, which could silently lose work).
    pub(crate) fn save_scene(&mut self) -> bool {
        // NEVER save during Play: the world holds simulation state (moved
        // bodies, script spawns), and a mid-play `scene.load(...)` may have
        // swapped in ANOTHER scene entirely — writing that over the edited
        // scene's file (and its terrain) is exactly how work gets lost.
        if self.playing {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "💾 not saved — can't save the scene during Play (Stop first; Play changes aren't kept)".into(),
                None,
            );
            return false;
        }
        // Editing a prefab on its own (`floptle/0090`): the world IS the prefab,
        // so a save writes it back over that file and stops. None of what
        // follows applies — a prefab has no terrain fields, no map geometry and
        // no paint sidecars, and writing them out under its name is exactly the
        // mess this branch exists to avoid.
        if self.editing_prefab.is_some() {
            let ok = self.save_prefab_in_place();
            if ok {
                self.scene_dirty = false;
                self.save_flash = Editor::SAVE_FLASH_SECS; // the status chip confirms it
                let _ = std::fs::remove_file(self.autosave_path()); // saved for real
            }
            return ok;
        }
        let path = self.scene_path();
        // The scene's OWN directory, not `scenes/` — a scene under
        // `scenes/cutscenes/` needs that folder to exist, and hardcoding the
        // parent was half of why a subfolder scene could never be written back
        // (`floptle/0111`).
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let doc = floptle_scene::to_doc(self.scene_name.clone(), &self.world);
        // Aggregated over EVERYTHING this save writes — the scene doc, terrain
        // fields, paint, map geometry, the palette. The always-visible status
        // chip rests on this flag, so it must never read "saved" while a
        // sidecar full of sculpting is still only in memory.
        let mut ok = match floptle_scene::save(&doc, &path) {
            Ok(()) => {
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!("💾 saved {}", path.display()),
                    None,
                );
                true
            }
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("💾 SAVE FAILED — {} — {e} (your changes are still unsaved!)", path.display()),
                    None,
                );
                false
            }
        };
        // Terrain fields are large, so each lives beside the scene (one file per
        // terrain id), not inline in the scene doc.
        let dir = self.project_root.join("terrain");
        let _ = std::fs::create_dir_all(&dir);
        let terrain_writes: Vec<(u32, Vec<u8>)> = self
            .terrains
            .iter()
            .filter_map(|(&e, t)| match self.world.get::<Matter>(e) {
                Some(Matter::Terrain { id }) => Some((*id, t.field.to_bytes())),
                _ => None,
            })
            .collect();
        let mut saved_ids: Vec<u32> = Vec::new();
        for (id, bytes) in terrain_writes {
            if let Err(e) = std::fs::write(self.terrain_field_path_id(id), bytes) {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("💾 save terrain {id} failed: {e}"),
                    None,
                );
                ok = false;
            } else {
                saved_ids.push(id);
            }
        }
        // G1 residency: a written field is no longer disk-dirty (an eviction can
        // drop it without re-saving). Flags for FAILED writes stay set — eviction
        // must never discard unsaved edits.
        let world = &self.world;
        self.terrain_disk_dirty.retain(|e| {
            !matches!(world.get::<Matter>(*e),
                Some(Matter::Terrain { id }) if saved_ids.contains(id))
        });
        // Stamp each saved celestial field's residency sidecar: impostor color +
        // the genspec hash it was written under. The hash is what lets streaming
        // trust this exact file for this exact body — without it, a regenerated
        // system's stale same-id file would refuse to load (or worse, an unstamped
        // one would load the WRONG planet).
        let stamps: Vec<(u32, [f32; 3], Option<u64>)> = self
            .terrains
            .iter()
            .filter_map(|(&e, t)| {
                let id = match self.world.get::<Matter>(e) {
                    Some(Matter::Terrain { id }) if saved_ids.contains(id) => *id,
                    _ => return None,
                };
                let cb = self.world.get::<floptle_core::CelestialBody>(e)?;
                let color = self
                    .terrain_render
                    .get(&e)
                    .and_then(|r| r.impostor_color)
                    .unwrap_or_else(|| {
                        crate::terrain_edit::impostor_surface_color(
                            &t.field,
                            cb.body_radius as f32,
                        )
                    });
                Some((id, color, self.terrain_spec_hash_of(e)))
            })
            .collect();
        for (id, color, hash) in stamps {
            self.write_terrain_meta(id, color, hash);
        }
        // Vertex paint: per-vertex arrays live beside the scene for the same reason
        // terrain fields do — they have no business in a .ron.
        ok &= self.save_paint();
        ok &= self.save_tex_paint();
        ok &= self.save_maps();
        // The texture PALETTE (which image fills each painted slot) is editor state,
        // not in the field — persist it so painted textures survive a reload. Glowing
        // slots keep their `|glow` marker (see adopt_terrain's load). Cold terrains
        // count: their fields still splat this palette when they stream back in.
        if !self.terrains.is_empty() || !self.terrain_cold.is_empty() {
            let palette: Vec<String> = self
                .terrain_textures
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    if self.terrain_glow_mask & (1 << i.min(31)) != 0 {
                        format!("{p}|glow")
                    } else {
                        p.clone()
                    }
                })
                .collect();
            if let Err(e) = std::fs::write(self.terrain_palette_path(), palette.join("\n")) {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("💾 save terrain palette failed: {e}"),
                    None,
                );
                ok = false;
            }
        }
        if ok {
            self.scene_dirty = false;
            // Visible confirmation, not just Console: the menu-bar status chip
            // glows "✓ saved" for a moment, wherever you're docked.
            self.save_flash = Editor::SAVE_FLASH_SECS;
            let _ = std::fs::remove_file(self.autosave_path()); // saved for real
        } else {
            // ANY failed write — the scene doc or a sidecar full of sculpting —
            // must be as visible as a success, and the chip stays "unsaved".
            self.toast = Some((
                "⚠  save FAILED — your changes are still unsaved, see the Console".into(),
                6.0,
            ));
        }
        ok
    }

    /// How long the save-status chip glows after a successful save.
    pub(crate) const SAVE_FLASH_SECS: f32 = 2.5;

    /// Where this scene's crash-recovery autosave lives (`.floptle` is the
    /// project's editor-cache dir, never exported).
    pub(crate) fn autosave_path(&self) -> PathBuf {
        let dir = self.project_root.join(".floptle/autosave");
        match &self.editing_prefab {
            // A prefab's autosave gets its own name (`floptle/0090`). Sharing the
            // scene's would mean a project that later grows a scene of the same
            // name is offered a recovery holding a prefab's nodes — a trap worth
            // one suffix to close.
            Some(_) => dir.join(format!("{}.prefab-autosave.ron", self.scene_name)),
            None => dir.join(format!("{}.ron", self.scene_name)),
        }
    }

    /// The file the editor is currently editing — the open prefab, or the
    /// scene. What an autosave is judged newer or older *than*.
    pub(crate) fn edited_file_path(&self) -> PathBuf {
        self.editing_prefab.clone().unwrap_or_else(|| self.scene_path())
    }

    /// Periodic crash safety: while the scene is dirty in edit mode, snapshot
    /// it to the autosave file every [`Self::AUTOSAVE_SECS`]. Real saves delete
    /// it; a crash leaves it behind, and the next open offers to restore.
    pub(crate) fn autosave_tick(&mut self) {
        const AUTOSAVE_SECS: u64 = 45;
        if !self.scene_dirty || self.playing || self.player_mode || self.anim_ui.record {
            return;
        }
        let due = self
            .last_autosave
            .is_none_or(|t| t.elapsed().as_secs() >= AUTOSAVE_SECS);
        if !due {
            return;
        }
        self.last_autosave = Some(std::time::Instant::now());
        let path = self.autosave_path();
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&self.project_root));
        let doc = floptle_scene::to_doc(self.scene_name.clone(), &self.world);
        if let Err(e) = floptle_scene::save(&doc, &path) {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!("autosave failed: {e}"),
                None,
            );
        }
    }

    /// After a scene loads: if a NEWER autosave exists (a crash or lost session
    /// left unsaved work behind), arm the recovery prompt.
    pub(crate) fn check_autosave(&mut self) {
        self.autosave_prompt = None;
        let auto = self.autosave_path();
        let Ok(auto_m) = std::fs::metadata(&auto).and_then(|m| m.modified()) else { return };
        let scene_m = std::fs::metadata(self.edited_file_path()).and_then(|m| m.modified()).ok();
        if scene_m.is_none_or(|s| auto_m > s) {
            self.autosave_prompt = Some(auto);
        }
    }

    /// Restore the armed autosave over the live world (the file stays until a
    /// real save — restoring must never destroy the only copy of the work).
    pub(crate) fn restore_autosave(&mut self) {
        let Some(path) = self.autosave_prompt.take() else { return };
        let doc = match floptle_scene::load(&path) {
            Ok(d) => d,
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("autosave restore failed: {e}"),
                    None,
                );
                return;
            }
        };
        self.reset_anim_bindings();
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.adopt_terrain();
        self.adopt_tilesets();
        // Maps FIRST: a blockout node's paint is keyed to its triangulation,
        // and the triangulation comes out of the map store — loading paint
        // before the geometry it belongs to would find nothing to attach to
        // and quietly drop it.
        self.adopt_maps();
        self.adopt_paint();
        self.adopt_tex_paint();
        self.register_scene_meshes();
        self.selection.clear();
        self.history = History::default();
        self.scene_dirty = true; // recovered work is UNSAVED until a real save
        self.console.push(
            floptle_script::LogLevel::Debug,
            "recovered the autosaved scene — Ctrl+S to keep it".into(),
            None,
        );
    }

    /// Where the scene's terrain texture palette (slot→image paths) is stored.
    pub(crate) fn terrain_palette_path(&self) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.palette", self.scene_name))
    }

    /// Is there work that would be lost by closing right now?
    ///
    /// ONE definition, so the window's close button, Ctrl+Q and the confirm
    /// dialog cannot disagree about what counts. Tilesets are in here because
    /// they are edited from a dock tab like everything else and their file is
    /// not the scene's — a level's collision shapes used to walk out the door
    /// without a word.
    pub(crate) fn unsaved_work(&self) -> bool {
        self.scene_dirty || self.image.dirty || !self.tiles.dirty.is_empty()
    }

    /// Ctrl+S: save everything — the project config, the open scene, and every
    /// dirty script open in the IDE (so "the script you're editing" is saved too).
    pub(crate) fn save_all(&mut self) {
        // While recording, the world carries previewed clip values — saving would
        // bake them into the scene file. End the recording (restoring the real
        // scene) first; the clip itself saves through its own dirty flag.
        self.stop_recording();
        // Any pending graph-canvas edit lands on disk with everything else.
        self.shader_graph.flush(&self.project_root, &mut self.ide, true, false);
        // …and so does an edited image document (Ctrl+S over the 🖼 tab means
        // the same thing it means everywhere else). A document that was never
        // given a name asks for one instead of inventing a file.
        if self.image.dirty && self.image.doc.is_some() {
            self.save_image_doc();
        }
        self.save_scene(); // clears scene_dirty ONLY on success + logs either way
        // Tilesets. **Ctrl+S did not write these**, and that is the whole of the
        // "my tile collision shapes are gone every time I reopen the project"
        // report: a tileset's solid flags, its collision polygons, its autotile
        // groups and its tags all live in `tilesets/*.tileset.ron`, and the ONLY
        // thing that ever wrote that file was a small `Save` button inside the
        // ◫ Tiles tab. Everything else about the level — the squares you painted,
        // the layer nodes — is scene state and saved fine, so the level came back
        // looking correct and collided with nothing.
        //
        // Save-everything has to mean everything. `save_tilesets` is already a
        // no-op when nothing is dirty and already refuses to write over a file it
        // could not parse, so this is safe to call unconditionally.
        self.save_tilesets();
        if let Err(e) = floptle_scene::save_project(&self.project, &self.project_cfg_path()) {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!("💾 save project.ron failed: {e}"),
                None,
            );
        }
        let mut saved_scripts = 0;
        for f in &mut self.ide.open {
            if f.dirty && std::fs::write(&f.path, &f.text).is_ok() {
                f.dirty = false;
                saved_scripts += 1;
            }
        }
        if saved_scripts > 0 {
            self.console.push(
                floptle_script::LogLevel::Debug,
                format!("💾 saved {saved_scripts} script(s)"),
                None,
            );
        }
    }
}

/// Open `path` in the OS file manager (xdg-open / open / explorer).
/// Every scene file in the project, as `scenes/...ron` project-root-relative
/// paths (recursive, sorted) — the entry-scene picker's option list. A free
/// function over the root so callers holding other `Editor` field borrows can
/// still use it.
pub(crate) fn scene_files_in(project_root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if p.extension().is_some_and(|x| x == "ron")
                && !p.to_string_lossy().ends_with(floptle_scene::PREFAB_EXT)
                && let Ok(rel) = p.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(&project_root.join("scenes"), project_root, &mut out);
    out.sort();
    out
}

pub(crate) fn open_in_file_manager(path: &Path) {
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    let _ = std::process::Command::new(cmd)
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Seed the built-in example shaders into `<project>/shaders/examples/` —
/// teaching material for the ◈ Shaders graph (each is a worked example of one
/// corner of the system). A project WITHOUT the folder gets the full set; an
/// existing folder only gains examples it doesn't have yet (so new built-ins
/// arrive with engine updates, edits to seeded files are never overwritten,
/// and deleting the whole folder is the opt-out that sticks).
/// Effects the shipped scripts fire by name. Written only if absent, so an
/// edited copy is never overwritten — and a script that names a missing effect
/// is a silent no-op, so this is the difference between the RTS starter kit
/// showing a puff where you clicked and showing nothing.
pub(crate) fn seed_default_effects(project_root: &Path) {
    const DEFAULT_EFFECTS: &[(&str, &str)] = &[(
        "MoveMarker.vfx.ron",
        include_str!("../../../assets/vfx/MoveMarker.vfx.ron"),
    )];
    let dir = project_root.join("vfx");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    for (name, body) in DEFAULT_EFFECTS {
        let p = dir.join(name);
        if !p.exists() {
            let _ = std::fs::write(&p, body);
        }
    }
}

pub(crate) fn seed_example_shaders(project_root: &Path) {
    let dir = project_root.join("shaders").join("examples");
    // The stamp remembers that this project was seeded once — so a missing
    // folder afterwards means the user deleted it, and it stays deleted.
    let stamp = project_root.join(".floptle").join("examples_seeded");
    if !dir.exists() {
        if stamp.exists() {
            return; // deleted on purpose
        }
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
    }
    for (name, src) in floptle_shader::examples::EXAMPLES {
        let path = dir.join(name);
        if !path.exists() {
            let _ = std::fs::write(path, src);
        }
    }
    let _ = std::fs::create_dir_all(project_root.join(".floptle"));
    let _ = std::fs::write(&stamp, "");
}

/// See [`Editor::resolve_asset_path`] — free so it's unit-testable without an Editor.
///
/// Resolution order: absolute as-is → as-written relative to the CWD (the legacy
/// repo-root workflow, where refs spell `assets/…`) → joined onto the project root
/// (the canonical, portable form: `textures/…`) → the LEGACY-PREFIX RESCUE: a ref
/// whose first component IS the project folder's name (`assets/textures/x.png`
/// inside a project rooted at `…/assets`) gets that component stripped and re-joined.
/// The rescue is what keeps old projects working when the editor is launched from
/// anywhere but the project's parent dir — the Hub launches with an absolute root
/// and the project dir as CWD, which broke every legacy ref ("everything
/// dereferenced", 2026-07-20). Missing files fall back to the canonical join.
pub(crate) fn resolve_asset_path(project_root: &Path, path: &str) -> PathBuf {
    // `pkg://<id>/<rest>` — a package's own file, addressed by the package's
    // IDENTITY rather than by where its folder happens to be. That is the whole
    // point of the scheme: the same reference works whether the package was
    // copied into the project, linked to a working copy on another disk, or
    // renamed on the way in.
    if let Some(p) = resolve_pkg_ref(project_root, path) {
        return p;
    }
    let p = PathBuf::from(path);
    if p.is_absolute() || p.exists() {
        return p;
    }
    let joined = project_root.join(&p);
    if joined.exists() {
        return joined;
    }
    if let (Some(first), Some(root_name)) = (p.components().next(), project_root.file_name())
        && first.as_os_str() == root_name
    {
        let stripped: PathBuf = p.components().skip(1).collect();
        let rescued = project_root.join(stripped);
        if rescued.exists() {
            return rescued;
        }
    }
    joined
}

/// Resolve a `pkg://<id>/<rest>` reference, or `None` if it is not one.
///
/// Two answers, in order. A **linked** package is read where it is being
/// written, so the editor keeps a small id → folder table filled in when
/// packages load ([`set_package_roots`]). Everything else lives at the one
/// place installed packages live, `<project>/packages/<id>/` — which is also
/// where an exported build's copy lands, so a `pkg://` reference in a scene
/// resolves in the player with no table at all.
pub(crate) fn resolve_pkg_ref(project_root: &Path, path: &str) -> Option<PathBuf> {
    let rest = path.strip_prefix(floptle_package::PKG_SCHEME)?;
    let (id, rel) = rest.split_once('/')?;
    if rel.is_empty() || id.is_empty() {
        return None;
    }
    // A `..` here would let one package address another's files, or the
    // project's. The manifest validator refuses them in folder lists for the
    // same reason.
    if Path::new(rel).components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return None;
    }
    let linked = PACKAGE_ROOTS.with(|m| m.borrow().get(id).cloned());
    Some(match linked {
        Some(root) => root.join(rel),
        None => project_root.join(floptle_package::PACKAGES_DIR).join(id).join(rel),
    })
}

thread_local! {
    /// Where each loaded package's folder is, by id.
    ///
    /// Global rather than threaded through `resolve_asset_path` because that
    /// function is called from around fifty places that have a project root and
    /// nothing else, and this is written exactly once per package load and only
    /// ever read. It exists for LINKED packages; every other kind resolves from
    /// the project root alone.
    static PACKAGE_ROOTS: std::cell::RefCell<std::collections::HashMap<String, PathBuf>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Tell `pkg://` where the loaded packages are. Called when they load.
pub(crate) fn set_package_roots(roots: Vec<(String, PathBuf)>) {
    PACKAGE_ROOTS.with(|m| {
        let mut m = m.borrow_mut();
        m.clear();
        m.extend(roots);
    });
}

/// An empty scene (just lighting) — used when a project is closed.
/// A default camera node (active) looking at the origin from up + back, so every new
/// scene starts with a viewpoint that play mode can render from.
fn default_camera_node() -> floptle_scene::NodeDoc {
    let pos = Vec3::new(0.0, 3.0, 9.0);
    let fwd = (Vec3::ZERO - pos).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd);
    let rot = Quat::from_mat3(&Mat3::from_cols(right, up, -fwd));
    floptle_scene::NodeDoc {
        camera_2d: None,
        sort_mode: None,
        parallax: None,
        id: None,
        parent_id: None,
        terrain_gen: None,
        name: "Camera".into(),
        transform: floptle_scene::TransformDoc {
            translation: [pos.x as f64, pos.y as f64, pos.z as f64],
            rotation: rot.to_array(),
            scale: [1.0, 1.0, 1.0],
        },
        matter: floptle_scene::MatterDoc::Camera {
            fov_y: 60f32.to_radians(),
            active: true,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: floptle_core::Matter::TARGET_W,
            target_h: floptle_core::Matter::TARGET_H,
            target_hz: 0.0,
            ortho: false,
            ortho_height: floptle_core::Matter::ORTHO_HEIGHT,
        },
        // The default camera flies on play (hold right-mouse to look, WASD to move).
        scripts: vec![floptle_scene::ScriptDoc {
            kind: "freelook".into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
        }],
        material: None,
        object_materials: Default::default(),
        tint: None,
        rigidbody: None,
        celestial: None,
        mesh_collider: false,
        disabled: false,
        paint: None,
        tex_paint: None,
        collidable: false,
        trigger: false,
        nav_exclude: false,
        visible: true,
        cast_shadow: true,
        anim_controller: None,
        particles: None,
        parent: None,
        attachment: None,
        net: None,
        ui_layer: None,
        ui: None,
        audio: None,
        layer: None,
        tags: Vec::new(),
        sorting: None,
        lit_2d: None,
        light_layers: Vec::new(),
        shadow_2d: None,
        light_inner: None,
        light_falloff: None,
        light_shadows: None,
    }
}

fn empty_scene() -> floptle_scene::SceneDoc {
    floptle_scene::SceneDoc {
        name: "untitled".into(),
        lighting: floptle_scene::LightDoc::default(),
        nodes: vec![default_camera_node()],
    }
}

/// A tiny built-in scene used if `assets/scenes/first.ron` is missing.
pub(crate) fn default_scene() -> floptle_scene::SceneDoc {
    // A polished blank slate (RON so serde defaults fill the long NodeDoc tail):
    // a warm sun with soft dithered distance fog, a Down gravity volume, and a
    // few physics shapes hovering over the ground — press Play and they tumble.
    // The camera flies (freelook). The ground terrain itself is generated by
    // `new_project` (terrain lives in sidecar field files, not scene RON).
    let mut doc: floptle_scene::SceneDoc = ron::from_str(
        r#"(
        name: "first",
        lighting: (
            direction: (0.35, 0.85, 0.4),
            color: (1.0, 0.97, 0.9),
            ambient: (0.16, 0.17, 0.21),
            intensity: 1.0,
            fog: true,
            fog_color: (0.63, 0.68, 0.78),
            fog_start: 45.0,
            fog_end: 220.0,
            fog_dither: true,
            fog_dither_strength: 0.4,
        ),
        nodes: [
            (
                name: "Gravity",
                transform: (
                    translation: (0.0, 0.0, 0.0),
                    rotation: (0.0, 0.0, 0.0, 1.0),
                    scale: (1.0, 1.0, 1.0),
                ),
                matter: GravityVolume(radial: false, strength: 9.81, radius: 80.0),
                scripts: [],
            ),
            (
                name: "crate",
                transform: (
                    translation: (-1.6, 2.2, 0.3),
                    rotation: (0.0, 0.13, 0.0, 0.99),
                    scale: (1.0, 1.0, 1.0),
                ),
                matter: Primitive(shape: Cube, color: (0.85, 0.55, 0.35)),
                scripts: [],
                rigidbody: Some((boxed: true, half_extents: (0.5, 0.5, 0.5))),
            ),
            (
                name: "ball",
                transform: (
                    translation: (1.4, 3.2, -0.4),
                    rotation: (0.0, 0.0, 0.0, 1.0),
                    scale: (1.0, 1.0, 1.0),
                ),
                matter: Primitive(shape: Sphere, color: (0.4, 0.7, 0.95)),
                scripts: [],
                rigidbody: Some((radius: 0.5, restitution: 0.45)),
            ),
            (
                name: "capsule",
                transform: (
                    translation: (0.3, 4.4, -1.4),
                    rotation: (0.0, 0.0, 0.0, 1.0),
                    scale: (1.0, 1.0, 1.0),
                ),
                matter: Primitive(shape: Capsule, color: (0.65, 0.85, 0.5)),
                scripts: [],
                rigidbody: Some((capsule: true, radius: 0.5, height: 2.0)),
            ),
        ],
    )"#,
    )
    .expect("default scene template");
    doc.nodes.push(default_camera_node());
    doc
}

/// Recursively copy `src` (a directory) to `dst`, creating `dst` and every
/// subfolder. Used when a whole folder is dragged in from the OS file explorer.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}


/// Scenes in a subfolder that have a same-named file sitting at `scenes/` root.
///
/// That pair is the signature of `floptle/0111`: before the fix, editing
/// `scenes/<sub>/<name>.ron` wrote `scenes/<name>.ron` instead. A project
/// carrying both has edits in the root copy that the game has never loaded, and
/// the root one is almost certainly the newer, wanted work.
///
/// Returned as the SUBFOLDER paths, because that is the file the user thinks
/// they have been editing and the one they will want the other merged into.
/// This only reports; nothing is moved. Guessing which of two files a person
/// wants to keep is not a guess an editor gets to make silently — and the whole
/// bug was an editor being confident about a path.
pub(crate) fn scenes_shadowed_by_a_root_copy(root: &Path) -> Vec<String> {
    let scenes = root.join("scenes");
    let at_root: std::collections::HashSet<String> = std::fs::read_dir(&scenes)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_suffix(".ron").map(str::to_owned)
        })
        .collect();
    if at_root.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = std::fs::read_dir(&scenes)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Some(stem) = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".ron"))
            else {
                continue;
            };
            if at_root.contains(stem)
                && let Ok(rel) = p.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod path_tests {
    use super::*;

    /// The starter-scene RON template must parse (it's an `expect` at project
    /// creation) and actually contain the polished-blank-slate pieces: gravity,
    /// physics shapes, fog on, and a camera.
    #[test]
    fn default_scene_template_parses_with_the_starter_pieces() {
        let d = default_scene();
        assert!(d.lighting.fog);
        assert!(d
            .nodes
            .iter()
            .any(|n| matches!(n.matter, floptle_scene::MatterDoc::GravityVolume { .. })));
        assert!(d.nodes.iter().filter(|n| n.rigidbody.is_some()).count() >= 3);
        let cam = d
            .nodes
            .iter()
            .find(|n| matches!(n.matter, floptle_scene::MatterDoc::Camera { .. }))
            .expect("camera node");
        assert!(cam.scripts.iter().any(|s| s.kind == "freelook"));
        // …and the flycam script it references is actually seeded into projects.
        assert!(crate::lua_support::DEFAULT_SCRIPTS.iter().any(|(n, _)| *n == "freelook.lua"));
    }

    /// The bug this guards: the asset picker stores paths as walked from
    /// `project_root` (default: the RELATIVE `assets`), and joining that root on
    /// again gave `assets/assets/…` — "can't read shader (os error 2)".
    #[test]
    fn asset_paths_resolve_without_double_join() {
        let dir = std::env::temp_dir().join(format!("floptle-resolve-{}", std::process::id()));
        let root = dir.join("assets");
        std::fs::create_dir_all(root.join("shaders")).unwrap();
        std::fs::write(root.join("shaders/s.flsl"), "shader s { stage fragment }").unwrap();

        // Absolute (project opened by full path): used as-is.
        let abs = root.join("shaders/s.flsl");
        assert_eq!(resolve_asset_path(&root, abs.to_str().unwrap()), abs);
        // Tree path already carrying the (relative) root: used as-is, NOT re-joined.
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        assert_eq!(
            resolve_asset_path(Path::new("assets"), "assets/shaders/s.flsl"),
            PathBuf::from("assets/shaders/s.flsl"),
        );
        // Bare project-relative (hand-edited scene file): root joined on.
        assert_eq!(
            resolve_asset_path(Path::new("assets"), "shaders/missing.flsl"),
            PathBuf::from("assets/shaders/missing.flsl"),
        );
        std::env::set_current_dir(cwd).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The 2026-07-20 "everything dereferenced" bug: legacy refs spell the project
    /// folder (`assets/textures/x.png`), which only ever resolved when the CWD was
    /// the project's PARENT. Launched any other way (the Hub passes an absolute
    /// root and sets CWD to the project dir), both the as-written and root-joined
    /// forms miss — the rescue strips the matching first component and re-joins.
    #[test]
    fn legacy_root_prefixed_refs_resolve_under_any_cwd() {
        let dir = std::env::temp_dir().join(format!("floptle-rescue-{}", std::process::id()));
        let root = dir.join("assets");
        std::fs::create_dir_all(root.join("textures")).unwrap();
        std::fs::write(root.join("textures/t.png"), b"png").unwrap();

        // Absolute root, CWD anywhere (never inside `dir`): the legacy ref rescues.
        assert_eq!(
            resolve_asset_path(&root, "assets/textures/t.png"),
            root.join("textures/t.png"),
        );
        // The canonical project-relative form works the same way.
        assert_eq!(resolve_asset_path(&root, "textures/t.png"), root.join("textures/t.png"));
        // A ref whose first component only HAPPENS to match the root name but has
        // no file behind it falls back to the canonical join (missing-file default).
        assert_eq!(
            resolve_asset_path(&root, "assets/textures/missing.png"),
            root.join("assets/textures/missing.png"),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scene loaded from a subfolder must save back to that same file
    /// (`floptle/0111`).
    ///
    /// It used to save to `scenes/<stem>.ron` — the subfolder thrown away — so
    /// the editor loaded one file and wrote another, reported success, cleared
    /// the dirty marker, and left every edit in a stray file at the project
    /// root. Reopening loaded the original, so the work looked reverted. The
    /// user lost hours to it before anyone found the second file.
    #[test]
    fn a_scene_in_a_subfolder_saves_to_the_file_it_came_from() {
        let root = std::env::temp_dir().join(format!("flop-scenepath-{}", std::process::id()));
        let _ = std::fs::create_dir_all(root.join("scenes/cutscenes"));
        let file = root.join("scenes/cutscenes/Opening.ron");
        let _ = std::fs::write(&file, "()");

        let mut ed = Editor { project_root: root.clone(), ..Default::default() };
        ed.set_scene_file(&file);

        assert_eq!(ed.scene_path(), file, "a save must land on the file that was opened");
        // The stem stays the stem: it is the hierarchy header, the window title
        // and what `scene.current()` hands a script, and a game keys off it.
        assert_eq!(ed.scene_name, "Opening");
        // …and the round trip holds at any depth.
        for rel in ["scenes/first.ron", "scenes/maps/arena.ron", "scenes/a/b/c/deep.ron"] {
            let p = root.join(rel);
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            ed.set_scene_file(&p);
            assert_eq!(ed.scene_path(), p, "{rel} did not round-trip");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A project already split by this bug is found and named, rather than left
    /// for the user to notice that half their edits are missing.
    #[test]
    fn a_project_already_split_by_the_old_bug_is_reported() {
        let root = std::env::temp_dir().join(format!("flop-scenesplit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(root.join("scenes/cutscenes"));
        let _ = std::fs::write(root.join("scenes/cutscenes/Opening.ron"), "()");
        let _ = std::fs::write(root.join("scenes/Opening.ron"), "()");
        let _ = std::fs::write(root.join("scenes/first.ron"), "()");

        let split = super::scenes_shadowed_by_a_root_copy(&root);
        assert_eq!(split, vec!["scenes/cutscenes/Opening.ron".to_string()]);

        // A project with no collision reports nothing — this must not cry wolf
        // at every project that happens to use subfolders.
        let _ = std::fs::remove_file(root.join("scenes/Opening.ron"));
        assert!(super::scenes_shadowed_by_a_root_copy(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Build a project whose scene owns one of every per-scene file.
    fn project_with_sidecars(tag: &str, stem: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("flop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["scenes/cutscenes", "terrain", "maps", "paint", ".floptle/autosave"] {
            let _ = std::fs::create_dir_all(root.join(d));
        }
        let scene = root.join(format!("scenes/cutscenes/{stem}.ron"));
        let _ = std::fs::write(&scene, "()");
        let _ = std::fs::write(root.join(format!("terrain/{stem}.1.cfield")), b"field-1");
        let _ = std::fs::write(root.join(format!("terrain/{stem}.2.cfield")), b"field-2");
        let _ = std::fs::write(root.join(format!("terrain/{stem}.1.meta")), b"meta");
        let _ = std::fs::write(root.join(format!("terrain/{stem}.palette")), b"palette");
        let _ = std::fs::write(root.join(format!("maps/{stem}.map.ron")), b"map");
        let _ = std::fs::write(root.join(format!("paint/{stem}.vpaint")), b"paint");
        let _ = std::fs::write(root.join(format!(".floptle/autosave/{stem}.ron")), b"auto");
        (root, scene)
    }

    /// The bug that presented as data loss: a scene rename moved the `.ron` and
    /// orphaned everything keyed by its stem, so the terrain opened empty and
    /// looked exactly like work that was never done.
    #[test]
    fn renaming_a_scene_takes_its_terrain_map_and_paint_with_it() {
        let (root, scene) = project_with_sidecars("rename-carries", "TheVision");
        let mut ed = Editor { project_root: root.clone(), ..Default::default() };
        ed.set_scene_file(&scene);

        ed.rename_asset(&scene.to_string_lossy(), "Part 1 Mission");

        for rel in [
            "terrain/Part 1 Mission.1.cfield",
            "terrain/Part 1 Mission.2.cfield",
            "terrain/Part 1 Mission.1.meta",
            "terrain/Part 1 Mission.palette",
            "maps/Part 1 Mission.map.ron",
            "paint/Part 1 Mission.vpaint",
            ".floptle/autosave/Part 1 Mission.ron",
            "scenes/cutscenes/Part 1 Mission.ron",
        ] {
            assert!(root.join(rel).exists(), "{rel} did not follow the rename");
        }
        // Multi-volume terrain keeps its per-id data distinct rather than
        // collapsing two fields onto one name.
        assert_eq!(
            std::fs::read(root.join("terrain/Part 1 Mission.1.cfield")).unwrap(),
            b"field-1"
        );
        // Nothing is left behind under the old name to be found later and
        // mistaken for the live data.
        assert!(!root.join("terrain/TheVision.1.cfield").exists());
        // The OPEN scene follows, or the next save writes its terrain back under
        // the old name and orphans it all over again.
        assert_eq!(ed.scene_name, "Part 1 Mission");
        assert_eq!(ed.scene_path(), root.join("scenes/cutscenes/Part 1 Mission.ron"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Half a rename would leave a scene pointing at another scene's terrain,
    /// which is worse than not renaming at all — so it is all or nothing.
    #[test]
    fn a_rename_that_would_clobber_a_sidecar_is_refused_whole() {
        let (root, scene) = project_with_sidecars("rename-clobber", "TheVision");
        // Something already owns the name being renamed to.
        let _ = std::fs::write(root.join("terrain/Part 1 Mission.1.cfield"), b"someone-else");
        let mut ed = Editor { project_root: root.clone(), ..Default::default() };
        ed.set_scene_file(&scene);

        ed.rename_asset(&scene.to_string_lossy(), "Part 1 Mission");

        assert!(scene.exists(), "the scene must not move when its data cannot");
        assert!(root.join("terrain/TheVision.1.cfield").exists());
        assert_eq!(
            std::fs::read(root.join("terrain/Part 1 Mission.1.cfield")).unwrap(),
            b"someone-else",
            "an existing file must never be overwritten by a rename"
        );
        assert_eq!(ed.scene_name, "TheVision");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A rename that already happened is diagnosable after the fact: the field
    /// under the old stem is what turns "my terrain is gone" into "your terrain
    /// is one rename away".
    #[test]
    fn a_field_left_under_the_old_scene_name_is_found_as_an_orphan() {
        let (root, _scene) = project_with_sidecars("rename-orphan", "TheVision");
        let mut ed = Editor { project_root: root.clone(), ..Default::default() };
        ed.set_scene_file(&root.join("scenes/cutscenes/Part 1 Mission.ron"));

        assert_eq!(ed.orphaned_field_stems(1), vec!["TheVision".to_string()]);
        // Once the data is where this scene expects it, nothing is reported —
        // this must not cry orphan at every project that has more than one scene.
        let _ = std::fs::rename(
            root.join("terrain/TheVision.1.cfield"),
            root.join("terrain/Part 1 Mission.1.cfield"),
        );
        assert!(ed.orphaned_field_stems(1).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
