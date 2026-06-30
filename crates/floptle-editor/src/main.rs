//! # Floptle Editor
//!
//! The authoring application (binary `floptle`) — an egui shell over a live wgpu
//! viewport (ADR-0004). It renders the World **loaded from a `.ron` scene** with
//! the engine's PS1/retro look, and lets you select an object, move it, and save —
//! the first "open and interact with it" slice. Hierarchy/Inspector are stock egui
//! today; the dock shell, gizmos, import, and sculpt tools layer on next.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use floptle_core::math::{DVec3, EulerRot, Mat3, Mat4, Quat, Vec2, Vec3, Vec4};
use floptle_core::transform::Transform;
use floptle_core::{Entity, Light, Material, Matter, Name, ScriptInst, Scripts, Shape, World};
use floptle_script::ScriptHost;
use floptle_render::{
    capsule, cube, instance_of, instance_of_mat, uv_sphere, FlyCamera, Globals, Gpu, Grid, Input,
    InstanceRaw, MaterialParams, MeshId, Outline, Projection, Raster, Raymarch, RaymarchGlobals,
    RenderCamera, Retro, TexId,
};
use floptle_scene::{
    MaterialDoc, MatterDoc, NodeDoc, ProjectConfigDoc, SceneDoc, ScriptDoc, ShapeDoc, TransformDoc,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

// ---- the overlay transform gizmo ----------------------------------------------
//
// A screen-space gizmo drawn over the selected object with egui's painter. The
// geometry (axis tips, rotation rings) is projected from the object's Transform
// once per frame into PHYSICAL pixels and cached in `GizmoFrame`, so window/device
// events can hit-test the cursor against it cheaply. Dragging a handle applies an
// absolute transform from a start-of-drag snapshot (no per-event accumulation, so
// no drift). The gizmo only PAINTS — it never registers an egui widget — so it
// never steals input from the panel or the RMB fly-camera; ownership is decided by
// our own pixel hit-test plus the existing `is_pointer_over_egui` gate.

/// Handle length on screen, in physical pixels (kept roughly constant with depth).
const GIZMO_PX: f32 = 90.0;
/// Cursor-to-handle pick radius, physical pixels.
const HANDLE_PX: f32 = 12.0;
/// Axis-scale drag sensitivity (scale factor per pixel along the axis).
const SCALE_SENS: f32 = 0.01;
/// Screen radius (px) of the Rotate tool's center trackball ring.
const CENTER_RING_PX: f32 = 52.0;
/// Trackball free-rotate sensitivity (radians per pixel).
const TRACKBALL_SENS: f32 = 0.01;

/// The active editing tool. Bound to number keys 1-4 (5-9 reserved).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tool {
    #[default]
    Select,
    Move,
    Rotate,
    Scale,
    /// Terrain sculpt/paint brush (LMB-drag edits the terrain field).
    Sculpt,
}

impl Tool {
    fn from_digit(n: u32) -> Option<Tool> {
        match n {
            1 => Some(Tool::Select),
            2 => Some(Tool::Move),
            3 => Some(Tool::Rotate),
            4 => Some(Tool::Scale),
            5 => Some(Tool::Sculpt),
            _ => None, // 6-9 reserved for future tools
        }
    }

    fn label(self) -> &'static str {
        match self {
            Tool::Select => "select",
            Tool::Move => "move",
            Tool::Rotate => "rotate",
            Tool::Scale => "scale",
            Tool::Sculpt => "sculpt",
        }
    }
}

/// Which part of the gizmo the cursor is over / grabbed. An axis handle's meaning
/// depends on the active `Tool` (move along / rotate about / scale along it).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Handle {
    AxisX,
    AxisY,
    AxisZ,
    Center,
}

impl Handle {
    /// Index into the world basis (X=0, Y=1, Z=2), or `None` for the center.
    fn axis_index(self) -> Option<usize> {
        match self {
            Handle::AxisX => Some(0),
            Handle::AxisY => Some(1),
            Handle::AxisZ => Some(2),
            Handle::Center => None,
        }
    }
}

/// Cached, projected gizmo geometry for the current frame (all in physical pixels).
struct GizmoFrame {
    center: Vec2,
    /// Local-axis arrow tips; `None` for an axis that projects behind the camera.
    tips: [Option<Vec2>; 3],
    /// Rotation-ring polylines, one per local axis (only filled for the Rotate tool).
    ring_pts: [Vec<Vec2>; 3],
    /// A flat screen-space ring around the center: the free/trackball handle for
    /// Rotate, drawn so the center handle is grabbable (Move/Scale use a box).
    center_ring: Vec<Vec2>,
    /// Which handle the cursor is hovering this frame, if any.
    hovered: Option<Handle>,
}

/// A start-of-drag snapshot, so drags apply an absolute transform (no drift).
#[derive(Clone, Copy)]
struct DragState {
    handle: Handle,
    /// The entity this snapshot belongs to — guards against the selection
    /// changing mid-drag and applying the wrong object's start transform.
    entity: Entity,
    start_xf: Transform,
    cursor_start: Vec2,
}

/// World basis vector for axis `i` (X=0, Y=1, Z=2).
fn axis_world(i: usize) -> Vec3 {
    [Vec3::X, Vec3::Y, Vec3::Z][i]
}

/// The object's LOCAL axis `i` expressed in world space (so the gizmo aligns with
/// the object's current orientation, not the world frame).
fn local_axis(rot: Quat, i: usize) -> Vec3 {
    rot * axis_world(i)
}

fn handle_for_axis(i: usize) -> Handle {
    [Handle::AxisX, Handle::AxisY, Handle::AxisZ][i]
}

/// Deferred editor commands raised by the UI inside `run_ui`, applied after the
/// frame (so they can call `&mut self` methods the UI closure can't reach).
#[derive(Default)]
struct EditorCmd {
    add: Option<MatterDoc>,
    delete: bool,
    duplicate: bool,
    copy: bool,
    paste: bool,
    undo: bool,
    redo: bool,
    /// An inspector widget changed this frame (opens a coalesced undo step).
    inspector_changed: bool,
    /// Dismiss the viewport context menu.
    close_menu: bool,
    /// Toggle play mode (run scripts).
    toggle_play: bool,
    /// Toggle pause (freeze the script clock while playing).
    toggle_pause: bool,
    /// An asset was dropped (path) — spawn a model or attach a script.
    drop_asset: Option<String>,
    /// A script file dropped onto a specific hierarchy node (path, entity).
    drop_script_on: Option<(String, Entity)>,
    /// Save a material as a named preset under assets/materials/.
    save_material: Option<(String, MaterialDoc)>,
    /// Give an entity a default Material component (start customizing its look).
    add_material: Option<Entity>,
    /// Add / remove a physics RigidBody on this entity.
    add_rigidbody: Option<Entity>,
    remove_rigidbody: Option<Entity>,
    /// Toggle the static MeshCollider marker on a Mesh node (`true` = add, `false` = remove).
    set_mesh_collider: Option<(Entity, bool)>,
    /// Remove an entity's Material component (back to the default look).
    remove_material: Option<Entity>,
    /// Apply a named material preset to an entity.
    apply_preset: Option<(Entity, String)>,
    /// Extract a model's embedded textures into assets/textures/ (a model path).
    extract_textures: Option<String>,
    /// Re-parent a node: (child, new parent or None = make it a root).
    reparent: Option<(Entity, Option<Entity>)>,
    /// Add a new node as a child of an entity (matter, parent).
    add_parented: Option<(MatterDoc, Entity)>,
    /// Create a fresh flat terrain.
    create_terrain: bool,
    /// Remove the terrain.
    clear_terrain: bool,
    /// The terrain texture palette changed — re-upload it.
    terrain_palette_changed: bool,
    /// Focus (or open) the Terrain dock tab.
    focus_terrain: bool,
    /// Fill the whole target terrain with a color or texture slot.
    fill_terrain: Option<TerrainFill>,
    /// "Fill bounds" tool: lay flat ground across the active terrain (uses the brush's
    /// fill_top / fill_floor / fill_inset settings).
    fill_bounds: bool,
    /// Open this scene file (double-clicked in Assets) — prompts on unsaved changes.
    open_scene: Option<String>,
    /// Confirmed scene open from the unsaved-changes modal: (path, save_first).
    do_open_scene: Option<(String, bool)>,
    /// Change a texture's sampling (filter/wrap): (image path, new setting).
    set_texture_setting: Option<(String, TexSetting)>,
    /// Give this camera node play-mode authority (clear the others).
    set_active_camera: Option<Entity>,
    /// Move this camera node to the current editor viewpoint.
    camera_from_view: Option<Entity>,
    /// Spawn a camera node, optionally parented to this entity.
    add_camera: Option<Option<Entity>>,
    /// Open the "new scene" name prompt.
    open_new_scene: bool,
    /// Create a new blank scene with this name (from Assets ⏵ New ⏵ Scene).
    new_scene: Option<String>,
    /// Switch the active tool (from the Scene-tab tool strip).
    set_tool: Option<Tool>,
    /// Save the current scene.
    save_scene: bool,
    /// Rescan the project asset tree.
    refresh_assets: bool,
    /// Open a script file in the Scripting IDE.
    open_script: Option<String>,
    /// Open a script in the user's PREFERRED editor (in-engine or external).
    open_script_pref: Option<String>,
    /// Jump to a Console line's source: (script name, 1-based line).
    open_log_source: Option<(String, u32)>,
    /// Focus the Scripting tab (e.g. after a double-click-to-open).
    focus_scripting: bool,
    /// A File-menu project action (New / Open / Close).
    project_action: Option<ProjectAction>,
    /// Create a new folder inside this directory (absolute path).
    new_folder_in: Option<String>,
    /// Create a new blank Lua script inside this directory (absolute path).
    new_script_in: Option<String>,
    /// Attach a named `.lua` script to an entity (seed params from its defaults).
    attach_named: Option<(String, Entity)>,
    /// Open this file in the user's external editor (ADR-0011).
    open_in_editor: Option<String>,
    /// Persist a new external-editor command (user preference).
    set_external_editor: Option<String>,
    /// Persist the "prefer external editor" toggle.
    set_prefer_external: Option<bool>,
    /// Open the rename modal for this asset (absolute path).
    rename_asset: Option<String>,
    /// Commit a rename from the modal: (current path, new file/folder name).
    do_rename: Option<(String, String)>,
    /// Delete this asset file/folder (absolute path).
    delete_asset: Option<String>,
}

/// Editor reference-grid display + snapping settings.
#[derive(Clone, Copy)]
struct GridConfig {
    show: bool,
    /// Spacing between grid lines (world units) — also the snap increment.
    size: f32,
    /// Cells out from the center the grid extends.
    extent: i32,
    color: [f32; 3],
    alpha: f32,
    /// Snap moved/created objects to the grid.
    snap: bool,
    /// How far BELOW the camera the grid plane sits (world units, snapped to `size`).
    y_offset: f32,
}

impl Default for GridConfig {
    fn default() -> Self {
        Self {
            show: true,
            size: 1.0,
            extent: 24,
            color: [0.45, 0.45, 0.58],
            alpha: 0.32,
            snap: false,
            y_offset: 0.0,
        }
    }
}

/// A node in the project asset tree (the bottom file browser).
enum AssetEntry {
    Dir(String, Vec<AssetEntry>),
    File { name: String, path: String },
}

/// What a dragged asset carries — its path. The drop target reads the extension to
/// decide what to do (a model spawns; a script attaches).
#[derive(Clone)]
struct AssetPayload {
    path: String,
}

/// What a hierarchy row carries while dragged — its entity, so dropping it on
/// another row re-parents it.
#[derive(Clone)]
struct NodePayload(Entity);

fn is_model(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".glb") || p.ends_with(".gltf")
}

/// Lowercase name for a key, for the script `input` API (`input.key("w")`).
fn key_name(code: KeyCode) -> Option<&'static str> {
    use KeyCode::*;
    Some(match code {
        KeyA => "a", KeyB => "b", KeyC => "c", KeyD => "d", KeyE => "e", KeyF => "f",
        KeyG => "g", KeyH => "h", KeyI => "i", KeyJ => "j", KeyK => "k", KeyL => "l",
        KeyM => "m", KeyN => "n", KeyO => "o", KeyP => "p", KeyQ => "q", KeyR => "r",
        KeyS => "s", KeyT => "t", KeyU => "u", KeyV => "v", KeyW => "w", KeyX => "x",
        KeyY => "y", KeyZ => "z",
        Digit0 => "0", Digit1 => "1", Digit2 => "2", Digit3 => "3", Digit4 => "4",
        Digit5 => "5", Digit6 => "6", Digit7 => "7", Digit8 => "8", Digit9 => "9",
        Space => "space", Enter | NumpadEnter => "enter", Escape => "escape", Tab => "tab",
        Backspace => "backspace", Delete => "delete",
        ShiftLeft | ShiftRight => "shift", ControlLeft | ControlRight => "ctrl",
        AltLeft | AltRight => "alt",
        ArrowLeft => "left", ArrowRight => "right", ArrowUp => "up", ArrowDown => "down",
        _ => return None,
    })
}
fn is_script(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".lua")
}

/// The script name (file stem) a `.lua` path refers to — what a `ScriptInst.kind`
/// stores and what resolves to `scripts/<name>.lua`.
fn script_name_of(path: &str) -> String {
    Path::new(path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

fn is_texture(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".png") || p.ends_with(".jpg") || p.ends_with(".jpeg")
}
fn is_markdown(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".md") || p.ends_with(".markdown")
}
/// A saved material preset (`materials/<name>.ron`) — distinguished from a scene
/// `.ron` by living under a `materials` directory.
fn is_material(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".ron") && p.replace('\\', "/").contains("materials/")
}

/// A scene file (`scenes/<name>.ron`).
fn is_scene(path: &str) -> bool {
    let p = path.to_ascii_lowercase().replace('\\', "/");
    p.ends_with(".ron") && p.contains("scenes/")
}

/// Shorten `name` to at most `max` chars (…-elided), for fixed-width grid tiles.
fn truncate_label(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_string();
    }
    let keep: String = name.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

/// A small type glyph + tint for an asset file, used in the browser tree + grid.
fn asset_kind_icon(path: &str) -> (&'static str, egui::Color32) {
    if is_model(path) {
        ("⬣", egui::Color32::from_rgb(120, 200, 210))
    } else if is_script(path) {
        ("¶", egui::Color32::from_rgb(130, 170, 240))
    } else if is_texture(path) {
        ("🖼", egui::Color32::from_rgb(140, 210, 140))
    } else if is_material(path) {
        ("◑", egui::Color32::from_rgb(240, 180, 110))
    } else if path.to_ascii_lowercase().ends_with(".ron") {
        ("⎙", egui::Color32::from_rgb(200, 150, 230)) // a scene
    } else if is_markdown(path) {
        ("§", egui::Color32::from_gray(190))
    } else {
        ("▣", egui::Color32::from_gray(170))
    }
}

/// Open the OS file manager at `path` (revealing the file where supported).
fn reveal_in_explorer(path: &Path) {
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
fn collect_texture_paths(entries: &[AssetEntry], out: &mut Vec<String>) {
    for e in entries {
        match e {
            AssetEntry::Dir(_, children) => collect_texture_paths(children, out),
            AssetEntry::File { path, .. } if is_texture(path) => out.push(path.clone()),
            AssetEntry::File { .. } => {}
        }
    }
}

/// Collect the names of every `.lua` script in the asset tree (for "Add Script").
fn collect_script_names(entries: &[AssetEntry], out: &mut Vec<String>) {
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
fn build_assets(dir: &std::path::Path) -> Vec<AssetEntry> {
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

/// The default Lua scripts every project ships with (ADR-0003): the engine's
/// built-in behaviors, now plain hot-reloadable Lua the user can read and edit.
const DEFAULT_SCRIPTS: &[(&str, &str)] = &[
    ("rotate.lua", include_str!("../../../assets/scripts/rotate.lua")),
    ("pulsate.lua", include_str!("../../../assets/scripts/pulsate.lua")),
    ("float.lua", include_str!("../../../assets/scripts/float.lua")),
];

// ---- "Open in IDE" (ADR-0011): launch the user's external editor ------------

/// Is `cmd` (a binary name) resolvable on PATH?
fn on_path(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| {
        dir.join(cmd).is_file()
            || (cfg!(windows)
                && ["exe", "cmd", "bat"].iter().any(|e| dir.join(format!("{cmd}.{e}")).is_file()))
    })
}

/// Pick a sensible default external editor by probing PATH (VSCode first).
fn auto_detect_editor() -> String {
    for c in ["code", "codium", "code-insiders", "zed", "subl", "nvim", "vim", "nano"] {
        if on_path(c) {
            return c.to_string();
        }
    }
    "code".to_string()
}

/// The per-user config directory for Floptle (platform-appropriate).
fn floptle_config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("floptle"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/floptle"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|c| c.join("floptle"))
    }
}

fn editor_pref_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("external_editor"))
}

/// The configured external editor command, or an auto-detected default if unset.
fn load_external_editor() -> String {
    editor_pref_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(auto_detect_editor)
}

fn save_external_editor(cmd: &str) {
    if let Some(p) = editor_pref_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(p, cmd.trim());
    }
}

fn prefer_pref_path() -> Option<PathBuf> {
    floptle_config_dir().map(|d| d.join("prefer_external_editor"))
}

/// Whether the user prefers their external editor over the in-engine IDE.
fn load_prefer_external() -> bool {
    prefer_pref_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

fn save_prefer_external(v: bool) {
    if let Some(p) = prefer_pref_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(p, if v { "1" } else { "0" });
    }
}

/// Launch the external editor on `file`. VSCode-family editors open the project as
/// the workspace root and jump to `file:line` (ADR-0011); others just open the file.
/// `cmd` may include leading args (e.g. "code -n").
fn open_external_editor(cmd: &str, project_root: &Path, file: &str, line: usize) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let Some((prog, pre)) = parts.split_first() else { return };
    let mut command = std::process::Command::new(prog);
    command.args(pre);
    if prog.contains("code") {
        command.arg(project_root).arg("--goto").arg(format!("{file}:{line}"));
    } else {
        command.arg(file);
    }
    if let Err(e) = command.spawn() {
        eprintln!("  Open in IDE ({prog}) failed: {e}");
    }
}

/// Deferred intents from [`material_props_ui`] (applied after the borrow ends).
#[derive(Default)]
struct MatEditResult {
    changed: bool,
    remove: bool,
    save_as: Option<String>,
}

/// In-depth material property editors — shared by the Inspector's Material section
/// and the floating Material Editor window. Edits `m` in place (so undo coalesces
/// via `inspector_changed`); preset apply/save/remove come back as intents.
fn material_props_ui(
    ui: &mut egui::Ui,
    m: &mut Material,
    presets: &[(String, floptle_scene::MaterialDoc)],
    textures: &[String],
    name_buf: &mut String,
) -> MatEditResult {
    let mut r = MatEditResult::default();

    egui::Grid::new("mat_top").num_columns(2).spacing([8.0, 5.0]).show(ui, |ui| {
        ui.label("base color");
        r.changed |= ui.color_edit_button_rgb(&mut m.color).changed();
        ui.end_row();
        ui.label("texture");
        let cur = m
            .texture
            .as_deref()
            .map(|p| Path::new(p).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default())
            .unwrap_or_else(|| "none".into());
        egui::ComboBox::from_id_salt("mat_tex").selected_text(cur).show_ui(ui, |ui| {
            if ui.selectable_label(m.texture.is_none(), "none").clicked() {
                m.texture = None;
                r.changed = true;
            }
            for path in textures {
                let name =
                    Path::new(path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                if ui.selectable_label(m.texture.as_deref() == Some(path.as_str()), name).clicked() {
                    m.texture = Some(path.clone());
                    r.changed = true;
                }
            }
        });
        ui.end_row();
        ui.label("emissive");
        ui.horizontal(|ui| {
            r.changed |= ui.color_edit_button_rgb(&mut m.emissive).changed();
            r.changed |= ui
                .add(egui::DragValue::new(&mut m.emissive_strength).speed(0.02).range(0.0..=20.0).prefix("×"))
                .on_hover_text("emissive strength")
                .changed();
        });
        ui.end_row();
        ui.label("unlit");
        r.changed |= ui.checkbox(&mut m.unlit, "fullbright / flat").changed();
        ui.end_row();
    });

    // These only affect the lit path, so grey them out when unlit.
    ui.add_enabled_ui(!m.unlit, |ui| {
        egui::Grid::new("mat_lit").num_columns(2).spacing([8.0, 5.0]).show(ui, |ui| {
            ui.label("specular");
            ui.horizontal(|ui| {
                r.changed |= ui.color_edit_button_rgb(&mut m.specular).changed();
                r.changed |= ui
                    .add(egui::DragValue::new(&mut m.specular_strength).speed(0.02).range(0.0..=8.0).prefix("×"))
                    .on_hover_text("specular strength")
                    .changed();
            });
            ui.end_row();
            ui.label("shininess");
            r.changed |= ui.add(egui::Slider::new(&mut m.shininess, 1.0..=256.0).logarithmic(true)).changed();
            ui.end_row();
            ui.label("rim");
            ui.horizontal(|ui| {
                r.changed |= ui.color_edit_button_rgb(&mut m.rim).changed();
                r.changed |= ui
                    .add(egui::DragValue::new(&mut m.rim_strength).speed(0.02).range(0.0..=8.0).prefix("×"))
                    .on_hover_text("rim / fresnel strength")
                    .changed();
            });
            ui.end_row();
            ui.label("ambient");
            r.changed |= ui.add(egui::Slider::new(&mut m.ambient, 0.0..=4.0)).changed();
            ui.end_row();
            ui.label("opacity");
            r.changed |= ui
                .add(egui::Slider::new(&mut m.alpha, 0.0..=1.0))
                .on_hover_text("1 = opaque; below 1 alpha-blends over the scene (drawn after opaque objects)")
                .changed();
            ui.end_row();
        });
    });

    ui.separator();
    ui.horizontal(|ui| {
        if !presets.is_empty() {
            ui.menu_button("Apply preset", |ui| {
                for (name, doc) in presets {
                    if ui.button(name).clicked() {
                        *m = doc.to_material();
                        r.changed = true;
                        ui.close();
                    }
                }
            });
        }
        ui.add(egui::TextEdit::singleline(name_buf).desired_width(100.0).hint_text("preset name"));
        if ui.button("Save preset").clicked() && !name_buf.trim().is_empty() {
            r.save_as = Some(name_buf.trim().to_string());
        }
    });
    if ui.button("🗑 Remove material").clicked() {
        r.remove = true;
    }
    r
}

/// Convert a core [`Material`] into the renderer's per-instance [`MaterialParams`].
fn material_params(m: &Material) -> MaterialParams {
    MaterialParams {
        color: m.color,
        emissive: m.emissive,
        emissive_strength: m.emissive_strength,
        specular: m.specular,
        shininess: m.shininess,
        specular_strength: m.specular_strength,
        rim: m.rim,
        rim_strength: m.rim_strength,
        unlit: m.unlit,
        ambient: m.ambient,
        alpha: m.alpha,
    }
}

/// The default look for a Blob with no Material: neutral tint plus the subtle blue
/// rim the blob shipped with, so material-less blobs render exactly as before while a
/// blob that DOES carry a Material is fully driven by it.
fn blob_default_material() -> MaterialParams {
    let mut m = MaterialParams::flat([1.0, 1.0, 1.0]);
    m.rim = [0.5, 0.6, 0.8];
    m.rim_strength = 0.12;
    m
}

/// Pack up to 16 blobs' materials into the raymarch uniform arrays (tint, emissive,
/// specular, params=[shininess,rim,unlit,ambient], rim), mirroring `terrain_*`.
type BlobMatArrays =
    ([[f32; 4]; 16], [[f32; 4]; 16], [[f32; 4]; 16], [[f32; 4]; 16], [[f32; 4]; 16]);
fn blob_mat_arrays(set: &[(DVec3, f32, MaterialParams)]) -> BlobMatArrays {
    let mut tint = [[1.0f32, 1.0, 1.0, 0.0]; 16];
    let mut emissive = [[0.0f32; 4]; 16];
    let mut specular = [[1.0f32, 1.0, 1.0, 0.0]; 16];
    let mut params = [[16.0f32, 0.0, 0.0, 1.0]; 16];
    let mut rim = [[0.0f32; 4]; 16];
    for (i, (_, _, m)) in set.iter().take(16).enumerate() {
        tint[i] = [m.color[0], m.color[1], m.color[2], 0.0];
        emissive[i] = [m.emissive[0], m.emissive[1], m.emissive[2], m.emissive_strength];
        specular[i] = [m.specular[0], m.specular[1], m.specular[2], m.specular_strength];
        params[i] = [m.shininess, m.rim_strength, if m.unlit { 1.0 } else { 0.0 }, m.ambient];
        rim[i] = [m.rim[0], m.rim[1], m.rim[2], 0.0];
    }
    (tint, emissive, specular, params, rim)
}

/// Collect up to 16 placeable point lights from the world into the camera-relative
/// uniform arrays (xyz pos + range; rgb = color×intensity) for the raster + raymarch
/// passes. Returns (count_vec4, positions, colors).
fn collect_point_lights(
    world: &World,
    cam_world: DVec3,
) -> ([f32; 4], [[f32; 4]; 16], [[f32; 4]; 16]) {
    let mut pos = [[0.0f32; 4]; 16];
    let mut col = [[0.0f32; 4]; 16];
    let mut n = 0usize;
    for (e, m) in world.query::<Matter>() {
        if let Matter::PointLight { color, intensity, range } = m {
            if n >= 16 {
                break;
            }
            let wp = floptle_core::world_transform(world, e).translation;
            let c = (wp - cam_world).as_vec3();
            pos[n] = [c.x, c.y, c.z, range.max(0.0001)];
            col[n] = [color[0] * intensity, color[1] * intensity, color[2] * intensity, 0.0];
            n += 1;
        }
    }
    ([n as f32, 0.0, 0.0, 0.0], pos, col)
}

/// How a texture is filtered — the serde-friendly mirror of [`floptle_render::TexFilter`],
/// persisted per texture in `.floptle/textures.ron`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
enum FilterMode {
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
enum WrapMode {
    #[default]
    Repeat,
    Clamp,
    Mirror,
}

/// A texture's sampling settings, persisted per project. Default = crisp tiling.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
struct TexSetting {
    #[serde(default)]
    filter: FilterMode,
    #[serde(default)]
    wrap: WrapMode,
}

impl TexSetting {
    fn to_sampling(self) -> floptle_render::TexSampling {
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

/// EmmyLua type annotations for the engine API, so an external Lua language server
/// (e.g. VSCode's Lua extension) gives hover docs + completion for `node`, `params`,
/// `time`, `dt`, the lifecycle hooks, etc. Written to `.floptle/library/`.
const LUA_ANNOTATIONS: &str = "\
---@meta
--- Floptle engine scripting API (ADR-0003). Generated — do not edit.

---@class Node The node's transform, synced to/from the engine each frame.
---@field x number World X position.
---@field y number World Y position.
---@field z number World Z position.
---@field scale number Uniform scale (shortcut; sets all axes).
---@field scale_x number Scale along X.
---@field scale_y number Scale along Y.
---@field scale_z number Scale along Z.
---@field yaw number Heading about Y, in radians.
---@field pitch number Pitch about X, in radians.
---@field roll number Roll about Z, in radians.
---@field grounded boolean Physics (rigidbody nodes): resting on a surface this frame.
---@field vx number Physics: body velocity X (read/write — set it to drive the body).
---@field vy number Physics: body velocity Y (read/write).
---@field vz number Physics: body velocity Z (read/write).
---@field up_x number Physics: body up (−gravity) X — radial on a planet.
---@field up_y number Physics: body up (−gravity) Y.
---@field up_z number Physics: body up (−gravity) Z.

---This instance's tunables, seeded from the script's `defaults` table.
---@type table<string, number>
params = {}

---Seconds since play started.
---@type number
time = 0.0

---Seconds since the last frame (also passed to update).
---@type number
dt = 0.0

---The tunables this script declares (shown in the Inspector).
---@type table<string, number>
defaults = {}

---Print a message to the engine console.
---@param msg string
function log(msg) end

---Runs once when play begins (optional).
---@param node Node
function start(node) end

---Runs every frame while playing.
---@param node Node
---@param dt number Seconds since the last frame.
function update(node, dt) end

---Player input (play mode) — poll the keyboard + mouse to make games interactive.
---@class Input
input = {}
---True while `name` is held. Names: a-z, 0-9, space, enter, shift, ctrl, alt, left/right/up/down, escape, tab.
---@param name string
---@return boolean
function input.key(name) end
---True only on the frame `name` goes down (a key-press edge).
---@param name string
---@return boolean
function input.pressed(name) end
---A -1/0/1 axis from a negative/positive key pair, e.g. input.axis(\"a\", \"d\").
---@param neg string
---@param pos string
---@return number
function input.axis(neg, pos) end
---The cursor position in pixels: `local x, y = input.mouse()`.
---@return number, number
function input.mouse() end
---Mouse movement since last frame: `local dx, dy = input.mouse_delta()`.
---@return number, number
function input.mouse_delta() end
---Mouse wheel delta this frame.
---@return number
function input.scroll() end
---True while a mouse button is held (0 left, 1 right, 2 middle).
---@param i integer
---@return boolean
function input.button(i) end
---True only on the frame a mouse button goes down.
---@param i integer
---@return boolean
function input.clicked(i) end
";

/// `.luarc.json` pointing the Lua language server at the annotation library and
/// declaring the engine globals (so they aren't flagged undefined).
const LUARC_JSON: &str = "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"log\", \"input\"]\n}\n";

/// Write the Lua language-server support files into a project (annotations always
/// refreshed; `.luarc.json` only if absent, so a user's own config is preserved).
fn write_lua_support(project_root: &Path) {
    let lib = project_root.join(".floptle").join("library");
    let _ = std::fs::create_dir_all(&lib);
    let _ = std::fs::write(lib.join("floptle.lua"), LUA_ANNOTATIONS);
    let luarc = project_root.join(".luarc.json");
    if !luarc.exists() {
        let _ = std::fs::write(luarc, LUARC_JSON);
    }
}

/// Write the default scripts into `scripts_dir` (each only if absent).
fn seed_default_scripts(scripts_dir: &Path) {
    let _ = std::fs::create_dir_all(scripts_dir);
    for (name, body) in DEFAULT_SCRIPTS {
        let p = scripts_dir.join(name);
        if !p.exists() {
            let _ = std::fs::write(&p, body);
        }
    }
}

/// A path inside `dir` named `stem[.ext]`, auto-suffixed (`stem_1`, `stem_2`, …)
/// until it doesn't collide with an existing entry. `ext: None` = a folder name.
fn unique_path(dir: &Path, stem: &str, ext: Option<&str>) -> PathBuf {
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

fn new_cube() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Cube, color: [0.8, 0.5, 0.4] }
}

/// The default node name for a matter kind.
fn matter_doc_name(m: &MatterDoc) -> &'static str {
    match m {
        MatterDoc::Primitive { shape: ShapeDoc::Cube, .. } => "Cube",
        MatterDoc::Primitive { shape: ShapeDoc::Sphere, .. } => "Sphere",
        MatterDoc::Primitive { shape: ShapeDoc::Capsule, .. } => "Capsule",
        MatterDoc::Blob { .. } => "Blob",
        MatterDoc::Mesh { .. } => "Mesh",
        MatterDoc::Empty => "Group",
        MatterDoc::Terrain { .. } => "Terrain",
        MatterDoc::Camera { .. } => "Camera",
        MatterDoc::PointLight { .. } => "Point Light",
        MatterDoc::GravityVolume { .. } => "Gravity Volume",
    }
}
fn new_sphere() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Sphere, color: [0.4, 0.6, 0.9] }
}
fn new_capsule() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Capsule, color: [0.5, 0.85, 0.6] }
}

/// Map a top-row number key to its digit (1-9), else `None`.
fn digit_of(code: KeyCode) -> Option<u32> {
    match code {
        KeyCode::Digit1 => Some(1),
        KeyCode::Digit2 => Some(2),
        KeyCode::Digit3 => Some(3),
        KeyCode::Digit4 => Some(4),
        KeyCode::Digit5 => Some(5),
        KeyCode::Digit6 => Some(6),
        KeyCode::Digit7 => Some(7),
        KeyCode::Digit8 => Some(8),
        KeyCode::Digit9 => Some(9),
        _ => None,
    }
}

/// Project an absolute world point to physical-pixel screen space (camera-relative,
/// ADR-0015). Returns `None` when the point is behind the camera.
fn project(world: DVec3, cam_world: DVec3, vp: Mat4, w: f32, h: f32) -> Option<Vec2> {
    let rel = (world - cam_world).as_vec3();
    let clip = vp * rel.extend(1.0);
    if clip.w <= 1e-4 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    Some(Vec2::new((ndc.x * 0.5 + 0.5) * w, (1.0 - (ndc.y * 0.5 + 0.5)) * h))
}

