//! The project asset tree (the bottom file browser): reading it from disk,
//! classifying files by extension, harvesting picker lists (textures, models,
//! script names), and the per-texture import settings persisted in
//! `.floptle/textures.ron`.

use std::path::{Path, PathBuf};

use crate::anim_ui;

/// A node in the project asset tree (the bottom file browser).
pub(crate) enum AssetEntry {
    Dir(String, Vec<AssetEntry>),
    File { name: String, path: String },
}

/// What a dragged asset carries — its path. The drop target reads the extension to
/// decide what to do (a model spawns; a script attaches).
#[derive(Clone)]
pub(crate) struct AssetPayload {
    pub(crate) path: String,
}

pub(crate) fn is_model(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".glb") || p.ends_with(".gltf")
}

pub(crate) fn is_script(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".lua")
}

/// The script name (file stem) a `.lua` path refers to.
pub(crate) fn script_name_of(path: &str) -> String {
    Path::new(path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

/// The script identity used by [`floptle_core::ScriptInst`]: a path relative to the
/// project's `scripts/` folder, without the `.lua` extension. This preserves nested
/// folder organization such as `fighterScripts/attack`.
pub(crate) fn script_kind_of(path: &str, scripts_dir: &Path) -> String {
    let p = Path::new(path);
    let rel = p.strip_prefix(scripts_dir).unwrap_or(p);
    if rel.is_absolute() {
        return script_name_of(path);
    }
    let kind = rel.with_extension("");
    kind.to_string_lossy().replace('\\', "/")
}

pub(crate) fn is_texture(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    // The decoder guesses format from content (see floptle_assets::load_texture), so
    // a mislabeled file loads regardless — but these are the extensions we surface in
    // texture pickers and the asset browser.
    [".png", ".jpg", ".jpeg", ".webp", ".bmp", ".tga", ".tif", ".tiff", ".gif", ".qoi"]
        .iter()
        .any(|ext| p.ends_with(ext))
}
pub(crate) fn is_markdown(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".md") || p.ends_with(".markdown")
}
/// A saved material preset (`materials/<name>.ron`) — distinguished from a scene
/// `.ron` by living under a `materials` directory.
pub(crate) fn is_material(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".ron") && p.replace('\\', "/").contains("materials/")
}

/// A prefab (`*.prefab.ron`) — a reusable node subtree. Drag into the scene
/// to place an instance; `spawn("name")` spawns one from Lua.
pub(crate) fn is_prefab(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(floptle_scene::PREFAB_EXT)
}

/// A scene file (`scenes/<name>.ron`).
/// A particle effect asset (`*.vfx.ron`).
pub(crate) fn is_vfx(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(floptle_scene::VFX_EXT)
}

/// A `.flsl` shader — the shader IR's text form (ADR-0007), assignable on a
/// Material and editable in the Scripting tab or VSCode.
pub(crate) fn is_shader(path: &str) -> bool {
    path.to_ascii_lowercase()
        .ends_with(&format!(".{}", floptle_shader::SHADER_TEXT_EXT))
}

/// An audio clip (`.wav/.ogg/.mp3/.flac`) — playable by the sound system.
pub(crate) fn is_audio(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    floptle_audio::AUDIO_EXTENSIONS.iter().any(|ext| p.ends_with(&format!(".{ext}")))
}

pub(crate) fn is_scene(path: &str) -> bool {
    let p = path.to_ascii_lowercase().replace('\\', "/");
    p.ends_with(".ron") && p.contains("scenes/")
}

/// Shorten `name` to at most `max` chars (…-elided), for fixed-width grid tiles.
pub(crate) fn truncate_label(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_string();
    }
    let keep: String = name.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

/// A small type glyph + tint for an asset file, used in the browser tree + grid.
pub(crate) fn asset_kind_icon(path: &str) -> (&'static str, egui::Color32) {
    if is_model(path) {
        ("⬣", egui::Color32::from_rgb(120, 200, 210))
    } else if is_script(path) {
        ("¶", egui::Color32::from_rgb(130, 170, 240))
    } else if crate::image_io::is_image_doc(path) {
        // The layered document (🖼 Image tab), beside the flat .png it exports.
        ("▨", egui::Color32::from_rgb(130, 200, 255))
    } else if is_texture(path) {
        ("🖼", egui::Color32::from_rgb(140, 210, 140))
    } else if is_material(path) {
        ("◑", egui::Color32::from_rgb(240, 180, 110))
    } else if anim_ui::is_anim_clip(path) {
        ("▶", egui::Color32::from_rgb(235, 200, 110)) // baked animation clip
    } else if anim_ui::is_anim_ctl(path) {
        ("◎", egui::Color32::from_rgb(180, 160, 250)) // animation controller
    } else if is_vfx(path) {
        ("✨", egui::Color32::from_rgb(250, 150, 190)) // particle effect
    } else if is_prefab(path) {
        ("⬡", egui::Color32::from_rgb(110, 190, 255)) // prefab (node subtree)
    } else if is_shader(path) {
        ("◈", egui::Color32::from_rgb(190, 140, 255)) // .flsl shader (ADR-0007)
    } else if is_audio(path) {
        ("♪", egui::Color32::from_rgb(120, 220, 180)) // audio clip
    } else if path.to_ascii_lowercase().ends_with(".ron") {
        ("⎙", egui::Color32::from_rgb(200, 150, 230)) // a scene
    } else if is_markdown(path) {
        ("§", egui::Color32::from_gray(190))
    } else {
        ("▣", egui::Color32::from_gray(170))
    }
}

