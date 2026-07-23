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
        // Live render targets ("rt:<name>") are registered by the camera
        // target pass (update_render_targets), never loaded from disk — the
        // lookup misses until the named camera has rendered once.
        if path.starts_with("rt:") {
            return self.texture_registry.get(path).copied();
        }
        let want = self.texture_settings.get(path).copied().unwrap_or_default();
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
        // The registry stays keyed by the ref as WRITTEN; only the fs read resolves.
        let file = resolve_asset_path(&self.project_root, path);
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return false;
        };
        // Rigged path first: any glTF with animations keeps its node tree +
        // clips (parts stay node-local and get posed each frame).
        match floptle_assets::import_rigged(&file) {
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
        match floptle_assets::gltf_import::import(&file) {
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
        self.adopt_paint();
        self.adopt_tex_paint();
        self.selection.clear();
        self.history = History::default();
        self.mesh_registry.clear();
        self.paint_meshes.clear(); // stale CPU geometry would paint the wrong vertices
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
        self.set_scene_file(p);
        self.adopt_terrain();
        self.adopt_paint();
        self.adopt_tex_paint();
        self.register_scene_meshes();
        self.selection.clear();
        self.selected_asset = None;
        self.history = History::default();
        self.scene_dirty = false;
        self.check_autosave(); // offer crash recovery if an autosave is newer
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
        if let Err(e) = std::fs::rename(&src, &dst) {
            eprintln!("  rename failed: {e}");
            return;
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

    /// Open the OS's native file picker (multi-select) on a background thread and
    /// import the chosen files into `dir` when the user confirms. This is the
    /// reliable cross-platform import path: rfd's XDG-desktop-portal backend works
    /// on Wayland — where winit delivers no drag-and-drop — as well as on X11,
    /// Windows and macOS. The dialog runs off the UI thread (a channel delivers
    /// the result), so the editor never freezes while it's open; the result is
    /// drained each frame in `apply_frame_commands`.
    pub(crate) fn open_import_dialog(&mut self, dir: std::path::PathBuf) {
        if self.import_rx.is_some() {
            return; // one dialog at a time
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // ashpd/rfd's portal backend needs an async runtime; a tiny
            // current-thread tokio runtime drives it on this worker thread.
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                return;
            };
            let picked = rt.block_on(async {
                rfd::AsyncFileDialog::new().set_title("Import assets into the project").pick_files().await
            });
            if let Some(handles) = picked {
                let paths: Vec<PathBuf> = handles.iter().map(|h| h.path().to_path_buf()).collect();
                if !paths.is_empty() {
                    let _ = tx.send((paths, dir));
                }
            }
        });
        self.import_rx = Some(rx);
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
        seed_example_shaders(&self.project_root);
        crate::ui_shader_lib::seed_ui_effects(&self.project_root);
        write_lua_support(&self.project_root);
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
        if let Some(entry) = cfg.entry_scene.as_deref() {
            let p = self.project_root.join(entry);
            match floptle_scene::load(&p) {
                Ok(doc) => return (p, doc),
                Err(e) => eprintln!("  entry scene {entry} failed to load ({e}); falling back"),
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
        self.adopt_terrain();
        self.adopt_paint();
        self.adopt_tex_paint();
        self.project = floptle_scene::load_project(&self.project_cfg_path());
        self.migrate_legacy_post(&doc);
        self.check_autosave(); // offer crash recovery if an autosave is newer
        self.materials = self.load_materials();
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
        seed_example_shaders(&root);
        crate::ui_shader_lib::seed_ui_effects(&root);
        self.open_project(root);
    }

    /// Close the current project: empty world, no selection, clean history.
    pub(crate) fn close_project(&mut self) {
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
        let _ = std::fs::create_dir_all(self.project_root.join("scenes"));
        let path = self.scene_path();
        let doc = floptle_scene::to_doc(self.scene_name.clone(), &self.world);
        let ok = match floptle_scene::save(&doc, &path) {
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
        self.save_paint();
        self.save_tex_paint();
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
            let _ = std::fs::write(self.terrain_palette_path(), palette.join("\n"));
        }
        if ok {
            self.scene_dirty = false;
            self.toast = Some(("💾  Saved".into(), 2.2)); // visible confirmation, not just Console
            let _ = std::fs::remove_file(self.autosave_path()); // saved for real
        }
        ok
    }

    /// Where this scene's crash-recovery autosave lives (`.floptle` is the
    /// project's editor-cache dir, never exported).
    pub(crate) fn autosave_path(&self) -> PathBuf {
        self.project_root.join(".floptle/autosave").join(format!("{}.ron", self.scene_name))
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
        let scene_m = std::fs::metadata(self.scene_path()).and_then(|m| m.modified()).ok();
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

    /// Ctrl+S: save everything — the project config, the open scene, and every
    /// dirty script open in the IDE (so "the script you're editing" is saved too).
    pub(crate) fn save_all(&mut self) {
        // While recording, the world carries previewed clip values — saving would
        // bake them into the scene file. End the recording (restoring the real
        // scene) first; the clip itself saves through its own dirty flag.
        self.stop_recording();
        // Any pending graph-canvas edit lands on disk with everything else.
        self.shader_graph.flush(&self.project_root, &mut self.ide, true, false);
        self.save_scene(); // clears scene_dirty ONLY on success + logs either way
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
        rigidbody: None,
        celestial: None,
        mesh_collider: false,
        paint: None,
        tex_paint: None,
        collidable: false,
        trigger: false,
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
                terrain_gen: None,
                name: "cube".into(),
                transform: TransformDoc { translation: [-2.0, 0.0, 0.0], ..Default::default() },
                matter: MatterDoc::Primitive { shape: ShapeDoc::Cube, color: [0.9, 0.45, 0.35] },
                scripts: Vec::new(),
                material: None,
                rigidbody: None,
                celestial: None,
                mesh_collider: false,
                paint: None,
                tex_paint: None,
                collidable: false,
                trigger: false,
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
            },
            NodeDoc {
                terrain_gen: None,
                name: "sphere".into(),
                transform: TransformDoc { translation: [2.0, 0.0, 0.0], ..Default::default() },
                matter: MatterDoc::Primitive { shape: ShapeDoc::Sphere, color: [0.4, 0.7, 0.95] },
                scripts: Vec::new(),
                material: None,
                rigidbody: None,
                celestial: None,
                mesh_collider: false,
                paint: None,
                tex_paint: None,
                collidable: false,
                trigger: false,
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
            },
            NodeDoc {
                terrain_gen: None,
                name: "blob".into(),
                transform: TransformDoc { translation: [0.0, 1.6, 0.0], ..Default::default() },
                matter: MatterDoc::Blob { scale: 1.0 },
                scripts: Vec::new(),
                material: None,
                rigidbody: None,
                celestial: None,
                mesh_collider: false,
                paint: None,
                tex_paint: None,
                collidable: false,
                trigger: false,
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
            },
            default_camera_node(),
        ],
    }
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

#[cfg(test)]
mod path_tests {
    use super::*;

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
}