/// The world point under `cursor` (physical px) — its ray's hit on the ground
/// plane y=0, or ~6 units ahead of the camera if the ray misses. `inv_vp` is the
/// inverse of the camera's view-projection at this `w`/`h` aspect.
fn cursor_ground(
    cam_world: DVec3,
    cam_rot: Quat,
    inv_vp: Mat4,
    w: f32,
    h: f32,
    cursor: Option<Vec2>,
) -> DVec3 {
    let fallback = cam_world + (cam_rot * Vec3::NEG_Z * 6.0).as_dvec3();
    let Some(cursor) = cursor else { return fallback };
    let ndc = Vec2::new(cursor.x / w * 2.0 - 1.0, 1.0 - cursor.y / h * 2.0);
    let near = inv_vp * Vec4::new(ndc.x, ndc.y, 0.0, 1.0);
    let far = inv_vp * Vec4::new(ndc.x, ndc.y, 1.0, 1.0);
    let ro = near.truncate() / near.w; // camera-relative
    let rd = (far.truncate() / far.w - ro).normalize();
    if rd.y.abs() > 1e-4 {
        let t = -(cam_world.y as f32 + ro.y) / rd.y;
        if (0.1..1000.0).contains(&t) {
            return cam_world + (ro + rd * t).as_dvec3();
        }
    }
    fallback
}

/// True when the cursor (physical px) is over the bare Scene viewport — inside the
/// Scene-tab rect and not under a *floating* egui area (toolbar, combo popup, the
/// context menu). egui_dock paints the panels and the Scene tab alike in the
/// Background layer, and egui registers that background as a full-window
/// interactable area, so `layer_id_at` returns `Some(Background)` over *everything*
/// in the window — never `None`. We therefore accept the Background layer (it means
/// "no float on top") and reject only Middle/Foreground areas, then use the Scene
/// rect to tell the viewport apart from the side panels (which are outside it).
fn scene_hit(ctx: &egui::Context, cursor: Option<Vec2>, rect: Option<egui::Rect>) -> bool {
    let (Some(cursor), Some(rect)) = (cursor, rect) else { return false };
    let ppp = ctx.pixels_per_point();
    let p = egui::pos2(cursor.x / ppp, cursor.y / ppp);
    if !rect.contains(p) {
        return false;
    }
    match ctx.layer_id_at(p) {
        None => true,
        Some(layer) => layer.order == egui::Order::Background,
    }
}

/// Distance from point `p` to segment `a`–`b` (pixel space).
fn seg_dist(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_squared();
    let t = if len2 < 1e-6 { 0.0 } else { ((p - a).dot(ab) / len2).clamp(0.0, 1.0) };
    (p - (a + ab * t)).length()
}

/// Snap each component of a world position to a grid `step` (no-op if step ≤ 0).
fn snap_dvec3(v: DVec3, step: f64) -> DVec3 {
    if step <= 1e-6 {
        return v;
    }
    DVec3::new((v.x / step).round() * step, (v.y / step).round() * step, (v.z / step).round() * step)
}

/// Nearest positive ray–sphere hit `t` (general — `rd` need not be unit), else None.
/// `t` is in the ray's own parameter space, so it stays comparable across objects
/// even when the ray was transformed into a non-uniformly-scaled local frame.
fn ray_sphere(ro: Vec3, rd: Vec3, center: Vec3, radius: f32) -> Option<f32> {
    let oc = ro - center;
    let a = rd.dot(rd);
    let b = 2.0 * oc.dot(rd);
    let c = oc.length_squared() - radius * radius;
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return None;
    }
    let s = disc.sqrt();
    let t0 = (-b - s) / (2.0 * a);
    if t0 > 1e-3 {
        return Some(t0);
    }
    let t1 = (-b + s) / (2.0 * a); // origin inside the sphere
    (t1 > 1e-3).then_some(t1)
}

/// Nearest positive ray–AABB hit `t` for a box centered at the origin with the given
/// `half` extent (slab method; `rd` need not be unit).
fn ray_aabb(ro: Vec3, rd: Vec3, half: f32) -> Option<f32> {
    let inv = Vec3::ONE / rd; // 0 components ⏵ ±inf, handled by the min/max
    let t1 = (Vec3::splat(-half) - ro) * inv;
    let t2 = (Vec3::splat(half) - ro) * inv;
    let near = t1.min(t2).max_element();
    let far = t1.max(t2).min_element();
    if near <= far && far > 1e-3 {
        Some(near.max(1e-3))
    } else {
        None
    }
}

/// Build the gizmo geometry for the selected entity and hit-test the cursor.
fn build_gizmo(
    tool: Tool,
    selection: Option<Entity>,
    world: &World,
    cursor: Option<Vec2>,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
) -> Option<GizmoFrame> {
    if tool == Tool::Select || tool == Tool::Sculpt {
        return None;
    }
    let e = selection?;
    // World transform, so the gizmo sits on the node's actual (parented) placement.
    let t = floptle_core::world_transform(world, e);
    let center = project(t.translation, cam_world, vp, w, h)?;
    let rot = t.rotation;

    // Pixel-constant handle length: world units that subtend ~GIZMO_PX at this depth
    // (60° vertical fov). Clamp the near distance so a close object doesn't explode.
    let dist = (t.translation - cam_world).length().max(0.4) as f32;
    let axis_len = GIZMO_PX * 2.0 * dist * (30f32.to_radians()).tan() / h;

    // Tips follow the object's LOCAL axes, so the gizmo aligns with its orientation.
    let mut tips = [None; 3];
    for i in 0..3 {
        let tip_world = t.translation + (local_axis(rot, i) * axis_len).as_dvec3();
        tips[i] = project(tip_world, cam_world, vp, w, h);
    }

    // Rotation rings live in the planes spanned by the object's local axes.
    let mut ring_pts: [Vec<Vec2>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut center_ring: Vec<Vec2> = Vec::new();
    if tool == Tool::Rotate {
        const N: usize = 48;
        for i in 0..3 {
            let u = local_axis(rot, (i + 1) % 3);
            let v = local_axis(rot, (i + 2) % 3);
            let mut pts = Vec::with_capacity(N + 1);
            for k in 0..=N {
                let a = (k as f32) / (N as f32) * std::f32::consts::TAU;
                let p = t.translation + ((u * a.cos() + v * a.sin()) * axis_len).as_dvec3();
                if let Some(s) = project(p, cam_world, vp, w, h) {
                    pts.push(s);
                }
            }
            ring_pts[i] = pts;
        }
        // A flat screen-space trackball ring around the center — the free-rotate handle.
        const M: usize = 40;
        for k in 0..=M {
            let a = (k as f32) / (M as f32) * std::f32::consts::TAU;
            center_ring.push(center + Vec2::new(a.cos(), a.sin()) * CENTER_RING_PX);
        }
    }

    let hovered = cursor.and_then(|c| hit_test(tool, c, center, &tips, &ring_pts, &center_ring));
    Some(GizmoFrame { center, tips, ring_pts, center_ring, hovered })
}

/// Nearest gizmo handle to the cursor within `HANDLE_PX`, if any.
fn hit_test(
    tool: Tool,
    cursor: Vec2,
    center: Vec2,
    tips: &[Option<Vec2>; 3],
    rings: &[Vec<Vec2>; 3],
    center_ring: &[Vec2],
) -> Option<Handle> {
    let mut cands: Vec<(Handle, f32)> = Vec::new();
    let ring_dist = |ring: &[Vec2]| {
        let mut dmin = f32::INFINITY;
        for win in ring.windows(2) {
            dmin = dmin.min(seg_dist(cursor, win[0], win[1]));
        }
        dmin
    };
    match tool {
        Tool::Move | Tool::Scale => {
            for i in 0..3 {
                if let Some(tip) = tips[i] {
                    cands.push((handle_for_axis(i), seg_dist(cursor, center, tip)));
                }
            }
            cands.push((Handle::Center, (cursor - center).length()));
        }
        Tool::Rotate => {
            for i in 0..3 {
                cands.push((handle_for_axis(i), ring_dist(&rings[i])));
            }
            // The trackball ring (free rotate) — only when not closer to an axis ring.
            cands.push((Handle::Center, ring_dist(center_ring)));
        }
        Tool::Select | Tool::Sculpt => {}
    }
    cands
        .into_iter()
        .filter(|(_, d)| *d <= HANDLE_PX)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(h, _)| h)
}

/// Brighten a handle color toward white when it is hovered or grabbed.
fn brighten(c: egui::Color32, on: bool) -> egui::Color32 {
    if !on {
        return c;
    }
    let mix = |x: u8| ((x as u16 + 255) / 2) as u8;
    egui::Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

/// A small filled arrowhead at `to`, pointing away from `from`.
fn arrow_head(painter: &egui::Painter, from: egui::Pos2, to: egui::Pos2, col: egui::Color32) {
    let dir = to - from;
    let len = dir.length();
    if len < 1.0 {
        return;
    }
    let d = dir / len;
    let n = egui::vec2(-d.y, d.x);
    let s = 8.0;
    let p2 = to - d * s + n * (s * 0.5);
    let p3 = to - d * s - n * (s * 0.5);
    painter.add(egui::Shape::convex_polygon(vec![to, p2, p3], col, egui::Stroke::NONE));
}

/// Paint the cached gizmo with the egui painter. Geometry is physical pixels; the
/// painter works in logical points, so divide by `ppp`.
fn paint_gizmo(painter: &egui::Painter, g: &GizmoFrame, tool: Tool, grabbed: Option<Handle>, ppp: f32) {
    use egui::{Color32, Pos2, Stroke};
    let pt = |v: Vec2| Pos2::new(v.x / ppp, v.y / ppp);
    let axis_col = [
        Color32::from_rgb(220, 70, 70),
        Color32::from_rgb(80, 200, 90),
        Color32::from_rgb(80, 130, 235),
    ];
    let active = |h: Handle| grabbed == Some(h) || g.hovered == Some(h);
    let center = pt(g.center);
    match tool {
        Tool::Move => {
            for i in 0..3 {
                if let Some(tip) = g.tips[i] {
                    let on = active(handle_for_axis(i));
                    let col = brighten(axis_col[i], on);
                    let tp = pt(tip);
                    painter.line_segment([center, tp], Stroke::new(if on { 4.0 } else { 2.5 }, col));
                    arrow_head(painter, center, tp, col);
                }
            }
            let on = active(Handle::Center);
            painter.rect_filled(
                egui::Rect::from_center_size(center, egui::vec2(9.0, 9.0)),
                0.0,
                brighten(Color32::from_gray(210), on),
            );
        }
        Tool::Scale => {
            for i in 0..3 {
                if let Some(tip) = g.tips[i] {
                    let on = active(handle_for_axis(i));
                    let col = brighten(axis_col[i], on);
                    let tp = pt(tip);
                    painter.line_segment([center, tp], Stroke::new(if on { 4.0 } else { 2.5 }, col));
                    painter.rect_filled(egui::Rect::from_center_size(tp, egui::vec2(8.0, 8.0)), 0.0, col);
                }
            }
            let on = active(Handle::Center);
            painter.rect_filled(
                egui::Rect::from_center_size(center, egui::vec2(10.0, 10.0)),
                0.0,
                brighten(Color32::from_gray(210), on),
            );
        }
        Tool::Rotate => {
            // The trackball (free-rotate) ring first, so axis rings draw on top.
            let on_c = active(Handle::Center);
            let cring: Vec<Pos2> = g.center_ring.iter().map(|v| pt(*v)).collect();
            if cring.len() >= 2 {
                painter.line(cring, Stroke::new(if on_c { 3.0 } else { 1.5 }, brighten(Color32::from_gray(170), on_c)));
            }
            for i in 0..3 {
                let on = active(handle_for_axis(i));
                let col = brighten(axis_col[i], on);
                let pts: Vec<Pos2> = g.ring_pts[i].iter().map(|v| pt(*v)).collect();
                if pts.len() >= 2 {
                    painter.line(pts, Stroke::new(if on { 3.5 } else { 2.0 }, col));
                }
            }
            painter.circle_filled(center, 3.0, Color32::from_gray(200));
        }
        Tool::Select | Tool::Sculpt => {}
    }
}

// ============================================================================
// Dockable panel system (egui_dock): Hierarchy / Inspector / Assets / Scene /
// Scripting. The Scene tab is transparent so the 3D viewport shows through it;
// all other tabs paint an opaque background over the full-window render.
// ============================================================================

/// Which dockable panel a tab shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EditorTab {
    Hierarchy,
    Inspector,
    Terrain,
    Assets,
    Console,
    Scene,
    Game,
    Scripting,
}

impl EditorTab {
    fn title(self) -> &'static str {
        match self {
            EditorTab::Hierarchy => "Hierarchy",
            EditorTab::Inspector => "Inspector",
            EditorTab::Terrain => "Δ Terrain",
            EditorTab::Assets => "Assets",
            EditorTab::Console => "Console",
            EditorTab::Scene => "⌖ Scene",
            EditorTab::Game => "⏵ Game",
            EditorTab::Scripting => "Scripting",
        }
    }
}

/// True when the Game tab is the front (active) tab of its dock leaf — i.e. the game
/// (active-camera) view should drive the full-window 3D render this frame. (When
/// false the editor free-fly camera renders, for the Scene tab.)
fn game_tab_active(dock: &egui_dock::DockState<EditorTab>) -> bool {
    dock.main_surface()
        .iter()
        .any(|n| n.get_leaf().and_then(|l| l.tabs.get(l.active.0)) == Some(&EditorTab::Game))
}

/// The default layout: Hierarchy left, Inspector right, Assets bottom, with the
/// Scene + Scripting tabs filling the center. Users can drag/re-dock freely.
fn default_dock() -> egui_dock::DockState<EditorTab> {
    use egui_dock::{DockState, NodeIndex};
    // Scene (editor view), Game (active-camera view), and Scripting share the central
    // leaf — only the front tab renders, and which of Scene/Game is front picks the
    // camera. Scene first so the editor view is the default on launch.
    let mut dock = DockState::new(vec![EditorTab::Scene, EditorTab::Game, EditorTab::Scripting]);
    let surface = dock.main_surface_mut();
    let [central, _] = surface.split_left(NodeIndex::root(), 0.18, vec![EditorTab::Hierarchy]);
    // Inspector + Terrain tabs share the right dock (Inspector shown first).
    let [central, _] =
        surface.split_right(central, 0.78, vec![EditorTab::Inspector, EditorTab::Terrain]);
    // Console sits as a tab beside Assets in the bottom dock (Assets shown first).
    let [_, _] = surface.split_below(central, 0.72, vec![EditorTab::Assets, EditorTab::Console]);
    dock
}

/// Focus the Scripting tab (used after double-click-to-open-a-script).
fn focus_scripting_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    let surface = dock.main_surface_mut();
    if let Some((node, tab)) = surface.find_tab(&EditorTab::Scripting) {
        let _ = surface.set_active_tab(node, tab);
    }
}

/// Focus the Terrain dock tab — re-adding it if the user closed it. Used when the
/// Sculpt tool is selected or "Open Terrain tools" is clicked.
fn focus_terrain_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    if let Some(path) = dock.find_tab(&EditorTab::Terrain) {
        let _ = dock.set_active_tab(path);
    } else {
        dock.push_to_focused_leaf(EditorTab::Terrain);
    }
}

/// Viewport framing presets for the in-Scene resolution simulator.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum AspectMode {
    #[default]
    Free,
    Desktop,
    Mobile,
    Square,
}

impl AspectMode {
    const ALL: [AspectMode; 4] =
        [AspectMode::Free, AspectMode::Desktop, AspectMode::Mobile, AspectMode::Square];
    fn label(self) -> &'static str {
        match self {
            AspectMode::Free => "Free",
            AspectMode::Desktop => "Desktop · 16:9",
            AspectMode::Mobile => "Mobile · 9:16",
            AspectMode::Square => "Square · 1:1",
        }
    }
    /// Width / height, or `None` for "fill the panel".
    fn ratio(self) -> Option<f32> {
        match self {
            AspectMode::Free => None,
            AspectMode::Desktop => Some(16.0 / 9.0),
            AspectMode::Mobile => Some(9.0 / 16.0),
            AspectMode::Square => Some(1.0),
        }
    }
}

/// A File-menu project action, applied after the frame.
#[derive(Clone)]
enum ProjectAction {
    New(String),
    Open(String),
    Close,
}

/// The built-in Scripting docs, shown on the IDE's Docs page.
const SCRIPT_DOCS: &str = "\
Floptle Scripting — Lua
=======================

Game logic is written in Lua (ADR-0003). A script is a `.lua` file in your
project's `scripts/` folder; attach it to a node and it runs every frame while
playing. A script defines plain functions:

    -- spin.lua
    defaults = { speed = 45 }          -- tunables (shown in the Inspector)

    function start(node)               -- once, when play begins (optional)
    end

    function update(node, dt)          -- every frame while playing
      node.yaw = node.yaw + math.rad(params.speed) * dt
    end

The node
--------
`node` is the node's transform, synced before the call and read back after — set
a field and the object moves:
  • node.x, node.y, node.z              position (world units)
  • node.scale                          uniform scale (shortcut)
  • node.scale_x / scale_y / scale_z    per-axis scale
  • node.yaw / pitch / roll             rotation, in radians

Globals
-------
  • params   this instance's values (a table; seeded from `defaults`)
  • time     seconds since play started
  • dt       seconds since last frame (also passed to update)
  • the full Lua standard library (math, string, table, …)
  • log(\"...\")  prints to the engine console

Each attached script keeps its own state across frames (set a variable in
start, read it in update), and hot-reloads the moment you save the file.

Attaching & running
--------------------
• Drag a `.lua` from Assets onto a node, drop it on the Inspector's Scripting
  section, or use Inspector ⏵ Scripting ⏵ + Add Script.
• Press F1 (⏵ Play) to run; F2 pauses the clock; ⏹ Stop restores the scene.
• The Inspector edits a script's params live; errors show at the top of this tab.

Defaults included with every project: rotate.lua, pulsate.lua, float.lua —
open one to see a working example.";

/// One script file open in the in-engine IDE.
struct OpenScript {
    path: String,
    name: String,
    text: String,
    dirty: bool,
}

/// One line in the engine Console. Consecutive identical lines are merged at ingest
/// (`count`), and `source` (script name + line) drives double-click-to-source.
struct ConsoleEntry {
    level: floptle_script::LogLevel,
    msg: String,
    source: Option<(String, u32)>,
    count: u32,
}

/// Console view state: which severities show, the search filter, and whether to
/// merge non-adjacent duplicates into one counted row.
struct ConsoleState {
    entries: Vec<ConsoleEntry>,
    show_debug: bool,
    show_warn: bool,
    show_error: bool,
    search: String,
    collapse: bool,
}

impl Default for ConsoleState {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            show_debug: true,
            show_warn: true,
            show_error: true,
            search: String::new(),
            collapse: true,
        }
    }
}

impl ConsoleState {
    /// Append a line, merging it into the previous row if identical (so a per-frame
    /// repeat becomes a count, not a flood). Caps retained history.
    fn push(&mut self, level: floptle_script::LogLevel, msg: String, source: Option<(String, u32)>) {
        if let Some(last) = self.entries.last_mut() {
            if last.level == level && last.msg == msg {
                last.count += 1;
                return;
            }
        }
        self.entries.push(ConsoleEntry { level, msg, source, count: 1 });
        const MAX: usize = 2000;
        if self.entries.len() > MAX {
            let drop = self.entries.len() - MAX;
            self.entries.drain(0..drop);
        }
    }
}

/// State of the Scripting-tab IDE: the open files and which one is shown
/// (`None` = the built-in Docs page).
struct IdeState {
    open: Vec<OpenScript>,
    active: Option<usize>,
    /// A pending "scroll to this 1-based line" request (Console jump-to-source),
    /// consumed by `scripting_ui` on the next frame it draws the editor.
    goto: Option<usize>,
}

impl Default for IdeState {
    fn default() -> Self {
        Self { open: Vec::new(), active: None, goto: None }
    }
}

impl IdeState {
    /// Open `path` in the IDE (or focus it if already open). Returns false on read error.
    fn open_file(&mut self, path: &str) -> bool {
        if let Some(i) = self.open.iter().position(|f| f.path == path) {
            self.active = Some(i);
            return true;
        }
        let Ok(text) = std::fs::read_to_string(path) else { return false };
        let name = std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        self.open.push(OpenScript { path: path.to_string(), name, text, dirty: false });
        self.active = Some(self.open.len() - 1);
        true
    }
}

/// Renders each dockable tab against borrowed slices of the editor's state, and
/// records UI intents on `cmd` to be applied after the frame.
struct EditorTabViewer<'a> {
    world: &'a mut World,
    selection: &'a mut Vec<Entity>,
    /// Folders collapsed in the Hierarchy (hide their children).
    collapsed: &'a mut std::collections::HashSet<Entity>,
    /// The engine Console (script logs / warnings / errors).
    console: &'a mut ConsoleState,
    /// The Inspector asset preview to draw (model/material render or texture image).
    preview: Option<PreviewView>,
    preview_zoom: &'a mut f32,
    preview_spin: &'a mut f32,
    preview_spinning: &'a mut bool,
    /// The material being previewed/edited when a material asset is selected.
    preview_material: &'a mut Option<(String, Material)>,
    entity_names: &'a [(Entity, String)],
    materials: &'a [(String, floptle_scene::MaterialDoc)],
    mat_name_buf: &'a mut String,
    /// Whether the floating Material Editor window is open.
    show_material_editor: &'a mut bool,
    asset_tree: &'a [AssetEntry],
    /// Per-texture sampling settings (read-only here; changes go via `cmd`).
    texture_settings: &'a HashMap<String, TexSetting>,
    /// The selected camera's live POV preview (if a camera is selected).
    cam_preview: Option<egui::TextureId>,
    /// Whether any camera holds play-mode authority (for the Game tab's warning).
    has_active_camera: bool,
    /// Terrain dock-tab state.
    terrain_brush: &'a mut TerrainBrush,
    terrain_detail: &'a mut u32,
    terrain_textures: &'a mut Vec<String>,
    terrain_present: bool,
    terrain_voxels: Option<(u32, u32, u32)>,
    /// Asset browser view mode (false = tree, true = grid) + the grid's folder.
    assets_grid: &'a mut bool,
    assets_grid_dir: &'a mut PathBuf,
    /// The project root — the directory the asset browser is rooted at.
    project_root: &'a Path,
    selected_asset: &'a mut Option<String>,
    ide: &'a mut IdeState,
    /// Errors from the last script frame (shown in the Scripting tab).
    script_errors: &'a [String],
    /// Syntax diagnostic for the active IDE file (line, message) — red squiggle.
    ide_diag: Option<&'a (usize, String)>,
    gizmo: Option<&'a GizmoFrame>,
    /// The terrain brush telegraph to draw over the viewport, if sculpting.
    terrain_viz: Option<&'a TerrainViz>,
    camera_gizmos: &'a [CameraGizmo],
    light_gizmos: &'a [Vec<(Vec2, Vec2)>],
    body_gizmos: &'a [Vec<(Vec2, Vec2)>],
    contact_gizmos: &'a [(Vec2, Vec2)],
    terrain_wire: &'a [(Vec2, Vec2)],
    grabbed: Option<Handle>,
    tool: Tool,
    scene_rect: &'a mut Option<egui::Rect>,
    aspect: &'a mut AspectMode,
    zoom: &'a mut f32,
    scene_name: &'a str,
    ppp: f32,
    cmd: &'a mut EditorCmd,
}

impl egui_dock::TabViewer for EditorTabViewer<'_> {
    type Tab = EditorTab;

    fn title(&mut self, tab: &mut EditorTab) -> egui::WidgetText {
        tab.title().into()
    }

    fn id(&mut self, tab: &mut EditorTab) -> egui::Id {
        egui::Id::new(("editor_tab", tab.title()))
    }

    // Core panels can't be closed (no way to bring them back yet).
    fn is_closeable(&self, _tab: &EditorTab) -> bool {
        false
    }

    // Keep every tab docked in the main surface: the 3D renders to the whole
    // window behind the Scene tab, so a torn-off floating Scene couldn't follow it.
    fn allowed_in_windows(&self, _tab: &mut EditorTab) -> bool {
        false
    }

    // The Scene + Game tabs are transparent so the 3D render shows through them.
    fn clear_background(&self, tab: &EditorTab) -> bool {
        !matches!(tab, EditorTab::Scene | EditorTab::Game)
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut EditorTab) {
        match tab {
            EditorTab::Hierarchy => self.hierarchy_ui(ui),
            EditorTab::Inspector => self.inspector_ui(ui),
            EditorTab::Terrain => self.terrain_ui(ui),
            EditorTab::Assets => self.assets_ui(ui),
            EditorTab::Console => self.console_ui(ui),
            // Scene = editor free-fly view (tools/gizmos); Game = active-camera view.
            EditorTab::Scene => self.scene_ui(ui, false),
            EditorTab::Game => self.scene_ui(ui, true),
            EditorTab::Scripting => self.scripting_ui(ui),
        }
    }
}