/// Open the OS file manager at `path` (revealing the file where supported).
pub(crate) fn reveal_in_explorer(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // xdg-open can't select a file, so open its containing folder.
        let target = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.to_path_buf())
        };
        let _ = std::process::Command::new("xdg-open").arg(target).spawn();
    }
}

/// A font the UI text can use (drop .ttf/.otf files anywhere in your assets).
pub(crate) fn is_font(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".ttf") || p.ends_with(".otf")
}

/// Collect every texture image path in the asset tree (for the material picker).
pub(crate) fn collect_texture_paths(entries: &[AssetEntry], out: &mut Vec<String>) {
    for e in entries {
        match e {
            AssetEntry::Dir(_, children) => collect_texture_paths(children, out),
            AssetEntry::File { path, .. } if is_texture(path) => out.push(path.clone()),
            AssetEntry::File { .. } => {}
        }
    }
}

/// The path the dev types after `Assets/` — `path` with the project root stripped, so it
/// round-trips through `assets.getFile(...)` in a script. Falls back to the full path.
pub(crate) fn asset_rel_path(path: &str, project_root: &Path) -> String {
    Path::new(path)
        .strip_prefix(project_root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// A texture's sampling settings, looked up by a path in EITHER form.
///
/// `texture_settings` is keyed the way scenes and materials reference a texture:
/// **project-relative**. The Assets browser works in absolute paths, though, so the
/// Inspector's filter/wrap combo used to store an absolute key that nothing ever read
/// back — the setting persisted, the Inspector showed it, and every renderer looking the
/// texture up by its `textures/ui/hud/Fill.png` ref missed and got the default. Pixel art
/// came out bilinear-blurred with `Pixelated` plainly selected (floptle/0026).
///
/// Keys are normalised on load and on write, so the fallback below is only for a stray
/// absolute path arriving from the Assets side.
pub(crate) fn tex_setting(
    settings: &std::collections::HashMap<String, TexSetting>,
    project_root: &Path,
    path: &str,
) -> TexSetting {
    settings
        .get(path)
        .or_else(|| settings.get(asset_rel_path(path, project_root).as_str()))
        .copied()
        .unwrap_or_default()
}

/// Collect the path of every importable model (.glb/.gltf) in the asset tree — for the
/// Inspector's mesh model picker and the Add Component menu.
pub(crate) fn collect_model_paths(entries: &[AssetEntry], out: &mut Vec<String>) {
    for e in entries {
        match e {
            AssetEntry::Dir(_, children) => collect_model_paths(children, out),
            AssetEntry::File { path, .. } if is_model(path) => out.push(path.clone()),
            AssetEntry::File { .. } => {}
        }
    }
}

/// Collect the full paths of every `.lua` script in the asset tree (for "Add Script").
/// Returns paths relative to the project root (e.g., "scripts/character.lua").
pub(crate) fn collect_script_names(entries: &[AssetEntry], out: &mut Vec<String>) {
    for e in entries {
        match e {
            AssetEntry::Dir(_, children) => collect_script_names(children, out),
            AssetEntry::File { path, .. } if is_script(path) => {
                if !out.contains(path) {
                    out.push(path.clone());
                }
            }
            AssetEntry::File { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_kind_preserves_nested_paths_relative_to_scripts_dir() {
        let scripts_dir = Path::new("/tmp/project/scripts");
        assert_eq!(
            script_kind_of("/tmp/project/scripts/fighterScripts/attack.lua", scripts_dir),
            "fighterScripts/attack"
        );
        assert_eq!(
            script_kind_of("/tmp/project/scripts/rotate.lua", scripts_dir),
            "rotate"
        );
    }

    /// The Inspector selects a texture by its ABSOLUTE path; a scene references it by a
    /// PROJECT-RELATIVE one. Both must reach the same settings entry, or a `Pixelated`
    /// pick shows in the Inspector and never reaches the sampler (floptle/0026).
    #[test]
    fn texture_settings_resolve_from_either_path_form() {
        let root = Path::new("/home/dev/Fofighter");
        let pixel = TexSetting { filter: FilterMode::Pixelated, ..Default::default() };
        let mut settings = std::collections::HashMap::new();
        settings.insert("textures/ui/hud/Fill.png".to_string(), pixel);

        // The form a UiElement / Material stores.
        assert_eq!(
            tex_setting(&settings, root, "textures/ui/hud/Fill.png").filter,
            FilterMode::Pixelated
        );
        // The form the Assets browser hands the Inspector.
        assert_eq!(
            tex_setting(&settings, root, "/home/dev/Fofighter/textures/ui/hud/Fill.png").filter,
            FilterMode::Pixelated
        );
        // An unknown texture still falls back to the default.
        assert_eq!(tex_setting(&settings, root, "textures/other.png").filter, TexSetting::default().filter);

        // A legacy file keyed by absolute path migrates to the relative form on load.
        let legacy: std::collections::HashMap<String, TexSetting> = [(
            "/home/dev/Fofighter/textures/ui/hud/Fill.png".to_string(),
            pixel,
        )]
        .into_iter()
        .collect();
        let migrated: std::collections::HashMap<String, TexSetting> =
            legacy.into_iter().map(|(k, v)| (asset_rel_path(&k, root), v)).collect();
        assert!(migrated.contains_key("textures/ui/hud/Fill.png"), "{migrated:?}");
    }
}

/// Read the project tree under `dir` (folders first, then files, alphabetically).
pub(crate) fn build_assets(dir: &std::path::Path) -> Vec<AssetEntry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| (e.path().is_file(), e.file_name()));
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if e.path().is_dir() {
            out.push(AssetEntry::Dir(name, build_assets(&e.path())));
        } else {
            out.push(AssetEntry::File { name, path: e.path().to_string_lossy().to_string() });
        }
    }
    out
}

/// How a texture is filtered — the serde-friendly mirror of [`floptle_render::TexFilter`],
/// persisted per texture in `.floptle/textures.ron`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(crate) enum FilterMode {
    /// Crisp nearest-neighbor (pixel art).
    #[default]
    Pixelated,
    /// Bilinear smoothing.
    Smooth,
    /// Trilinear (bilinear + mipmaps) — smooth and shimmer-free into the distance.
    SmoothMipmaps,
}

/// How a texture wraps outside [0,1] — serde mirror of [`floptle_render::TexWrap`].
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(crate) enum WrapMode {
    #[default]
    Repeat,
    Clamp,
    Mirror,
}

/// A texture's sampling settings, persisted per project. Default = crisp tiling.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(crate) struct TexSetting {
    #[serde(default)]
    pub(crate) filter: FilterMode,
    #[serde(default)]
    pub(crate) wrap: WrapMode,
    /// Spritesheet columns (0/1 = not a sheet). Slices the texture into
    /// `sheet_cols`×`sheet_rows` cells that a UI image can pick individually.
    #[serde(default)]
    pub(crate) sheet_cols: u32,
    #[serde(default)]
    pub(crate) sheet_rows: u32,
}

