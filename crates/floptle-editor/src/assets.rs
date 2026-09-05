//! The project asset tree (the bottom file browser): reading it from disk,
//! classifying files by extension, harvesting picker lists (textures, models,
//! script names), and the per-texture import settings persisted in
//! `.floptle/textures.ron`.

use std::path::{Path, PathBuf};

use crate::anim;

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

/// A model the engine cannot open, but can convert.
///
/// Kept beside [`is_model`] because the two together are the whole answer to
/// "can I do anything with this file": one opens, the other becomes one that
/// opens.
#[cfg(feature = "editor-ui")]
pub(crate) fn is_convertible_model(path: &str) -> bool {
    floptle_convert::is_convertible(std::path::Path::new(path))
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

/// A map-geometry sidecar (`maps/<scene>.map.ron`) — the blockout shapes a
/// scene's Map tool built. Select one for a floor-plan preview; drag it into
/// the viewport (or right-click → Add to scene) to bring its geometry into the
/// open scene as fresh, independent map nodes.
pub(crate) fn is_map_sidecar(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".map.ron")
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
#[cfg(feature = "editor-ui")]
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
    } else if anim::is_sprite_anim(path) {
        ("▦", egui::Color32::from_rgb(235, 200, 110)) // sprite animation (frames)
    } else if anim::is_anim_clip(path) {
        ("▶", egui::Color32::from_rgb(235, 200, 110)) // baked animation clip
    } else if anim::is_anim_ctl(path) {
        ("◎", egui::Color32::from_rgb(180, 160, 250)) // animation controller
    } else if is_vfx(path) {
        ("✨", egui::Color32::from_rgb(250, 150, 190)) // particle effect
    } else if is_prefab(path) {
        ("◇", egui::Color32::from_rgb(110, 190, 255)) // prefab (node subtree)
    } else if is_shader(path) {
        ("◈", egui::Color32::from_rgb(190, 140, 255)) // .flsl shader (ADR-0007)
    } else if is_audio(path) {
        ("♪", egui::Color32::from_rgb(120, 220, 180)) // audio clip
    } else if is_map_sidecar(path) {
        ("▦", egui::Color32::from_rgb(150, 205, 170)) // a scene's map geometry
    } else if path.to_ascii_lowercase().ends_with(".ron") {
        ("⎙", egui::Color32::from_rgb(200, 150, 230)) // a scene
    } else if is_markdown(path) {
        ("§", egui::Color32::from_gray(190))
    } else {
        ("▣", egui::Color32::from_gray(170))
    }
}