impl<'a> EditorTabViewer<'a> {
    fn hierarchy_ui(&mut self, ui: &mut egui::Ui) {
        // Scene name + save at the top of the hierarchy.
        ui.horizontal(|ui| {
            ui.strong(format!("⎙ {}", self.scene_name));
            if ui.small_button("Save").on_hover_text("Save scene (Ctrl+S)").clicked() {
                self.cmd.save_scene = true;
            }
            ui.label("?").on_hover_text(
                "Right-click here for New ▸ Cube / Sphere / Folder / Terrain / Camera …\n\
                 Tools: 1 select · 2 move · 3 rotate · 4 scale · 5 sculpt\n\
                 F focus · Q unselect · G grid · ⏶/⏷ step selection · Del delete\n\
                 F1 play · F2 pause · Ctrl+S save · Ctrl+Z/Y undo/redo\n\
                 Viewport: LMB select · Shift+LMB multi · RMB-drag look · RMB-click menu",
            );
            ui.menu_button("✚ New", |ui| self.node_new_menu(ui, None));
        });
        ui.separator();

        // Build the parent⏵children tree from the world (owned copies, so the
        // recursive render can freely borrow `self`).
        let names: HashMap<Entity, String> = self.entity_names.iter().cloned().collect();
        let order: Vec<Entity> = self.entity_names.iter().map(|(e, _)| *e).collect();
        let mut children: HashMap<Entity, Vec<Entity>> = HashMap::new();
        let mut roots: Vec<Entity> = Vec::new();
        for &e in &order {
            match self.world.get::<floptle_core::Parent>(e).copied() {
                Some(floptle_core::Parent(p)) if names.contains_key(&p) => {
                    children.entry(p).or_default().push(e)
                }
                _ => roots.push(e),
            }
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for r in roots {
                self.hierarchy_node(ui, r, &children, &names, 0);
            }
            // Empty area below the tree: drop a node here to unparent it; right-click
            // for the New menu (create at scene root).
            let bg = ui.allocate_response(ui.available_size(), egui::Sense::click());
            if let Some(p) = bg.dnd_release_payload::<NodePayload>() {
                self.cmd.reparent = Some((p.0, None));
            }
            bg.context_menu(|ui| {
                ui.menu_button("✚ New", |ui| self.node_new_menu(ui, None));
            });
        });
    }

    /// The shared "New node" menu — used by the Hierarchy header, the empty-area
    /// right-click (creates at scene root, `parent = None`), and each node's
    /// "Add child" submenu (`parent = Some(e)`).
    fn node_new_menu(&mut self, ui: &mut egui::Ui, parent: Option<Entity>) {
        let mut pick: Option<MatterDoc> = None;
        if ui.button("■ Cube").clicked() {
            pick = Some(new_cube());
            ui.close();
        }
        if ui.button("○ Sphere").clicked() {
            pick = Some(new_sphere());
            ui.close();
        }
        if ui.button("⬭ Capsule").on_hover_text("a capsule primitive (ideal for a physics character body)").clicked() {
            pick = Some(new_capsule());
            ui.close();
        }
        if ui.button("◑ Blob").clicked() {
            pick = Some(MatterDoc::Blob { scale: 1.0 });
            ui.close();
        }
        if ui.button("🗀 Folder").on_hover_text("an empty group to organize / parent nodes").clicked() {
            pick = Some(MatterDoc::Empty);
            ui.close();
        }
        ui.separator();
        if ui.button("Δ Terrain").on_hover_text("a sculptable SDF terrain node").clicked() {
            self.cmd.create_terrain = true;
            ui.close();
        }
        if ui.button("⌖ Camera").on_hover_text("a viewpoint you can give play-mode authority").clicked() {
            self.cmd.add_camera = Some(parent);
            ui.close();
        }
        if ui.button("● Point Light").on_hover_text("a placeable omni light (color / intensity / range)").clicked() {
            pick = Some(MatterDoc::PointLight { color: [1.0, 0.95, 0.85], intensity: 1.0, range: 10.0 });
            ui.close();
        }
        if ui.button("⬇ Gravity Volume").on_hover_text("physics gravity: Down (level) or Radial (planet)").clicked() {
            pick = Some(MatterDoc::GravityVolume { radial: false, strength: 9.81, radius: 20.0 });
            ui.close();
        }
        if let Some(m) = pick {
            match parent {
                Some(p) => self.cmd.add_parented = Some((m, p)),
                None => self.cmd.add = Some(m),
            }
        }
    }

    /// Render one hierarchy row (indented by `depth`) + its children. The row is a
    /// drag source (drop it on another row to re-parent) and a drop target (for a
    /// dragged node or a script).
    fn hierarchy_node(
        &mut self,
        ui: &mut egui::Ui,
        e: Entity,
        children: &HashMap<Entity, Vec<Entity>>,
        names: &HashMap<Entity, String>,
        depth: usize,
    ) {
        let name = names.get(&e).cloned().unwrap_or_default();
        let matter = self.world.get::<Matter>(e);
        let is_folder = matches!(matter, Some(Matter::Empty));
        let has_kids = children.get(&e).map(|c| !c.is_empty()).unwrap_or(false);
        let collapsed = self.collapsed.contains(&e);
        let icon = if is_folder {
            "🗀"
        } else if matches!(matter, Some(Matter::Camera { .. })) {
            "⌖"
        } else if matches!(matter, Some(Matter::Terrain { .. })) {
            "Δ"
        } else if matches!(matter, Some(Matter::PointLight { .. })) {
            "●"
        } else if matches!(matter, Some(Matter::GravityVolume { .. })) {
            "⬇"
        } else if has_kids {
            "⏷"
        } else {
            "•"
        };
        let selected = self.selection.contains(&e);

        // A folder with children gets a clickable disclosure triangle.
        let mut toggle = false;
        let resp = ui
            .horizontal(|ui| {
                ui.add_space(depth as f32 * 14.0);
                if is_folder && has_kids {
                    let tri = if collapsed { "⏵" } else { "⏷" };
                    let t = ui.add(
                        egui::Label::new(tri).selectable(false).sense(egui::Sense::click()),
                    );
                    if t.clicked() {
                        toggle = true;
                    }
                } else {
                    ui.add_space(12.0);
                }
                let text = if selected {
                    egui::RichText::new(format!("{icon} {name}")).strong().color(ui.visuals().selection.stroke.color)
                } else {
                    egui::RichText::new(format!("{icon} {name}"))
                };
                ui.add(egui::Label::new(text).selectable(false).sense(egui::Sense::click_and_drag()))
            })
            .inner;
        if toggle {
            if collapsed {
                self.collapsed.remove(&e);
            } else {
                self.collapsed.insert(e);
            }
        }
        resp.dnd_set_drag_payload(NodePayload(e));

        // Highlight when a node/script is dragged over this row.
        if resp.dnd_hover_payload::<NodePayload>().is_some()
            || resp.dnd_hover_payload::<AssetPayload>().is_some_and(|p| is_script(&p.path))
        {
            ui.painter().rect_stroke(
                resp.rect,
                3.0,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 230, 140)),
                egui::StrokeKind::Inside,
            );
        }

        if resp.clicked() {
            *self.selected_asset = None;
            if ui.input(|i| i.modifiers.shift) {
                if let Some(pos) = self.selection.iter().position(|x| *x == e) {
                    self.selection.remove(pos);
                } else {
                    self.selection.push(e);
                }
            } else {
                self.selection.clear();
                self.selection.push(e);
            }
        }
        if resp.secondary_clicked() && !selected {
            self.selection.clear();
            self.selection.push(e);
        }
        resp.context_menu(|ui| {
            ui.menu_button("✚ Add child", |ui| self.node_new_menu(ui, Some(e)));
            if self.world.get::<floptle_core::Parent>(e).is_some() && ui.button("⮪ Unparent").clicked() {
                self.cmd.reparent = Some((e, None));
                ui.close();
            }
            ui.separator();
            if ui.button("Duplicate").clicked() {
                self.cmd.duplicate = true;
                ui.close();
            }
            if ui.button("Copy").clicked() {
                self.cmd.copy = true;
                ui.close();
            }
            if ui.button("Delete").clicked() {
                self.cmd.delete = true;
                ui.close();
            }
        });
        // Drops: a node re-parents under me; a script attaches to me.
        if let Some(p) = resp.dnd_release_payload::<NodePayload>() {
            if p.0 != e {
                self.cmd.reparent = Some((p.0, Some(e)));
            }
        }
        if let Some(p) = resp.dnd_release_payload::<AssetPayload>() {
            if is_script(&p.path) {
                self.cmd.drop_script_on = Some((p.path.clone(), e));
            }
        }

        // Recurse into children unless this folder is collapsed.
        if !self.collapsed.contains(&e) {
            if let Some(kids) = children.get(&e) {
                for &c in kids {
                    self.hierarchy_node(ui, c, children, names, depth + 1);
                }
            }
        }
    }

    fn inspector_ui(&mut self, ui: &mut egui::Ui) {
        // The Inspector shows *only* the current selection (the scene name + save
        // live in the Hierarchy header). An asset selected in the browser shows here.
        if let Some(path) = self.selected_asset.clone() {
            ui.strong("Asset");
            let name_resp = ui.selectable_label(false, &path);
            if is_model(&path) {
                ui.label("glTF model — drag onto the scene to place it.");
                self.asset_preview_ui(ui);
            } else if is_material(&path) {
                ui.label("material preset");
                self.asset_preview_ui(ui);
                self.material_asset_ui(ui, &path);
            } else if is_texture(&path) {
                self.asset_preview_ui(ui);
                self.texture_settings_ui(ui, &path);
            } else if is_script(&path) {
                ui.label("script — drag onto a node, double-click, or:");
                if ui.button("🖊  Open in Scripting").clicked() {
                    self.cmd.open_script = Some(path.clone());
                    self.cmd.focus_scripting = true;
                }
                if name_resp.double_clicked() {
                    self.cmd.open_script_pref = Some(path.clone());
                }
            }
            ui.separator();
        }

        let primary = self.selection.last().copied();
        if self.selection.len() > 1 {
            ui.small(format!("{} selected", self.selection.len()));
        }
        let cmd = &mut *self.cmd;
        let world = &mut *self.world;
        match primary {
            Some(e) if world.get::<Light>(e).is_some() => {
                if let Some(l) = world.get_mut::<Light>(e) {
                    ui.label("Lighting node");
                    ui.label("direction");
                    ui.horizontal(|ui| {
                        cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut l.direction[0]).speed(0.02).prefix("x ")).changed();
                        cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut l.direction[1]).speed(0.02).prefix("y ")).changed();
                        cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut l.direction[2]).speed(0.02).prefix("z ")).changed();
                    });
                    ui.horizontal(|ui| {
                        ui.label("light");
                        cmd.inspector_changed |= ui.color_edit_button_rgb(&mut l.color).changed();
                        ui.label("ambient");
                        cmd.inspector_changed |= ui.color_edit_button_rgb(&mut l.ambient).changed();
                    });
                    cmd.inspector_changed |=
                        ui.add(egui::Slider::new(&mut l.intensity, 0.0..=8.0).text("intensity")).changed();
                }
            }
            Some(e) if world.get::<Transform>(e).is_some() => {
                if let Some(n) = world.get_mut::<Name>(e) {
                    ui.horizontal(|ui| {
                        ui.label("name");
                        cmd.inspector_changed |= ui.text_edit_singleline(&mut n.0).changed();
                    });
                }
                if let Some(m) = world.get_mut::<Matter>(e) {
                    match m {
                        Matter::Primitive { shape, color } => {
                            ui.horizontal(|ui| {
                                ui.label("shape");
                                egui::ComboBox::from_id_salt("shape")
                                    .selected_text(format!("{shape:?}"))
                                    .show_ui(ui, |ui| {
                                        cmd.inspector_changed |= ui.selectable_value(shape, Shape::Cube, "Cube").clicked();
                                        cmd.inspector_changed |= ui.selectable_value(shape, Shape::Sphere, "Sphere").clicked();
                                        cmd.inspector_changed |= ui.selectable_value(shape, Shape::Capsule, "Capsule").clicked();
                                    });
                            });
                            ui.horizontal(|ui| {
                                ui.label("color");
                                cmd.inspector_changed |= ui.color_edit_button_rgb(color).changed();
                                ui.small("(base color — add a Material below for emissive, specular, …)");
                            });
                        }
                        Matter::Blob { scale } => {
                            cmd.inspector_changed |= ui
                                .add(egui::DragValue::new(scale).speed(0.02).prefix("blob size ").range(0.05..=50.0))
                                .changed();
                        }
                        Matter::Mesh { asset_path } => {
                            ui.label("imported mesh");
                            ui.small(asset_path.as_str());
                            if ui
                                .button("⏏ Extract textures")
                                .on_hover_text("Save this model's embedded textures to assets/textures/ so you can build materials from them")
                                .clicked()
                            {
                                cmd.extract_textures = Some(asset_path.clone());
                            }
                        }
                        Matter::Empty => {
                            ui.label("group / empty");
                            ui.small("a folder — organizes child nodes; has a transform but no geometry");
                        }
                        Matter::Terrain { .. } => {
                            ui.label("editable terrain");
                            ui.small("a sculptable SDF field — move it with the transform below");
                            if ui.button("Δ Open Terrain tools").clicked() {
                                cmd.focus_terrain = true;
                            }
                        }
                        Matter::Camera { fov_y, active } => {
                            ui.label("camera");
                            ui.small("a viewpoint — play mode renders from the active camera");
                            // Live preview of what this camera sees.
                            if let Some(tex) = self.cam_preview {
                                let w = ui.available_width().min(300.0);
                                let size = egui::vec2(w, w * 9.0 / 16.0);
                                ui.add(egui::Image::new((tex, size)).corner_radius(4.0));
                                ui.small("preview — what this camera sees");
                            }
                            ui.horizontal(|ui| {
                                ui.label("field of view");
                                let mut deg = fov_y.to_degrees();
                                if ui.add(egui::Slider::new(&mut deg, 20.0..=120.0).suffix("°")).changed() {
                                    *fov_y = deg.to_radians();
                                    cmd.inspector_changed = true;
                                }
                            });
                            if *active {
                                ui.colored_label(egui::Color32::from_rgb(120, 200, 140), "⌖ active camera");
                            } else if ui.button("⌖ Make active camera").clicked() {
                                cmd.set_active_camera = Some(e);
                            }
                            if ui.button("⎙ Snap to this view").on_hover_text("move the camera to the current editor viewpoint").clicked() {
                                cmd.camera_from_view = Some(e);
                            }
                        }
                        Matter::PointLight { color, intensity, range } => {
                            ui.label("point light");
                            ui.small("an omni light — position comes from the transform below");
                            ui.horizontal(|ui| {
                                ui.label("color");
                                cmd.inspector_changed |= ui.color_edit_button_rgb(color).changed();
                            });
                            cmd.inspector_changed |=
                                ui.add(egui::Slider::new(intensity, 0.0..=20.0).text("intensity")).changed();
                            cmd.inspector_changed |=
                                ui.add(egui::Slider::new(range, 0.1..=200.0).text("range")).changed();
                        }
                        Matter::GravityVolume { mode, strength, radius } => {
                            use floptle_core::GravityMode;
                            ui.label("gravity volume");
                            ui.small("level physics gravity — Down (normal) or Radial (planet)");
                            ui.horizontal(|ui| {
                                let mut radial = *mode == GravityMode::Radial;
                                if ui.selectable_label(!radial, "⬇ Down").clicked() {
                                    radial = false;
                                }
                                if ui.selectable_label(radial, "◎ Radial (planet)").clicked() {
                                    radial = true;
                                }
                                let new =
                                    if radial { GravityMode::Radial } else { GravityMode::Down };
                                if new != *mode {
                                    *mode = new;
                                    cmd.inspector_changed = true;
                                }
                            });
                            cmd.inspector_changed |=
                                ui.add(egui::Slider::new(strength, 0.0..=60.0).text("strength")).changed();
                            if *mode == GravityMode::Radial {
                                cmd.inspector_changed |= ui
                                    .add(egui::Slider::new(radius, 0.5..=500.0).text("well radius"))
                                    .changed();
                            }
                        }
                    }
                }

                // ---- Material (surface look) ----
                ui.separator();
                let has_mat = world.get::<Material>(e).is_some();
                let mut tex_list = Vec::new();
                collect_texture_paths(self.asset_tree, &mut tex_list);
                egui::CollapsingHeader::new("◑ Material").default_open(has_mat).show(ui, |ui| {
                    if let Some(mat) = world.get_mut::<Material>(e) {
                        let res = material_props_ui(ui, mat, self.materials, &tex_list, self.mat_name_buf);
                        cmd.inspector_changed |= res.changed;
                        if res.remove {
                            cmd.remove_material = Some(e);
                        }
                        if let Some(name) = res.save_as {
                            cmd.save_material =
                                Some((name, floptle_scene::MaterialDoc::from_material(mat)));
                        }
                        if ui.button("⛶ Open in Material Editor").clicked() {
                            *self.show_material_editor = true;
                        }
                    } else {
                        ui.small("Default look. Add a material to customize emissive, specular, rim, unlit shading…");
                        ui.horizontal(|ui| {
                            if ui.button("✚ Add material").clicked() {
                                cmd.add_material = Some(e);
                            }
                            if !self.materials.is_empty() {
                                ui.menu_button("Apply preset", |ui| {
                                    for (name, _) in self.materials {
                                        if ui.button(name).clicked() {
                                            cmd.apply_preset = Some((e, name.clone()));
                                            ui.close();
                                        }
                                    }
                                });
                            }
                        });
                    }
                });

                // ---- Physics (Rigidbody) ----
                let has_rb = world.get::<floptle_core::RigidBody>(e).is_some();
                egui::CollapsingHeader::new("◆ Rigidbody").default_open(has_rb).show(ui, |ui| {
                    if let Some(rb) = world.get_mut::<floptle_core::RigidBody>(e) {
                        use floptle_core::BodyKind;
                        ui.horizontal(|ui| {
                            ui.label("shape");
                            egui::ComboBox::from_id_salt("rb-shape")
                                .selected_text(match rb.kind {
                                    BodyKind::Sphere => "Sphere",
                                    BodyKind::Capsule => "Capsule",
                                })
                                .show_ui(ui, |ui| {
                                    cmd.inspector_changed |=
                                        ui.selectable_value(&mut rb.kind, BodyKind::Sphere, "Sphere").changed();
                                    cmd.inspector_changed |=
                                        ui.selectable_value(&mut rb.kind, BodyKind::Capsule, "Capsule").changed();
                                });
                        });
                        cmd.inspector_changed |=
                            ui.add(egui::Slider::new(&mut rb.radius, 0.05..=10.0).text("radius")).changed();
                        if rb.kind == BodyKind::Capsule {
                            cmd.inspector_changed |=
                                ui.add(egui::Slider::new(&mut rb.height, 0.2..=20.0).text("height")).changed();
                        }
                        cmd.inspector_changed |=
                            ui.add(egui::Slider::new(&mut rb.restitution, 0.0..=1.0).text("bounce")).changed();
                        cmd.inspector_changed |=
                            ui.add(egui::Slider::new(&mut rb.friction, 0.0..=1.0).text("friction")).changed();
                        ui.horizontal(|ui| {
                            ui.label("freeze pos");
                            for (i, ax) in ["x", "y", "z"].iter().enumerate() {
                                cmd.inspector_changed |= ui.toggle_value(&mut rb.lock_pos[i], *ax).changed();
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("freeze rot");
                            for (i, ax) in ["x", "y", "z"].iter().enumerate() {
                                cmd.inspector_changed |= ui.toggle_value(&mut rb.lock_rot[i], *ax).changed();
                            }
                        });
                        if ui.button("🗑 Remove rigidbody").clicked() {
                            cmd.remove_rigidbody = Some(e);
                        }
                    } else {
                        ui.small("A dynamic physics body — falls under gravity and collides with the terrain on Play.");
                        if ui.button("✚ Add rigidbody").clicked() {
                            cmd.add_rigidbody = Some(e);
                        }
                    }
                });

                // ---- Static mesh collider (walkable map) — Mesh nodes only ----
                if matches!(world.get::<Matter>(e), Some(Matter::Mesh { .. })) {
                    let mut on = world.get::<floptle_core::MeshCollider>(e).is_some();
                    if ui
                        .checkbox(&mut on, "▦ Mesh collider (walkable)")
                        .on_hover_text("collide against this model's triangles on Play, so a character can walk on it")
                        .changed()
                    {
                        cmd.set_mesh_collider = Some((e, on));
                        cmd.inspector_changed = true;
                    }
                }

                if let Some(t) = world.get_mut::<Transform>(e) {
                    ui.label("translation");
                    ui.horizontal(|ui| {
                        cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.translation.x).speed(0.05).prefix("x ")).changed();
                        cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.translation.y).speed(0.05).prefix("y ")).changed();
                        cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.translation.z).speed(0.05).prefix("z ")).changed();
                    });
                    ui.label("rotation (deg)");
                    let (ey, ex, ez) = t.rotation.to_euler(EulerRot::YXZ);
                    let mut deg = [ey.to_degrees(), ex.to_degrees(), ez.to_degrees()];
                    let mut rot_changed = false;
                    ui.horizontal(|ui| {
                        rot_changed |= ui.add(egui::DragValue::new(&mut deg[0]).speed(1.0).prefix("y ")).changed();
                        rot_changed |= ui.add(egui::DragValue::new(&mut deg[1]).speed(1.0).prefix("x ")).changed();
                        rot_changed |= ui.add(egui::DragValue::new(&mut deg[2]).speed(1.0).prefix("z ")).changed();
                    });
                    if rot_changed {
                        t.rotation = Quat::from_euler(
                            EulerRot::YXZ,
                            deg[0].to_radians(),
                            deg[1].to_radians(),
                            deg[2].to_radians(),
                        );
                        cmd.inspector_changed = true;
                    }
                    ui.label("scale");
                    ui.horizontal(|ui| {
                        cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.scale.x).speed(0.02).prefix("x ")).changed();
                        cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.scale.y).speed(0.02).prefix("y ")).changed();
                        cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.scale.z).speed(0.02).prefix("z ")).changed();
                    });
                }
                // ---- Scripting ----
                ui.separator();
                egui::CollapsingHeader::new("Scripting")
                    .default_open(true)
                    .show(ui, |ui| {
                        // A clear drop target: drag a script here to attach it.
                        let (_, dropped) = ui.dnd_drop_zone::<AssetPayload, ()>(
                            egui::Frame::group(ui.style()),
                            |ui| {
                                ui.set_min_height(20.0);
                                ui.small("⏷  drop a script here to attach");
                            },
                        );
                        if let Some(p) = dropped {
                            if is_script(&p.path) {
                                cmd.drop_script_on = Some((p.path.clone(), e));
                            }
                        }
                        let mut remove: Option<usize> = None;
                        if let Some(scr) = world.get_mut::<Scripts>(e) {
                            for (i, inst) in scr.0.iter_mut().enumerate() {
                                ui.horizontal(|ui| {
                                    cmd.inspector_changed |= ui.checkbox(&mut inst.enabled, "").changed();
                                    ui.strong(&inst.kind);
                                    if ui.small_button("🖊").on_hover_text("edit script").clicked() {
                                        let p = self
                                            .project_root
                                            .join("scripts")
                                            .join(format!("{}.lua", inst.kind));
                                        cmd.open_script_pref = Some(p.to_string_lossy().to_string());
                                    }
                                    if ui.small_button("×").on_hover_text("remove").clicked() {
                                        remove = Some(i);
                                    }
                                });
                                for (k, v) in inst.params.iter_mut() {
                                    cmd.inspector_changed |= ui
                                        .add(egui::DragValue::new(v).speed(0.05).prefix(format!("{k}  ")))
                                        .changed();
                                }
                                ui.add_space(4.0);
                            }
                            if let Some(i) = remove {
                                scr.0.remove(i);
                                cmd.inspector_changed = true;
                            }
                        } else {
                            ui.small("(no scripts — add one or drag from Assets)");
                        }
                        ui.menu_button("+ Add Script", |ui| {
                            let mut names = Vec::new();
                            collect_script_names(self.asset_tree, &mut names);
                            if names.is_empty() {
                                ui.small("no .lua scripts yet — make one in Assets");
                            }
                            for n in names {
                                if ui.button(&n).clicked() {
                                    // Routed through a command so params can be seeded
                                    // from the script's `defaults` (needs the Lua host).
                                    cmd.attach_named = Some((n, e));
                                    ui.close();
                                }
                            }
                        });
                    });
            }
            Some(_) => {
                ui.label("(no editable properties)");
            }
            None => {
                if self.selected_asset.is_none() {
                    ui.weak("Nothing selected. Click an object, or a node in the Hierarchy.");
                }
            }
        }

        // ---- floating Material Editor window (edits the primary selection) ----
        if *self.show_material_editor {
            let mut open = true;
            egui::Window::new("◑ Material Editor")
                .open(&mut open)
                .default_width(300.0)
                .show(ui.ctx(), |ui| match self.selection.last().copied() {
                    Some(e) if world.get::<Matter>(e).is_some() => {
                        let nm = self
                            .entity_names
                            .iter()
                            .find(|(x, _)| *x == e)
                            .map(|(_, n)| n.clone())
                            .unwrap_or_default();
                        ui.label(format!("editing: {nm}"));
                        ui.separator();
                        let mut tex_list = Vec::new();
                        collect_texture_paths(self.asset_tree, &mut tex_list);
                        if let Some(mat) = world.get_mut::<Material>(e) {
                            let res = material_props_ui(ui, mat, self.materials, &tex_list, self.mat_name_buf);
                            cmd.inspector_changed |= res.changed;
                            if res.remove {
                                cmd.remove_material = Some(e);
                            }
                            if let Some(name) = res.save_as {
                                cmd.save_material =
                                    Some((name, floptle_scene::MaterialDoc::from_material(mat)));
                            }
                        } else {
                            ui.label("This object uses the default look.");
                            if ui.button("✚ Add material").clicked() {
                                cmd.add_material = Some(e);
                            }
                        }
                    }
                    _ => {
                        ui.label("Select an object to edit its material.");
                    }
                });
            if !open {
                *self.show_material_editor = false;
            }
        }
    }

    /// The Terrain dock tab: detail, sculpt brush, and texture palette controls.
    /// (Rebinds fields to locals so each egui closure captures disjoint state.)
    fn terrain_ui(&mut self, ui: &mut egui::Ui) {
        use floptle_field::Brush;
        let cmd = &mut *self.cmd;
        let terrain_brush = &mut *self.terrain_brush;
        let terrain_detail = &mut *self.terrain_detail;
        let terrain_textures = &mut *self.terrain_textures;
        let materials = self.materials;
        let asset_tree = self.asset_tree;
        let terrain_present = self.terrain_present;
        let terrain_voxels = self.terrain_voxels;

        // Detail (resolution) — higher = finer terrain, but heavier.
        ui.horizontal(|ui| {
            ui.label("detail");
            egui::ComboBox::from_id_salt("terrain_detail")
                .selected_text(match *terrain_detail {
                    d if d <= 48 => "Low",
                    d if d <= 80 => "Medium",
                    d if d <= 112 => "High",
                    _ => "Ultra",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut *terrain_detail, 40, "Low");
                    ui.selectable_value(&mut *terrain_detail, 64, "Medium");
                    ui.selectable_value(&mut *terrain_detail, 96, "High");
                    ui.selectable_value(&mut *terrain_detail, 144, "Ultra");
                });
        });
        if let Some((a, b, c)) = terrain_voxels {
            ui.small(format!("combined: {a}×{b}×{c} voxels"));
        }
        // New terrains can be added any time — each is a node you place + blend.
        if ui.button("✚ New terrain").on_hover_text("adds another terrain node at the cursor; overlapping terrains blend").clicked() {
            cmd.create_terrain = true;
        }
        if !terrain_present {
            ui.small("Adds a flat slab; then press 5 (Sculpt) and LMB-drag. Add more — they fuse where they overlap.");
            return;
        }
        ui.separator();
        ui.label("Sculpt tool (key 5) — LMB-drag brushes the terrain under the");
        ui.label("cursor. Sculpt past an edge to grow it (infinite bounds).");
        ui.label("Ctrl+Z/Y undo strokes. Move a terrain with the gizmo to blend.");
        ui.label("Brush");
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut terrain_brush.mode, Brush::Raise, "⏶ Raise");
            ui.selectable_value(&mut terrain_brush.mode, Brush::Lower, "⏷ Lower");
            ui.selectable_value(&mut terrain_brush.mode, Brush::Flatten, "⊟ Flatten");
            ui.selectable_value(&mut terrain_brush.mode, Brush::Smooth, "≈ Smooth");
            ui.selectable_value(&mut terrain_brush.mode, Brush::Paint, "◑ Paint");
        });
        ui.add(egui::Slider::new(&mut terrain_brush.radius, 0.5..=8.0).text("radius"));
        ui.add(egui::Slider::new(&mut terrain_brush.strength, 0.05..=1.0).text("strength"));
        if terrain_brush.mode == Brush::Paint {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("paint:");
                ui.selectable_value(&mut terrain_brush.tex_slot, -1, "Color");
            });
            // Fill the whole terrain with the current paint target.
            if terrain_brush.tex_slot < 0 {
                if ui.button("▣ Fill terrain with this color").on_hover_text("fills the active terrain (or selected terrain node)").clicked() {
                    cmd.fill_terrain = Some(TerrainFill::Color(terrain_brush.color));
                }
            } else if ui.button("▣ Fill terrain with this texture").clicked() {
                cmd.fill_terrain = Some(TerrainFill::Texture(terrain_brush.tex_slot as u8 + 1));
            }
            if terrain_brush.tex_slot < 0 {
                ui.horizontal(|ui| {
                    ui.label("color");
                    ui.color_edit_button_rgb(&mut terrain_brush.color);
                    if !materials.is_empty() {
                        ui.menu_button("from material", |ui| {
                            for (name, doc) in materials {
                                if ui.button(name).clicked() {
                                    terrain_brush.color = doc.color;
                                    ui.close();
                                }
                            }
                        });
                    }
                });
            }
            // Texture palette: assign an image per slot, then click a slot to paint
            // that texture (triplanar) onto the terrain.
            ui.label("Texture palette");
            let mut tex_list = Vec::new();
            collect_texture_paths(asset_tree, &mut tex_list);
            for slot in 0..terrain_textures.len() {
                ui.horizontal(|ui| {
                    let sel = terrain_brush.tex_slot == slot as i32;
                    let label = if terrain_textures[slot].is_empty() {
                        format!("slot {}", slot + 1)
                    } else {
                        Path::new(&terrain_textures[slot])
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default()
                    };
                    if ui.selectable_label(sel, format!("🖊 {label}")).clicked() {
                        terrain_brush.tex_slot = slot as i32;
                    }
                    egui::ComboBox::from_id_salt(("tslot", slot))
                        .selected_text("set…")
                        .width(70.0)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(false, "(none)").clicked() {
                                terrain_textures[slot].clear();
                                cmd.terrain_palette_changed = true;
                            }
                            for p in &tex_list {
                                let n = Path::new(p)
                                    .file_name()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                if ui.selectable_label(terrain_textures[slot] == *p, n).clicked() {
                                    terrain_textures[slot] = p.clone();
                                    cmd.terrain_palette_changed = true;
                                }
                            }
                        });
                });
            }
            ui.small("Extract a model's textures (Inspector) or add PNGs to textures/, assign them to slots, then paint. Color tints the texture.");
        }
        ui.separator();
        // Fill-bounds tool. Sculpting near an edge now grows only the BOUNDS (the
        // surface no longer auto-extends into flat land), so this is the deliberate way
        // to lay flat ground: pour solid up to `height`, from `floor` below, kept
        // `inset` in from the walls.
        egui::CollapsingHeader::new("▦ Fill bounds (flat ground)").default_open(false).show(ui, |ui| {
            ui.add(egui::Slider::new(&mut terrain_brush.fill_top, -20.0..=20.0).text("fill height (top)"));
            ui.add(egui::Slider::new(&mut terrain_brush.fill_floor, -40.0..=20.0).text("floor (bottom)"));
            ui.add(egui::Slider::new(&mut terrain_brush.fill_inset, 0.0..=20.0).text("edge inset"));
            if ui.button("▦ Fill bounds with flat ground")
                .on_hover_text("union solid ground into the active terrain up to the height (uses the brush color)")
                .clicked()
            {
                cmd.fill_bounds = true;
            }
        });
        ui.separator();
        if ui.button("🗑 Clear all terrain").on_hover_text("delete every terrain node (or select one + Delete)").clicked() {
            cmd.clear_terrain = true;
        }
    }

    fn assets_ui(&mut self, ui: &mut egui::Ui) {
        let root = self.project_root.to_path_buf();
        ui.horizontal(|ui| {
            ui.strong("Assets");
            if ui.small_button("⟳").on_hover_text("rescan").clicked() {
                self.cmd.refresh_assets = true;
            }
            ui.menu_button("✚ New", |ui| {
                self.new_asset_menu(ui, &root);
            });
            ui.separator();
            // Tree / Grid view toggle.
            if ui.selectable_label(!*self.assets_grid, "☰").on_hover_text("file tree").clicked() {
                *self.assets_grid = false;
            }
            if ui.selectable_label(*self.assets_grid, "⊞").on_hover_text("icon grid").clicked() {
                *self.assets_grid = true;
            }
            ui.separator();
            ui.small("right-click for New · double-click a script/folder to open · drag onto the scene");
        });
        ui.separator();
        if *self.assets_grid {
            self.assets_grid_ui(ui, &root);
            return;
        }
        let tree = self.asset_tree; // Copy the slice ref so the recursion can &mut self.
        let resp = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.asset_node_ui(ui, tree, &root);
                // Catch right-clicks on the empty space below the list so New
                // Folder / New Script is reachable even when the tree is short.
                ui.allocate_response(ui.available_size(), egui::Sense::click())
            })
            .inner;
        resp.context_menu(|ui| {
            self.new_asset_menu(ui, &root);
        });
    }

    /// Find the asset entries inside `dir` (absolute, under the project root) by
    /// walking the cached tree. The returned slice borrows the tree (lifetime `'a`),
    /// not `self`, so the caller can still `&mut self` while iterating it.
    fn grid_entries(&self, dir: &Path) -> Option<&'a [AssetEntry]> {
        let rel = dir.strip_prefix(self.project_root).ok()?;
        let mut cur: &'a [AssetEntry] = self.asset_tree;
        for comp in rel.components() {
            let name = comp.as_os_str().to_string_lossy();
            cur = cur.iter().find_map(|e| match e {
                AssetEntry::Dir(n, kids) if n.as_str() == name => Some(kids.as_slice()),
                _ => None,
            })?;
        }
        Some(cur)
    }

    /// The icon-grid asset browser: a wrapped flow of tiles for the current folder.
    /// Folders descend on double-click; files select / open / drag like the tree.
    fn assets_grid_ui(&mut self, ui: &mut egui::Ui, root: &Path) {
        // Keep the grid folder valid (e.g. after switching projects).
        if !self.assets_grid_dir.starts_with(root) {
            *self.assets_grid_dir = root.to_path_buf();
        }
        let dir = self.assets_grid_dir.clone();

        // Breadcrumb row: up button + relative path.
        ui.horizontal(|ui| {
            let at_root = dir == root;
            if ui.add_enabled(!at_root, egui::Button::new("⏶")).on_hover_text("up").clicked() {
                if let Some(p) = dir.parent() {
                    *self.assets_grid_dir = p.to_path_buf();
                }
            }
            let rel = dir.strip_prefix(root).ok().map(|p| p.to_string_lossy().to_string());
            let crumb = match rel.as_deref() {
                Some("") | None => "assets".to_string(),
                Some(r) => format!("assets/{r}"),
            };
            ui.weak(crumb);
        });
        ui.separator();

        let Some(entries) = self.grid_entries(&dir) else {
            ui.weak("(empty)");
            return;
        };
        let mut enter: Option<PathBuf> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for entry in entries {
                    match entry {
                        AssetEntry::Dir(name, _) => {
                            if self.asset_tile(ui, "🗀", egui::Color32::from_rgb(225, 200, 130), name.as_str(), None) {
                                enter = Some(dir.join(name));
                            }
                        }
                        AssetEntry::File { name, path } => {
                            let (icon, color) = asset_kind_icon(path.as_str());
                            self.asset_file_tile(ui, icon, color, name.as_str(), path.as_str());
                        }
                    }
                }
            });
            // Right-click empty space ⏵ New menu.
            let bg = ui.allocate_response(ui.available_size(), egui::Sense::click());
            bg.context_menu(|ui| self.new_asset_menu(ui, &dir));
        });
        if let Some(d) = enter {
            *self.assets_grid_dir = d;
        }
    }

    /// A bare clickable tile (icon + name). Returns true on double-click (used for
    /// folders ⏵ descend). 84-pt wide so several fit per row.
    fn asset_tile(
        &mut self,
        ui: &mut egui::Ui,
        icon: &str,
        color: egui::Color32,
        name: &str,
        _path: Option<&str>,
    ) -> bool {
        let resp = self.tile_frame(ui, icon, color, name, false);
        resp.double_clicked()
    }

    /// A file tile: select on click, open on double-click (scripts/markdown), drag a
    /// payload (models/scripts), and the shared context menu.
    fn asset_file_tile(&mut self, ui: &mut egui::Ui, icon: &str, color: egui::Color32, name: &str, path: &str) {
        let selected = self.selected_asset.as_deref() == Some(path);
        let draggable = is_model(path) || is_script(path);
        let resp = self.tile_frame(ui, icon, color, name, selected);
        if draggable {
            resp.dnd_set_drag_payload(AssetPayload { path: path.to_string() });
        }
        if resp.clicked() {
            *self.selected_asset = Some(path.to_string());
        }
        let openable = is_script(path) || is_markdown(path);
        if resp.double_clicked() {
            if is_scene(path) {
                self.cmd.open_scene = Some(path.to_string());
            } else if openable {
                self.cmd.open_script_pref = Some(path.to_string());
            }
        }
        let dir = Path::new(path).parent().map(|p| p.to_path_buf());
        resp.context_menu(|ui| {
            if openable && ui.button("🖊 Open in Scripting tab").clicked() {
                self.cmd.open_script = Some(path.to_string());
                self.cmd.focus_scripting = true;
                ui.close();
            }
            if ui.button("🗀 Open in file explorer").clicked() {
                reveal_in_explorer(Path::new(path));
                ui.close();
            }
            if ui.button("🖊 Rename…").clicked() {
                self.cmd.rename_asset = Some(path.to_string());
                ui.close();
            }
            if ui.button("🗑 Delete").clicked() {
                self.cmd.delete_asset = Some(path.to_string());
                ui.close();
            }
            if let Some(d) = &dir {
                ui.separator();
                self.new_asset_menu(ui, d);
            }
        });
    }

    /// Paint one tile (a framed icon over a name), returning its click_and_drag
    /// response. Highlights when `selected`.
    fn tile_frame(
        &self,
        ui: &mut egui::Ui,
        icon: &str,
        color: egui::Color32,
        name: &str,
        selected: bool,
    ) -> egui::Response {
        let size = egui::vec2(86.0, 84.0);
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
        let p = ui.painter_at(rect);
        let bg = if selected {
            ui.visuals().selection.bg_fill.gamma_multiply(0.5)
        } else if resp.hovered() {
            ui.visuals().widgets.hovered.bg_fill
        } else {
            ui.visuals().faint_bg_color
        };
        p.rect_filled(rect.shrink(2.0), 5.0, bg);
        if selected {
            p.rect_stroke(rect.shrink(2.0), 5.0, egui::Stroke::new(1.5, ui.visuals().selection.stroke.color), egui::StrokeKind::Inside);
        }
        // Icon glyph centered in the upper part.
        let icon_pos = egui::pos2(rect.center().x, rect.top() + 30.0);
        p.text(icon_pos, egui::Align2::CENTER_CENTER, icon, egui::FontId::proportional(30.0), color);
        // Name, truncated to two-ish lines at the bottom.
        let short = truncate_label(name, 22);
        p.text(
            egui::pos2(rect.center().x, rect.bottom() - 16.0),
            egui::Align2::CENTER_CENTER,
            short,
            egui::FontId::proportional(11.0),
            ui.visuals().text_color(),
        );
        resp.on_hover_text(name)
    }

    /// The shared "New Folder / New Script" submenu, targeting `dir`.
    fn new_asset_menu(&mut self, ui: &mut egui::Ui, dir: &Path) {
        if ui.button("🗀 New Folder").clicked() {
            self.cmd.new_folder_in = Some(dir.to_string_lossy().to_string());
            ui.close();
        }
        if ui.button("🖊 New Lua Script").clicked() {
            self.cmd.new_script_in = Some(dir.to_string_lossy().to_string());
            ui.close();
        }
        if ui.button("⎙ New Scene").clicked() {
            self.cmd.open_new_scene = true;
            ui.close();
        }
    }

    fn asset_node_ui(&mut self, ui: &mut egui::Ui, entries: &[AssetEntry], dir: &Path) {
        for entry in entries {
            match entry {
                AssetEntry::Dir(name, children) => {
                    let child_dir = dir.join(name);
                    let header = egui::CollapsingHeader::new(format!("🗀 {name}"))
                        .id_salt(name)
                        .show(ui, |ui| {
                            self.asset_node_ui(ui, children, &child_dir);
                        });
                    header.header_response.context_menu(|ui| {
                        self.new_asset_menu(ui, &child_dir);
                        ui.separator();
                        if ui.button("🗑 Delete folder").clicked() {
                            self.cmd.delete_asset = Some(child_dir.to_string_lossy().to_string());
                            ui.close();
                        }
                    });
                }
                AssetEntry::File { name, path } => {
                    let model = is_model(path);
                    let script = is_script(path);
                    let draggable = model || script;
                    let selected = self.selected_asset.as_deref() == Some(path.as_str());
                    let (icon, _) = asset_kind_icon(path);
                    let grip = if draggable { "¦" } else { " " };
                    let label = format!("{grip} {icon} {name}");
                    // A single widget that senses BOTH click and drag. (The old
                    // dnd_drag_source layered a drag-sense interaction over the label,
                    // and the drag sense swallowed double-clicks — so a script could
                    // only be dragged, never opened.) One click_and_drag widget lets
                    // egui tell a tap from a drag cleanly: tap ⏵ select / double-tap
                    // ⏵ open; press-and-move ⏵ drag a payload onto the scene or a node.
                    let resp = if draggable {
                        let text = if selected {
                            egui::RichText::new(label).strong().color(ui.visuals().selection.stroke.color)
                        } else {
                            egui::RichText::new(label)
                        };
                        let r = ui.add(
                            egui::Label::new(text)
                                .selectable(false)
                                .sense(egui::Sense::click_and_drag()),
                        );
                        r.dnd_set_drag_payload(AssetPayload { path: path.clone() });
                        r
                    } else {
                        ui.selectable_label(selected, label)
                    };
                    if resp.clicked() {
                        *self.selected_asset = Some(path.clone());
                    }
                    let openable = script || is_markdown(path);
                    if resp.double_clicked() {
                        if is_scene(path) {
                            self.cmd.open_scene = Some(path.clone());
                        } else if openable {
                            self.cmd.open_script_pref = Some(path.clone());
                        }
                    }
                    resp.context_menu(|ui| {
                        if openable && ui.button("🖊 Open in Scripting tab").clicked() {
                            self.cmd.open_script = Some(path.clone());
                            self.cmd.focus_scripting = true;
                            ui.close();
                        }
                        if ui.button("🗀 Open in file explorer").clicked() {
                            reveal_in_explorer(Path::new(path));
                            ui.close();
                        }
                        if ui.button("🖊 Rename…").clicked() {
                            self.cmd.rename_asset = Some(path.clone());
                            ui.close();
                        }
                        if ui.button("🗑 Delete").clicked() {
                            self.cmd.delete_asset = Some(path.clone());
                            ui.close();
                        }
                        ui.separator();
                        self.new_asset_menu(ui, dir);
                    });
                }
            }
        }
    }

    fn scene_ui(&mut self, ui: &mut egui::Ui, game: bool) {
        // This tab's rect IS the 3D viewport; cache it for picking / gizmo gating.
        let rect = ui.max_rect();
        *self.scene_rect = Some(rect);

        // The Game tab is the active-camera gameplay view — no editor tools/gizmos.
        // Warn if there's no active camera (the render falls back to the editor view).
        if game && !self.has_active_camera {
            egui::Area::new(egui::Id::new("game_no_cam"))
                .fixed_pos(rect.left_top() + egui::vec2(8.0, 8.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(235, 200, 90),
                            "Δ no active camera — using editor view",
                        );
                    });
                });
        }

        // Overlay toolbar: tools (left) + resolution simulator (right). Editor view only.
        if !game {
            egui::Area::new(egui::Id::new("scene_toolbar"))
                .fixed_pos(rect.left_top() + egui::vec2(8.0, 8.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for t in [Tool::Select, Tool::Move, Tool::Rotate, Tool::Scale, Tool::Sculpt] {
                                if ui.selectable_label(self.tool == t, t.label()).clicked() {
                                    self.cmd.set_tool = Some(t);
                                }
                            }
                            ui.separator();
                            egui::ComboBox::from_id_salt("aspect_mode")
                                .selected_text(self.aspect.label())
                                .show_ui(ui, |ui| {
                                    for m in AspectMode::ALL {
                                        if ui.selectable_label(*self.aspect == m, m.label()).clicked() {
                                            *self.aspect = m;
                                        }
                                    }
                                });
                            if self.aspect.ratio().is_some() {
                                ui.add(egui::Slider::new(self.zoom, 0.4..=1.0).text("fit").show_value(false));
                            }
                        });
                    });
                });
        }

        // Resolution simulator: a centered device frame for the chosen aspect.
        if let Some(r) = self.aspect.ratio() {
            let avail = rect.shrink(10.0);
            let zoom = self.zoom.clamp(0.2, 1.0);
            let (mut w, mut h) = (avail.width(), avail.height());
            if w / h > r {
                w = h * r;
            } else {
                h = w / r;
            }
            w *= zoom;
            h *= zoom;
            let frame = egui::Rect::from_center_size(rect.center(), egui::vec2(w, h));
            let painter = ui.painter_at(rect);
            // Dim outside the device frame so the framing is obvious.
            let shade = egui::Color32::from_black_alpha(150);
            painter.rect_filled(egui::Rect::from_min_max(rect.left_top(), egui::pos2(rect.right(), frame.top())), 0.0, shade);
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(rect.left(), frame.bottom()), rect.right_bottom()), 0.0, shade);
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(rect.left(), frame.top()), egui::pos2(frame.left(), frame.bottom())), 0.0, shade);
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(frame.right(), frame.top()), egui::pos2(rect.right(), frame.bottom())), 0.0, shade);
            painter.rect_stroke(frame, 2.0, egui::Stroke::new(1.5, egui::Color32::from_gray(180)), egui::StrokeKind::Inside);
        }

        // The gizmo paints on a layer above the scene, clipped to this tab (editor only).
        if let Some(g) = self.gizmo.filter(|_| !game) {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("gizmo")))
                .with_clip_rect(rect);
            paint_gizmo(&painter, g, self.tool, self.grabbed, self.ppp);
        }

        // Terrain brush telegraph: a ring at the surface + a normal line, so you can
        // see exactly where (and on what facing) a stroke will land.
        if let Some(viz) = self.terrain_viz.filter(|_| !game) {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("terrain_brush")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            if viz.ring.len() >= 2 {
                let mut pts: Vec<egui::Pos2> = viz.ring.iter().map(|v| pt(*v)).collect();
                pts.push(pts[0]); // close the loop
                painter.line(pts, egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 230, 120)));
            }
            if let Some((a, b)) = viz.normal {
                painter.line_segment(
                    [pt(a), pt(b)],
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 200, 255)),
                );
            }
        }

        // Camera frustums (active = bright green, others = dim) so cameras are visible.
        if !self.camera_gizmos.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("camera_gizmos")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            for g in self.camera_gizmos {
                let col = if g.active {
                    egui::Color32::from_rgb(120, 230, 140)
                } else {
                    egui::Color32::from_rgb(150, 160, 175)
                };
                for (a, b) in &g.lines {
                    painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(1.5, col));
                }
            }
        }

        // Point-light gizmos (a warm cross + range ring) so unselected lights are
        // visible/placeable. Editor view only (the gather is gated on !game_view).
        if !self.light_gizmos.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("light_gizmos")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            let col = egui::Color32::from_rgb(245, 210, 110);
            for lines in self.light_gizmos {
                for (a, b) in lines {
                    painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(1.5, col));
                }
            }
        }

        // Rigidbody collider outlines (cyan) + collision-contact crosses (orange).
        if !self.body_gizmos.is_empty() || !self.contact_gizmos.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("physics_gizmos")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            let body_col = egui::Color32::from_rgb(110, 220, 210);
            for lines in self.body_gizmos {
                for (a, b) in lines {
                    painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(1.2, body_col));
                }
            }
            let hit_col = egui::Color32::from_rgb(255, 150, 60);
            for (a, b) in self.contact_gizmos {
                painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(2.0, hit_col));
            }
        }

        // Terrain collider wireframe (where the player can walk) — a soft yellow net.
        if !self.terrain_wire.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("terrain_wire")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            let col = egui::Color32::from_rgba_unmultiplied(235, 225, 120, 130);
            for (a, b) in self.terrain_wire {
                painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(0.8, col));
            }
        }
    }

    /// Draw the selected asset's preview: a spinning model/material render (drag to
    /// orbit, scroll to zoom, with spin + zoom controls) or a texture image.
    fn asset_preview_ui(&mut self, ui: &mut egui::Ui) {
        match self.preview.clone() {
            Some(PreviewView::Rendered(id)) => {
                let size = egui::vec2(240.0, 240.0);
                let resp = ui.add(
                    egui::Image::new((id, size))
                        .sense(egui::Sense::click_and_drag())
                        .corner_radius(4.0),
                );
                // Drag to orbit (pauses auto-spin); scroll over the image to zoom.
                if resp.dragged() {
                    *self.preview_spinning = false;
                    *self.preview_spin += resp.drag_delta().x * 0.01;
                }
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if resp.hovered() && scroll != 0.0 {
                    *self.preview_zoom = (*self.preview_zoom * (1.0 - scroll * 0.002)).clamp(0.4, 4.0);
                }
                ui.horizontal(|ui| {
                    ui.toggle_value(self.preview_spinning, "⟲ spin");
                    ui.add(egui::Slider::new(self.preview_zoom, 0.4..=4.0).text("zoom"));
                });
            }
            Some(PreviewView::Image(handle, dims)) => {
                let max = 256.0;
                let (w, h) = (dims[0].max(1) as f32, dims[1].max(1) as f32);
                let s = (max / w.max(h)).min(1.0);
                ui.add(
                    egui::Image::new(&handle)
                        .fit_to_exact_size(egui::vec2(w * s, h * s))
                        .corner_radius(4.0),
                );
                ui.small(format!("{}×{} px", dims[0], dims[1]));
            }
            None => {
                ui.weak("(building preview…)");
            }
        }
    }

    /// Editable properties for a selected material preset, with a Save back to its
    /// `.ron`. Edits mutate the live preview material, so the sphere updates as you go.
    /// Per-texture sampling controls (filter + wrap), shown when a texture asset is
    /// selected. Changes are recorded on `cmd` and applied (persist + re-register)
    /// after the frame.
    fn texture_settings_ui(&mut self, ui: &mut egui::Ui, path: &str) {
        ui.separator();
        ui.strong("Sampling");
        let mut s = self.texture_settings.get(path).copied().unwrap_or_default();
        let before = s;
        egui::Grid::new("tex-sampling").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label("filter");
            egui::ComboBox::from_id_salt("tex-filter")
                .selected_text(match s.filter {
                    FilterMode::Pixelated => "Pixelated",
                    FilterMode::Smooth => "Smooth",
                    FilterMode::SmoothMipmaps => "Smooth + Mipmaps",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut s.filter, FilterMode::Pixelated, "Pixelated");
                    ui.selectable_value(&mut s.filter, FilterMode::Smooth, "Smooth");
                    ui.selectable_value(&mut s.filter, FilterMode::SmoothMipmaps, "Smooth + Mipmaps");
                });
            ui.end_row();
            ui.label("wrap");
            egui::ComboBox::from_id_salt("tex-wrap")
                .selected_text(match s.wrap {
                    WrapMode::Repeat => "Repeat",
                    WrapMode::Clamp => "Clamp",
                    WrapMode::Mirror => "Mirror",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut s.wrap, WrapMode::Repeat, "Repeat");
                    ui.selectable_value(&mut s.wrap, WrapMode::Clamp, "Clamp");
                    ui.selectable_value(&mut s.wrap, WrapMode::Mirror, "Mirror");
                });
            ui.end_row();
        });
        ui.small("Pixelated = crisp · Smooth = bilinear · +Mipmaps = no shimmer at distance.");
        if s != before {
            self.cmd.set_texture_setting = Some((path.to_string(), s));
        }
    }

    fn material_asset_ui(&mut self, ui: &mut egui::Ui, path: &str) {
        let Some((mpath, mat)) = self.preview_material.as_mut() else { return };
        if mpath != path {
            return;
        }
        ui.separator();
        let r = material_props_ui(ui, mat, self.materials, &[], self.mat_name_buf);
        if let Some(name) = r.save_as {
            if !name.is_empty() {
                self.cmd.save_material = Some((name, MaterialDoc::from_material(mat)));
            }
        }
        if ui.button("Save to this preset").clicked() {
            let stem = Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if !stem.is_empty() {
                self.cmd.save_material = Some((stem, MaterialDoc::from_material(mat)));
            }
        }
    }

    /// The engine Console: a filterable, searchable feed of script `print`/`log`
    /// output, warnings and errors. Double-click a line to open its source.
    fn console_ui(&mut self, ui: &mut egui::Ui) {
        use floptle_script::LogLevel;
        let c = &mut *self.console;

        // Tally per-severity counts (summing merged duplicates).
        let (mut nd, mut nw, mut ne) = (0u32, 0u32, 0u32);
        for e in &c.entries {
            match e.level {
                LogLevel::Debug => nd += e.count,
                LogLevel::Warn => nw += e.count,
                LogLevel::Error => ne += e.count,
            }
        }

        // ---- toolbar: severity toggles, collapse, search, copy, clear ----
        let mut do_copy = false;
        let mut do_clear = false;
        ui.horizontal_wrapped(|ui| {
            ui.toggle_value(&mut c.show_debug, format!("· {nd}")).on_hover_text("messages");
            ui.toggle_value(&mut c.show_warn, format!("Δ {nw}")).on_hover_text("warnings");
            ui.toggle_value(&mut c.show_error, format!("⊗ {ne}")).on_hover_text("errors");
            ui.separator();
            ui.toggle_value(&mut c.collapse, "⊟").on_hover_text("collapse duplicate lines");
            ui.separator();
            ui.label("○");
            ui.add(
                egui::TextEdit::singleline(&mut c.search)
                    .hint_text("search")
                    .desired_width(150.0),
            );
            if !c.search.is_empty() && ui.small_button("×").clicked() {
                c.search.clear();
            }
            ui.separator();
            if ui.button("⎘ Copy").on_hover_text("copy the visible lines").clicked() {
                do_copy = true;
            }
            if ui.button("🗑 Clear").clicked() {
                do_clear = true;
            }
        });
        ui.separator();

        // ---- build the visible row set: filter, then optionally fully collapse ----
        let needle = c.search.to_ascii_lowercase();
        let passes = |e: &ConsoleEntry| {
            let on = match e.level {
                LogLevel::Debug => c.show_debug,
                LogLevel::Warn => c.show_warn,
                LogLevel::Error => c.show_error,
            };
            if !on {
                return false;
            }
            if needle.is_empty() {
                return true;
            }
            e.msg.to_ascii_lowercase().contains(&needle)
                || e.source.as_ref().is_some_and(|(n, _)| n.to_ascii_lowercase().contains(&needle))
        };
        // (level, msg, source, count)
        let mut rows: Vec<(LogLevel, &str, Option<&(String, u32)>, u32)> = Vec::new();
        if c.collapse {
            // Merge identical messages across the whole feed into one counted row.
            let mut idx: std::collections::HashMap<(u8, &str), usize> = std::collections::HashMap::new();
            for e in c.entries.iter().filter(|e| passes(e)) {
                let key = (e.level as u8, e.msg.as_str());
                if let Some(&r) = idx.get(&key) {
                    rows[r].3 += e.count;
                } else {
                    idx.insert(key, rows.len());
                    rows.push((e.level, &e.msg, e.source.as_ref(), e.count));
                }
            }
        } else {
            for e in c.entries.iter().filter(|e| passes(e)) {
                rows.push((e.level, &e.msg, e.source.as_ref(), e.count));
            }
        }

        if do_copy {
            let mut text = String::new();
            for (lvl, msg, src, n) in &rows {
                let tag = match lvl {
                    LogLevel::Debug => "log",
                    LogLevel::Warn => "warn",
                    LogLevel::Error => "error",
                };
                if let Some((name, line)) = src {
                    text.push_str(&format!("[{tag}] {name}:{line}: {msg}"));
                } else {
                    text.push_str(&format!("[{tag}] {msg}"));
                }
                if *n > 1 {
                    text.push_str(&format!("  (x{n})"));
                }
                text.push('\n');
            }
            ui.ctx().copy_text(text);
        }

        // ---- the log list ----
        let mut jump: Option<(String, u32)> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if rows.is_empty() {
                    ui.weak("No console output. Press F1 to play — script print/log and errors appear here.");
                }
                for (lvl, msg, src, n) in &rows {
                    let color = match lvl {
                        LogLevel::Debug => egui::Color32::from_gray(205),
                        LogLevel::Warn => egui::Color32::from_rgb(240, 200, 90),
                        LogLevel::Error => egui::Color32::from_rgb(235, 95, 95),
                    };
                    let icon = match lvl {
                        LogLevel::Debug => "·",
                        LogLevel::Warn => "Δ",
                        LogLevel::Error => "⊗",
                    };
                    let resp = ui
                        .horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 5.0;
                            if let Some((name, line)) = src {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("{name}:{line}"))
                                            .monospace()
                                            .weak(),
                                    )
                                    .selectable(true),
                                );
                            }
                            // Selectable so you can drag-select + copy a line; a
                            // double-click still jumps to its source.
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!("{icon} {msg}")).color(color).monospace(),
                                )
                                .selectable(true),
                            )
                        })
                        .inner;
                    if *n > 1 {
                        // count badge sits at the row's right edge.
                        let badge = format!("×{n}");
                        ui.painter().text(
                            egui::pos2(resp.rect.right() + 26.0, resp.rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            badge,
                            egui::FontId::monospace(11.0),
                            egui::Color32::from_gray(140),
                        );
                    }
                    if resp.double_clicked() {
                        if let Some((name, line)) = src {
                            jump = Some(((*name).clone(), *line));
                        }
                    }
                    resp.on_hover_text("double-click to open the source");
                }
            });

        if do_clear {
            c.entries.clear();
        }
        if let Some(j) = jump {
            self.cmd.open_log_source = Some(j);
        }
    }

    fn scripting_ui(&mut self, ui: &mut egui::Ui) {
        // Live script errors (from the last play frame) surface here in red.
        if !self.script_errors.is_empty() {
            egui::Frame::NONE
                .fill(egui::Color32::from_rgb(60, 20, 20))
                .inner_margin(6.0)
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Δ script errors").strong().color(egui::Color32::from_rgb(255, 150, 150)));
                    for e in self.script_errors {
                        ui.label(egui::RichText::new(e).monospace().color(egui::Color32::from_rgb(255, 180, 180)));
                    }
                });
        }
        // Tab strip: Docs + each open file.
        ui.horizontal_wrapped(|ui| {
            if ui.selectable_label(self.ide.active.is_none(), "§ Docs").clicked() {
                self.ide.active = None;
            }
            let mut close: Option<usize> = None;
            for i in 0..self.ide.open.len() {
                let f = &self.ide.open[i];
                let title = if f.dirty { format!("{} *", f.name) } else { f.name.clone() };
                if ui.selectable_label(self.ide.active == Some(i), title).clicked() {
                    self.ide.active = Some(i);
                }
                if ui.small_button("×").clicked() {
                    close = Some(i);
                }
            }
            if let Some(i) = close {
                self.ide.open.remove(i);
                self.ide.active = match self.ide.active {
                    Some(a) if a == i => None,
                    Some(a) if a > i => Some(a - 1),
                    other => other,
                };
            }
        });
        ui.separator();

        match self.ide.active {
            None => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.monospace(SCRIPT_DOCS);
                    ui.add_space(12.0);
                    ui.separator();
                    egui::CollapsingHeader::new("§ API reference")
                        .default_open(true)
                        .show(ui, |ui| {
                            ui.small("Hover an entry for details. (Inside a script, start typing for the same suggestions inline.)");
                            ui.add_space(4.0);
                            for e in LUA_API {
                                ui.horizontal(|ui| {
                                    ui.monospace(
                                        egui::RichText::new(e.label)
                                            .color(egui::Color32::from_rgb(78, 201, 176)),
                                    )
                                    .on_hover_text(e.doc);
                                    ui.small(e.doc);
                                });
                            }
                        });
                });
            }
            Some(i) if i < self.ide.open.len() => {
                ui.horizontal(|ui| {
                    ui.small(self.ide.open[i].path.clone());
                    if ui.button("Save").clicked() {
                        let f = &mut self.ide.open[i];
                        if std::fs::write(&f.path, &f.text).is_ok() {
                            f.dirty = false;
                            self.cmd.refresh_assets = true;
                        }
                    }
                    if ui
                        .button("⏵ Open in IDE")
                        .on_hover_text("Open the project in your external editor (set it in Project Settings)")
                        .clicked()
                    {
                        // Save first so the external editor sees the latest text.
                        let f = &mut self.ide.open[i];
                        if std::fs::write(&f.path, &f.text).is_ok() {
                            f.dirty = false;
                        }
                        self.cmd.open_in_editor = Some(self.ide.open[i].path.clone());
                    }
                    ui.menu_button("Insert snippet", |ui| {
                        for (label, snippet) in LUA_SNIPPETS {
                            if ui.button(*label).clicked() {
                                self.ide.open[i].text.push_str(snippet);
                                self.ide.open[i].dirty = true;
                                ui.close();
                            }
                        }
                    });
                });
                // Hint: the tunables this script declares via its `defaults` table.
                let hint = script_hint(&self.ide.open[i].text);
                if !hint.is_empty() {
                    ui.small(egui::RichText::new(hint).color(egui::Color32::from_gray(160)));
                }
                // Code editor: Lua syntax highlighting (plain for non-Lua files), a
                // line-number gutter, an autocomplete popup and red squiggles.
                let editor_id = egui::Id::new(("ide_editor", self.ide.open[i].path.clone()));
                let is_lua = self.ide.open[i].path.ends_with(".lua");
                let font = egui::FontId::monospace(13.0);
                let lfont = font.clone();
                let mut layouter = move |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
                    // No wrap (code editor) — logical lines == rows, so the gutter aligns.
                    let mut job =
                        if is_lua { lua_highlight(buf.as_str(), lfont.clone()) } else { plain_job(buf.as_str(), lfont.clone()) };
                    job.wrap.max_width = f32::INFINITY;
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                // Tab accepts the top completion: if the popup was open last frame,
                // eat Tab *before* the editor runs so it doesn't shift focus instead.
                let ac_id = egui::Id::new(("ide_ac_open", editor_id));
                let ac_was_open = ui.ctx().data(|d| d.get_temp::<bool>(ac_id).unwrap_or(false));
                let tab_accept =
                    ac_was_open && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab));
                let line_count = self.ide.open[i].text.matches('\n').count() + 1;
                let output = egui::ScrollArea::both()
                    .id_salt("ide_scroll")
                    .show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            // Line-number gutter (aligned with the un-wrapped rows).
                            let nums: String = (1..=line_count).fold(String::new(), |mut s, n| {
                                s.push_str(&format!("{n}\n"));
                                s
                            });
                            ui.add(egui::Label::new(
                                egui::RichText::new(nums).font(font.clone()).color(egui::Color32::from_gray(100)),
                            ));
                            egui::TextEdit::multiline(&mut self.ide.open[i].text)
                                .id(editor_id)
                                .code_editor()
                                .desired_width(f32::INFINITY)
                                .desired_rows(20)
                                .layouter(&mut layouter)
                                .show(ui)
                        })
                        .inner
                    })
                    .inner;
                if output.response.response.changed() {
                    self.ide.open[i].dirty = true;
                }
                // A pending Console jump scrolls the requested line into view.
                if let Some(line) = self.ide.goto.take() {
                    let row = line.saturating_sub(1).min(output.galley.rows.len().saturating_sub(1));
                    if let Some(r) = output.galley.rows.get(row) {
                        let rr = r.rect();
                        let target = egui::Rect::from_min_max(
                            output.galley_pos + rr.left_top().to_vec2(),
                            output.galley_pos + rr.right_bottom().to_vec2(),
                        );
                        ui.scroll_to_rect(target, Some(egui::Align::Center));
                    }
                }
                // Red squiggle on the line of a Lua syntax error.
                if let Some((line, _)) = self.ide_diag {
                    let row = line.saturating_sub(1).min(output.galley.rows.len().saturating_sub(1));
                    if let Some(r) = output.galley.rows.get(row) {
                        let rr = r.rect();
                        let y = output.galley_pos.y + rr.bottom();
                        let x0 = output.galley_pos.x + rr.left();
                        let x1 = output.galley_pos.x + rr.right().max(rr.left() + 30.0);
                        ui.painter().line_segment(
                            [egui::pos2(x0, y), egui::pos2(x1, y)],
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(235, 80, 80)),
                        );
                    }
                }
                if let Some((line, msg)) = self.ide_diag {
                    ui.colored_label(egui::Color32::from_rgb(235, 120, 120), format!("Δ line {line}: {msg}"));
                }
                let ac_open = self.ide_autocomplete(
                    ui,
                    i,
                    editor_id,
                    output.response.response.has_focus(),
                    output.cursor_range,
                    &output.galley,
                    output.galley_pos,
                    tab_accept,
                );
                ui.ctx().data_mut(|d| d.insert_temp(ac_id, ac_open));

                // Hover doc: hovering an API identifier in the code shows its tooltip.
                if let Some(p) = output.response.response.hover_pos() {
                    let rel = p - output.galley_pos;
                    let cc = output.galley.cursor_from_pos(rel);
                    let word = word_at(&self.ide.open[i].text, cc.index.0);
                    if let Some(api) = LUA_API.iter().find(|a| a.label == word) {
                        output.response.response.clone().on_hover_ui_at_pointer(|ui| {
                            ui.set_max_width(360.0);
                            ui.monospace(egui::RichText::new(api.label).color(egui::Color32::from_rgb(78, 201, 176)));
                            ui.label(api.doc);
                        });
                    }
                }
            }
            _ => {
                self.ide.active = None;
            }
        }
    }

    /// An autocomplete popup at the caret offering the engine API. Click a row or
    /// press Tab (`tab_accept`) to insert; hover a row for its doc. Returns whether
    /// the popup is showing (so the caller can route Tab to it next frame).
    #[allow(clippy::too_many_arguments)]
    fn ide_autocomplete(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        editor_id: egui::Id,
        has_focus: bool,
        cursor_range: Option<egui::text::CCursorRange>,
        galley: &egui::text::Galley,
        galley_pos: egui::Pos2,
        tab_accept: bool,
    ) -> bool {
        if !has_focus {
            return false;
        }
        let Some(range) = cursor_range else { return false };
        if !range.is_empty() {
            return false; // a selection, not a caret
        }
        let cursor = range.primary.index.0;
        let (start, token) = current_token(&self.ide.open[i].text, cursor);
        // Pop only on a real prefix: ≥2 chars for a plain word, or any member access.
        if token.len() < 2 && !token.contains('.') {
            return false;
        }
        let lower = token.to_ascii_lowercase();
        let matches: Vec<&ApiEntry> = LUA_API
            .iter()
            .filter(|e| {
                let l = e.label.to_ascii_lowercase();
                l.starts_with(&lower) && l != lower
            })
            .take(8)
            .collect();
        if matches.is_empty() {
            return false;
        }

        let caret = galley.pos_from_cursor(egui::text::CCursor::new(cursor));
        let pos = galley_pos + caret.left_bottom().to_vec2();
        // Tab inserts the top match; otherwise a click does.
        let mut chosen: Option<&'static str> = if tab_accept { Some(matches[0].insert) } else { None };
        egui::Area::new(egui::Id::new(("ide_ac", editor_id)))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(340.0);
                    ui.small("Tab to accept");
                    for (n, e) in matches.iter().enumerate() {
                        let label = if n == 0 {
                            egui::RichText::new(e.label).monospace().strong()
                        } else {
                            egui::RichText::new(e.label).monospace()
                        };
                        if ui.selectable_label(false, label).on_hover_text(e.doc).clicked() {
                            chosen = Some(e.insert);
                        }
                    }
                });
            });

        if let Some(insert) = chosen {
            replace_chars(&mut self.ide.open[i].text, start, cursor, insert);
            self.ide.open[i].dirty = true;
            let new_idx = start + insert.chars().count();
            if let Some(mut state) = egui::text_edit::TextEditState::load(ui.ctx(), editor_id) {
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(new_idx))));
                state.store(ui.ctx(), editor_id);
            }
            ui.ctx().memory_mut(|m| m.request_focus(editor_id));
            return false; // inserted — popup closes
        }
        true
    }
}

