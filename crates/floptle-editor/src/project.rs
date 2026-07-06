//! Project + asset IO: open/new/close project, scene load/save (with legacy
//! migrations), asset file management, textures, and save-all.

use floptle_core::Matter;
use floptle_core::World;
use floptle_core::math::Mat3;
use floptle_core::math::Quat;
use floptle_core::math::Vec3;
use floptle_render::TexId;
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
        let want = self.texture_settings.get(path).copied().unwrap_or_default();
        if let (Some(id), Some(prev)) =
            (self.texture_registry.get(path), self.texture_registry_setting.get(path))
            && *prev == want {
                return Some(*id);
            }
        let data = floptle_assets::load_texture(Path::new(path))?;
        let (gpu, raster) = (self.gpu.as_ref()?, self.raster.as_mut()?);
        let id = raster.register_texture(gpu, &data, want.to_sampling());
        self.texture_registry.insert(path.to_string(), id);
        self.texture_registry_setting.insert(path.to_string(), want);
        Some(id)
    }

    /// Persist the per-texture sampling settings to `.floptle/textures.ron`.
    pub(crate) fn save_texture_settings(&self) {
        let dir = self.project_root.join(".floptle");
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(s) = ron::ser::to_string_pretty(&self.texture_settings, Default::default()) {
            let _ = std::fs::write(dir.join("textures.ron"), s);
        }
    }

    /// Load the per-texture sampling settings from `.floptle/textures.ron` (if present).
    pub(crate) fn load_texture_settings(&mut self) {
        let path = self.project_root.join(".floptle").join("textures.ron");
        self.texture_settings = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| ron::from_str(&s).ok())
            .unwrap_or_default();
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
    pub(crate) fn import_model(&mut self, path: &str) -> bool {
        if self.mesh_registry.contains_key(path) {
            return true;
        }
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return false;
        };
        // Rigged path first: any glTF with animations keeps its node tree +
        // clips (parts stay node-local and get posed each frame).
        match floptle_assets::import_rigged(std::path::Path::new(path)) {
            Ok(Some(model)) => {
                let parts = model
                    .parts
                    .iter()
                    .map(|p| raster.register(gpu, &p.mesh, p.texture.map(|i| &model.textures[i])))
                    .collect();
                let rig = anim::rig_from_model(&model);
                let skinned = model.parts.iter().filter(|p| p.skin.is_some()).count();
                let verts: usize = model.parts.iter().map(|p| p.mesh.vertices.len()).sum();
                self.mesh_registry.insert(
                    path.to_string(),
                    MeshAsset { parts, size: model.size, rig: Some(rig) },
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
                println!("  imported {path} (rigged, {} clip(s))", model.clips.len());
                return true;
            }
            Ok(None) => {} // no animations — fall through to the static bake
            Err(e) => eprintln!("  rig import {path} failed ({e}); trying static"),
        }
        match floptle_assets::gltf_import::import(std::path::Path::new(path)) {
            Ok(model) => {
                let parts = model
                    .parts
                    .iter()
                    .map(|p| raster.register(gpu, &p.mesh, p.texture.map(|i| &model.textures[i])))
                    .collect();
                self.mesh_registry
                    .insert(path.to_string(), MeshAsset { parts, size: model.size, rig: None });
                println!("  imported {path}");
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
        let name = {
            let n = name.trim();
            if n.is_empty() { "untitled".to_string() } else { n.to_string() }
        };
        let _ = std::fs::create_dir_all(self.project_root.join("scenes"));
        let path = self.project_root.join("scenes").join(format!("{name}.ron"));
        let doc = floptle_scene::SceneDoc {
            name: name.clone(),
            lighting: floptle_scene::LightDoc::default(),
            nodes: vec![default_camera_node()],
        };
        if let Err(e) = floptle_scene::save(&doc, &path) {
            eprintln!("  new scene failed: {e}");
            return;
        }
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.scene_name = name;
        self.adopt_terrain();
        self.selection.clear();
        self.history = History::default();
        self.mesh_registry.clear();
        self.mesh_wire_cache.clear(); // keep the collider-wire cache in lockstep
        self.scene_dirty = false;
        self.asset_tree = build_assets(&self.project_root);
        println!("  new scene: {}", path.display());
    }

    /// Open an existing scene `.ron` (double-clicked in Assets). Resets the world to
    /// it, loads its terrain + meshes. The caller handles unsaved-changes prompting.
    pub(crate) fn open_scene_file(&mut self, path: &str) {
        self.reset_anim_bindings();
        let p = Path::new(path);
        let doc = match floptle_scene::load(p) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("  open scene failed: {e}");
                return;
            }
        };
        self.playing = false;
        self.paused = false;
        self.play_snapshot = None;
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.migrate_legacy_post(&doc);
        self.scene_name = Self::scene_name_of(p);
        self.adopt_terrain();
        self.register_scene_meshes();
        self.selection.clear();
        self.selected_asset = None;
        self.history = History::default();
        self.scene_dirty = false;
        println!("  opened scene: {}", p.display());
    }

    /// Register the GPU meshes for every imported model the current scene references.
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
    pub(crate) fn scene_path(&self) -> PathBuf {
        self.project_root.join("scenes").join(format!("{}.ron", self.scene_name))
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

    /// Rename a file/folder to `new_name` within its current parent directory. If the
    /// typed name has no extension, the original file's extension is kept (so naming a new
    /// `.lua` script "player" yields "player.lua", and a rename can't drop the extension).
    pub(crate) fn rename_asset(&mut self, from: &str, new_name: &str) {
        let typed = new_name.trim();
        if typed.is_empty() {
            return;
        }
        let src = PathBuf::from(from);
        let final_name = match src.extension().and_then(|e| e.to_str()) {
            Some(ext) if !src.is_dir() && Path::new(typed).extension().is_none() => {
                format!("{typed}.{ext}")
            }
            _ => typed.to_string(),
        };
        let dst = src.parent().unwrap_or(Path::new(".")).join(&final_name);
        if dst == src {
            return;
        }
        if dst.exists() {
            eprintln!("  rename: {} already exists", dst.display());
            return;
        }
        if let Err(e) = std::fs::rename(&src, &dst) {
            eprintln!("  rename failed: {e}");
            return;
        }
        let dst_str = dst.to_string_lossy().to_string();
        // Follow the file in any open IDE tab and the asset selection.
        for f in &mut self.ide.open {
            if f.path == from {
                f.path = dst_str.clone();
                f.name = final_name.clone();
            }
        }
        if self.selected_asset.as_deref() == Some(from) {
            self.selected_asset = Some(dst_str);
        }
        self.asset_tree = build_assets(&self.project_root);
    }

    /// Delete a file or folder (recursively) and drop any references to it.
    pub(crate) fn delete_asset(&mut self, path: &str) {
        let p = Path::new(path);
        let res = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
        if let Err(e) = res {
            eprintln!("  delete failed: {e}");
            return;
        }
        self.ide.open.retain(|f| f.path != path);
        self.ide.active = self.ide.active.filter(|&i| i < self.ide.open.len());
        if self.selected_asset.as_deref() == Some(path) {
            self.selected_asset = None;
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
        write_lua_support(&self.project_root);
    }

    pub(crate) fn load_materials(&self) -> Vec<(String, floptle_scene::MaterialDoc)> {
        floptle_scene::load_materials(&self.materials_dir())
    }

    /// Load the project's active scene + the file it came from: `scenes/first.ron`
    /// if present, else the first `.ron` in `scenes/`, else a tiny built-in default.
    /// The returned path's stem becomes `scene_name`, so edits save back to the same
    /// file even if the scene's internal name differs.
    pub(crate) fn load_active_scene(&self) -> (PathBuf, floptle_scene::SceneDoc) {
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
        self.scene_name = Self::scene_name_of(&path);
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.adopt_terrain();
        self.project = floptle_scene::load_project(&self.project_cfg_path());
        self.migrate_legacy_post(&doc);
        self.materials = self.load_materials();
        self.asset_tree = build_assets(&self.project_root);
        self.load_texture_settings();
        self.texture_registry.clear();
        self.texture_registry_setting.clear();
        self.selection.clear();
        self.selected_asset = None;
        self.ide = IdeState::default();
        self.history = History::default();
        self.playing = false;
        self.paused = false;
        // A different project's models live behind the same path strings, so drop the
        // old GPU-mesh cache before re-importing (else import_model early-returns).
        self.mesh_registry.clear();
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
        println!("  opened project {}", self.project_root.display());
    }

    /// Create a fresh project at `root` (folders + a starter scene + example
    /// scripts), then open it.
    pub(crate) fn new_project(&mut self, root: PathBuf) {
        let _ = std::fs::create_dir_all(root.join("scenes"));
        let _ = std::fs::create_dir_all(root.join("scripts"));
        // A starter scene if none exists yet.
        let first = root.join("scenes/first.ron");
        if !first.exists() {
            let _ = floptle_scene::save(&default_scene(), &first);
        }
        // Ship the default Lua scripts so the IDE/docs have something to show.
        seed_default_scripts(&root.join("scripts"));
        self.open_project(root);
    }

    /// Close the current project: empty world, no selection, clean history.
    pub(crate) fn close_project(&mut self) {
        self.reset_anim_bindings();
        self.world = World::new();
        floptle_scene::spawn_into(&empty_scene(), &mut self.world);
        self.scene_name = "untitled".into();
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
        self.mesh_wire_cache.clear(); // keep the collider-wire cache in lockstep
    }

    pub(crate) fn save_scene(&self) {
        let _ = std::fs::create_dir_all(self.project_root.join("scenes"));
        let path = self.scene_path();
        let doc = floptle_scene::to_doc(self.scene_name.clone(), &self.world);
        match floptle_scene::save(&doc, &path) {
            Ok(()) => println!("  saved {}", path.display()),
            Err(e) => eprintln!("  save failed: {e}"),
        }
        // Terrain fields are large, so each lives beside the scene (one file per
        // terrain id), not inline in the scene doc.
        let dir = self.project_root.join("terrain");
        let _ = std::fs::create_dir_all(&dir);
        for (&e, t) in &self.terrains {
            let id = match self.world.get::<Matter>(e) {
                Some(Matter::Terrain { id }) => *id,
                _ => continue,
            };
            if let Err(e) = std::fs::write(self.terrain_field_path_id(id), t.to_bytes()) {
                eprintln!("  save terrain failed: {e}");
            }
        }
        // The texture PALETTE (which image fills each painted slot) is editor state,
        // not in the field — persist it so painted textures survive a reload.
        if !self.terrains.is_empty() {
            let palette = self.terrain_textures.join("\n");
            let _ = std::fs::write(self.terrain_palette_path(), palette);
        }
    }

    /// Where the scene's terrain texture palette (slot→image paths) is stored.
    pub(crate) fn terrain_palette_path(&self) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.palette", self.scene_name))
    }

    /// Ctrl+S: save everything — the project config, the open scene, and every
    /// dirty script open in the IDE (so "the script you're editing" is saved too).
    pub(crate) fn save_all(&mut self) {
        self.save_scene();
        self.scene_dirty = false;
        if let Err(e) = floptle_scene::save_project(&self.project, &self.project_cfg_path()) {
            eprintln!("  save project failed: {e}");
        }
        let mut saved_scripts = 0;
        for f in &mut self.ide.open {
            if f.dirty && std::fs::write(&f.path, &f.text).is_ok() {
                f.dirty = false;
                saved_scripts += 1;
            }
        }
        if saved_scripts > 0 {
            println!("  saved {saved_scripts} script(s)");
        }
    }
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
        name: "Camera".into(),
        transform: floptle_scene::TransformDoc {
            translation: [pos.x as f64, pos.y as f64, pos.z as f64],
            rotation: rot.to_array(),
            scale: [1.0, 1.0, 1.0],
        },
        matter: floptle_scene::MatterDoc::Camera { fov_y: 60f32.to_radians(), active: true },
        // The default camera flies on play (hold right-mouse to look, WASD to move).
        scripts: vec![floptle_scene::ScriptDoc {
            kind: "freelook".into(),
            enabled: true,
            params: Vec::new(),
        }],
        material: None,
        rigidbody: None,
        mesh_collider: false,
        collidable: false,
        visible: true,
        cast_shadow: true,
        anim_controller: None,
        particles: None,
        parent: None,
        attachment: None,
        net: None,
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
    use floptle_scene::*;
    SceneDoc {
        name: "first".into(),
        lighting: LightDoc::default(),
        nodes: vec![
            NodeDoc {
                name: "cube".into(),
                transform: TransformDoc { translation: [-2.0, 0.0, 0.0], ..Default::default() },
                matter: MatterDoc::Primitive { shape: ShapeDoc::Cube, color: [0.9, 0.45, 0.35] },
                scripts: Vec::new(),
                material: None,
                rigidbody: None,
                mesh_collider: false,
                collidable: false,
                visible: true,
                cast_shadow: true,
                anim_controller: None,
                particles: None,
                parent: None,
                attachment: None,
                net: None,
            },
            NodeDoc {
                name: "sphere".into(),
                transform: TransformDoc { translation: [2.0, 0.0, 0.0], ..Default::default() },
                matter: MatterDoc::Primitive { shape: ShapeDoc::Sphere, color: [0.4, 0.7, 0.95] },
                scripts: Vec::new(),
                material: None,
                rigidbody: None,
                mesh_collider: false,
                collidable: false,
                visible: true,
                cast_shadow: true,
                anim_controller: None,
                particles: None,
                parent: None,
                attachment: None,
                net: None,
            },
            NodeDoc {
                name: "blob".into(),
                transform: TransformDoc { translation: [0.0, 1.6, 0.0], ..Default::default() },
                matter: MatterDoc::Blob { scale: 1.0 },
                scripts: Vec::new(),
                material: None,
                rigidbody: None,
                mesh_collider: false,
                collidable: false,
                visible: true,
                cast_shadow: true,
                anim_controller: None,
                particles: None,
                parent: None,
                attachment: None,
                net: None,
            },
            default_camera_node(),
        ],
    }
}