/// Open the OS file manager at `path` (revealing the file where supported).
#[cfg(feature = "editor-ui")]
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
        let target = if floptle_vfs::is_dir(path) {
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
///
/// **Separators come out as `/`, whichever way they went in.** An asset ref is
/// written into a scene, a material or a script, and those files move between
/// machines: a `textures\ui\Fill.png` saved on Windows is not the same string
/// as the `textures/ui/Fill.png` every lookup is keyed by, so the texture
/// resolved on the machine that authored it and nowhere else. Normalising here
/// rather than at each of the dozen call sites is what makes that one rule
/// instead of twelve — `asset_key` has always done the same.
pub(crate) fn asset_rel_path(path: &str, project_root: &Path) -> String {
    let slashed = path.replace('\\', "/");
    Path::new(&slashed)
        .strip_prefix(project_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or(slashed)
}

/// Put a texture's sheet grid onto **every material that wears it**.
///
/// The grid lives on the TEXTURE — "slice the .png once, every material using it
/// inherits the same cells" — but the only thing that ever copied it onto a
/// `Material` was the Inspector's material editor, which runs for the one node
/// somebody happens to have selected. So slicing a sheet from the Assets panel
/// left every existing sprite believing its texture was one whole cell: the
/// sprite drew the ENTIRE SHEET stretched across its quad, and came out sized
/// from the whole image rather than from one frame. That reads as spritesheets
/// being broken, which is a long way from one number being stale.
///
/// A cell that no longer exists falls back INTO range rather than drawing off
/// the end of the image — and a `Matter::Sprite` carries its own cell, which is
/// the one its draw actually reads, so it gets the same clamp.
/// **What a material wearing this texture should say.** Its sheet grid comes
/// from the texture's import settings, and its cell has to exist inside that
/// grid.
///
/// One function because there are two callers who must not disagree: the editor
/// applies it to a live world on load, and `floptle check` asks the same
/// question of the files on disk without one. A checker that reimplemented the
/// rule would eventually report a disagreement the editor does not see, or miss
/// one it does.
pub(crate) fn sheet_for(setting: TexSetting, cell: u32) -> (u32, u32, u32) {
    let (sc, sr) = setting.sheet();
    (sc, sr, cell.min((sc * sr).saturating_sub(1)))
}

pub(crate) fn reslice_materials(
    world: &mut floptle_core::World,
    project_root: &Path,
    rel_path: &str,
    setting: TexSetting,
) -> bool {
    let (sc, sr) = setting.sheet();
    let last = (sc * sr).saturating_sub(1);
    let mut wearing: Vec<floptle_core::Entity> = Vec::new();
    let mut changed = false;
    for (e, m) in world.query_mut::<floptle_core::Material>() {
        let wears = m
            .texture
            .as_deref()
            .is_some_and(|t| asset_rel_path(t, project_root) == rel_path);
        if !wears {
            continue;
        }
        let was = (m.sheet_cols, m.sheet_rows, m.cell);
        (m.sheet_cols, m.sheet_rows, m.cell) = sheet_for(setting, m.cell);
        changed |= was != (m.sheet_cols, m.sheet_rows, m.cell);
        wearing.push(e);
    }
    for e in wearing {
        if let Some(floptle_core::Matter::Sprite { cell, .. }) =
            world.get_mut::<floptle_core::Matter>(e)
        {
            let was = *cell;
            *cell = (*cell).min(last);
            changed |= was != *cell;
        }
    }
    changed
}

/// Put EVERY texture's sheet grid onto every material that wears it.
///
/// [`reslice_materials`] answers "this texture was re-sliced"; this answers "a
/// world just arrived". A scene saved before its textures were sliced — or one
/// whose materials were built by a script, which never goes near the Inspector —
/// carries materials that disagree with the project's own import settings, and
/// the disagreement draws as a sprite showing its whole sheet.
///
/// Cheap enough to run on load and nowhere near cheap enough to run per frame:
/// it is a string normalisation per material.
/// Returns whether anything actually moved — the caller uses that to say so,
/// because a correction the person is not told about is one they cannot save.
pub(crate) fn sync_sheet_grids(
    world: &mut floptle_core::World,
    settings: &std::collections::HashMap<String, TexSetting>,
    project_root: &Path,
) -> bool {
    let stale: Vec<(String, TexSetting)> = world
        .query::<floptle_core::Material>()
        .filter_map(|(_, m)| m.texture.as_deref())
        .map(|t| asset_rel_path(t, project_root))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter_map(|rel| settings.get(&rel).map(|s| (rel, *s)))
        .collect();
    let mut changed = false;
    for (rel, setting) in stale {
        changed |= reslice_materials(world, project_root, &rel, setting);
    }
    changed
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

    /// **Slicing a texture into a sheet has to reach the sprites already
    /// wearing it.** Reported from a real project: a sprite was given a sheet
    /// texture, the cell was changed, and nothing on screen moved — while the
    /// node drew a stretched picture of the whole sheet. Both are the same
    /// stale number: the grid lived on the texture and was only ever copied onto
    /// a `Material` by the Inspector's material editor, for the one node
    /// selected at the time.
    #[test]
    fn slicing_a_texture_reslices_every_material_wearing_it() {
        let root = std::path::Path::new("/proj");
        let mut world = floptle_core::World::new();
        // Two nodes on the sheet, one on something else — and the sprite's own
        // cell is out past the end of the new grid.
        let sprite = world.spawn();
        world.insert(
            sprite,
            floptle_core::Material {
                texture: Some("art/hero.png".into()),
                ..Default::default()
            },
        );
        world.insert(
            sprite,
            floptle_core::Matter::Sprite {
                ppu: 32.0,
                size: 1.0,
                cell: 99,
                flip_x: false,
                flip_y: false,
                pivot: [0.5, 0.5],
            },
        );
        // Referenced ABSOLUTELY, which is a spelling the Assets side hands out.
        let other = world.spawn();
        world.insert(
            other,
            floptle_core::Material {
                texture: Some("/proj/art/hero.png".into()),
                cell: 7,
                ..Default::default()
            },
        );
        let elsewhere = world.spawn();
        world.insert(
            elsewhere,
            floptle_core::Material {
                texture: Some("art/sky.png".into()),
                ..Default::default()
            },
        );

        reslice_materials(
            &mut world,
            root,
            "art/hero.png",
            TexSetting { sheet_cols: 16, sheet_rows: 2, ..Default::default() },
        );

        for e in [sprite, other] {
            let m = world.get::<floptle_core::Material>(e).unwrap();
            assert_eq!((m.sheet_cols, m.sheet_rows), (16, 2), "a material kept a stale grid");
            assert!(m.cell < 32, "a cell was left past the end of the new grid");
        }
        // The node's OWN cell is what a sprite draws, so it is clamped too.
        assert!(matches!(
            world.get::<floptle_core::Matter>(sprite),
            Some(floptle_core::Matter::Sprite { cell: 31, .. })
        ));
        // A material on another texture is untouched.
        let m = world.get::<floptle_core::Material>(elsewhere).unwrap();
        assert_eq!((m.sheet_cols, m.sheet_rows), (0, 0));
    }

    /// **A scene that arrives disagreeing with the project is put in step.** A
    /// scene saved before its textures were sliced, or whose materials a script
    /// built, has no route through the Inspector at all — and the disagreement
    /// draws as a sprite showing its whole sheet.
    #[test]
    fn loading_a_world_puts_every_material_in_step_with_its_texture() {
        let root = std::path::Path::new("/proj");
        let mut world = floptle_core::World::new();
        let e = world.spawn();
        world.insert(
            e,
            floptle_core::Material { texture: Some("art/hero.png".into()), ..Default::default() },
        );
        let untouched = world.spawn();
        world.insert(
            untouched,
            floptle_core::Material { texture: Some("art/sky.png".into()), ..Default::default() },
        );
        let settings = std::collections::HashMap::from([(
            "art/hero.png".to_string(),
            TexSetting { sheet_cols: 4, sheet_rows: 4, ..Default::default() },
        )]);

        sync_sheet_grids(&mut world, &settings, root);

        let m = world.get::<floptle_core::Material>(e).unwrap();
        assert_eq!((m.sheet_cols, m.sheet_rows), (4, 4), "a loaded material kept a stale grid");
        // A texture with no settings of its own is left exactly as authored.
        let m = world.get::<floptle_core::Material>(untouched).unwrap();
        assert_eq!((m.sheet_cols, m.sheet_rows), (0, 0));
    }

    /// **An asset ref written on Windows has to resolve everywhere else.** The
    /// settings map, every material and every scene key a texture by its
    /// project-relative path with forward slashes; a backslash ref missed every
    /// one of them, so the texture drew with default sampling on any machine
    /// but the one that saved it — with the right setting plainly selected in
    /// the Inspector.
    #[test]
    fn a_windows_asset_ref_normalises_to_the_key_everything_looks_up() {
        let root = std::path::Path::new("/proj");
        assert_eq!(
            asset_rel_path("/proj/textures/ui/Fill.png", root),
            "textures/ui/Fill.png"
        );
        // Already relative, backslashed: still the key.
        assert_eq!(
            asset_rel_path("textures\\ui\\Fill.png", root),
            "textures/ui/Fill.png"
        );
        // Already the key: unchanged.
        assert_eq!(
            asset_rel_path("textures/ui/Fill.png", root),
            "textures/ui/Fill.png"
        );
        // And the lookup it exists for finds the setting either way round.
        let mut settings = std::collections::HashMap::new();
        settings.insert("textures/ui/Fill.png".to_string(), TexSetting::default());
        for probe in ["/proj/textures/ui/Fill.png", "textures\\ui\\Fill.png"] {
            assert!(
                settings.contains_key(asset_rel_path(probe, root).as_str()),
                "{probe} missed the settings map"
            );
        }
    }

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
    let Ok(rd) = floptle_vfs::read_dir(dir) else { return out };
    let mut entries = rd;
    entries.sort_by_key(|e| (e.is_file(), e.file_name()));
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if e.is_dir() {
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
    /// `sheet_cols`×`sheet_rows` cells that a UI image or a mesh's Material can
    /// pick individually — one grid per texture, inherited by everything using it.
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
    while floptle_vfs::exists(&p) {
        p = make(format!("{stem}_{n}"));
        n += 1;
    }
    p
}

/// Turn what somebody typed into a filename stem.
///
/// Deliberately permissive about what it ACCEPTS and strict about what it
/// writes: a person naming an effect types "muzzle flash", and refusing that
/// with a validation error teaches them nothing except to type underscores.
/// Spaces become camel humps (the engine's naming convention), and anything a
/// filesystem or a project-relative asset key could not carry is dropped.
///
/// Never returns empty — a blank result would make `vfx/.vfx.ron`, a hidden
/// file nobody would find again.
pub(crate) fn sanitize_asset_name(input: &str) -> String {
    let mut out = String::new();
    let mut upper_next = false;
    for c in input.trim().chars() {
        if c.is_ascii_alphanumeric() {
            if upper_next {
                out.extend(c.to_uppercase());
                upper_next = false;
            } else {
                out.push(c);
            }
        } else if c == '-' || c == '_' {
            out.push(c);
        } else {
            // Any other run (spaces, punctuation, slashes — a slash especially,
            // which would silently write into another folder) becomes a hump.
            // Leading junk is simply dropped: `../secret` is `secret`, not
            // `Secret`, because the leading dots are not a word boundary the
            // person typed.
            upper_next = !out.is_empty();
        }
    }
    if out.is_empty() { "untitled".to_string() } else { out }
}

#[cfg(test)]
mod name_tests {
    use super::sanitize_asset_name;

    #[test]
    fn a_typed_name_becomes_a_filename_without_refusing_anything() {
        assert_eq!(sanitize_asset_name("muzzle flash"), "muzzleFlash");
        assert_eq!(sanitize_asset_name("  Rain  "), "Rain");
        assert_eq!(sanitize_asset_name("hit-spark_2"), "hit-spark_2");
        // A slash must not survive: `vfx/../../etc` is a path, not a name.
        assert_eq!(sanitize_asset_name("a/b"), "aB");
        assert_eq!(sanitize_asset_name("../secret"), "secret");
        // Never empty — `vfx/.vfx.ron` is a file you cannot find again.
        assert_eq!(sanitize_asset_name("   "), "untitled");
        assert_eq!(sanitize_asset_name("!!!"), "untitled");
    }
}