/// A starter Lua script body (ADR-0003) — named after the file it lands in.
fn script_template(name: &str) -> String {
    format!(
        "-- {name}.lua\n\
         --\n\
         -- `defaults` are tunables shown in the Inspector; `params` are this\n\
         -- instance's live values. `node` is the node's transform (x/y/z,\n\
         -- scale/scale_x..z, yaw/pitch/roll in radians). `time` = seconds since\n\
         -- play started, `dt` = frame delta. The full Lua stdlib is in scope.\n\
         \n\
         defaults = {{ speed = 1.0 }}\n\
         \n\
         function start(node)\n\
         \x20 -- runs once when play begins\n\
         end\n\
         \n\
         function update(node, dt)\n\
         \x20 node.yaw = node.yaw + params.speed * dt\n\
         end\n"
    )
}

/// Insert-menu snippets for the in-engine IDE: (label, Lua to append).
const LUA_SNIPPETS: &[(&str, &str)] = &[
    (
        "update",
        "\nfunction update(node, dt)\n  \nend\n",
    ),
    (
        "start",
        "\nfunction start(node)\n  \nend\n",
    ),
    (
        "spin (yaw)",
        "\ndefaults = { speed = 45 }\nfunction update(node, dt)\n  node.yaw = node.yaw + math.rad(params.speed) * dt\nend\n",
    ),
    (
        "pulse (scale)",
        "\ndefaults = { amplitude = 0.3, speed = 2.0, base = 1.0 }\nfunction update(node, dt)\n  node.scale = math.max(params.base * (1.0 + params.amplitude * math.sin(params.speed * time)), 0.01)\nend\n",
    ),
];

/// A one-line hint listing the tunables a script declares (parsed from its
/// `defaults = { ... }` table), shown above the code editor.
fn script_hint(text: &str) -> String {
    let Some(start) = text.find("defaults") else { return String::new() };
    let Some(open) = text[start..].find('{') else { return String::new() };
    let body_start = start + open + 1;
    let Some(close) = text[body_start..].find('}') else { return String::new() };
    let body = &text[body_start..body_start + close];
    let keys: Vec<&str> = body
        .split(',')
        .filter_map(|p| p.split('=').next())
        .map(|k| k.trim())
        .filter(|k| !k.is_empty())
        .collect();
    if keys.is_empty() {
        String::new()
    } else {
        format!("params: {}", keys.join(", "))
    }
}

// ---- in-engine IDE: Lua syntax highlighting + autocomplete -----------------

/// Lua reserved words (highlighted as keywords).
const LUA_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// Identifiers highlighted as engine/builtin API (teal).
const LUA_API_WORDS: &[&str] = &[
    "node", "params", "time", "dt", "defaults", "log", "start", "update", "input", "math",
    "string", "table", "ipairs", "pairs", "print", "tostring", "tonumber", "pcall", "select",
];

/// One completion / docs entry for the in-engine IDE.
struct ApiEntry {
    label: &'static str,
    insert: &'static str,
    doc: &'static str,
}

/// The engine scripting API, surfaced as autocomplete + hover docs (and the Docs
/// page's reference). Lua stdlib highlights are included so completion is useful.
const LUA_API: &[ApiEntry] = &[
    ApiEntry { label: "update", insert: "update", doc: "function update(node, dt) — runs every frame while playing." },
    ApiEntry { label: "start", insert: "start", doc: "function start(node) — runs once when play begins." },
    ApiEntry { label: "defaults", insert: "defaults", doc: "defaults = { name = value } — tunables shown in the Inspector." },
    ApiEntry { label: "params", insert: "params", doc: "This instance's tunables, a table seeded from `defaults` (params.speed, …)." },
    ApiEntry { label: "node", insert: "node", doc: "The node's transform: x/y/z, scale, scale_x/y/z, yaw/pitch/roll." },
    ApiEntry { label: "node.x", insert: "node.x", doc: "World X position (number)." },
    ApiEntry { label: "node.y", insert: "node.y", doc: "World Y position (number)." },
    ApiEntry { label: "node.z", insert: "node.z", doc: "World Z position (number)." },
    ApiEntry { label: "node.scale", insert: "node.scale", doc: "Uniform scale (shortcut). Setting it scales all axes." },
    ApiEntry { label: "node.scale_x", insert: "node.scale_x", doc: "Scale along X." },
    ApiEntry { label: "node.scale_y", insert: "node.scale_y", doc: "Scale along Y." },
    ApiEntry { label: "node.scale_z", insert: "node.scale_z", doc: "Scale along Z." },
    ApiEntry { label: "node.yaw", insert: "node.yaw", doc: "Heading about Y, in radians." },
    ApiEntry { label: "node.pitch", insert: "node.pitch", doc: "Pitch about X, in radians." },
    ApiEntry { label: "node.roll", insert: "node.roll", doc: "Roll about Z, in radians." },
    ApiEntry { label: "time", insert: "time", doc: "Seconds since play started (number)." },
    ApiEntry { label: "dt", insert: "dt", doc: "Seconds since the last frame (number)." },
    ApiEntry { label: "log", insert: "log(", doc: "log(\"message\") — print to the engine console." },
    ApiEntry { label: "input", insert: "input", doc: "Player input (play mode). input.key/pressed/axis/mouse/button — make interactive games." },
    ApiEntry { label: "input.key", insert: "input.key(", doc: "input.key(\"w\") — true while the key is held. Names: a-z, 0-9, space, enter, shift, ctrl, alt, left/right/up/down, escape, tab." },
    ApiEntry { label: "input.pressed", insert: "input.pressed(", doc: "input.pressed(\"space\") — true only on the frame the key goes down (an edge)." },
    ApiEntry { label: "input.axis", insert: "input.axis(", doc: "input.axis(\"a\", \"d\") — returns -1/0/1 from a negative/positive key pair (e.g. strafing)." },
    ApiEntry { label: "input.mouse", insert: "input.mouse(", doc: "local x, y = input.mouse() — cursor position in pixels." },
    ApiEntry { label: "input.mouse_delta", insert: "input.mouse_delta(", doc: "local dx, dy = input.mouse_delta() — mouse movement since last frame." },
    ApiEntry { label: "input.button", insert: "input.button(", doc: "input.button(0) — true while a mouse button is held (0 left, 1 right, 2 middle)." },
    ApiEntry { label: "input.clicked", insert: "input.clicked(", doc: "input.clicked(0) — true only on the frame a mouse button goes down." },
    ApiEntry { label: "input.scroll", insert: "input.scroll(", doc: "input.scroll() — mouse wheel delta this frame." },
    ApiEntry { label: "math.sin", insert: "math.sin(", doc: "math.sin(x) — sine of x (radians)." },
    ApiEntry { label: "math.cos", insert: "math.cos(", doc: "math.cos(x) — cosine of x (radians)." },
    ApiEntry { label: "math.rad", insert: "math.rad(", doc: "math.rad(deg) — degrees to radians." },
    ApiEntry { label: "math.deg", insert: "math.deg(", doc: "math.deg(rad) — radians to degrees." },
    ApiEntry { label: "math.pi", insert: "math.pi", doc: "The constant π." },
    ApiEntry { label: "math.abs", insert: "math.abs(", doc: "math.abs(x) — absolute value." },
    ApiEntry { label: "math.max", insert: "math.max(", doc: "math.max(a, b, …) — largest argument." },
    ApiEntry { label: "math.min", insert: "math.min(", doc: "math.min(a, b, …) — smallest argument." },
    ApiEntry { label: "math.sqrt", insert: "math.sqrt(", doc: "math.sqrt(x) — square root." },
    ApiEntry { label: "math.floor", insert: "math.floor(", doc: "math.floor(x) — round down." },
    ApiEntry { label: "math.random", insert: "math.random(", doc: "math.random() — random in [0,1); math.random(n) — 1..n." },
    ApiEntry { label: "string.format", insert: "string.format(", doc: "string.format(fmt, …) — printf-style formatting." },
    ApiEntry { label: "function", insert: "function ", doc: "Define a function." },
    ApiEntry { label: "local", insert: "local ", doc: "Declare a local variable." },
];