impl TexSetting {
    /// The spritesheet grid `(cols, rows)`, each ≥1.
    pub(crate) fn sheet(self) -> (u32, u32) {
        (self.sheet_cols.max(1), self.sheet_rows.max(1))
    }
}

impl TexSetting {
    pub(crate) fn to_sampling(self) -> floptle_render::TexSampling {
        use floptle_render::{TexFilter, TexSampling, TexWrap};
        TexSampling {
            filter: match self.filter {
                FilterMode::Pixelated => TexFilter::Pixelated,
                FilterMode::Smooth => TexFilter::Smooth,
                FilterMode::SmoothMipmaps => TexFilter::SmoothMipmaps,
            },
            wrap: match self.wrap {
                WrapMode::Repeat => TexWrap::Repeat,
                WrapMode::Clamp => TexWrap::Clamp,
                WrapMode::Mirror => TexWrap::Mirror,
            },
        }
    }
}

/// A path inside `dir` named `stem[.ext]`, auto-suffixed (`stem_1`, `stem_2`, …)
/// until it doesn't collide with an existing entry. `ext: None` = a folder name.
pub(crate) fn unique_path(dir: &Path, stem: &str, ext: Option<&str>) -> PathBuf {
    let make = |name: String| match ext {
        Some(e) => dir.join(format!("{name}.{e}")),
        None => dir.join(name),
    };
    let mut p = make(stem.to_string());
    let mut n = 1;
    while p.exists() {
        p = make(format!("{stem}_{n}"));
        n += 1;
    }
    p
}
