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

/// The script name (file stem) a `.lua` path refers to — what a `ScriptInst.kind`
/// stores and what resolves to `scripts/<name>.lua`.
pub(crate) fn script_name_of(path: &str) -> String {
    Path::new(path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

pub(crate) fn is_texture(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".png") || p.ends_with(".jpg") || p.ends_with(".jpeg")
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

/// A scene file (`scenes/<name>.ron`).
/// A particle effect asset (`*.vfx.ron`).
pub(crate) fn is_vfx(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(floptle_scene::VFX_EXT)
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
    } else if is_texture(path) {
        ("🖼", egui::Color32::from_rgb(140, 210, 140))
    } else if is_material(path) {
        ("◑", egui::Color32::from_rgb(240, 180, 110))
    } else if anim_ui::is_anim_clip(path) {
        ("▶", egui::Color32::from_rgb(235, 200, 110)) // baked animation clip
    } else if anim_ui::is_anim_ctl(path) {
        ("◉", egui::Color32::from_rgb(180, 160, 250)) // animation controller
    } else if is_vfx(path) {
        ("❋", egui::Color32::from_rgb(250, 150, 190)) // particle effect
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

/// Collect the names of every `.lua` script in the asset tree (for "Add Script").
pub(crate) fn collect_script_names(entries: &[AssetEntry], out: &mut Vec<String>) {
    for e in entries {
        match e {
            AssetEntry::Dir(_, children) => collect_script_names(children, out),
            AssetEntry::File { path, .. } if is_script(path) => {
                let n = script_name_of(path);
                if !out.contains(&n) {
                    out.push(n);
                }
            }
            AssetEntry::File { .. } => {}
        }
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