/// Build a colored layout for Lua source (keywords, strings, numbers, comments,
/// engine API). A simple single-pass tokenizer — good enough for an in-engine IDE.
fn lua_highlight(text: &str, font: egui::FontId) -> egui::text::LayoutJob {
    use egui::Color32;
    let c_kw = Color32::from_rgb(86, 156, 214);
    let c_api = Color32::from_rgb(78, 201, 176);
    let c_str = Color32::from_rgb(206, 145, 120);
    let c_num = Color32::from_rgb(181, 206, 168);
    let c_com = Color32::from_rgb(106, 153, 85);
    let c_def = Color32::from_rgb(212, 212, 212);

    let mut job = egui::text::LayoutJob::default();
    let mut push = |s: &str, color: Color32| {
        job.append(s, 0.0, egui::text::TextFormat { font_id: font.clone(), color, ..Default::default() });
    };

    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // line comment
        if c == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            let s = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            push(&text[s..i], c_com);
        } else if c == b'"' || c == b'\'' {
            // string (single line; handles \" escapes)
            let q = c;
            let s = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i = (i + 2).min(b.len());
                    continue;
                }
                if b[i] == q || b[i] == b'\n' {
                    i = (i + 1).min(b.len());
                    break;
                }
                i += 1;
            }
            push(&text[s..i], c_str);
        } else if c.is_ascii_digit() {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            push(&text[s..i], c_num);
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &text[s..i];
            let color = if LUA_KEYWORDS.contains(&word) {
                c_kw
            } else if LUA_API_WORDS.contains(&word) {
                c_api
            } else {
                c_def
            };
            push(word, color);
        } else {
            // one (possibly multibyte) character verbatim
            let ch = text[i..].chars().next().unwrap();
            let l = ch.len_utf8();
            push(&text[i..i + l], c_def);
            i += l;
        }
    }
    job
}

/// A plain monospace layout (no highlighting) — used for non-Lua files (Markdown).
fn plain_job(text: &str, font: egui::FontId) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        text,
        0.0,
        egui::text::TextFormat { font_id: font, color: egui::Color32::from_gray(212), ..Default::default() },
    );
    job
}

/// The token (run of identifier/`.` chars) ending at `cursor_char`, plus its start
/// char index — what autocomplete matches against.
fn current_token(text: &str, cursor_char: usize) -> (usize, String) {
    let chars: Vec<char> = text.chars().collect();
    let cur = cursor_char.min(chars.len());
    let mut start = cur;
    while start > 0 {
        let c = chars[start - 1];
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
            start -= 1;
        } else {
            break;
        }
    }
    (start, chars[start..cur].iter().collect())
}

/// The full identifier (run of `[A-Za-z0-9_.]`) containing char index `idx`, or
/// empty if that char isn't part of one. Used for hover docs.
fn word_at(text: &str, idx: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    let i = idx.min(chars.len() - 1);
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.';
    if !is_word(chars[i]) {
        return String::new();
    }
    let mut s = i;
    while s > 0 && is_word(chars[s - 1]) {
        s -= 1;
    }
    let mut e = i;
    while e + 1 < chars.len() && is_word(chars[e + 1]) {
        e += 1;
    }
    chars[s..=e].iter().collect()
}

/// Replace the characters in `[start, end)` (char indices) of `s` with `ins`.
fn replace_chars(s: &mut String, start: usize, end: usize, ins: &str) {
    let byte = |n: usize| s.char_indices().nth(n).map(|(b, _)| b).unwrap_or(s.len());
    let (bs, be) = (byte(start), byte(end));
    s.replace_range(bs..be, ins);
}

fn main() {
    env_logger::init();
    println!("{} editor v{}", floptle_core::ENGINE_NAME, floptle_core::ENGINE_VERSION);
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut editor = Editor::default();
    event_loop.run_app(&mut editor).expect("run editor");
}

/// Terrain sculpt/paint brush settings.
#[derive(Clone, Copy)]
struct TerrainBrush {
    mode: floptle_field::Brush,
    radius: f32,
    strength: f32,
    color: [f32; 3],
    /// Paint target: -1 = flat color, else a terrain texture palette slot.
    tex_slot: i32,
    /// "Fill bounds" tool: lay flat ground up to `fill_top`, from `fill_floor` below,
    /// kept `fill_inset` in from the X/Z walls. (Edge-sculpt no longer auto-extends the
    /// ground, so this is the deliberate way to make flat areas.)
    fill_top: f32,
    fill_floor: f32,
    fill_inset: f32,
}

/// A "fill the whole terrain" request from the Terrain tab.
#[derive(Clone, Copy)]
enum TerrainFill {
    Color([f32; 3]),
    /// A palette slot stored as slot+1 (0 = untextured).
    Texture(u8),
}

impl Default for TerrainBrush {
    fn default() -> Self {
        Self {
            mode: floptle_field::Brush::Raise,
            radius: 2.5,
            strength: 0.5,
            color: [0.45, 0.32, 0.2],
            tex_slot: -1,
            fill_top: 0.0,
            fill_floor: -8.0,
            fill_inset: 0.0,
        }
    }
}

/// The on-screen brush telegraph: a projected ring at the terrain hit point + a
/// surface-normal line, so you can see exactly where (and on what facing) a stroke
/// will land. Points are full-window physical pixels (divided by ppp when drawn).
#[derive(Default)]
struct TerrainViz {
    ring: Vec<Vec2>,
    normal: Option<(Vec2, Vec2)>,
}

/// A camera's frustum drawn in the viewport (screen-space px line pairs) so you can
/// see + position cameras. `active` is the camera holding play-mode authority.
struct CameraGizmo {
    lines: Vec<(Vec2, Vec2)>,
    active: bool,
}

/// Build a camera frustum's projected screen-space line segments (apex → 4 far
/// corners + the far rectangle), or empty if it doesn't project.
#[allow(clippy::too_many_arguments)]
fn camera_frustum_lines(
    pos: DVec3,
    rot: Quat,
    fov_y: f32,
    aspect: f32,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
) -> Vec<(Vec2, Vec2)> {
    let fwd = rot * Vec3::NEG_Z;
    let up = rot * Vec3::Y;
    let right = rot * Vec3::X;
    let far = 2.2f32; // a compact visualization length, not the real far plane
    let hh = far * (fov_y * 0.5).tan();
    let hw = hh * aspect.max(0.1);
    let apex = pos;
    let center = pos + (fwd * far).as_dvec3();
    let corners = [
        center + ((right * hw + up * hh).as_dvec3()),
        center + ((-right * hw + up * hh).as_dvec3()),
        center + ((-right * hw - up * hh).as_dvec3()),
        center + ((right * hw - up * hh).as_dvec3()),
    ];
    let pa = project(apex, cam_world, vp, w, h);
    let pc: Vec<Option<Vec2>> = corners.iter().map(|&c| project(c, cam_world, vp, w, h)).collect();
    let mut lines = Vec::new();
    for i in 0..4 {
        if let (Some(a), Some(b)) = (pa, pc[i]) {
            lines.push((a, b)); // apex → corner
        }
        if let (Some(a), Some(b)) = (pc[i], pc[(i + 1) % 4]) {
            lines.push((a, b)); // far-rect edge
        }
    }
    lines
}

/// World-space line segments tracing the terrain's collision iso-surface (where the SDF
/// crosses zero — exactly what the player collides with), via coarse Surface Nets: one
/// vertex per straddling cell (averaged edge crossings), connected to its +X/+Y/+Z
/// neighbors. `stride` sets coarseness (bigger = fewer lines). Cached by the caller and
/// projected to screen each frame.
fn terrain_collider_wire(t: &floptle_field::Terrain, stride: u32) -> Vec<(Vec3, Vec3)> {
    let b = &t.baked;
    let [w, h, d] = b.dims;
    let s = stride.max(1);
    if w < 2 || h < 2 || d < 2 {
        return Vec::new();
    }
    let dist = |x: u32, y: u32, z: u32| -> f32 {
        b.distance[((z.min(d - 1) * h + y.min(h - 1)) * w + x.min(w - 1)) as usize]
    };
    let gpos = |x: u32, y: u32, z: u32| -> Vec3 {
        let f = |i: u32, n: u32, c: f32, hf: f32| c - hf + (i as f32 + 0.5) / n as f32 * 2.0 * hf;
        Vec3::new(
            f(x.min(w - 1), w, b.center[0], b.half_extent[0]),
            f(y.min(h - 1), h, b.center[1], b.half_extent[1]),
            f(z.min(d - 1), d, b.center[2], b.half_extent[2]),
        )
    };
    // Coarse cell grid (each cell spans `s` voxels). One optional vertex per cell.
    let (cx_n, cy_n, cz_n) = ((w - 1) / s, (h - 1) / s, (d - 1) / s);
    let ci = |cx: u32, cy: u32, cz: u32| ((cz * cy_n + cy) * cx_n + cx) as usize;
    let mut verts: Vec<Option<Vec3>> = vec![None; (cx_n * cy_n * cz_n) as usize];
    // The 12 edges of a cube (corner index pairs), corners ordered (x,y,z) bit = 1<<axis.
    const EDGES: [(usize, usize); 12] =
        [(0, 1), (0, 2), (0, 4), (1, 3), (1, 5), (2, 3), (2, 6), (4, 5), (4, 6), (3, 7), (5, 7), (6, 7)];
    for cz in 0..cz_n {
        for cy in 0..cy_n {
            for cx in 0..cx_n {
                let (x0, y0, z0) = (cx * s, cy * s, cz * s);
                let corner = |k: usize| {
                    (x0 + (k as u32 & 1) * s, y0 + ((k as u32 >> 1) & 1) * s, z0 + ((k as u32 >> 2) & 1) * s)
                };
                let ds: [f32; 8] = std::array::from_fn(|k| {
                    let (x, y, z) = corner(k);
                    dist(x, y, z)
                });
                if ds.iter().all(|&v| v > 0.0) || ds.iter().all(|&v| v <= 0.0) {
                    continue; // doesn't straddle the surface
                }
                let cp: [Vec3; 8] = std::array::from_fn(|k| {
                    let (x, y, z) = corner(k);
                    gpos(x, y, z)
                });
                let mut acc = Vec3::ZERO;
                let mut n = 0.0f32;
                for (a, c) in EDGES {
                    if (ds[a] > 0.0) != (ds[c] > 0.0) {
                        let f = (ds[a] / (ds[a] - ds[c])).clamp(0.0, 1.0);
                        acc += cp[a].lerp(cp[c], f);
                        n += 1.0;
                    }
                }
                if n > 0.0 {
                    verts[ci(cx, cy, cz)] = Some(acc / n);
                }
            }
        }
    }
    // Connect each cell's vertex to its +X/+Y/+Z neighbour (a surface-conforming net).
    let mut segs = Vec::new();
    for cz in 0..cz_n {
        for cy in 0..cy_n {
            for cx in 0..cx_n {
                let Some(v) = verts[ci(cx, cy, cz)] else { continue };
                if cx + 1 < cx_n {
                    if let Some(v2) = verts[ci(cx + 1, cy, cz)] {
                        segs.push((v, v2));
                    }
                }
                if cy + 1 < cy_n {
                    if let Some(v2) = verts[ci(cx, cy + 1, cz)] {
                        segs.push((v, v2));
                    }
                }
                if cz + 1 < cz_n {
                    if let Some(v2) = verts[ci(cx, cy, cz + 1)] {
                        segs.push((v, v2));
                    }
                }
            }
        }
    }
    segs
}

/// Build a point light's projected gizmo: a small 3-axis cross at its position plus a
/// horizontal ring at its `range` (so its reach on the ground is visible). Empty if
/// it doesn't project in front of the camera.
fn point_light_lines(pos: DVec3, range: f32, cam_world: DVec3, vp: Mat4, w: f32, h: f32) -> Vec<(Vec2, Vec2)> {
    let mut lines = Vec::new();
    let s = 0.5; // cross half-size (world units)
    for a in [DVec3::X, DVec3::Y, DVec3::Z] {
        if let (Some(p0), Some(p1)) = (
            project(pos - a * s, cam_world, vp, w, h),
            project(pos + a * s, cam_world, vp, w, h),
        ) {
            lines.push((p0, p1));
        }
    }
    let r = range.clamp(0.2, 500.0) as f64;
    let segs = 28;
    let mut prev = project(pos + DVec3::new(r, 0.0, 0.0), cam_world, vp, w, h);
    for i in 1..=segs {
        let a = (i as f64 / segs as f64) * std::f64::consts::TAU;
        let p = project(pos + DVec3::new(a.cos() * r, 0.0, a.sin() * r), cam_world, vp, w, h);
        if let (Some(pp), Some(cp)) = (prev, p) {
            lines.push((pp, cp));
        }
        prev = p;
    }
    lines
}

/// Build a gravity-volume gizmo: a radial well is a 3-ring sphere wireframe at its
/// `radius`; a Down volume is a downward arrow. Empty if it doesn't project.
fn gravity_volume_lines(
    pos: DVec3,
    radial: bool,
    radius: f32,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
) -> Vec<(Vec2, Vec2)> {
    let mut lines = Vec::new();
    if radial {
        let r = radius.clamp(0.2, 500.0) as f64;
        let segs = 28;
        for plane in 0..3 {
            let ring = |a: f64| -> DVec3 {
                let (c, s) = (a.cos() * r, a.sin() * r);
                match plane {
                    0 => DVec3::new(c, 0.0, s), // XZ (ground)
                    1 => DVec3::new(c, s, 0.0), // XY
                    _ => DVec3::new(0.0, c, s), // YZ
                }
            };
            let mut prev = project(pos + ring(0.0), cam_world, vp, w, h);
            for i in 1..=segs {
                let p = project(pos + ring((i as f64 / segs as f64) * std::f64::consts::TAU), cam_world, vp, w, h);
                if let (Some(pp), Some(cp)) = (prev, p) {
                    lines.push((pp, cp));
                }
                prev = p;
            }
        }
    } else {
        let top = project(pos + DVec3::new(0.0, 1.0, 0.0), cam_world, vp, w, h);
        let bot = project(pos + DVec3::new(0.0, -1.2, 0.0), cam_world, vp, w, h);
        if let (Some(a), Some(b)) = (top, bot) {
            lines.push((a, b));
        }
        for dx in [-0.35, 0.35] {
            let head = project(pos + DVec3::new(dx, -0.55, 0.0), cam_world, vp, w, h);
            if let (Some(a), Some(b)) = (bot, head) {
                lines.push((a, b));
            }
        }
    }
    lines
}

/// Build a rigidbody collider outline: a 3-ring wireframe sphere, or a capsule (two
/// end rings + side connectors + cap arcs). Y-up (the editor doesn't tilt the gizmo).
fn rigidbody_lines(
    pos: DVec3,
    capsule: bool,
    radius: f32,
    height: f32,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
) -> Vec<(Vec2, Vec2)> {
    let mut lines = Vec::new();
    let r = radius.max(0.02) as f64;
    let segs = 24;
    let ring = |center: DVec3, plane: u8, out: &mut Vec<(Vec2, Vec2)>| {
        let at = |a: f64| -> DVec3 {
            let (c, s) = (a.cos() * r, a.sin() * r);
            match plane {
                0 => DVec3::new(c, 0.0, s), // XZ
                1 => DVec3::new(c, s, 0.0), // XY
                _ => DVec3::new(0.0, c, s), // YZ
            }
        };
        let mut prev = project(center + at(0.0), cam_world, vp, w, h);
        for i in 1..=segs {
            let p = project(center + at((i as f64 / segs as f64) * std::f64::consts::TAU), cam_world, vp, w, h);
            if let (Some(a), Some(b)) = (prev, p) {
                out.push((a, b));
            }
            prev = p;
        }
    };
    if capsule {
        let half = ((height.max(2.0 * radius) as f64) * 0.5 - r).max(0.0);
        let top = pos + DVec3::new(0.0, half, 0.0);
        let bot = pos - DVec3::new(0.0, half, 0.0);
        ring(top, 0, &mut lines);
        ring(bot, 0, &mut lines);
        ring(top, 1, &mut lines);
        ring(bot, 1, &mut lines);
        for (dx, dz) in [(r, 0.0), (-r, 0.0), (0.0, r), (0.0, -r)] {
            let a = project(top + DVec3::new(dx, 0.0, dz), cam_world, vp, w, h);
            let b = project(bot + DVec3::new(dx, 0.0, dz), cam_world, vp, w, h);
            if let (Some(a), Some(b)) = (a, b) {
                lines.push((a, b));
            }
        }
    } else {
        ring(pos, 0, &mut lines);
        ring(pos, 1, &mut lines);
        ring(pos, 2, &mut lines);
    }
    lines
}

/// Seconds an F-key focus glide takes to settle.
const FOCUS_SECS: f32 = 0.35;

/// An in-progress camera focus glide (the F key): ease the position from `from` to
/// `to` over [`FOCUS_SECS`] while the view angle is held fixed.
struct FocusAnim {
    from: DVec3,
    to: DVec3,
    t: f32,
}

// Field order is drop order: every GPU-resource holder (raster/raymarch/retro/egui)
// must drop BEFORE `gpu` (the device + surface), so `gpu` is intentionally last.
#[derive(Default)]
struct Editor {
    window: Option<Arc<Window>>,
    raster: Option<Raster>,
    raymarch: Option<Raymarch>,
    retro: Option<Retro>,
    /// Post-processing stack (bloom + vignette), full frame res.
    post: Option<floptle_render::PostStack>,
    /// Selection-outline post-process (silhouette mask + edge detect).
    outline: Option<Outline>,
    /// Editor reference-grid renderer.
    grid_render: Option<Grid>,
    egui: Option<Egui>,
    camera: FlyCamera,
    input: Input,
    world: World,
    /// Mesh handles indexed by `Shape as usize` (Cube=0, Sphere=1).
    mesh_ids: Vec<MeshId>,
    /// Imported glTF models, keyed by asset path ⏵ registered mesh parts.
    mesh_registry: HashMap<String, MeshAsset>,
    /// Material textures registered on the GPU, keyed by image path ⏵ handle.
    texture_registry: HashMap<String, TexId>,
    /// The sampling each registered texture was last built with, so a settings change
    /// forces a re-register (with the new sampler / mips).
    texture_registry_setting: HashMap<String, TexSetting>,
    /// Per-texture sampling settings (filter + wrap), keyed by image path. Persisted to
    /// `.floptle/textures.ron`. Absent ⏵ the crisp tiling default.
    texture_settings: HashMap<String, TexSetting>,
    /// Editable terrain SDF fields, keyed by their scene node Entity (each in its
    /// node's LOCAL space). Empty until "New Terrain". Multiple terrains are folded
    /// into one combined field for rendering ([`rebuild_combined`]).
    terrains: HashMap<Entity, floptle_field::Terrain>,
    /// The terrain the sculpt brush currently targets (the one under the cursor),
    /// chosen each frame.
    active_terrain: Option<Entity>,
    /// Cached world-space union of every terrain field (what the GPU renders).
    combined: Option<floptle_field::Terrain>,
    /// The (node, world origin) set the `combined` was built from — so a moved
    /// terrain (gizmo/inspector/undo) is detected and triggers a rebuild.
    combined_origins: Vec<(Entity, DVec3)>,
    /// The combined field needs rebuilding + re-uploading (any add/edit/move/delete).
    combined_dirty: bool,
    /// A paint dab on a single terrain only dirties a small voxel box — uploaded to the
    /// GPU directly (no full re-clone + re-upload), so painting a big terrain stays
    /// smooth. `(entity, min inclusive, max exclusive)`, merged across dabs in a frame.
    terrain_region_dirty: Option<(Entity, [u32; 3], [u32; 3])>,
    /// Monotonic id assigned to each new terrain node (stable across save/load).
    next_terrain_id: u32,
    /// LMB held with the Sculpt tool — keep brushing on mouse motion.
    sculpting: bool,
    /// Where the last brush dab landed + when — for movement-spaced, rate-limited
    /// strokes (so the brush behaves like a real paint tool, not 200 dabs/sec).
    last_dab_pos: Option<DVec3>,
    last_dab_time: Option<Instant>,
    /// Pre-stroke field bytes captured on mouse-down — pushed to the undo timeline
    /// on mouse-up if the stroke actually deformed the terrain. `None` between
    /// strokes. The whole stroke collapses to a single undo step.
    stroke_snapshot: Option<(u32, Vec<u8>)>,
    /// At least one dab landed during the current stroke (so it's worth undoing).
    stroke_dabbed: bool,
    /// Terrain brush settings.
    terrain_brush: TerrainBrush,
    /// New-terrain resolution along the long axis (user-controllable detail).
    terrain_detail: u32,
    /// Terrain texture palette — image paths per slot (empty = unused).
    terrain_textures: Vec<String>,
    /// The terrain palette needs re-uploading to the GPU.
    terrain_textures_dirty: bool,
    /// The brush telegraph for this frame (projected ring + normal).
    terrain_viz: Option<TerrainViz>,
    /// Camera frustums to draw in the viewport this frame (so cameras are visible).
    camera_gizmos: Vec<CameraGizmo>,
    /// Projected point-light gizmos (cross + range ring) for this frame.
    light_gizmos: Vec<Vec<(Vec2, Vec2)>>,
    /// Projected rigidbody collider outlines (sphere/capsule) for this frame.
    body_gizmos: Vec<Vec<(Vec2, Vec2)>>,
    /// Projected collision-contact crosses (telegraphed during Play).
    contact_gizmos: Vec<(Vec2, Vec2)>,
    /// Show the terrain's collision surface as a wireframe overlay (View menu toggle).
    show_terrain_collider: bool,
    /// Cached WORLD-space wireframe of the combined terrain's collision surface; rebuilt
    /// when the terrain changes (cleared on `combined_dirty`), projected each frame.
    terrain_wire_world: Vec<(Vec3, Vec3)>,
    /// This frame's projected terrain-collider wireframe segments (screen space).
    terrain_wire_gizmo: Vec<(Vec2, Vec2)>,
    /// Project-wide render settings (retro / matter), edited in Project Settings.
    project: ProjectConfigDoc,
    /// The open project's root folder (holds `scenes/`, `models/`, `scripts/`…).
    project_root: PathBuf,
    /// Whether the Project Settings window is open.
    show_project_settings: bool,
    /// Whether the New/Open Project window is open, + its path text field.
    show_project_mgr: bool,
    project_path_buf: String,
    /// Dockable panel layout (Hierarchy / Inspector / Assets / Scene / Scripting).
    dock_state: Option<egui_dock::DockState<EditorTab>>,
    /// The in-engine Scripting IDE (open files + Docs page).
    ide: IdeState,
    /// The asset selected in the browser (shown in the Inspector); `None` = a node.
    selected_asset: Option<String>,
    /// Resolution-simulator framing for the Scene tab.
    aspect_mode: AspectMode,
    viewport_zoom: f32,
    /// The Scene tab's rect (logical points), captured each frame — gates picking.
    scene_rect: Option<egui::Rect>,
    scene_name: String,
    /// Selected entities (multi-select); the gizmo/inspector act on the last one.
    selection: Vec<Entity>,
    /// Folder nodes collapsed in the Hierarchy (their children are hidden). Toggle
    /// with the triangle or Enter on a selected folder.
    collapsed: std::collections::HashSet<Entity>,
    /// The engine Console: captured script logs/warnings/errors + its view filters.
    console: ConsoleState,
    /// Player-input state fed to scripts (the Lua `input` API), accumulated from
    /// window events. Edge sets + deltas are cleared each frame after scripts run.
    input_keys: std::collections::HashSet<String>,
    input_keys_pressed: std::collections::HashSet<String>,
    input_buttons: [bool; 3],
    input_buttons_pressed: [bool; 3],
    input_mouse_delta: (f32, f32),
    input_scroll: f32,
    /// Offscreen target for the Inspector's spinning model / material preview.
    preview: Option<PreviewTarget>,
    /// Offscreen 16:9 target for the Inspector's selected-camera POV preview.
    cam_preview: Option<PreviewTarget>,
    /// Preview orbit angle (radians), whether it auto-spins, and the zoom (camera
    /// distance multiplier — smaller = closer).
    preview_spin: f32,
    preview_spinning: bool,
    preview_zoom: f32,
    /// Cached image for a selected texture asset: (path, egui handle, dims).
    preview_image: Option<(String, egui::TextureHandle, [usize; 2])>,
    /// The material being previewed/edited when a material asset is selected:
    /// (path, editable Material).
    preview_material: Option<(String, Material)>,
    /// Active editing tool (keys 1-4); drives which gizmo handles are shown.
    tool: Tool,
    /// Cursor position in physical pixels (cached from `CursorMoved`).
    cursor: Option<Vec2>,
    /// Gizmo geometry + hover state, rebuilt every frame.
    gizmo: Option<GizmoFrame>,
    /// The gizmo handle currently being dragged, if any.
    grabbed: Option<Handle>,
    /// Start-of-drag snapshot for the grabbed handle.
    drag: Option<DragState>,
    /// Modifier key state (tracked from key events).
    ctrl: bool,
    shift: bool,
    /// Undo/redo history of whole-scene snapshots.
    history: History,
    /// Copied nodes (Ctrl+C), re-spawned by Ctrl+V.
    clipboard: Vec<floptle_scene::NodeDoc>,
    /// An inspector/gizmo edit session is open — coalesces a drag into one undo step.
    editing: bool,
    /// The pre-edit scene snapshot captured at the start of this frame.
    frame_snapshot: Option<floptle_scene::SceneDoc>,
    /// RMB press position + accumulated motion — distinguishes a look-drag from a
    /// context-menu click.
    rmb_press: Option<Vec2>,
    rmb_moved: f32,
    /// A pending viewport context menu at (screen-point, entity-under-cursor).
    context_menu: Option<(egui::Pos2, Option<Entity>)>,
    /// Reference grid + snap settings.
    grid: GridConfig,
    show_grid_settings: bool,
    /// Project asset tree shown in the bottom file browser.
    asset_tree: Vec<AssetEntry>,
    /// Asset browser view mode: false = file tree, true = icon grid.
    assets_grid: bool,
    /// The folder the icon grid is currently showing (grid view only).
    assets_grid_dir: PathBuf,
    /// Named material presets loaded from assets/materials/.
    materials: Vec<(String, floptle_scene::MaterialDoc)>,
    /// Whether the floating Material Editor window is open.
    show_material_editor: bool,
    /// Scratch buffer for the "save material" name field.
    mat_name_buf: String,
    /// Play mode: scripts run; the pre-play authored scene is restored on stop.
    playing: bool,
    /// The physics sim while playing (built on Play, dropped on Stop).
    sim: Option<floptle_physics::Sim>,
    /// Paused (in play mode): the script clock freezes.
    paused: bool,
    /// Accumulated play-mode seconds (advances only while playing and not paused).
    play_t: f32,
    play_snapshot: Option<SceneDoc>,
    /// The Lua VM that runs node scripts in play mode (ADR-0003).
    script_host: ScriptHost,
    /// Errors from the most recent script frame, shown in the Scripting tab.
    script_errors: Vec<String>,
    /// Syntax diagnostic (line, message) for the active IDE file, for red squiggles.
    ide_diag: Option<(usize, String)>,
    /// The external editor command for "Open in IDE" (ADR-0011); a user preference.
    external_editor: String,
    /// Prefer the external editor over the in-engine IDE for opening scripts.
    prefer_external_editor: bool,
    /// Smoothed frames-per-second + a throttle so the window title isn't rewritten
    /// every frame.
    fps: f32,
    fps_timer: f32,
    /// Active camera focus glide (F), or `None`.
    focus_anim: Option<FocusAnim>,
    /// Asset pending rename: (current path, edited new-name buffer). Drives a modal.
    rename_target: Option<(String, String)>,
    /// New-scene name buffer (Some = the prompt is open).
    new_scene_buf: Option<String>,
    /// The scene has unsaved edits (drives the "save before opening?" prompt).
    scene_dirty: bool,
    /// A scene the user asked to open while there were unsaved changes — the
    /// confirm modal is shown until they Save / Discard / Cancel.
    pending_open_scene: Option<String>,
    last: Option<Instant>,
    started: Option<Instant>,
    gpu: Option<Gpu>,
}

/// One reversible step on the unified timeline. Scene edits store a whole-scene
/// doc; terrain strokes store the field's serialized bytes. Keeping both kinds on
/// one stack means Ctrl+Z walks back through scene + terrain edits in true order.
enum Snapshot {
    Scene(floptle_scene::SceneDoc),
    /// A terrain field snapshot: `(terrain id, serialized field)` — keyed by the
    /// stable id (not Entity) so it survives scene restores.
    Terrain(u32, Vec<u8>),
}

/// Undo/redo stack of whole-scene + terrain snapshots (simple + robust here).
struct History {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Max retained undo steps (a user preference later).
    max: usize,
}

impl Default for History {
    fn default() -> Self {
        Self { undo: Vec::new(), redo: Vec::new(), max: 32 }
    }
}

struct Egui {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
}

/// An imported model's registered GPU mesh parts + its rough world size.
struct MeshAsset {
    parts: Vec<MeshId>,
    size: f32,
}

/// An offscreen target the Inspector renders an asset preview into (a spinning
/// model or a material sphere), exposed to egui as a texture id.
struct PreviewTarget {
    color_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
    tex_id: egui::TextureId,
}

/// What the Inspector preview shows this frame (built from the selected asset).
#[derive(Clone)]
enum PreviewView {
    /// A GPU-rendered spinning subject (model or material sphere).
    Rendered(egui::TextureId),
    /// A loaded image + its pixel dimensions (texture asset).
    Image(egui::TextureHandle, [usize; 2]),
}

impl ApplicationHandler for Editor {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // The default project is the repo's `assets/` folder; File ⏵ Open/New
        // re-points this elsewhere.
        self.project_root = PathBuf::from("assets");
        self.dock_state = Some(default_dock());
        self.viewport_zoom = 0.9;
        self.terrain_detail = 64;
        self.terrain_textures = vec![String::new(); floptle_render::TERRAIN_SLOTS as usize];
        self.external_editor = load_external_editor();
        self.prefer_external_editor = load_prefer_external();
        self.preview_spinning = true;
        self.preview_zoom = 1.0;
        self.assets_grid_dir = self.project_root.clone();
        let attrs = Window::default_attributes()
            .with_title("Floptle Editor")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        let gpu = Gpu::new(window.clone());
        let mut raster = Raster::new(&gpu);
        // Registration order defines the Shape→MeshId mapping (Shape as usize):
        // Cube=0, Sphere=1, Capsule=2.
        let cube_id = raster.register(&gpu, &cube(0.7), None);
        let sphere_id = raster.register(&gpu, &uv_sphere(0.85, 24, 36), None);
        let capsule_id = raster.register(&gpu, &capsule(0.5, 0.5, 16, 24), None);
        self.mesh_ids = vec![cube_id, sphere_id, capsule_id];
        self.raymarch = Some(Raymarch::new(&gpu));

        // Seed the project folder structure + default assets, then load the scene,
        // project settings, materials and asset tree from `project_root`.
        self.seed_project_dirs();
        let (scene_file, doc) = self.load_active_scene();
        self.scene_name = Self::scene_name_of(&scene_file);
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.adopt_terrain();
        self.project = floptle_scene::load_project(&self.project_cfg_path());
        self.asset_tree = build_assets(&self.project_root);
        self.materials = self.load_materials();
        self.load_texture_settings();

        self.retro = Some(Retro::new(&gpu, self.project.retro_height.max(80)));
        self.post = Some(floptle_render::PostStack::new(&gpu, gpu.config.width, gpu.config.height));
        self.outline = Some(Outline::new(&gpu));
        self.grid_render = Some(Grid::new(&gpu));

        let ctx = egui::Context::default();
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let renderer = egui_wgpu::Renderer::new(
            &gpu.device,
            gpu.surface_format(),
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: false,
                predictable_texture_filtering: false,
            },
        );
        self.egui = Some(Egui { ctx, state, renderer });

        self.gpu = Some(gpu);
        self.raster = Some(raster);
        // Register any imported meshes the loaded scene references.
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
        let now = Instant::now();
        self.last = Some(now);
        self.started = Some(now);
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Always feed egui so its widgets stay live. We deliberately IGNORE the
        // returned `consumed` flag: egui_dock paints the whole editor in the
        // Background layer, which makes egui report `consumed == true` for mouse
        // input even over the *transparent* Scene tab — so trusting it would (and
        // previously did) kill viewport look / pick / context-menu entirely. We
        // instead gate viewport actions geometrically via `cursor_over_scene()`,
        // and gate keyboard shortcuts on `typing`, so panels and viewport coexist.
        if let (Some(egui), Some(window)) = (self.egui.as_mut(), self.window.as_ref()) {
            let _ = egui.state.on_window_event(window, &event);
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size.width, size.height);
                    if let Some(retro) = self.retro.as_mut() {
                        retro.resize(gpu, self.project.retro_height.max(80));
                    }
                    if let Some(outline) = self.outline.as_mut() {
                        outline.resize(gpu, size.width, size.height);
                    }
                    if let Some(post) = self.post.as_mut() {
                        post.resize(gpu, size.width, size.height);
                    }
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            // Always cache the cursor (even over the panel) so hit-testing and the
            // over-UI gate stay correct; device_event only gives deltas.
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some(Vec2::new(position.x as f32, position.y as f32));
                // Sculpting is driven each frame in `terrain_frame_update` (which
                // spaces the dabs by cursor movement), so motion needs nothing here.
            }
            // Modifier state, tracked separately so Ctrl/Shift combos work even while
            // a field is focused (this event isn't gated by `consumed`).
            WindowEvent::ModifiersChanged(mods) => {
                self.ctrl = mods.state().control_key();
                self.shift = mods.state().shift_key();
                self.input.boost = self.shift;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                // Don't trigger shortcuts/tools (or fly the camera) while typing
                // into a field. `typing` is read live each event.
                let typing = self.egui.as_ref().is_some_and(|e| e.ctx.egui_wants_keyboard_input());
                if let PhysicalKey::Code(code) = event.physical_key {
                    // Held movement keys. The bit is `pressed && !typing && !ctrl`:
                    // a RELEASE (pressed == false) always clears it, so a key can
                    // never stick on if the release lands while a field is focused
                    // (e.g. hold W, click into the IDE, release W). C moves DOWN.
                    let mv = pressed && !typing;
                    match code {
                        KeyCode::KeyW => self.input.forward = mv && !self.ctrl,
                        KeyCode::KeyS => self.input.back = mv && !self.ctrl,
                        KeyCode::KeyA => self.input.left = mv && !self.ctrl,
                        KeyCode::KeyD => self.input.right = mv && !self.ctrl,
                        KeyCode::Space => self.input.up = mv,
                        KeyCode::KeyC => self.input.down = mv && !self.ctrl,
                        _ => {}
                    }
                    // Track raw key state for the script `input` API (works in play
                    // mode regardless of which panel has focus).
                    if let Some(name) = key_name(code) {
                        if pressed {
                            if self.input_keys.insert(name.to_string()) {
                                self.input_keys_pressed.insert(name.to_string());
                            }
                        } else {
                            self.input_keys.remove(name);
                        }
                    }
                    // Discrete commands fire on press only.
                    if pressed && !typing {
                        if self.ctrl {
                            match code {
                                KeyCode::KeyZ => self.undo(),
                                KeyCode::KeyY => self.redo(),
                                KeyCode::KeyC => self.copy_selected(),
                                KeyCode::KeyV => self.paste(),
                                KeyCode::KeyD => self.duplicate_selected(),
                                KeyCode::KeyA => self.select_all(),
                                KeyCode::KeyS => self.save_all(),
                                _ => {}
                            }
                        } else {
                            match code {
                                KeyCode::Escape => event_loop.exit(),
                                KeyCode::Delete | KeyCode::Backspace => self.delete_selected(),
                                KeyCode::KeyF => self.focus_selected(),
                                KeyCode::KeyQ => self.selection.clear(), // unselect
                                KeyCode::KeyG => self.grid.show = !self.grid.show, // toggle grid
                                KeyCode::ArrowUp => self.step_selection(-1),
                                KeyCode::ArrowDown => self.step_selection(1),
                                KeyCode::Enter | KeyCode::NumpadEnter => self.toggle_folder_selected(),
                                KeyCode::F1 => self.toggle_play(),
                                KeyCode::F2 => self.toggle_pause(),
                                _ => {
                                    if let Some(t) = digit_of(code).and_then(Tool::from_digit) {
                                        self.set_tool(t);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                // Gated geometrically: `cursor_over_scene()` is true only over the bare
                // viewport, so a press on a panel/toolbar falls through to egui untouched.
                let pressed = state == ElementState::Pressed;
                self.track_mouse_button(0, pressed);
                if pressed {
                    let over_scene = self.cursor_over_scene();
                    let hovered = self.gizmo.as_ref().and_then(|g| g.hovered);
                    if over_scene && self.tool == Tool::Sculpt {
                        // Sculpt tool: start a brush stroke on the terrain (applied
                        // next frame in terrain_frame_update).
                        self.context_menu = None;
                        if !self.terrains.is_empty() {
                            self.sculpting = true;
                            self.last_dab_pos = None; // first dab fires immediately
                            self.last_dab_time = None;
                            // The pre-stroke field is captured on the first dab (once
                            // we know which terrain is under the cursor).
                            self.stroke_snapshot = None;
                            self.stroke_dabbed = false;
                        }
                    } else if over_scene {
                        // Clicking the viewport dismisses an open context menu (but
                        // clicking a panel/menu, which isn't over_scene, keeps it).
                        self.context_menu = None;
                        if let (Some(h), Some(e)) = (hovered, self.primary()) {
                            // On a gizmo handle ⏵ start an undoable edit and grab it.
                            // start_xf is the WORLD transform; gizmo math runs in world
                            // space and is converted back to local on write (parenting).
                            if self.world.get::<Transform>(e).is_some() {
                                let start_xf = floptle_core::world_transform(&self.world, e);
                                self.begin_edit();
                                self.grabbed = Some(h);
                                self.drag = Some(DragState {
                                    handle: h,
                                    entity: e,
                                    start_xf,
                                    cursor_start: self.cursor.unwrap_or(Vec2::ZERO),
                                });
                            }
                        } else if let Some(cursor) = self.cursor {
                            // Empty viewport ⏵ pick: single-select, or Shift to add.
                            match self.pick(cursor) {
                                Some(e) if self.shift => self.select_toggle(e),
                                Some(e) => self.select_single(e),
                                None if !self.shift => self.selection.clear(),
                                None => {}
                            }
                        }
                    }
                } else {
                    self.grabbed = None;
                    self.drag = None;
                    self.editing = false;
                    self.sculpting = false;
                    // End of a sculpt stroke: bank one undo step if it changed anything.
                    if let Some((id, snap)) = self.stroke_snapshot.take() {
                        if self.stroke_dabbed {
                            self.push_history(Snapshot::Terrain(id, snap));
                        }
                    }
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Middle, .. } => {
                self.track_mouse_button(2, state == ElementState::Pressed);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.input_scroll += match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y,
                    winit::event::MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                };
            }
            WindowEvent::MouseInput { state, button: MouseButton::Right, .. } => {
                let pressed = state == ElementState::Pressed;
                self.track_mouse_button(1, pressed);
                let over_scene = self.cursor_over_scene();
                if pressed {
                    // Begin a possible look; if the cursor barely moves before release
                    // it's a click ⏵ open a context menu instead.
                    self.rmb_press = self.cursor;
                    self.rmb_moved = 0.0;
                    self.context_menu = None;
                    if over_scene {
                        self.input.looking = true;
                        if let Some(window) = self.window.as_ref() {
                            let _ = window
                                .set_cursor_grab(CursorGrabMode::Confined)
                                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Locked));
                            window.set_cursor_visible(false);
                        }
                        self.cursor = None;
                    }
                } else {
                    let was_looking = self.input.looking;
                    self.input.looking = false;
                    if let Some(window) = self.window.as_ref() {
                        let _ = window.set_cursor_grab(CursorGrabMode::None);
                        window.set_cursor_visible(true);
                    }
                    // A click (negligible motion) over the viewport ⏵ context menu.
                    if was_looking && self.rmb_moved < 6.0 {
                        if let Some(p) = self.rmb_press {
                            self.cursor = Some(p);
                            let ppp = self
                                .egui
                                .as_ref()
                                .map(|e| e.ctx.pixels_per_point())
                                .unwrap_or(1.0);
                            let hit = self.pick(p);
                            if let Some(e) = hit {
                                if self.shift {
                                    self.select_toggle(e);
                                } else if !self.selection.contains(&e) {
                                    self.select_single(e);
                                }
                            }
                            self.context_menu =
                                Some((egui::Pos2::new(p.x / ppp, p.y / ppp), hit));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            // Accumulate raw mouse delta for the script `input` API.
            self.input_mouse_delta.0 += delta.0 as f32;
            self.input_mouse_delta.1 += delta.1 as f32;
            // Priority: RMB-look > grabbed gizmo handle. (Free dragging an object now
            // requires the Move tool's center handle — no more accidental moves.)
            if self.input.looking {
                self.camera.look(delta.0 as f32, delta.1 as f32);
                self.rmb_moved += (delta.0.abs() + delta.1.abs()) as f32;
            } else if self.grabbed.is_some() {
                self.gizmo_drag();
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }
}

impl Editor {
    fn render(&mut self) {
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
        self.ide_diag = self.ide.active.and_then(|i| self.ide.open.get(i)).and_then(|f| {
            if f.path.ends_with(".lua") {
                self.script_host.check_syntax(&f.text)
            } else {
                None
            }
        });

        // A terrain node moved (gizmo/inspector/undo) → rebuild the combined field.
        if !self.combined_dirty && self.terrains_moved() {
            self.combined_dirty = true;
        }
        // Rebuild the combined terrain (fold all nodes' fields) + re-upload to the GPU
        // after any add/edit/move/delete (needs &mut Raymarch, before the destructure).
        if self.combined_dirty {
            self.rebuild_combined();
            if let (Some(gpu), Some(raymarch), Some(combined)) =
                (self.gpu.as_ref(), self.raymarch.as_mut(), self.combined.as_ref())
            {
                raymarch.set_volume(gpu, &combined.baked);
            }
            self.combined_dirty = false;
            self.terrain_region_dirty = None; // the full upload supersedes any region
            self.terrain_wire_world.clear(); // terrain changed → rebuild the wireframe
        } else if let Some((e, mn, mx)) = self.terrain_region_dirty.take() {
            // Fast paint path: upload only the dabbed voxel box. For a single terrain the
            // GPU volume mirrors its field 1:1, so its baked data maps directly. (`combined`
            // keeps the correct geometry — paint never touches distance — and is re-cloned
            // on the next full rebuild.)
            if let (Some(gpu), Some(raymarch), Some(t)) =
                (self.gpu.as_ref(), self.raymarch.as_mut(), self.terrains.get(&e))
            {
                raymarch.set_volume_region(gpu, &t.baked, mn, mx);
            }
        }
        // Re-upload the terrain texture palette when it changes. Each slot resolves
        // to a 256² layer (empty / unreadable slots become white so indices align).
        if self.terrain_textures_dirty {
            let layers: Vec<floptle_render::TextureData> = self
                .terrain_textures
                .iter()
                .map(|p| {
                    if !p.is_empty() {
                        if let Some(t) = floptle_assets::load_texture_sized(Path::new(p), 256, 256) {
                            return t;
                        }
                    }
                    floptle_render::TextureData { pixels: vec![255; 256 * 256 * 4], width: 256, height: 256 }
                })
                .collect();
            if let (Some(gpu), Some(raymarch)) = (self.gpu.as_ref(), self.raymarch.as_mut()) {
                raymarch.set_terrain_textures(gpu, &layers);
            }
            self.terrain_textures_dirty = false;
        }

        // Inspector camera POV preview: if a Camera node is selected, render the scene
        // from its viewpoint into the 16:9 offscreen target (before the destructure).
        let cam_elapsed = self.started.map(|s| s.elapsed().as_secs_f32()).unwrap_or(0.0);
        self.update_camera_preview(cam_elapsed);

        let (
            Some(gpu),
            Some(raster),
            Some(raymarch),
            Some(retro),
            Some(outline),
            Some(grid_render),
            Some(post),
            Some(egui),
            Some(window),
        ) = (
            self.gpu.as_mut(),
            self.raster.as_mut(),
            self.raymarch.as_ref(),
            self.retro.as_mut(),
            self.outline.as_ref(),
            self.grid_render.as_mut(),
            self.post.as_ref(),
            self.egui.as_mut(),
            self.window.as_ref(),
        ) else {
            return;
        };
        let window = window.clone();

        let now = Instant::now();
        let dt = self.last.map(|l| (now - l).as_secs_f32()).unwrap_or(0.0);
        self.last = Some(now);
        let elapsed = self.started.map(|s| (now - s).as_secs_f32()).unwrap_or(0.0);
        self.camera.update(&self.input, dt);

        // FPS in the window title (smoothed, refreshed a few times a second).
        if dt > 0.0 {
            let inst = 1.0 / dt;
            self.fps = if self.fps > 0.0 { self.fps * 0.9 + inst * 0.1 } else { inst };
            self.fps_timer += dt;
            if self.fps_timer >= 0.4 {
                self.fps_timer = 0.0;
                window.set_title(&format!("Floptle Editor — {:.0} fps", self.fps));
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

        // Capture this frame's pre-edit scene, so an inspector/gizmo edit can push it
        // as a single undo step (see `begin_edit`). Inlined (not via `self.snapshot()`)
        // so it only touches disjoint fields while gpu/egui are borrowed. Not while
        // playing — script-driven transforms must not enter the undo history.
        if !self.playing {
            self.frame_snapshot =
                Some(floptle_scene::to_doc(self.scene_name.clone(), &self.world));
        }

        // Play mode: advance the (pausable) script clock and run the Lua scripts
        // attached to nodes (ADR-0003). Scripts hot-reload as their files change.
        if self.playing {
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
            // Feed the player input to scripts (the Lua `input` API).
            self.script_host.set_input(floptle_script::InputSnapshot {
                keys_down: self.input_keys.clone(),
                keys_pressed: self.input_keys_pressed.clone(),
                mouse: self.cursor.map(|c| (c.x, c.y)).unwrap_or((0.0, 0.0)),
                mouse_delta: self.input_mouse_delta,
                scroll: self.input_scroll,
                buttons_down: self.input_buttons,
                buttons_pressed: self.input_buttons_pressed,
            });
            self.script_host.run(&mut self.world, &dir, sdt, self.play_t);
            self.script_errors = self.script_host.errors().to_vec();
            // Apply script velocity writes, then advance physics (writes transforms back).
            if let Some(sim) = self.sim.as_mut() {
                for (eid, v) in self.script_host.take_body_changes() {
                    sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
                }
                for (eid, h) in self.script_host.take_body_height_changes() {
                    sim.set_body_height(eid, h);
                }
                sim.advance(&mut self.world, sdt);
            }
        } else if !self.script_errors.is_empty() {
            self.script_errors.clear();
        }
        // Clear per-frame input edges after scripts consumed them.
        self.input_keys_pressed.clear();
        self.input_buttons_pressed = [false; 3];
        self.input_mouse_delta = (0.0, 0.0);
        self.input_scroll = 0.0;
        // Drain any script logs/errors into the Console (consecutive dups merge).
        for l in self.script_host.drain_logs() {
            self.console.push(l.level, l.msg, l.source);
        }

        // ---- gather the scene from the World ----
        let aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
        // The Game dock tab being front = render from the active camera node; otherwise
        // (Scene tab) use the editor's free-fly camera. Works whether or not we're
        // playing, so you can frame the active camera's shot without entering play.
        // (Inlined — self methods can't be called while gpu/egui are borrowed.)
        let game_view = self.dock_state.as_ref().is_some_and(game_tab_active);
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
        if !game_view {
            let (gw, gh) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
            // Only cameras and point lights get gizmos — gather the few Copy fields we
            // need (no per-frame Matter clone over the whole world).
            enum Giz {
                Cam(f32, bool),
                Light(f32),
                Gravity(bool, f32), // radial?, radius
            }
            let gizmos: Vec<(Entity, Giz)> = self
                .world
                .query::<Matter>()
                .filter_map(|(e, m)| match m {
                    Matter::Camera { fov_y, active } => Some((e, Giz::Cam(*fov_y, *active))),
                    Matter::PointLight { range, .. } => Some((e, Giz::Light(*range))),
                    Matter::GravityVolume { mode, radius, .. } => {
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
            // Rigidbody collider outlines, so physics bodies are visible/placeable.
            let bodies: Vec<(Entity, floptle_core::RigidBody)> =
                self.world.query::<floptle_core::RigidBody>().map(|(e, rb)| (e, *rb)).collect();
            for (e, rb) in bodies {
                let p = floptle_core::world_transform(&self.world, e).translation;
                let lines = rigidbody_lines(
                    p,
                    rb.kind == floptle_core::BodyKind::Capsule,
                    rb.radius,
                    rb.height,
                    cam.world_position,
                    view_proj,
                    gw,
                    gh,
                );
                if !lines.is_empty() {
                    self.body_gizmos.push(lines);
                }
            }
            // Collision telegraph: a small cross at each contact resolved this step.
            if let Some(sim) = self.sim.as_ref() {
                let cs = 0.15;
                for c in &sim.world.contacts {
                    let cp = DVec3::new(c.point.x as f64, c.point.y as f64, c.point.z as f64);
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
            // Terrain collider wireframe (the SDF surface you walk on). The world-space
            // segments are cached + rebuilt only when the terrain changes; here we just
            // re-project them. Coarseness scales with the grid so the line count stays sane.
            if self.show_terrain_collider {
                if self.terrain_wire_world.is_empty() {
                    if let Some(c) = self.combined.as_ref() {
                        let stride = (c.baked.dims.into_iter().max().unwrap_or(64) / 48).max(2);
                        self.terrain_wire_world = terrain_collider_wire(c, stride);
                    }
                }
                for &(a, b) in &self.terrain_wire_world {
                    let wa = DVec3::new(a.x as f64, a.y as f64, a.z as f64);
                    let wb = DVec3::new(b.x as f64, b.y as f64, b.z as f64);
                    if let (Some(pa), Some(pb)) = (
                        project(wa, cam.world_position, view_proj, gw, gh),
                        project(wb, cam.world_position, view_proj, gw, gh),
                    ) {
                        self.terrain_wire_gizmo.push((pa, pb));
                    }
                }
            }
        }

        // Rebuild the overlay gizmo for the selected object (projects + hit-tests).
        self.gizmo = build_gizmo(
            self.tool,
            self.selection.last().copied(),
            &self.world,
            self.cursor,
            cam.world_position,
            view_proj,
            gpu.config.width as f32,
            gpu.config.height.max(1) as f32,
        );

        // Lighting comes from the scene's mandatory Lighting node (a Light component).
        let light_node = self.world.query::<Light>().next().map(|(_, l)| *l).unwrap_or_default();
        let light = Vec3::from(light_node.direction).normalize_or_zero();
        let li = light_node.intensity;
        let (pl_count, pl_pos, pl_col) = collect_point_lights(&self.world, cam.world_position);
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

        let ents: Vec<(Entity, Matter)> =
            self.world.query::<Matter>().map(|(e, m)| (e, m.clone())).collect();
        let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
        let mut blobs: Vec<(DVec3, f32, MaterialParams)> = Vec::new();
        if let Some((path, pos)) = &drag_ghost {
            if let Some(asset) = self.mesh_registry.get(path) {
                let ghost = Transform { translation: *pos, ..Transform::default() };
                let model = ghost.render_matrix(cam.world_position);
                for &mid in &asset.parts {
                    instances.push((mid, None, instance_of(model, [0.7, 0.85, 1.0])));
                }
            }
        }
        for (e, matter) in &ents {
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
            match matter {
                Matter::Primitive { shape, color } => {
                    if let Some(&mesh) = self.mesh_ids.get(*shape as usize) {
                        let model = t.render_matrix(cam.world_position);
                        let mp = mat.as_ref().map(material_params).unwrap_or_else(|| MaterialParams::flat(*color));
                        instances.push((mesh, tex, instance_of_mat(model, &mp)));
                    }
                }
                Matter::Blob { scale } => {
                    let mp = mat.as_ref().map(material_params).unwrap_or_else(blob_default_material);
                    blobs.push((t.translation, scale * t.scale.x, mp));
                }
                Matter::Mesh { asset_path } => {
                    if let Some(asset) = self.mesh_registry.get(asset_path) {
                        let model = t.render_matrix(cam.world_position);
                        let mp = mat.as_ref().map(material_params).unwrap_or_else(|| MaterialParams::flat([1.0, 1.0, 1.0]));
                        for &mid in &asset.parts {
                            instances.push((mid, tex, instance_of_mat(model, &mp)));
                        }
                    }
                }
                // group / terrain / camera / light / gravity render via other passes.
                Matter::Empty
                | Matter::Terrain { .. }
                | Matter::Camera { .. }
                | Matter::PointLight { .. }
                | Matter::GravityVolume { .. } => {}
            }
        }

        let clear = [0.02f32, 0.02, 0.05, 1.0];
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
                vol_center: [0.0, 0.0, 0.0, 0.0], // no baked volume in v1
                vol_half: [1.0, 1.0, 1.0, 0.5],
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
            }
        };

        // Selection outline source: the selected object's silhouette into the mask —
        // a mesh instance, or (for a blob) a one-blob raymarch so the outline hugs
        // only the selected blob.
        let mut mask_mesh: Vec<(MeshId, InstanceRaw)> = Vec::new();
        let mut mask_blob: Option<RaymarchGlobals> = None;
        if let Some(e) = self.selection.last().copied() {
            if let Some(m) = self.world.get::<Matter>(e) {
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
                            for &mid in &asset.parts {
                                mask_mesh.push((mid, instance_of(model, [1.0, 1.0, 1.0])));
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
                    Matter::Empty
                    | Matter::Terrain { .. }
                    | Matter::Camera { .. }
                    | Matter::PointLight { .. }
                    | Matter::GravityVolume { .. } => {}
                }
            }
        }

        // The raymarch pass renders the blob matter (gated by the SDF-matter toggle)
        // and/or the combined terrain volume. Build its globals if either is present.
        let show_blobs = self.project.matter && !blobs.is_empty();
        let rm = if show_blobs || self.combined.is_some() {
            let mut g = make_rm(if show_blobs { &blobs } else { &[] });
            if let Some((hf, bc)) =
                self.combined.as_ref().map(|t| (t.baked.half_extent, t.baked.center))
            {
                // The combined field is WORLD-space (its node sits at world origin),
                // so the box center is just its `baked.center`, camera-relative.
                let cr = DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - cam.world_position;
                g.vol_center = [cr.x as f32, cr.y as f32, cr.z as f32, 1.0]; // present
                g.vol_half = [hf[0], hf[1], hf[2], 0.1];
            }
            Some(g)
        } else {
            None
        };

        // ---- build the egui UI (mutating the World) ----
        let raw_input = egui.state.take_egui_input(&window);
        let ctx = egui.ctx.clone();
        // Every named entity, Matter nodes and the Lighting node alike.
        let entity_names: Vec<(Entity, String)> =
            self.world.query::<Name>().map(|(e, n)| (e, n.0.clone())).collect();
        let old_retro_h = self.project.retro_height;
        let ppp = ctx.pixels_per_point();
        let dock_state = self.dock_state.get_or_insert_with(default_dock);
        let world = &mut self.world;
        let selection = &mut self.selection;
        let collapsed = &mut self.collapsed;
        let console = &mut self.console;
        let preview_zoom = &mut self.preview_zoom;
        let preview_spin = &mut self.preview_spin;
        let preview_spinning = &mut self.preview_spinning;
        let preview_material = &mut self.preview_material;
        let project = &mut self.project;
        let show_project_settings = &mut self.show_project_settings;
        let show_project_mgr = &mut self.show_project_mgr;
        let project_path_buf = &mut self.project_path_buf;
        let grid = &mut self.grid;
        let show_grid_settings = &mut self.show_grid_settings;
        let show_terrain_collider = &mut self.show_terrain_collider;
        let rename_target = &mut self.rename_target;
        let new_scene_buf = &mut self.new_scene_buf;
        let pending_open_scene = &mut self.pending_open_scene;
        let terrain_brush = &mut self.terrain_brush;
        let terrain_detail = &mut self.terrain_detail;
        let terrain_textures = &mut self.terrain_textures;
        let terrain_present = !self.terrains.is_empty();
        let terrain_voxels = self.combined.as_ref().map(|t| {
            let [a, b, c] = t.baked.dims;
            (a, b, c)
        });
        let external_editor = &mut self.external_editor;
        let prefer_external = &mut self.prefer_external_editor;
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
        let materials = &self.materials;
        let mat_name_buf = &mut self.mat_name_buf;
        let show_material_editor = &mut self.show_material_editor;
        let ide = &mut self.ide;
        let script_errors = self.script_errors.as_slice();
        let ide_diag = self.ide_diag.as_ref();
        let selected_asset = &mut self.selected_asset;
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
        let terrain_wire = self.terrain_wire_gizmo.as_slice();
        let grabbed = self.grabbed;
        let tool = self.tool;
        let context_menu = self.context_menu;
        let mut cmd = EditorCmd::default();
        let mut want_save = false;
        let mut want_save_project = false;
        let full_output = ctx.run_ui(raw_input, |ui| {
            // ---- top menu bar ----
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
                        if ui.button("Exit").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                    ui.menu_button("Edit", |ui| {
                        if ui.button("Undo  (Ctrl+Z)").clicked() { cmd.undo = true; ui.close(); }
                        if ui.button("Redo  (Ctrl+Y)").clicked() { cmd.redo = true; ui.close(); }
                        ui.separator();
                        if ui.button("Copy  (Ctrl+C)").clicked() { cmd.copy = true; ui.close(); }
                        if ui.button("Paste  (Ctrl+V)").clicked() { cmd.paste = true; ui.close(); }
                        if ui.button("Duplicate  (Ctrl+D)").clicked() { cmd.duplicate = true; ui.close(); }
                        if ui.button("Delete  (Del)").clicked() { cmd.delete = true; ui.close(); }
                        ui.separator();
                        if ui.button("Project Settings…").clicked() {
                            *show_project_settings = true;
                            ui.close();
                        }
                    });
                    ui.menu_button("Add", |ui| {
                        if ui.button("Cube").clicked() { cmd.add = Some(new_cube()); ui.close(); }
                        if ui.button("Sphere").clicked() { cmd.add = Some(new_sphere()); ui.close(); }
                        if ui.button("Blob").clicked() {
                            cmd.add = Some(MatterDoc::Blob { scale: 1.0 });
                            ui.close();
                        }
                    });
                    ui.menu_button("View", |ui| {
                        ui.checkbox(&mut grid.show, "Grid");
                        ui.checkbox(&mut grid.snap, "Snap to grid");
                        if ui.button("Grid Settings…").clicked() {
                            *show_grid_settings = true;
                            ui.close();
                        }
                        ui.separator();
                        ui.checkbox(&mut *show_material_editor, "Material Editor");
                        ui.checkbox(&mut *show_terrain_collider, "Terrain collider wireframe")
                            .on_hover_text("show the terrain's collision surface (what the player walks on)");
                        if ui.button("Δ Terrain tools").clicked() {
                            cmd.focus_terrain = true;
                            ui.close();
                        }
                    });
                    ui.menu_button("Project", |ui| {
                        if ui.button("Settings…").clicked() {
                            *show_project_settings = true;
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
                    // The view is now chosen by the Scene / Game dock tabs (the editor
                    // free-fly view vs the active-camera gameplay view), not a toggle here.
                });
            });

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
                collapsed,
                console,
                preview: preview_view.clone(),
                preview_zoom,
                preview_spin,
                preview_spinning,
                preview_material,
                entity_names: &entity_names,
                materials,
                mat_name_buf,
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
                ide,
                script_errors,
                ide_diag,
                gizmo,
                terrain_viz,
                camera_gizmos,
                light_gizmos,
                body_gizmos,
                contact_gizmos,
                terrain_wire,
                grabbed,
                tool,
                scene_rect: &mut *scene_rect,
                aspect: aspect_mode,
                zoom: viewport_zoom,
                scene_name: &scene_name,
                ppp,
                cmd: &mut cmd,
            };
            egui_dock::DockArea::new(dock_state)
                .style(egui_dock::Style::from_egui(ui.style()))
                .show_inside(ui, &mut viewer);

            // Viewport drop: spawn a model when an asset is released over the Scene
            // tab (panel drops — script-on-node — are consumed by those tabs first).
            // No opaque region is allocated, so the viewport never greys mid-drag.
            if egui::DragAndDrop::has_payload_of_type::<AssetPayload>(ui.ctx())
                && ui.input(|i| i.pointer.any_released())
            {
                let pos = ui.input(|i| i.pointer.interact_pos());
                let over_scene = matches!((pos, *scene_rect), (Some(p), Some(r)) if r.contains(p));
                if over_scene {
                    if let Some(p) = egui::DragAndDrop::take_payload::<AssetPayload>(ui.ctx()) {
                        cmd.drop_asset = Some(p.path.clone());
                    }
                }
            }

            // ---- project settings window (project-wide rendering) ----
            egui::Window::new("Project Settings")
                .open(show_project_settings)
                .resizable(false)
                .default_width(280.0)
                .show(ui.ctx(), |ui| {
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
                    ui.label("Post-processing");
                    ui.separator();
                    if ui.checkbox(&mut project.bloom, "Bloom").changed() {
                        want_save_project = true;
                    }
                    if project.bloom {
                        want_save_project |= ui
                            .add(egui::Slider::new(&mut project.bloom_threshold, 0.0..=2.0).text("threshold"))
                            .changed();
                        want_save_project |= ui
                            .add(egui::Slider::new(&mut project.bloom_intensity, 0.0..=2.0).text("intensity"))
                            .changed();
                    }
                    if ui.checkbox(&mut project.vignette, "Vignette").changed() {
                        want_save_project = true;
                    }
                    if project.vignette {
                        want_save_project |= ui
                            .add(egui::Slider::new(&mut project.vignette_strength, 0.0..=1.0).text("strength"))
                            .changed();
                        want_save_project |= ui
                            .add(egui::Slider::new(&mut project.vignette_radius, 0.3..=1.0).text("radius"))
                            .changed();
                    }

                    ui.add_space(6.0);
                    ui.small("saved to assets/project.ron");

                    ui.add_space(10.0);
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
                });

            // ---- grid settings window ----
            egui::Window::new("Grid Settings")
                .open(show_grid_settings)
                .resizable(false)
                .default_width(240.0)
                .show(ui.ctx(), |ui| {
                    ui.checkbox(&mut grid.show, "show grid");
                    ui.checkbox(&mut grid.snap, "snap objects to grid");
                    ui.add(egui::Slider::new(&mut grid.size, 0.1..=10.0).text("cell size"));
                    ui.add(egui::Slider::new(&mut grid.extent, 4..=120).text("extent (cells)"));
                    ui.add(
                        egui::Slider::new(&mut grid.y_offset, 0.0..=50.0)
                            .text("drop below camera")
                            .suffix(" m"),
                    );
                    ui.add(egui::Slider::new(&mut grid.alpha, 0.0..=1.0).text("opacity"));
                    ui.horizontal(|ui| {
                        ui.label("color");
                        ui.color_edit_button_rgb(&mut grid.color);
                    });
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
                                if ui.button("Duplicate").clicked() {
                                    cmd.duplicate = true;
                                    cmd.close_menu = true;
                                }
                                if ui.button("Copy").clicked() {
                                    cmd.copy = true;
                                    cmd.close_menu = true;
                                }
                                if ui.button("Delete").clicked() {
                                    cmd.delete = true;
                                    cmd.close_menu = true;
                                }
                                ui.separator();
                            }
                            if ui.button("Paste").clicked() {
                                cmd.paste = true;
                                cmd.close_menu = true;
                            }
                            ui.menu_button("Add", |ui| {
                                if ui.button("Cube").clicked() {
                                    cmd.add = Some(new_cube());
                                    cmd.close_menu = true;
                                    ui.close();
                                }
                                if ui.button("Sphere").clicked() {
                                    cmd.add = Some(new_sphere());
                                    cmd.close_menu = true;
                                    ui.close();
                                }
                                if ui.button("Blob").clicked() {
                                    cmd.add = Some(MatterDoc::Blob { scale: 1.0 });
                                    cmd.close_menu = true;
                                    ui.close();
                                }
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
                egui::Window::new("Rename asset")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(320.0)
                    .show(ui.ctx(), |ui| {
                        ui.small(path.as_str());
                        let edit = ui.add(
                            egui::TextEdit::singleline(buf).desired_width(280.0).hint_text("new name"),
                        );
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

        // Post-processing (bloom/vignette) runs at full frame res after the scene is
        // composited (and after any retro downsample), before the outline + egui.
        let post_settings = floptle_render::PostSettings {
            bloom: self.project.bloom,
            bloom_threshold: self.project.bloom_threshold,
            bloom_intensity: self.project.bloom_intensity,
            vignette: self.project.vignette,
            vignette_strength: self.project.vignette_strength,
            vignette_radius: self.project.vignette_radius,
        };
        let post_on = post_settings.any();

        // ---- draw: scene into the retro target, blit, then egui on top ----
        match gpu.acquire() {
            Some(frame) => {
                let (color, depth) = if self.project.retro {
                    (retro.color_view(), retro.depth_view())
                } else if post_on {
                    // Non-retro + post: render the scene into the post input target.
                    (post.input_view(), gpu.depth_view())
                } else {
                    (&frame.view, gpu.depth_view())
                };
                // `rm` already accounts for the matter toggle + terrain presence.
                let raster_clear = if let Some(rm) = rm {
                    raymarch.draw_into(gpu, color, depth, rm);
                    None
                } else {
                    Some(clear.map(|c| c as f64))
                };
                raster.draw_scene(gpu, color, depth, globals, &instances, raster_clear);
                if self.grid.show {
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
                // Retro upscales the low-res scene; into the post input if post is on,
                // else straight to the frame. Then post (if on) writes to the frame.
                if self.project.retro {
                    if post_on {
                        retro.blit_to(gpu, post.input_view());
                    } else {
                        retro.blit(gpu, &frame);
                    }
                }
                if post_on {
                    post.run(gpu, &post_settings, &frame.view);
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
        if want_save_project {
            if let Err(e) = floptle_scene::save_project(&self.project, &self.project_cfg_path()) {
                eprintln!("  save project failed: {e}");
            }
        }

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
        if cmd.focus_scripting {
            if let Some(dock) = self.dock_state.as_mut() {
                focus_scripting_tab(dock);
            }
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
                MatterDoc::Blob { .. } => "Blob",
                MatterDoc::Mesh { .. } => "Mesh",
                MatterDoc::Empty => "Group",
                MatterDoc::Terrain { .. } => "Terrain",
                MatterDoc::Camera { .. } => "Camera",
                MatterDoc::PointLight { .. } => "Point Light",
                MatterDoc::GravityVolume { .. } => "Gravity Volume",
            };
            self.add_node(name, m);
        }
        if cmd.inspector_changed {
            self.begin_edit();
        }
        if cmd.toggle_play {
            self.toggle_play();
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
        }
        if let Some(e) = cmd.remove_rigidbody {
            self.record();
            self.world.remove::<floptle_core::RigidBody>(e);
        }
        if let Some((e, on)) = cmd.set_mesh_collider {
            self.record();
            if on {
                self.world.insert(e, floptle_core::MeshCollider);
            } else {
                self.world.remove::<floptle_core::MeshCollider>(e);
            }
        }
        if let Some((e, name)) = cmd.apply_preset {
            if let Some((_, doc)) = self.materials.iter().find(|(n, _)| n == &name) {
                let mat = doc.to_material();
                self.record();
                self.world.insert(e, mat);
            }
        }
        if let Some(path) = cmd.extract_textures {
            self.extract_textures(&path);
        }
        if let Some((child, parent)) = cmd.reparent {
            self.reparent(child, parent);
        }
        if let Some((matter, parent)) = cmd.add_parented {
            self.add_parented(matter, parent);
        }
        if cmd.create_terrain {
            self.create_terrain();
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
                self.combined = None;
                self.combined_dirty = true;
            }
        }
        if cmd.terrain_palette_changed {
            self.terrain_textures_dirty = true;
        }
        if let Some(fill) = cmd.fill_terrain {
            if let Some(e) = self.target_terrain() {
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
                    self.combined_dirty = true;
                }
            }
        }
        if cmd.fill_bounds {
            if let Some(e) = self.target_terrain() {
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
                    self.combined_dirty = true;
                }
            }
        }
        if cmd.focus_terrain {
            self.focus_terrain();
        }
        if let Some(path) = cmd.open_scene {
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
        }
        if let Some(dir) = cmd.new_folder_in {
            self.new_folder(&dir);
        }
        if let Some(dir) = cmd.new_script_in {
            self.new_script(&dir);
        }
        if let Some(path) = cmd.rename_asset {
            // Seed the rename modal with the current file/folder name.
            let name = Path::new(&path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            self.rename_target = Some((path, name));
        }
        if let Some((from, to)) = cmd.do_rename {
            self.rename_asset(&from, &to);
        }
        if let Some(path) = cmd.delete_asset {
            self.delete_asset(&path);
        }
        // Pre-warm a model being dragged so its live ghost can render next frame
        // (the gather can't import — gpu/raster are borrowed there).
        if let Some(p) =
            self.egui.as_ref().and_then(|e| egui::DragAndDrop::payload::<AssetPayload>(&e.ctx))
        {
            if is_model(&p.path) && !self.mesh_registry.contains_key(&p.path) {
                let path = p.path.clone();
                self.import_model(&path);
            }
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

    /// Decode a model's embedded textures and write them to `<project>/textures/`
    /// as PNGs (so they can be reused as material textures — e.g. a grass material
    /// from the retro map). Refreshes the asset tree.
    fn extract_textures(&mut self, model_path: &str) {
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
    fn open_script_preferred(&mut self, path: &str) {
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
    fn open_source_at(&mut self, name: &str, line: u32) {
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
    fn ensure_texture(&mut self, path: &str) -> Option<TexId> {
        let want = self.texture_settings.get(path).copied().unwrap_or_default();
        if let (Some(id), Some(prev)) =
            (self.texture_registry.get(path), self.texture_registry_setting.get(path))
        {
            if *prev == want {
                return Some(*id);
            }
        }
        let data = floptle_assets::load_texture(Path::new(path))?;
        let (gpu, raster) = (self.gpu.as_ref()?, self.raster.as_mut()?);
        let id = raster.register_texture(gpu, &data, want.to_sampling());
        self.texture_registry.insert(path.to_string(), id);
        self.texture_registry_setting.insert(path.to_string(), want);
        Some(id)
    }

    /// Persist the per-texture sampling settings to `.floptle/textures.ron`.
    fn save_texture_settings(&self) {
        let dir = self.project_root.join(".floptle");
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(s) = ron::ser::to_string_pretty(&self.texture_settings, Default::default()) {
            let _ = std::fs::write(dir.join("textures.ron"), s);
        }
    }

    /// Load the per-texture sampling settings from `.floptle/textures.ron` (if present).
    fn load_texture_settings(&mut self) {
        let path = self.project_root.join(".floptle").join("textures.ron");
        self.texture_settings = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| ron::from_str(&s).ok())
            .unwrap_or_default();
    }

    /// Switch the active tool and cancel any in-progress gizmo drag.
    fn set_tool(&mut self, tool: Tool) {
        self.tool = tool;
        self.grabbed = None;
        self.drag = None;
        // Selecting Sculpt focuses the Terrain tools so the brush controls are at hand.
        if tool == Tool::Sculpt {
            self.focus_terrain();
        }
    }

    /// Focus (re-adding if closed) the Terrain dock tab.
    fn focus_terrain(&mut self) {
        if let Some(dock) = self.dock_state.as_mut() {
            focus_terrain_tab(dock);
        }
    }

    // ---- selection ----------------------------------------------------------
    /// The entity the gizmo + inspector act on (the most recently selected).
    fn primary(&self) -> Option<Entity> {
        self.selection.last().copied()
    }
    fn select_single(&mut self, e: Entity) {
        self.selection.clear();
        self.selection.push(e);
    }
    fn select_toggle(&mut self, e: Entity) {
        if let Some(i) = self.selection.iter().position(|&x| x == e) {
            self.selection.remove(i);
        } else {
            self.selection.push(e);
        }
    }
    fn select_all(&mut self) {
        self.selection = self.world.query::<Matter>().map(|(e, _)| e).collect();
    }
    /// Selected entities that are real Matter nodes (excludes the Lighting node).
    fn selected_matter(&self) -> Vec<Entity> {
        self.selection.iter().copied().filter(|&e| self.world.get::<Matter>(e).is_some()).collect()
    }

    // ---- undo / redo (whole-scene snapshots) --------------------------------
    fn snapshot(&self) -> SceneDoc {
        floptle_scene::to_doc(self.scene_name.clone(), &self.world)
    }
    fn push_history(&mut self, snap: Snapshot) {
        self.history.redo.clear();
        self.history.undo.push(snap);
        while self.history.undo.len() > self.history.max {
            self.history.undo.remove(0);
        }
        self.scene_dirty = true; // any undo-able edit (scene or terrain) is unsaved
    }
    /// Record the current scene as an undo point (call BEFORE a discrete edit).
    fn record(&mut self) {
        let s = self.snapshot();
        self.push_history(Snapshot::Scene(s));
    }
    /// Open an edit session for undo coalescing (gizmo/inspector drag = one step),
    /// using this frame's pre-edit snapshot.
    fn begin_edit(&mut self) {
        if !self.editing {
            if let Some(snap) = self.frame_snapshot.take() {
                self.push_history(Snapshot::Scene(snap));
            }
            self.editing = true;
        }
    }
    fn restore(&mut self, doc: SceneDoc) {
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.adopt_terrain();
        self.selection.clear();
        self.grabbed = None;
        self.drag = None;
    }
    /// Swap the live terrain field for serialized `bytes`, queuing a GPU re-upload.
    /// The terrain node carrying `id` (if any), for keyed undo/save.
    fn terrain_entity_of_id(&self, id: u32) -> Option<Entity> {
        self.terrains.keys().copied().find(|&e| {
            matches!(self.world.get::<Matter>(e), Some(Matter::Terrain { id: i }) if *i == id)
        })
    }

    /// Restore a terrain field (by id) from serialized `bytes`. Returns the current
    /// bytes first (for the redo/undo counterpart), or `None` if the id is gone.
    fn swap_terrain_bytes(&mut self, id: u32, bytes: &[u8]) -> Option<Vec<u8>> {
        let e = self.terrain_entity_of_id(id)?;
        let cur = self.terrains.get(&e).map(|t| t.to_bytes());
        if let Some(t) = floptle_field::Terrain::from_bytes(bytes) {
            self.terrains.insert(e, t);
            self.combined_dirty = true;
        }
        cur
    }
    fn undo(&mut self) {
        if self.playing {
            return; // stop play before editing history
        }
        match self.history.undo.pop() {
            Some(Snapshot::Scene(prev)) => {
                let cur = self.snapshot();
                self.history.redo.push(Snapshot::Scene(cur));
                self.restore(prev);
            }
            Some(Snapshot::Terrain(id, prev)) => {
                if let Some(cur) = self.swap_terrain_bytes(id, &prev) {
                    self.history.redo.push(Snapshot::Terrain(id, cur));
                }
            }
            None => {}
        }
    }
    fn redo(&mut self) {
        if self.playing {
            return;
        }
        match self.history.redo.pop() {
            Some(Snapshot::Scene(next)) => {
                let cur = self.snapshot();
                self.history.undo.push(Snapshot::Scene(cur));
                self.restore(next);
            }
            Some(Snapshot::Terrain(id, next)) => {
                if let Some(cur) = self.swap_terrain_bytes(id, &next) {
                    self.history.undo.push(Snapshot::Terrain(id, cur));
                }
            }
            None => {}
        }
    }

    /// Enter/leave play mode. Play snapshots the authored scene and runs scripts;
    /// Stop restores the authored scene so script-driven changes aren't persisted.
    /// Build the physics gravity field from the scene's GravityVolume nodes: `Down`
    /// volumes add uniform −Y gravity (the level's base), `Radial` volumes add a planet
    /// gravity well at the node. With no volumes, a default −Y 9.81 keeps normal games
    /// working out of the box.
    fn build_gravity_field(&self) -> floptle_physics::GravityField {
        use floptle_core::{GravityMode, Matter};
        let mut field = floptle_physics::GravityField::default();
        for (e, m) in self.world.query::<Matter>() {
            if let Matter::GravityVolume { mode, strength, radius } = m {
                match mode {
                    GravityMode::Down => field
                        .sources
                        .push(floptle_physics::GravitySource::Uniform(Vec3::new(0.0, -*strength, 0.0))),
                    GravityMode::Radial => {
                        let p = floptle_core::world_transform(&self.world, e).translation;
                        field.sources.push(floptle_physics::GravitySource::Point {
                            center: Vec3::new(p.x as f32, p.y as f32, p.z as f32),
                            strength: *strength,
                            radius: *radius,
                        });
                    }
                }
            }
        }
        if field.sources.is_empty() {
            field.sources.push(floptle_physics::GravitySource::Uniform(Vec3::new(0.0, -9.81, 0.0)));
        }
        field
    }

    /// Register a static triangle collider for every Mesh node flagged `MeshCollider`,
    /// so an imported map is walkable. Re-imports the model to get CPU triangles (the
    /// registry keeps only GPU handles) and bakes the node's world transform into them —
    /// the same transform the renderer draws with, so the collider lines up with what
    /// you see. Done once at Play; imports are cached by the OS, the cost is one-time.
    fn add_mesh_colliders(&self, sim: &mut floptle_physics::Sim) {
        let meshes: Vec<(Entity, String)> = self
            .world
            .query::<floptle_core::MeshCollider>()
            .filter_map(|(e, _)| match self.world.get::<Matter>(e) {
                Some(Matter::Mesh { asset_path }) => Some((e, asset_path.clone())),
                _ => None,
            })
            .collect();
        for (e, path) in meshes {
            let Ok(model) = floptle_assets::gltf_import::import(std::path::Path::new(&path)) else {
                eprintln!("mesh collider: failed to load {path}");
                continue;
            };
            let wt = floptle_core::world_transform(&self.world, e);
            let m = Mat4::from_scale_rotation_translation(wt.scale, wt.rotation, wt.translation.as_vec3());
            let mut verts: Vec<Vec3> = Vec::new();
            let mut indices: Vec<u32> = Vec::new();
            for part in &model.parts {
                let base = verts.len() as u32;
                verts.extend(part.mesh.vertices.iter().map(|v| m.transform_point3(Vec3::from(v.pos))));
                indices.extend(part.mesh.indices.iter().map(|i| i + base));
            }
            sim.add_static_mesh(&verts, &indices);
        }
    }

    fn toggle_play(&mut self) {
        if self.playing {
            self.playing = false;
            self.paused = false;
            self.sim = None; // drop the physics sim; restore reverts moved transforms
            if let Some(snap) = self.play_snapshot.take() {
                self.restore(snap);
            }
        } else {
            self.play_snapshot = Some(self.snapshot());
            self.play_t = 0.0;
            self.paused = false;
            // Build the physics sim from the scene: RigidBody nodes + the combined
            // terrain (SDF collider) + the gravity field from GravityVolume nodes.
            let gravity = self.build_gravity_field();
            let mut sim = floptle_physics::Sim::build(&self.world, self.combined.as_ref(), gravity);
            // Add static mesh colliders (imported maps flagged "Mesh collider") so a
            // character can walk on them, not just the terrain.
            self.add_mesh_colliders(&mut sim);
            self.sim = Some(sim);
            // Start play with a clean Console so you only see this run's output.
            self.console.entries.clear();
            // Press Play → bring the Game tab to the front (active-camera view), so it's
            // clear you're testing the game, not the editor scene view.
            if let Some(dock) = self.dock_state.as_mut() {
                if let Some(path) = dock.find_tab(&EditorTab::Game) {
                    let _ = dock.set_active_tab(path);
                }
            }
            self.playing = true;
        }
    }
    /// Freeze/unfreeze the script clock while playing.
    fn toggle_pause(&mut self) {
        if self.playing {
            self.paused = !self.paused;
        }
    }

    // ---- node create / delete / clipboard -----------------------------------
    fn node_of(&self, e: Entity) -> Option<NodeDoc> {
        let matter = self.world.get::<Matter>(e)?;
        let transform =
            self.world.get::<Transform>(e).map(TransformDoc::from).unwrap_or_default();
        let name = self.world.get::<Name>(e).map(|n| n.0.clone()).unwrap_or_else(|| "node".into());
        let scripts = self
            .world
            .get::<Scripts>(e)
            .map(|s| {
                s.0.iter()
                    .map(|i| ScriptDoc {
                        kind: i.kind.clone(),
                        enabled: i.enabled,
                        params: i.params.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let material = self.world.get::<Material>(e).map(MaterialDoc::from_material);
        let rigidbody =
            self.world.get::<floptle_core::RigidBody>(e).map(floptle_scene::RigidBodyDoc::from_rigidbody);
        let mesh_collider = self.world.get::<floptle_core::MeshCollider>(e).is_some();
        Some(NodeDoc {
            name,
            transform,
            matter: MatterDoc::from(matter),
            scripts,
            material,
            rigidbody,
            mesh_collider,
            parent: None,
        })
    }
    fn spawn_node(&mut self, node: &NodeDoc) -> Entity {
        let e = self.world.spawn();
        self.world.insert(e, node.transform.to_transform());
        self.world.insert(e, Name(node.name.clone()));
        self.world.insert(e, node.matter.to_matter());
        if !node.scripts.is_empty() {
            let insts = node
                .scripts
                .iter()
                .map(|s| ScriptInst {
                    kind: s.kind.clone(),
                    enabled: s.enabled,
                    params: s.params.clone(),
                })
                .collect();
            self.world.insert(e, Scripts(insts));
        }
        if let Some(m) = &node.material {
            self.world.insert(e, m.to_material());
        }
        if let Some(rb) = &node.rigidbody {
            self.world.insert(e, rb.to_rigidbody());
        }
        if node.mesh_collider {
            self.world.insert(e, floptle_core::MeshCollider);
        }
        e
    }
    /// Spawn a new node ~5 units in front of the camera, and select it.
    fn add_node(&mut self, name: &str, matter: MatterDoc) {
        self.record();
        let cam = self.camera.render_camera();
        let mut pos = cam.world_position + (cam.rotation * Vec3::NEG_Z * 5.0).as_dvec3();
        if self.grid.snap {
            pos = snap_dvec3(pos, self.grid.size as f64);
        }
        let node = NodeDoc {
            name: name.into(),
            transform: TransformDoc { translation: [pos.x, pos.y, pos.z], ..Default::default() },
            matter,
            scripts: Vec::new(),
            material: None,
            rigidbody: None,
            mesh_collider: false,
            parent: None,
        };
        let e = self.spawn_node(&node);
        self.select_single(e);
    }
    /// Import + register a glTF model (cached by path). Returns true on success.
    fn import_model(&mut self, path: &str) -> bool {
        if self.mesh_registry.contains_key(path) {
            return true;
        }
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return false;
        };
        match floptle_assets::gltf_import::import(std::path::Path::new(path)) {
            Ok(model) => {
                let parts = model
                    .parts
                    .iter()
                    .map(|p| raster.register(gpu, &p.mesh, p.texture.map(|i| &model.textures[i])))
                    .collect();
                self.mesh_registry
                    .insert(path.to_string(), MeshAsset { parts, size: model.size });
                println!("  imported {path}");
                true
            }
            Err(e) => {
                eprintln!("  import {path} failed: {e}");
                false
            }
        }
    }

    // ---- asset preview (Inspector) ------------------------------------------
    /// Lazily create the 320² offscreen target the asset preview renders into, and
    /// register its color view with egui so the Inspector can draw it as an image.
    fn ensure_preview_target(&mut self) {
        if self.preview.is_some() {
            return;
        }
        let (Some(gpu), Some(egui)) = (self.gpu.as_ref(), self.egui.as_mut()) else { return };
        let size = 320u32;
        let make = |fmt: wgpu::TextureFormat, usage: wgpu::TextureUsages, label| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: fmt,
                usage,
                view_formats: &[],
            })
        };
        let color = make(
            gpu.surface_format(),
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            "preview-color",
        );
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let depth = make(Gpu::DEPTH_FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT, "preview-depth");
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let tex_id =
            egui.renderer.register_native_texture(&gpu.device, &color_view, wgpu::FilterMode::Linear);
        self.preview = Some(PreviewTarget { color_view, depth_view, tex_id });
    }

    /// (Re)load a selected texture asset into an egui texture handle for preview.
    fn ensure_preview_image(&mut self, path: &str) {
        if self.preview_image.as_ref().is_some_and(|(p, _, _)| p == path) {
            return;
        }
        let Some(egui) = self.egui.as_ref() else { return };
        if let Some(img) = floptle_assets::load_texture(Path::new(path)) {
            let dims = [img.width as usize, img.height as usize];
            let color = egui::ColorImage::from_rgba_unmultiplied(dims, &img.pixels);
            let handle = egui.ctx.load_texture(
                format!("preview:{path}"),
                color,
                egui::TextureOptions::LINEAR,
            );
            self.preview_image = Some((path.to_string(), handle, dims));
        }
    }

    /// Each frame: build the Inspector preview for the selected asset. Models and
    /// material presets render as a turntable-spinning subject into the offscreen
    /// target; textures load as an egui image.
    fn update_asset_preview(&mut self, dt: f32) {
        let Some(path) = self.selected_asset.clone() else {
            self.preview_material = None;
            return;
        };
        if is_texture(&path) {
            self.ensure_preview_image(&path);
            return;
        }
        if !is_model(&path) && !is_material(&path) {
            return;
        }
        if self.preview_spinning {
            self.preview_spin += dt * 0.8;
        }

        // Resolve the subject into drawable parts + a bounding radius.
        let mut parts: Vec<(MeshId, Option<TexId>)> = Vec::new();
        let mut radius = 1.0f32;
        let mut mat = MaterialParams::flat([0.8, 0.8, 0.82]);
        let is_mat = is_material(&path);
        if is_model(&path) {
            if !self.import_model(&path) {
                return;
            }
            if let Some(a) = self.mesh_registry.get(&path) {
                radius = (a.size * 0.5).max(0.2);
                parts = a.parts.iter().map(|m| (*m, None)).collect();
            }
        } else {
            // Material preset: (re)load it from the loaded presets by file stem.
            if self.preview_material.as_ref().is_none_or(|(p, _)| p != &path) {
                let stem = Path::new(&path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if let Some((_, doc)) = self.materials.iter().find(|(n, _)| *n == stem) {
                    self.preview_material = Some((path.clone(), doc.to_material()));
                }
            }
            if let Some((_, material)) = self.preview_material.clone() {
                let tex = material.texture.as_ref().and_then(|t| self.ensure_texture(t));
                mat = material_params(&material);
                radius = 0.85;
                if let Some(s) = self.mesh_ids.get(1).copied() {
                    parts.push((s, tex));
                }
            }
        }
        if parts.is_empty() {
            return;
        }

        // Turntable camera: orbit the subject, looking at the origin (the subject is
        // drawn camera-relative since the view matrix carries no translation).
        let dist = (radius * 3.0 * self.preview_zoom).max(0.4);
        let a = self.preview_spin;
        let eye = Vec3::new(a.cos() * dist, radius * 0.55, a.sin() * dist);
        let fwd = (Vec3::ZERO - eye).normalize();
        let right = fwd.cross(Vec3::Y).normalize();
        let up = right.cross(fwd);
        let rot = Quat::from_mat3(&Mat3::from_cols(right, up, -fwd));
        let cam = RenderCamera::new(
            eye.as_dvec3(),
            rot,
            Projection::Perspective { fov_y: 0.7, near: 0.02, far: 1000.0 },
        );
        let vp = cam.view_proj(1.0);
        let model = Mat4::from_translation(-eye); // obj at origin, camera-relative
        let raw = if is_mat {
            instance_of_mat(model, &mat)
        } else {
            instance_of(model, [1.0, 1.0, 1.0])
        };
        let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> =
            parts.iter().map(|(m, t)| (*m, *t, raw)).collect();
        let l = Vec3::new(0.5, 0.8, 0.6).normalize();
        let globals = Globals {
            view_proj: vp.to_cols_array_2d(),
            light_dir: [l.x, l.y, l.z, 0.0],
            light_color: [1.0, 0.98, 0.93, 0.0],
            ambient: [0.30, 0.32, 0.38, 0.0],
            ..Default::default()
        };

        self.ensure_preview_target();
        if let (Some(gpu), Some(raster), Some(preview)) =
            (self.gpu.as_ref(), self.raster.as_mut(), self.preview.as_ref())
        {
            raster.draw_scene(
                gpu,
                &preview.color_view,
                &preview.depth_view,
                globals,
                &instances,
                Some([0.07, 0.08, 0.10, 1.0]),
            );
        }
    }

    /// Lazily create the 16:9 offscreen target the selected-camera POV preview renders
    /// into, registering its color view with egui as a texture id for the Inspector.
    fn ensure_cam_preview_target(&mut self) {
        if self.cam_preview.is_some() {
            return;
        }
        let (Some(gpu), Some(egui)) = (self.gpu.as_ref(), self.egui.as_mut()) else { return };
        let (w, h) = (320u32, 180u32);
        let make = |fmt: wgpu::TextureFormat, usage: wgpu::TextureUsages, label| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: fmt,
                usage,
                view_formats: &[],
            })
        };
        let color = make(
            gpu.surface_format(),
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            "cam-preview-color",
        );
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let depth = make(Gpu::DEPTH_FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT, "cam-preview-depth");
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let tex_id =
            egui.renderer.register_native_texture(&gpu.device, &color_view, wgpu::FilterMode::Linear);
        self.cam_preview = Some(PreviewTarget { color_view, depth_view, tex_id });
    }

    /// Each frame: if a single Camera node is selected, render the scene from its POV
    /// into the 16:9 offscreen target so the Inspector can show what it sees. Mirrors
    /// the main render path (raster meshes + raymarch blobs/terrain), camera-relative
    /// to the selected camera.
    fn update_camera_preview(&mut self, elapsed: f32) {
        let Some(e) = self.selection.last().copied() else { return };
        let fov_y = match self.world.get::<Matter>(e) {
            Some(Matter::Camera { fov_y, .. }) => *fov_y,
            _ => return,
        };
        let wt = floptle_core::world_transform(&self.world, e);
        let cam = RenderCamera::new(
            wt.translation,
            wt.rotation,
            Projection::Perspective { fov_y, near: 0.05, far: 4000.0 },
        );
        let aspect = 16.0 / 9.0;
        let view_proj = cam.view_proj(aspect);

        let light_node = self.world.query::<Light>().next().map(|(_, l)| *l).unwrap_or_default();
        let light = Vec3::from(light_node.direction).normalize_or_zero();
        let li = light_node.intensity;
        let (pl_count, pl_pos, pl_col) = collect_point_lights(&self.world, cam.world_position);
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
        let mut blobs: Vec<(DVec3, f32, MaterialParams)> = Vec::new();
        for (ent, matter) in &ents {
            let t = floptle_core::world_transform(&self.world, *ent);
            let mat = self.world.get::<Material>(*ent).cloned();
            let tex = mat
                .as_ref()
                .and_then(|m| m.texture.as_deref())
                .and_then(|p| self.texture_registry.get(p).copied());
            match matter {
                Matter::Primitive { shape, color } => {
                    if let Some(&mesh) = self.mesh_ids.get(*shape as usize) {
                        let model = t.render_matrix(cam.world_position);
                        let mp =
                            mat.as_ref().map(material_params).unwrap_or_else(|| MaterialParams::flat(*color));
                        instances.push((mesh, tex, instance_of_mat(model, &mp)));
                    }
                }
                Matter::Blob { scale } => {
                    let mp = mat.as_ref().map(material_params).unwrap_or_else(blob_default_material);
                    blobs.push((t.translation, scale * t.scale.x, mp));
                }
                Matter::Mesh { asset_path } => {
                    if let Some(asset) = self.mesh_registry.get(asset_path) {
                        let model = t.render_matrix(cam.world_position);
                        let mp = mat
                            .as_ref()
                            .map(material_params)
                            .unwrap_or_else(|| MaterialParams::flat([1.0, 1.0, 1.0]));
                        for &mid in &asset.parts {
                            instances.push((mid, tex, instance_of_mat(model, &mp)));
                        }
                    }
                }
                _ => {}
            }
        }

        let clear = [0.02f32, 0.02, 0.05, 1.0];
        let terrain_mat = self.terrain_material();
        let show_blobs = self.project.matter && !blobs.is_empty();
        let rm = if show_blobs || self.combined.is_some() {
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
                vol_center: [0.0, 0.0, 0.0, 0.0],
                vol_half: [1.0, 1.0, 1.0, 0.5],
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
            };
            if let Some((hf, bc)) =
                self.combined.as_ref().map(|t| (t.baked.half_extent, t.baked.center))
            {
                let cr = DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - cam.world_position;
                g.vol_center = [cr.x as f32, cr.y as f32, cr.z as f32, 1.0];
                g.vol_half = [hf[0], hf[1], hf[2], 0.1];
            }
            Some(g)
        } else {
            None
        };

        self.ensure_cam_preview_target();
        if let (Some(gpu), Some(raster), Some(raymarch), Some(prev)) = (
            self.gpu.as_ref(),
            self.raster.as_mut(),
            self.raymarch.as_mut(),
            self.cam_preview.as_ref(),
        ) {
            let raster_clear = if let Some(rm) = rm {
                raymarch.draw_into(gpu, &prev.color_view, &prev.depth_view, rm);
                None
            } else {
                Some(clear.map(|c| c as f64))
            };
            raster.draw_scene(gpu, &prev.color_view, &prev.depth_view, globals, &instances, raster_clear);
        }
    }

    /// What the Inspector should draw for the current selection's preview.
    fn preview_view(&self) -> Option<PreviewView> {
        let path = self.selected_asset.as_ref()?;
        if is_texture(path) {
            let (_, handle, dims) = self.preview_image.as_ref()?;
            Some(PreviewView::Image(handle.clone(), *dims))
        } else if is_model(path) || is_material(path) {
            Some(PreviewView::Rendered(self.preview.as_ref()?.tex_id))
        } else {
            None
        }
    }

    /// Drop of an asset from the browser: spawn a model, or attach a script to the
    /// selection (a model dropped on the viewport, a script anywhere).
    fn drop_asset(&mut self, path: &str) {
        if is_model(path) {
            if !self.import_model(path) {
                return;
            }
            self.record();
            let pos = self.cursor_world();
            let name = std::path::Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "mesh".into());
            let node = NodeDoc {
                name,
                transform: TransformDoc {
                    translation: [pos.x, pos.y, pos.z],
                    ..Default::default()
                },
                matter: MatterDoc::Mesh { asset_path: path.to_string() },
                scripts: Vec::new(),
                material: None,
                rigidbody: None,
                mesh_collider: false,
                parent: None,
            };
            let e = self.spawn_node(&node);
            self.select_single(e);
        } else if is_script(path) {
            self.attach_script_file(path, self.primary());
        }
    }
    /// Attach the `.lua` script at `path` to `target`, seeding its `params` from
    /// the script's declared `defaults`.
    fn attach_script_file(&mut self, path: &str, target: Option<Entity>) {
        let Some(e) = target else { return };
        if self.world.get::<Transform>(e).is_none() || !is_script(path) {
            return;
        }
        if !Path::new(path).exists() {
            eprintln!("  script not found: {path}");
            return;
        }
        let name = script_name_of(path);
        let params = self.script_host.script_defaults(Path::new(path));
        self.record();
        let inst = ScriptInst { kind: name, enabled: true, params };
        if let Some(scr) = self.world.get_mut::<Scripts>(e) {
            scr.0.push(inst);
        } else {
            self.world.insert(e, Scripts(vec![inst]));
        }
    }
    fn delete_selected(&mut self) {
        let targets = self.selected_matter();
        if targets.is_empty() {
            return;
        }
        self.record();
        for e in targets {
            if self.terrains.remove(&e).is_some() {
                if self.active_terrain == Some(e) {
                    self.active_terrain = None;
                }
                self.combined_dirty = true;
            }
            self.world.despawn(e);
        }
        self.selection.clear();
        self.grabbed = None;
        self.drag = None;
    }
    fn copy_selected(&mut self) {
        let nodes: Vec<NodeDoc> =
            self.selected_matter().iter().filter_map(|&e| self.node_of(e)).collect();
        if !nodes.is_empty() {
            self.clipboard = nodes;
        }
    }
    /// Spawn the given nodes (offset slightly) and select them — used by paste/dup.
    fn spawn_offset(&mut self, nodes: Vec<NodeDoc>) {
        if nodes.is_empty() {
            return;
        }
        self.record();
        self.selection.clear();
        for mut node in nodes {
            node.transform.translation[0] += 0.5;
            node.transform.translation[2] += 0.5;
            let e = self.spawn_node(&node);
            self.selection.push(e);
        }
    }
    fn paste(&mut self) {
        let nodes = self.clipboard.clone();
        self.spawn_offset(nodes);
    }
    fn duplicate_selected(&mut self) {
        let nodes: Vec<NodeDoc> =
            self.selected_matter().iter().filter_map(|&e| self.node_of(e)).collect();
        self.spawn_offset(nodes);
    }

    /// True when the cursor is over the Scene viewport tab and not under a popup —
    /// the gate for viewport picking, gizmo grabs and camera look. egui_dock keeps
    /// the side panels in the background layer, so `is_pointer_over_egui` alone
    /// can't separate them from the viewport; the Scene-tab rect is what does.
    fn cursor_over_scene(&self) -> bool {
        let Some(eg) = self.egui.as_ref() else { return false };
        scene_hit(&eg.ctx, self.cursor, self.scene_rect)
    }

    /// The world point under the cursor — its ray's hit on the ground plane (y=0),
    /// or ~6 units in front of the camera if the ray doesn't meet the ground. Used to
    /// place a dropped asset where the cursor is.
    fn cursor_world(&self) -> DVec3 {
        let cam = self.camera.render_camera();
        let Some(gpu) = self.gpu.as_ref() else {
            return cam.world_position + (cam.rotation * Vec3::NEG_Z * 6.0).as_dvec3();
        };
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let inv = cam.view_proj(w / h).inverse();
        cursor_ground(cam.world_position, cam.rotation, inv, w, h, self.cursor)
    }

    /// Move the selection up (-1) / down (+1) through the hierarchy (arrow keys).
    fn step_selection(&mut self, delta: i32) {
        let order: Vec<Entity> = self.world.query::<Matter>().map(|(e, _)| e).collect();
        if order.is_empty() {
            return;
        }
        let cur = self.selection.last().and_then(|s| order.iter().position(|e| e == s));
        let next = match cur {
            Some(i) => (i as i32 + delta).clamp(0, order.len() as i32 - 1) as usize,
            None if delta > 0 => 0,
            None => order.len() - 1,
        };
        self.select_single(order[next]);
    }

    /// Track a mouse button for the script `input` API (edge + held).
    fn track_mouse_button(&mut self, i: usize, pressed: bool) {
        if i < 3 {
            if pressed && !self.input_buttons[i] {
                self.input_buttons_pressed[i] = true;
            }
            self.input_buttons[i] = pressed;
        }
    }

    /// Toggle the selected folder's open/closed state in the Hierarchy (Enter key).
    fn toggle_folder_selected(&mut self) {
        let Some(e) = self.selection.last().copied() else { return };
        if matches!(self.world.get::<Matter>(e), Some(Matter::Empty)) {
            if !self.collapsed.remove(&e) {
                self.collapsed.insert(e);
            }
        }
    }

    /// Frame the selected object in the viewport (the F key): keep the view angle,
    /// move the camera so the object is centered at a size-appropriate distance.
    fn focus_selected(&mut self) {
        let Some(e) = self.selection.last().copied() else { return };
        let Some(t) = self.world.get::<Transform>(e) else { return };
        let target = t.translation;
        let scale = t.scale.abs().max_element() as f64;
        let base = match self.world.get::<Matter>(e) {
            Some(Matter::Mesh { asset_path }) => {
                self.mesh_registry.get(asset_path).map(|a| a.size as f64).unwrap_or(1.0)
            }
            Some(Matter::Blob { scale: s }) => *s as f64,
            _ => 1.0,
        };
        let radius = (base * scale).max(0.3);
        let distance = (radius * 3.0 + 2.0).clamp(2.5, 80.0);
        // Keep the current view direction; glide the position so the target ends up
        // `distance` straight ahead. The eased move runs in the per-frame update.
        let forward = (self.camera.rotation() * Vec3::NEG_Z).as_dvec3();
        let dest = target - forward * distance;
        self.focus_anim = Some(FocusAnim { from: self.camera.position, to: dest, t: 0.0 });
    }

    /// Pick the nearest selectable entity under a viewport cursor (physical px).
    /// Casts a ray and tests each object's EXACT primitive in its own local space
    /// (box for a cube, sphere for a sphere/blob), so picking stays accurate however
    /// the object is rotated or non-uniformly scaled. `None` = empty space.
    fn pick(&self, cursor: Vec2) -> Option<Entity> {
        let gpu = self.gpu.as_ref()?;
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let inv = cam.view_proj(w / h).inverse();
        // Camera-relative ray (the world is offset to the camera, ADR-0015).
        let ndc = Vec2::new(cursor.x / w * 2.0 - 1.0, 1.0 - cursor.y / h * 2.0);
        let near = inv * Vec4::new(ndc.x, ndc.y, 0.0, 1.0);
        let far = inv * Vec4::new(ndc.x, ndc.y, 1.0, 1.0);
        let ro = near.truncate() / near.w;
        let rd = (far.truncate() / far.w - ro).normalize();

        let mut best: Option<(Entity, f32)> = None;
        for (e, m) in self.world.query::<Matter>() {
            // Ray-test against the node's WORLD placement (so parented nodes pick).
            let t = floptle_core::world_transform(&self.world, e);
            let hit = match m {
                Matter::Primitive { shape, .. } => {
                    // Transform the ray into the object's local frame (the same `t`
                    // parameter is valid in both spaces, so hits stay comparable).
                    let m_inv = t.render_matrix(cam.world_position).inverse();
                    if !m_inv.is_finite() {
                        continue;
                    }
                    let ro_l = (m_inv * ro.extend(1.0)).truncate();
                    let rd_l = (m_inv * rd.extend(0.0)).truncate();
                    match shape {
                        Shape::Cube => ray_aabb(ro_l, rd_l, 0.7),
                        Shape::Sphere => ray_sphere(ro_l, rd_l, Vec3::ZERO, 0.85),
                        // capsule(0.5, 0.5): total Y half-extent radius+half = 1.0; a
                        // bounding sphere of that radius contains it for picking.
                        Shape::Capsule => ray_sphere(ro_l, rd_l, Vec3::ZERO, 1.0),
                    }
                }
                Matter::Blob { scale } => {
                    let center = (t.translation - cam.world_position).as_vec3();
                    ray_sphere(ro, rd, center, 0.85 * scale * t.scale.x)
                }
                Matter::Mesh { asset_path } => {
                    let r = self.mesh_registry.get(asset_path).map(|a| a.size * 0.5).unwrap_or(1.0);
                    let center = (t.translation - cam.world_position).as_vec3();
                    ray_sphere(ro, rd, center, (r * t.scale.max_element()).max(0.1))
                }
                // no mesh — select via the hierarchy.
                Matter::Empty
                | Matter::Terrain { .. }
                | Matter::Camera { .. }
                | Matter::PointLight { .. }
                | Matter::GravityVolume { .. } => None,
            };
            if let Some(th) = hit {
                if best.is_none_or(|(_, bt)| th < bt) {
                    best = Some((e, th));
                }
            }
        }
        best.map(|(e, _)| e)
    }

    /// Apply a gizmo drag for the grabbed handle, as an ABSOLUTE transform from the
    /// start-of-drag snapshot (no per-event accumulation ⏵ no drift).
    fn gizmo_drag(&mut self) {
        let (Some(drag), Some(cursor), Some(e)) = (self.drag, self.cursor, self.primary()) else {
            return;
        };
        // The snapshot must belong to the still-selected entity (guards against the
        // selection changing mid-drag and applying the wrong object's transform).
        if drag.entity != e {
            self.grabbed = None;
            self.drag = None;
            return;
        }
        let handle = drag.handle;
        let (w, h) = self
            .gpu
            .as_ref()
            .map(|g| (g.config.width as f32, g.config.height.max(1) as f32))
            .unwrap_or((1280.0, 720.0));
        let cam = self.camera.render_camera();
        let vp = cam.view_proj(w / h);
        let cam_world = cam.world_position;
        let start = drag.start_xf;
        let cursor_delta = cursor - drag.cursor_start;
        let (snap, step) = (self.grid.snap, self.grid.size as f64);

        match self.tool {
            Tool::Move => {
                if let Some(i) = handle.axis_index() {
                    let dir = local_axis(start.rotation, i);
                    // Project the axis (a 1-unit step) to screen; the move distance is
                    // the cursor delta projected onto that screen direction.
                    let (Some(s0), Some(s1)) = (
                        project(start.translation, cam_world, vp, w, h),
                        project(start.translation + dir.as_dvec3(), cam_world, vp, w, h),
                    ) else {
                        return;
                    };
                    let sdir = s1 - s0;
                    let len2 = sdir.length_squared();
                    if len2 < 1e-6 {
                        return; // axis points (almost) straight at the camera
                    }
                    let units = cursor_delta.dot(sdir) / len2;
                    let mut p = start.translation + (dir * units).as_dvec3();
                    if snap {
                        p = snap_dvec3(p, step);
                    }
                    self.set_world_transform(e, Transform { translation: p, ..start });
                } else {
                    // Center handle: free move in the camera plane.
                    let rot = cam.rotation;
                    let right = rot * Vec3::X;
                    let up = rot * Vec3::Y;
                    let dist = (start.translation - cam_world).length().max(0.1) as f32;
                    let wpp = 2.0 * dist * (30f32.to_radians()).tan() / h;
                    let mv = right * (cursor_delta.x * wpp) - up * (cursor_delta.y * wpp);
                    let mut p = start.translation + mv.as_dvec3();
                    if snap {
                        p = snap_dvec3(p, step);
                    }
                    self.set_world_transform(e, Transform { translation: p, ..start });
                }
            }
            Tool::Rotate => {
                if let Some(i) = handle.axis_index() {
                    // Rotate about the object's local axis (in world space).
                    let dir = local_axis(start.rotation, i);
                    let Some(center) = project(start.translation, cam_world, vp, w, h) else {
                        return;
                    };
                    let v1 = drag.cursor_start - center;
                    let v2 = cursor - center;
                    if v1.length_squared() < 1.0 || v2.length_squared() < 1.0 {
                        return;
                    }
                    let mut angle = (v1.x * v2.y - v1.y * v2.x).atan2(v1.x * v2.x + v1.y * v2.y);
                    // Screen-y points down; flip when the axis faces away from the camera
                    // so a drag always spins the visible way.
                    if dir.dot((start.translation - cam_world).as_vec3()) > 0.0 {
                        angle = -angle;
                    }
                    let rot = (Quat::from_axis_angle(dir, angle) * start.rotation).normalize();
                    self.set_world_transform(e, Transform { rotation: rot, ..start });
                } else {
                    // Center handle: free / trackball rotate about the camera axes —
                    // drag horizontally to spin about camera-up, vertically about
                    // camera-right.
                    let cam_right = cam.rotation * Vec3::X;
                    let cam_up = cam.rotation * Vec3::Y;
                    let q = Quat::from_axis_angle(cam_up, cursor_delta.x * TRACKBALL_SENS)
                        * Quat::from_axis_angle(cam_right, cursor_delta.y * TRACKBALL_SENS);
                    let rot = (q * start.rotation).normalize();
                    self.set_world_transform(e, Transform { rotation: rot, ..start });
                }
            }
            Tool::Scale => {
                if let Some(i) = handle.axis_index() {
                    let dir = local_axis(start.rotation, i);
                    let (Some(s0), Some(s1)) = (
                        project(start.translation, cam_world, vp, w, h),
                        project(start.translation + dir.as_dvec3(), cam_world, vp, w, h),
                    ) else {
                        return;
                    };
                    let n = (s1 - s0).normalize_or_zero();
                    let factor = 1.0 + cursor_delta.dot(n) * SCALE_SENS;
                    let mut sc = start.scale;
                    sc[i] = (start.scale[i] * factor).max(0.01);
                    self.set_world_transform(e, Transform { scale: sc, ..start });
                } else {
                    // Center handle: uniform scale by the cursor's distance ratio.
                    let Some(center) = project(start.translation, cam_world, vp, w, h) else {
                        return;
                    };
                    let d0 = (drag.cursor_start - center).length().max(1.0);
                    let d1 = (cursor - center).length();
                    let factor = (d1 / d0).max(0.01);
                    let sc = (start.scale * factor).max(Vec3::splat(0.01));
                    self.set_world_transform(e, Transform { scale: sc, ..start });
                }
            }
            Tool::Select | Tool::Sculpt => {}
        }
    }

    /// Write `world_xf` (an absolute transform) to `e`, converting it back to the
    /// node's *local* transform when it has a parent (so dragging a child's gizmo
    /// edits its local placement, and parents still carry it).
    fn set_world_transform(&mut self, e: Entity, world_xf: Transform) {
        let local = match self.world.get::<floptle_core::Parent>(e).copied() {
            None => world_xf,
            Some(floptle_core::Parent(p)) => {
                let pw = floptle_core::world_transform(&self.world, p);
                let lm = pw.world_matrix().inverse() * world_xf.world_matrix();
                let (s, r, t) = lm.to_scale_rotation_translation();
                Transform { translation: t, rotation: r.as_quat(), scale: s.as_vec3() }
            }
        };
        if let Some(t) = self.world.get_mut::<Transform>(e) {
            *t = local;
        }
    }

    // ---- terrain sculpting --------------------------------------------------
    /// Once per frame (with the Sculpt tool): cast the cursor ray at the terrain,
    /// build the brush telegraph (ring + normal), and — if a stroke is queued —
    /// apply the brush. Editing is throttled here to one stroke per frame so a fast
    /// drag doesn't stall on the per-voxel work + GPU re-upload.
    fn terrain_frame_update(&mut self) {
        self.terrain_viz = None;
        if self.tool != Tool::Sculpt || self.terrains.is_empty() || !self.cursor_over_scene() {
            return;
        }
        let (Some(cursor), Some(gpu)) = (self.cursor, self.gpu.as_ref()) else { return };
        let cam = self.camera.render_camera();
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let vp = cam.view_proj(w / h);
        let inv = vp.inverse();
        let ndc = Vec2::new(cursor.x / w * 2.0 - 1.0, 1.0 - cursor.y / h * 2.0);
        let near = inv * Vec4::new(ndc.x, ndc.y, 0.0, 1.0);
        let far = inv * Vec4::new(ndc.x, ndc.y, 1.0, 1.0);
        let ro_rel = near.truncate() / near.w;
        let rd = (far.truncate() / far.w - ro_rel).normalize();
        let rd_a = [rd.x, rd.y, rd.z];

        // Each field is in its node's LOCAL space — raycast every terrain and brush
        // the one whose surface the cursor ray hits NEAREST the camera.
        let entities: Vec<Entity> = self.terrains.keys().copied().collect();
        let mut best: Option<(Entity, [f32; 3], DVec3, f64)> = None;
        for e in entities {
            let origin = self.terrain_world_origin(e);
            let ro_local = cam.world_position + ro_rel.as_dvec3() - origin;
            let ro = [ro_local.x as f32, ro_local.y as f32, ro_local.z as f32];
            if let Some(hit) = self.terrains[&e].raycast(ro, rd_a) {
                let hitw = DVec3::new(hit[0] as f64, hit[1] as f64, hit[2] as f64) + origin;
                let dist = (hitw - cam.world_position).length();
                if best.as_ref().is_none_or(|b| dist < b.3) {
                    best = Some((e, hit, origin, dist));
                }
            }
        }
        let Some((active, hit, origin, _)) = best else {
            return;
        };
        self.active_terrain = Some(active);
        let nrm = self.terrains[&active].normal(hit);
        let radius = self.terrain_brush.radius;

        // Telegraph: a ring of `radius` around the hit in the surface tangent plane.
        let hitw = DVec3::new(hit[0] as f64, hit[1] as f64, hit[2] as f64) + origin;
        let n = Vec3::new(nrm[0], nrm[1], nrm[2]);
        let t1 = n.cross(if n.y.abs() > 0.9 { Vec3::X } else { Vec3::Y }).normalize_or_zero();
        let t2 = n.cross(t1);
        let mut ring = Vec::with_capacity(40);
        for i in 0..40 {
            let a = i as f32 / 40.0 * std::f32::consts::TAU;
            let wp = hitw + ((t1 * a.cos() + t2 * a.sin()) * radius).as_dvec3();
            if let Some(s) = project(wp, cam.world_position, vp, w, h) {
                ring.push(s);
            }
        }
        let normal = match (
            project(hitw, cam.world_position, vp, w, h),
            project(hitw + (n * (radius * 0.7)).as_dvec3(), cam.world_position, vp, w, h),
        ) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        };
        self.terrain_viz = Some(TerrainViz { ring, normal });

        // Apply a dab — but only when the cursor has moved ~a third of the brush
        // along the surface since the last one, or after a short interval if held
        // still. This spaces strokes like a real paint tool instead of dumping one
        // every frame (which at high FPS made the brush impossible to control).
        let due = if self.sculpting {
            let now = Instant::now();
            let moved = self
                .last_dab_pos
                .is_none_or(|p| (hitw - p).length() as f32 >= radius * 0.34);
            let timed = self
                .last_dab_time
                .is_none_or(|t| (now - t).as_secs_f32() >= 0.10);
            if moved || timed {
                self.last_dab_pos = Some(hitw);
                self.last_dab_time = Some(now);
                true
            } else {
                false
            }
        } else {
            false
        };
        if due {
            let brush = self.terrain_brush;
            // Capture the pre-stroke field once per stroke, keyed by terrain id, so
            // the whole stroke is a single (restorable) undo step.
            if self.stroke_snapshot.is_none() {
                let id = match self.world.get::<Matter>(active) {
                    Some(Matter::Terrain { id }) => *id,
                    _ => 0,
                };
                if let Some(t) = self.terrains.get(&active) {
                    self.stroke_snapshot = Some((id, t.to_bytes()));
                }
            }
            let terrain = self.terrains.get_mut(&active).unwrap();
            // Infinite terrain: grow the field outward when the brush nears an edge,
            // so the slab has no fixed bounds. (Skip for Paint — painting never
            // extends the shape.) Growth keeps voxel size constant.
            if !matches!(brush.mode, floptle_field::Brush::Paint) {
                terrain.ensure_contains(hit, brush.radius * 1.5);
            }
            let is_paint = matches!(brush.mode, floptle_field::Brush::Paint);
            match brush.mode {
                floptle_field::Brush::Paint if brush.tex_slot >= 0 => {
                    // Paint a texture palette slot (stored as slot+1; 0 = untextured).
                    terrain.paint_texture(hit, brush.radius, brush.tex_slot as u8 + 1);
                }
                floptle_field::Brush::Paint => {
                    terrain.paint(hit, brush.radius, brush.strength, brush.color)
                }
                m => terrain.sculpt(m, hit, brush.radius, brush.strength),
            }
            // Painting only edits color within the brush box (no geometry/bounds change),
            // so on a single terrain we upload just that voxel sub-box to the GPU instead
            // of re-cloning + re-uploading the whole field — that's the painting lag.
            // Sculpt (CSG spreads beyond the brush) and multi-terrain take the full path.
            let region = if is_paint { Some(terrain.brush_range(hit, brush.radius)) } else { None };
            self.stroke_dabbed = true; // mark this stroke as worth an undo step
            match (region, self.terrains.len() == 1) {
                (Some([mn, mx]), true) => {
                    let hi = [mx[0] + 1, mx[1] + 1, mx[2] + 1];
                    self.terrain_region_dirty = Some(match self.terrain_region_dirty {
                        Some((e, omn, omx)) if e == active => (
                            active,
                            [omn[0].min(mn[0]), omn[1].min(mn[1]), omn[2].min(mn[2])],
                            [omx[0].max(hi[0]), omx[1].max(hi[1]), omx[2].max(hi[2])],
                        ),
                        _ => (active, mn, hi),
                    });
                }
                _ => self.combined_dirty = true,
            }
        }
    }

    /// Voxel dims for the current detail setting over the terrain box (≈2:1:2).
    fn terrain_dims(&self) -> [u32; 3] {
        let d = self.terrain_detail.clamp(24, 192);
        [d, (d * 3 / 8).max(8), d]
    }

    /// Create a fresh flat terrain as a NEW scene node (you can have any number).
    /// It is placed at the cursor's ground point so multiple terrains can be laid
    /// out and blended; its field is centered in the node's local space.
    fn create_terrain(&mut self) {
        self.record();
        let id = self.next_terrain_id;
        self.next_terrain_id += 1;
        let pos = self.cursor_world();
        let field = floptle_field::Terrain::flat(
            self.terrain_dims(),
            [0.0, 0.0, 0.0],
            [16.0, 6.0, 16.0],
            0.0,
            [0.35, 0.6, 0.28],
        );
        let e = self.world.spawn();
        self.world.insert(e, Transform { translation: pos, ..Transform::IDENTITY });
        let n = self.terrains.len() + 1;
        self.world.insert(e, Name(format!("Terrain {n}")));
        self.world.insert(e, Matter::Terrain { id });
        self.terrains.insert(e, field);
        self.active_terrain = Some(e);
        self.combined_dirty = true;
        self.select_single(e);
    }

    // ---- cameras -----------------------------------------------------------
    /// The camera node that currently holds play-mode authority (active = true).
    fn active_camera(&self) -> Option<Entity> {
        self.world
            .query::<Matter>()
            .find_map(|(e, m)| matches!(m, Matter::Camera { active: true, .. }).then_some(e))
    }

    /// Spawn a camera node at the current editor viewpoint (so "what you see is the
    /// shot"). The first camera in a scene becomes the active one.
    fn add_camera_node(&mut self, parent: Option<Entity>) {
        self.record();
        let cam = self.camera.render_camera();
        let active = self.active_camera().is_none();
        let e = self.world.spawn();
        self.world.insert(
            e,
            Transform {
                translation: cam.world_position,
                rotation: cam.rotation,
                scale: Vec3::ONE,
            },
        );
        let n = self.world.query::<Matter>().filter(|(_, m)| matches!(m, Matter::Camera { .. })).count() + 1;
        self.world.insert(e, Name(format!("Camera {n}")));
        self.world.insert(e, Matter::Camera { fov_y: 60f32.to_radians(), active });
        if let Some(p) = parent {
            self.world.insert(e, floptle_core::Parent(p));
        }
        self.select_single(e);
    }

    /// Give `e` play-mode authority, clearing it from every other camera.
    fn set_active_camera(&mut self, e: Entity) {
        let cams: Vec<Entity> = self
            .world
            .query::<Matter>()
            .filter_map(|(c, m)| matches!(m, Matter::Camera { .. }).then_some(c))
            .collect();
        for c in cams {
            if let Some(Matter::Camera { active, .. }) = self.world.get_mut::<Matter>(c) {
                *active = c == e;
            }
        }
        self.scene_dirty = true;
    }

    /// Move a camera node to the current editor viewpoint.
    fn camera_to_view(&mut self, e: Entity) {
        self.record();
        let cam = self.camera.render_camera();
        if let Some(t) = self.world.get_mut::<Transform>(e) {
            t.translation = cam.world_position;
            t.rotation = cam.rotation;
        }
    }

    /// Where a terrain node's field is stored — one file per terrain id, per scene.
    fn terrain_field_path_id(&self, id: u32) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.{id}.tfield", self.scene_name))
    }

    /// The legacy single-terrain field path (migrated to the id-keyed name on load).
    fn legacy_terrain_field_path(&self) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.tfield", self.scene_name))
    }

    /// After loading a scene, adopt every terrain node + load its field from disk
    /// (id-keyed, with a one-time legacy fallback). Call once `scene_name` is set.
    fn adopt_terrain(&mut self) {
        self.terrains.clear();
        self.active_terrain = None;
        self.combined = None;
        let nodes: Vec<(Entity, u32)> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Terrain { id } => Some((e, *id)),
                _ => None,
            })
            .collect();
        let mut max_id = 0u32;
        let single = nodes.len() == 1;
        for (e, id) in nodes {
            max_id = max_id.max(id);
            let field = std::fs::read(self.terrain_field_path_id(id))
                .ok()
                .and_then(|b| floptle_field::Terrain::from_bytes(&b))
                // legacy single-terrain scenes stored one `<scene>.tfield`.
                .or_else(|| {
                    if single {
                        std::fs::read(self.legacy_terrain_field_path())
                            .ok()
                            .and_then(|b| floptle_field::Terrain::from_bytes(&b))
                    } else {
                        None
                    }
                })
                // a terrain node with no/garbled field → start it flat.
                .unwrap_or_else(|| {
                    floptle_field::Terrain::flat(
                        self.terrain_dims(),
                        [0.0, 0.0, 0.0],
                        [16.0, 6.0, 16.0],
                        0.0,
                        [0.35, 0.6, 0.28],
                    )
                });
            self.terrains.insert(e, field);
        }
        self.next_terrain_id = max_id + 1;
        self.combined_dirty = !self.terrains.is_empty();
        // Restore the texture palette so painted-texture slots map to images again.
        if !self.terrains.is_empty() {
            if let Ok(text) = std::fs::read_to_string(self.terrain_palette_path()) {
                let slots = floptle_render::TERRAIN_SLOTS as usize;
                let mut palette: Vec<String> = text.lines().map(|s| s.to_string()).collect();
                palette.resize(slots, String::new());
                self.terrain_textures = palette;
                self.terrain_textures_dirty = true;
            }
        }
    }

    /// The world translation of a terrain node (places its field in world space).
    fn terrain_world_origin(&self, e: Entity) -> DVec3 {
        floptle_core::world_transform(&self.world, e).translation
    }

    /// Which terrain a whole-terrain op (Fill) targets: the selected terrain node, or
    /// the one last sculpted, or — if there's exactly one — that terrain.
    fn target_terrain(&self) -> Option<Entity> {
        if let Some(&e) = self.selection.last() {
            if self.terrains.contains_key(&e) {
                return Some(e);
            }
        }
        if let Some(e) = self.active_terrain {
            if self.terrains.contains_key(&e) {
                return Some(e);
            }
        }
        if self.terrains.len() == 1 {
            return self.terrains.keys().next().copied();
        }
        None
    }

    /// Fold every terrain field (each at its node's world translation) into one
    /// world-space combined field for rendering. Cheap no-op clone for one terrain.
    /// True if any terrain node has moved (or the set changed) since the combined
    /// field was last built — i.e. a rebuild is needed.
    fn terrains_moved(&self) -> bool {
        if self.terrains.len() != self.combined_origins.len() {
            return true;
        }
        self.terrains.keys().any(|&e| {
            let o = self.terrain_world_origin(e);
            !self
                .combined_origins
                .iter()
                .any(|(ce, co)| *ce == e && (*co - o).length() < 1e-5)
        })
    }

    /// The surface [`Material`] that drives terrain shading. Terrain uses the same
    /// lighting model as the meshes, so this picks whose lighting params (ambient,
    /// specular/reflectiveness, rim, emissive, unlit, color tint) the combined terrain
    /// adopts: the active terrain's material if it has one, else any terrain that has
    /// one, else a neutral matte default. Per-terrain color still comes from painting.
    fn terrain_material(&self) -> MaterialParams {
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
    }

    fn rebuild_combined(&mut self) {
        if self.terrains.is_empty() {
            self.combined = None;
            self.combined_origins.clear();
            return;
        }
        // Fast path: a single terrain needs no resample — clone it and shift its box
        // center by the node origin so the field reads in world space.
        if self.terrains.len() == 1 {
            let (&e, t) = self.terrains.iter().next().unwrap();
            let o = self.terrain_world_origin(e);
            let mut world = t.clone();
            world.baked.center[0] += o.x as f32;
            world.baked.center[1] += o.y as f32;
            world.baked.center[2] += o.z as f32;
            self.combined = Some(world);
            self.combined_origins = vec![(e, o)];
            return;
        }
        // Deterministic order (by Matter::Terrain id) so the fold is stable.
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
        let mut origins: Vec<(Entity, DVec3)> = Vec::new();
        let volumes: Vec<([f64; 3], &floptle_field::Terrain)> = items
            .iter()
            .filter_map(|(_, e)| {
                let o = self.terrain_world_origin(*e);
                origins.push((*e, o));
                self.terrains.get(e).map(|t| ([o.x, o.y, o.z], t))
            })
            .collect();
        self.combined = Some(floptle_field::Terrain::combine(&volumes, 0.6));
        self.combined_origins = origins;
    }

    // ---- scene-graph (parenting) -------------------------------------------
    /// True if `e` is `ancestor` or one of its descendants (cycle guard).
    fn is_descendant(&self, e: Entity, ancestor: Entity) -> bool {
        let mut cur = e;
        for _ in 0..64 {
            if cur == ancestor {
                return true;
            }
            match self.world.get::<floptle_core::Parent>(cur).copied() {
                Some(floptle_core::Parent(p)) => cur = p,
                None => return false,
            }
        }
        false
    }

    /// Re-parent `child` under `parent` (or make it a root if `None`), preserving
    /// its world placement. Rejects cycles (can't parent under your own descendant).
    fn reparent(&mut self, child: Entity, parent: Option<Entity>) {
        if let Some(p) = parent {
            if self.is_descendant(p, child) {
                return;
            }
        }
        self.record();
        let world = floptle_core::world_transform(&self.world, child);
        match parent {
            Some(p) => self.world.insert(child, floptle_core::Parent(p)),
            None => {
                self.world.remove::<floptle_core::Parent>(child);
            }
        }
        self.set_world_transform(child, world); // keep the same world placement
    }

    /// Spawn a new node as a child of `parent`, sitting at the parent's origin.
    fn add_parented(&mut self, matter: MatterDoc, parent: Entity) {
        self.record();
        let name = matter_doc_name(&matter);
        let e = self.world.spawn();
        self.world.insert(e, Transform::IDENTITY);
        self.world.insert(e, Name(name.into()));
        self.world.insert(e, matter.to_matter());
        self.world.insert(e, floptle_core::Parent(parent));
        self.select_single(e);
    }

    /// Create a new blank scene `<name>.ron`, save it, and switch the editor to it.
    fn new_scene(&mut self, name: &str) {
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
        self.scene_dirty = false;
        self.asset_tree = build_assets(&self.project_root);
        println!("  new scene: {}", path.display());
    }

    /// Open an existing scene `.ron` (double-clicked in Assets). Resets the world to
    /// it, loads its terrain + meshes. The caller handles unsaved-changes prompting.
    fn open_scene_file(&mut self, path: &str) {
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
    fn register_scene_meshes(&mut self) {
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
    fn scene_path(&self) -> PathBuf {
        self.project_root.join("scenes").join(format!("{}.ron", self.scene_name))
    }
    fn project_cfg_path(&self) -> PathBuf {
        self.project_root.join("project.ron")
    }
    fn materials_dir(&self) -> PathBuf {
        self.project_root.join("materials")
    }
    fn scripts_dir(&self) -> PathBuf {
        self.project_root.join("scripts")
    }

    // ---- asset file operations (the in-engine create / rename / delete) --------
    /// Create a new folder inside `dir` (auto-numbered if `new_folder` is taken),
    /// then rescan so it appears in the browser.
    fn new_folder(&mut self, dir: &str) {
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
    fn new_script(&mut self, dir: &str) {
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
        self.selected_asset = Some(p);
    }

    /// Rename a file/folder to `new_name` within its current parent directory.
    fn rename_asset(&mut self, from: &str, new_name: &str) {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return;
        }
        let src = PathBuf::from(from);
        let dst = src.parent().unwrap_or(Path::new(".")).join(new_name);
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
                f.name = new_name.to_string();
            }
        }
        if self.selected_asset.as_deref() == Some(from) {
            self.selected_asset = Some(dst_str);
        }
        self.asset_tree = build_assets(&self.project_root);
    }

    /// Delete a file or folder (recursively) and drop any references to it.
    fn delete_asset(&mut self, path: &str) {
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
    fn seed_project_dirs(&self) {
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

    fn load_materials(&self) -> Vec<(String, floptle_scene::MaterialDoc)> {
        floptle_scene::load_materials(&self.materials_dir())
    }

    /// Load the project's active scene + the file it came from: `scenes/first.ron`
    /// if present, else the first `.ron` in `scenes/`, else a tiny built-in default.
    /// The returned path's stem becomes `scene_name`, so edits save back to the same
    /// file even if the scene's internal name differs.
    fn load_active_scene(&self) -> (PathBuf, floptle_scene::SceneDoc) {
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
    fn scene_name_of(path: &std::path::Path) -> String {
        path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "untitled".into())
    }

    /// Switch the editor to the project rooted at `root`, reloading everything.
    fn open_project(&mut self, root: PathBuf) {
        self.project_root = root;
        self.seed_project_dirs();
        let (path, doc) = self.load_active_scene();
        self.scene_name = Self::scene_name_of(&path);
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.adopt_terrain();
        self.project = floptle_scene::load_project(&self.project_cfg_path());
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
    fn new_project(&mut self, root: PathBuf) {
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
    fn close_project(&mut self) {
        self.world = World::new();
        floptle_scene::spawn_into(&empty_scene(), &mut self.world);
        self.scene_name = "untitled".into();
        self.terrains.clear();
        self.active_terrain = None;
        self.combined = None;
        self.selection.clear();
        self.selected_asset = None;
        self.ide = IdeState::default();
        self.history = History::default();
        self.playing = false;
        self.paused = false;
        self.mesh_registry.clear();
    }

    fn save_scene(&self) {
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
    fn terrain_palette_path(&self) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.palette", self.scene_name))
    }

    /// Ctrl+S: save everything — the project config, the open scene, and every
    /// dirty script open in the IDE (so "the script you're editing" is saved too).
    fn save_all(&mut self) {
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
        parent: None,
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
fn default_scene() -> floptle_scene::SceneDoc {
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
                parent: None,
            },
            NodeDoc {
                name: "sphere".into(),
                transform: TransformDoc { translation: [2.0, 0.0, 0.0], ..Default::default() },
                matter: MatterDoc::Primitive { shape: ShapeDoc::Sphere, color: [0.4, 0.7, 0.95] },
                scripts: Vec::new(),
                material: None,
                rigidbody: None,
                mesh_collider: false,
                parent: None,
            },
            NodeDoc {
                name: "blob".into(),
                transform: TransformDoc { translation: [0.0, 1.6, 0.0], ..Default::default() },
                matter: MatterDoc::Blob { scale: 1.0 },
                scripts: Vec::new(),
                material: None,
                rigidbody: None,
                mesh_collider: false,
                parent: None,
            },
            default_camera_node(),
        ],
    }
}

