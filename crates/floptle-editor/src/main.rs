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

use floptle_core::math::{DVec3, EulerRot, Mat4, Quat, Vec2, Vec3, Vec4};
use floptle_core::transform::Transform;
use floptle_core::{Entity, Light, Material, Matter, Name, ScriptInst, Scripts, Shape, World};
use floptle_script::ScriptHost;
use floptle_render::{
    cube, instance_of, instance_of_mat, uv_sphere, FlyCamera, Globals, Gpu, Grid, Input,
    InstanceRaw, MaterialParams, MeshId, Outline, Raster, Raymarch, RaymarchGlobals, Retro, TexId,
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
    /// Open the Terrain window.
    show_terrain: bool,
    /// Open the "new scene" name prompt.
    open_new_scene: bool,
    /// Create a new blank scene with this name (from Assets ▸ New ▸ Scene).
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
}

impl Default for GridConfig {
    fn default() -> Self {
        Self { show: true, size: 1.0, extent: 24, color: [0.45, 0.45, 0.58], alpha: 0.32, snap: false }
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
";

/// `.luarc.json` pointing the Lua language server at the annotation library and
/// declaring the engine globals (so they aren't flagged undefined).
const LUARC_JSON: &str = "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"log\"]\n}\n";

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
        MatterDoc::Blob { .. } => "Blob",
        MatterDoc::Mesh { .. } => "Mesh",
        MatterDoc::Empty => "Group",
        MatterDoc::Terrain => "Terrain",
    }
}
fn new_sphere() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Sphere, color: [0.4, 0.6, 0.9] }
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
    let inv = Vec3::ONE / rd; // 0 components → ±inf, handled by the min/max
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
    Assets,
    Scene,
    Scripting,
}

impl EditorTab {
    fn title(self) -> &'static str {
        match self {
            EditorTab::Hierarchy => "Hierarchy",
            EditorTab::Inspector => "Inspector",
            EditorTab::Assets => "Assets",
            EditorTab::Scene => "Scene",
            EditorTab::Scripting => "Scripting",
        }
    }
}

/// The default layout: Hierarchy left, Inspector right, Assets bottom, with the
/// Scene + Scripting tabs filling the center. Users can drag/re-dock freely.
fn default_dock() -> egui_dock::DockState<EditorTab> {
    use egui_dock::{DockState, NodeIndex};
    let mut dock = DockState::new(vec![EditorTab::Scene, EditorTab::Scripting]);
    let surface = dock.main_surface_mut();
    let [central, _] = surface.split_left(NodeIndex::root(), 0.18, vec![EditorTab::Hierarchy]);
    let [central, _] = surface.split_right(central, 0.78, vec![EditorTab::Inspector]);
    let [_, _] = surface.split_below(central, 0.72, vec![EditorTab::Assets]);
    dock
}

/// Focus the Scripting tab (used after double-click-to-open-a-script).
fn focus_scripting_tab(dock: &mut egui_dock::DockState<EditorTab>) {
    let surface = dock.main_surface_mut();
    if let Some((node, tab)) = surface.find_tab(&EditorTab::Scripting) {
        let _ = surface.set_active_tab(node, tab);
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
  section, or use Inspector ▸ Scripting ▸ + Add Script.
• Press F1 (▶ Play) to run; F2 pauses the clock; ⏹ Stop restores the scene.
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

/// State of the Scripting-tab IDE: the open files and which one is shown
/// (`None` = the built-in Docs page).
struct IdeState {
    open: Vec<OpenScript>,
    active: Option<usize>,
}

impl Default for IdeState {
    fn default() -> Self {
        Self { open: Vec::new(), active: None }
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
    entity_names: &'a [(Entity, String)],
    materials: &'a [(String, floptle_scene::MaterialDoc)],
    mat_name_buf: &'a mut String,
    /// Whether the floating Material Editor window is open.
    show_material_editor: &'a mut bool,
    asset_tree: &'a [AssetEntry],
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

    // The Scene tab is transparent so the 3D render shows through it.
    fn clear_background(&self, tab: &EditorTab) -> bool {
        !matches!(tab, EditorTab::Scene)
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut EditorTab) {
        match tab {
            EditorTab::Hierarchy => self.hierarchy_ui(ui),
            EditorTab::Inspector => self.inspector_ui(ui),
            EditorTab::Assets => self.assets_ui(ui),
            EditorTab::Scene => self.scene_ui(ui),
            EditorTab::Scripting => self.scripting_ui(ui),
        }
    }
}

impl EditorTabViewer<'_> {
    fn hierarchy_ui(&mut self, ui: &mut egui::Ui) {
        // Scene name + save at the top of the hierarchy.
        ui.horizontal(|ui| {
            ui.strong(format!("🎬 {}", self.scene_name));
            if ui.small_button("💾").on_hover_text("Save scene (Ctrl+S)").clicked() {
                self.cmd.save_scene = true;
            }
            ui.label("ⓘ").on_hover_text(
                "Tools: 1 select · 2 move · 3 rotate · 4 scale · 5 sculpt\n\
                 F focus · Q unselect · ↑/↓ step selection · Del delete\n\
                 F1 play · F2 pause · Ctrl+S save · Ctrl+Z/Y undo/redo\n\
                 Viewport: LMB select · Shift+LMB multi · RMB-drag look · RMB-click menu",
            );
        });
        ui.horizontal_wrapped(|ui| {
            if ui.small_button("+ Cube").clicked() {
                self.cmd.add = Some(new_cube());
            }
            if ui.small_button("+ Sphere").clicked() {
                self.cmd.add = Some(new_sphere());
            }
            if ui.small_button("+ Blob").clicked() {
                self.cmd.add = Some(MatterDoc::Blob { scale: 1.0 });
            }
            if ui.small_button("🗀 Folder").on_hover_text("an empty group to organize / parent nodes").clicked() {
                self.cmd.add = Some(MatterDoc::Empty);
            }
        });
        ui.separator();

        // Build the parent→children tree from the world (owned copies, so the
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
            // Drop a node onto the empty area below → make it a root (unparent).
            let bg = ui.allocate_response(ui.available_size(), egui::Sense::hover());
            if let Some(p) = bg.dnd_release_payload::<NodePayload>() {
                self.cmd.reparent = Some((p.0, None));
            }
        });
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
        let is_folder = matches!(self.world.get::<Matter>(e), Some(Matter::Empty));
        let has_kids = children.get(&e).map(|c| !c.is_empty()).unwrap_or(false);
        let icon = if is_folder { "🗀" } else if has_kids { "▾" } else { "•" };
        let selected = self.selection.contains(&e);

        let resp = ui
            .horizontal(|ui| {
                ui.add_space(depth as f32 * 14.0);
                let text = if selected {
                    egui::RichText::new(format!("{icon} {name}")).strong().color(ui.visuals().selection.stroke.color)
                } else {
                    egui::RichText::new(format!("{icon} {name}"))
                };
                ui.add(egui::Label::new(text).selectable(false).sense(egui::Sense::click_and_drag()))
            })
            .inner;
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
            ui.menu_button("➕ Add child", |ui| {
                if ui.button("🗀 Folder").clicked() {
                    self.cmd.add_parented = Some((MatterDoc::Empty, e));
                    ui.close();
                }
                if ui.button("Cube").clicked() {
                    self.cmd.add_parented = Some((new_cube(), e));
                    ui.close();
                }
                if ui.button("Sphere").clicked() {
                    self.cmd.add_parented = Some((new_sphere(), e));
                    ui.close();
                }
                if ui.button("Blob").clicked() {
                    self.cmd.add_parented = Some((MatterDoc::Blob { scale: 1.0 }, e));
                    ui.close();
                }
            });
            if self.world.get::<floptle_core::Parent>(e).is_some() && ui.button("⤴ Unparent").clicked() {
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

        if let Some(kids) = children.get(&e) {
            for &c in kids {
                self.hierarchy_node(ui, c, children, names, depth + 1);
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
            } else if is_script(&path) {
                ui.label("script — drag onto a node, double-click, or:");
                if ui.button("✎  Open in Scripting").clicked() {
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
                                .button("⤓ Extract textures")
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
                        Matter::Terrain => {
                            ui.label("editable terrain");
                            ui.small("a sculptable SDF field — move it with the transform below");
                            if ui.button("⛰ Open Terrain tools").clicked() {
                                cmd.show_terrain = true;
                            }
                        }
                    }
                }

                // ---- Material (surface look) ----
                ui.separator();
                let has_mat = world.get::<Material>(e).is_some();
                let mut tex_list = Vec::new();
                collect_texture_paths(self.asset_tree, &mut tex_list);
                egui::CollapsingHeader::new("🎨 Material").default_open(has_mat).show(ui, |ui| {
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
                        if ui.button("⤢ Open in Material Editor").clicked() {
                            *self.show_material_editor = true;
                        }
                    } else {
                        ui.small("Default look. Add a material to customize emissive, specular, rim, unlit shading…");
                        ui.horizontal(|ui| {
                            if ui.button("➕ Add material").clicked() {
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
                                ui.small("⬇  drop a script here to attach");
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
                                    if ui.small_button("✎").on_hover_text("edit script").clicked() {
                                        let p = self
                                            .project_root
                                            .join("scripts")
                                            .join(format!("{}.lua", inst.kind));
                                        cmd.open_script_pref = Some(p.to_string_lossy().to_string());
                                    }
                                    if ui.small_button("✕").on_hover_text("remove").clicked() {
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
            egui::Window::new("🎨 Material Editor")
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
                            if ui.button("➕ Add material").clicked() {
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

    fn assets_ui(&mut self, ui: &mut egui::Ui) {
        let root = self.project_root.to_path_buf();
        ui.horizontal(|ui| {
            ui.strong("Assets");
            if ui.small_button("⟳").on_hover_text("rescan").clicked() {
                self.cmd.refresh_assets = true;
            }
            ui.menu_button("➕ New", |ui| {
                self.new_asset_menu(ui, &root);
            });
            ui.separator();
            ui.small("right-click for New Folder / New Script · double-click a script to edit · drag onto the scene or a node");
        });
        ui.separator();
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

    /// The shared "New Folder / New Script" submenu, targeting `dir`.
    fn new_asset_menu(&mut self, ui: &mut egui::Ui, dir: &Path) {
        if ui.button("🗀 New Folder").clicked() {
            self.cmd.new_folder_in = Some(dir.to_string_lossy().to_string());
            ui.close();
        }
        if ui.button("✎ New Lua Script").clicked() {
            self.cmd.new_script_in = Some(dir.to_string_lossy().to_string());
            ui.close();
        }
        if ui.button("🎬 New Scene").clicked() {
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
                    let label = if draggable { format!("⠿  {name}") } else { format!("    {name}") };
                    // A single widget that senses BOTH click and drag. (The old
                    // dnd_drag_source layered a drag-sense interaction over the label,
                    // and the drag sense swallowed double-clicks — so a script could
                    // only be dragged, never opened.) One click_and_drag widget lets
                    // egui tell a tap from a drag cleanly: tap → select / double-tap
                    // → open; press-and-move → drag a payload onto the scene or a node.
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
                    if resp.double_clicked() && openable {
                        self.cmd.open_script_pref = Some(path.clone());
                    }
                    resp.context_menu(|ui| {
                        if openable && ui.button("✎ Open in Scripting tab").clicked() {
                            self.cmd.open_script = Some(path.clone());
                            self.cmd.focus_scripting = true;
                            ui.close();
                        }
                        if ui.button("🗂 Open in file explorer").clicked() {
                            reveal_in_explorer(Path::new(path));
                            ui.close();
                        }
                        if ui.button("✏ Rename…").clicked() {
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

    fn scene_ui(&mut self, ui: &mut egui::Ui) {
        // This tab's rect IS the 3D viewport; cache it for picking / gizmo gating.
        let rect = ui.max_rect();
        *self.scene_rect = Some(rect);

        // Overlay toolbar: tools (left) + resolution simulator (right).
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

        // The gizmo paints on a layer above the scene, clipped to this tab.
        if let Some(g) = self.gizmo {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("gizmo")))
                .with_clip_rect(rect);
            paint_gizmo(&painter, g, self.tool, self.grabbed, self.ppp);
        }

        // Terrain brush telegraph: a ring at the surface + a normal line, so you can
        // see exactly where (and on what facing) a stroke will land.
        if let Some(viz) = self.terrain_viz {
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
    }

    fn scripting_ui(&mut self, ui: &mut egui::Ui) {
        // Live script errors (from the last play frame) surface here in red.
        if !self.script_errors.is_empty() {
            egui::Frame::NONE
                .fill(egui::Color32::from_rgb(60, 20, 20))
                .inner_margin(6.0)
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("⚠ script errors").strong().color(egui::Color32::from_rgb(255, 150, 150)));
                    for e in self.script_errors {
                        ui.label(egui::RichText::new(e).monospace().color(egui::Color32::from_rgb(255, 180, 180)));
                    }
                });
        }
        // Tab strip: Docs + each open file.
        ui.horizontal_wrapped(|ui| {
            if ui.selectable_label(self.ide.active.is_none(), "📖 Docs").clicked() {
                self.ide.active = None;
            }
            let mut close: Option<usize> = None;
            for i in 0..self.ide.open.len() {
                let f = &self.ide.open[i];
                let title = if f.dirty { format!("{} *", f.name) } else { f.name.clone() };
                if ui.selectable_label(self.ide.active == Some(i), title).clicked() {
                    self.ide.active = Some(i);
                }
                if ui.small_button("✕").clicked() {
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
                    egui::CollapsingHeader::new("📑 API reference")
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
                    if ui.button("💾 Save").clicked() {
                        let f = &mut self.ide.open[i];
                        if std::fs::write(&f.path, &f.text).is_ok() {
                            f.dirty = false;
                            self.cmd.refresh_assets = true;
                        }
                    }
                    if ui
                        .button("⮕ Open in IDE")
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
                    ui.colored_label(egui::Color32::from_rgb(235, 120, 120), format!("⚠ line {line}: {msg}"));
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
                    ui.small("↹ Tab to accept");
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
    "node", "params", "time", "dt", "defaults", "log", "start", "update", "math", "string",
    "table", "ipairs", "pairs", "print", "tostring", "tonumber", "pcall", "select",
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
}

impl Default for TerrainBrush {
    fn default() -> Self {
        Self {
            mode: floptle_field::Brush::Raise,
            radius: 2.5,
            strength: 0.5,
            color: [0.45, 0.32, 0.2],
            tex_slot: -1,
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
    /// Imported glTF models, keyed by asset path → registered mesh parts.
    mesh_registry: HashMap<String, MeshAsset>,
    /// Material textures registered on the GPU, keyed by image path → handle.
    texture_registry: HashMap<String, TexId>,
    /// The editable terrain SDF field (None until "Create terrain").
    terrain: Option<floptle_field::Terrain>,
    /// The scene node that *is* the terrain (its transform places the volume).
    terrain_entity: Option<Entity>,
    /// The terrain volume needs re-uploading to the GPU.
    terrain_dirty: bool,
    /// LMB held with the Sculpt tool — keep brushing on mouse motion.
    sculpting: bool,
    /// Where the last brush dab landed + when — for movement-spaced, rate-limited
    /// strokes (so the brush behaves like a real paint tool, not 200 dabs/sec).
    last_dab_pos: Option<DVec3>,
    last_dab_time: Option<Instant>,
    /// Pre-stroke field bytes captured on mouse-down — pushed to the undo timeline
    /// on mouse-up if the stroke actually deformed the terrain. `None` between
    /// strokes. The whole stroke collapses to a single undo step.
    stroke_snapshot: Option<Vec<u8>>,
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
    /// Whether the Terrain window is open.
    show_terrain: bool,
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
    /// Named material presets loaded from assets/materials/.
    materials: Vec<(String, floptle_scene::MaterialDoc)>,
    /// Whether the floating Material Editor window is open.
    show_material_editor: bool,
    /// Scratch buffer for the "save material" name field.
    mat_name_buf: String,
    /// Play mode: scripts run; the pre-play authored scene is restored on stop.
    playing: bool,
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
    last: Option<Instant>,
    started: Option<Instant>,
    gpu: Option<Gpu>,
}

/// One reversible step on the unified timeline. Scene edits store a whole-scene
/// doc; terrain strokes store the field's serialized bytes. Keeping both kinds on
/// one stack means Ctrl+Z walks back through scene + terrain edits in true order.
enum Snapshot {
    Scene(floptle_scene::SceneDoc),
    Terrain(Vec<u8>),
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

impl ApplicationHandler for Editor {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // The default project is the repo's `assets/` folder; File ▸ Open/New
        // re-points this elsewhere.
        self.project_root = PathBuf::from("assets");
        self.dock_state = Some(default_dock());
        self.viewport_zoom = 0.9;
        self.terrain_detail = 64;
        self.terrain_textures = vec![String::new(); floptle_render::TERRAIN_SLOTS as usize];
        self.external_editor = load_external_editor();
        self.prefer_external_editor = load_prefer_external();
        let attrs = Window::default_attributes()
            .with_title("Floptle Editor")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        let gpu = Gpu::new(window.clone());
        let mut raster = Raster::new(&gpu);
        let cube_id = raster.register(&gpu, &cube(0.7), None);
        let sphere_id = raster.register(&gpu, &uv_sphere(0.85, 24, 36), None);
        self.mesh_ids = vec![cube_id, sphere_id];
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

        self.retro = Some(Retro::new(&gpu, self.project.retro_height.max(80)));
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
                                KeyCode::ArrowUp => self.step_selection(-1),
                                KeyCode::ArrowDown => self.step_selection(1),
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
                if pressed {
                    let over_scene = self.cursor_over_scene();
                    let hovered = self.gizmo.as_ref().and_then(|g| g.hovered);
                    if over_scene && self.tool == Tool::Sculpt {
                        // Sculpt tool: start a brush stroke on the terrain (applied
                        // next frame in terrain_frame_update).
                        self.context_menu = None;
                        if let Some(t) = self.terrain.as_ref() {
                            self.sculpting = true;
                            self.last_dab_pos = None; // first dab fires immediately
                            self.last_dab_time = None;
                            // Snapshot the field so the whole stroke is one undo step.
                            self.stroke_snapshot = Some(t.to_bytes());
                            self.stroke_dabbed = false;
                        }
                    } else if over_scene {
                        // Clicking the viewport dismisses an open context menu (but
                        // clicking a panel/menu, which isn't over_scene, keeps it).
                        self.context_menu = None;
                        if let (Some(h), Some(e)) = (hovered, self.primary()) {
                            // On a gizmo handle → start an undoable edit and grab it.
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
                            // Empty viewport → pick: single-select, or Shift to add.
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
                    if let Some(snap) = self.stroke_snapshot.take() {
                        if self.stroke_dabbed {
                            self.push_history(Snapshot::Terrain(snap));
                        }
                    }
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Right, .. } => {
                let pressed = state == ElementState::Pressed;
                let over_scene = self.cursor_over_scene();
                if pressed {
                    // Begin a possible look; if the cursor barely moves before release
                    // it's a click → open a context menu instead.
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
                    // A click (negligible motion) over the viewport → context menu.
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

        // Live Lua syntax check for the active IDE file (drives red squiggles).
        self.ide_diag = self.ide.active.and_then(|i| self.ide.open.get(i)).and_then(|f| {
            if f.path.ends_with(".lua") {
                self.script_host.check_syntax(&f.text)
            } else {
                None
            }
        });

        // Re-upload the terrain volume to the GPU after an edit (needs &mut Raymarch,
        // before the read-only destructure below).
        if self.terrain_dirty {
            if let (Some(gpu), Some(raymarch), Some(terrain)) =
                (self.gpu.as_ref(), self.raymarch.as_mut(), self.terrain.as_ref())
            {
                raymarch.set_volume(gpu, &terrain.baked);
            }
            self.terrain_dirty = false;
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

        let (
            Some(gpu),
            Some(raster),
            Some(raymarch),
            Some(retro),
            Some(outline),
            Some(grid_render),
            Some(egui),
            Some(window),
        ) = (
            self.gpu.as_mut(),
            self.raster.as_mut(),
            self.raymarch.as_ref(),
            self.retro.as_mut(),
            self.outline.as_ref(),
            self.grid_render.as_mut(),
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
            self.script_host.run(&mut self.world, &dir, sdt, self.play_t);
            self.script_errors = self.script_host.errors().to_vec();
        } else if !self.script_errors.is_empty() {
            self.script_errors.clear();
        }

        // ---- gather the scene from the World ----
        let aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
        let cam = self.camera.render_camera();
        let view_proj = cam.view_proj(aspect);

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
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [light_node.color[0], light_node.color[1], light_node.color[2], 0.0],
            ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
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
        let mut blobs: Vec<(DVec3, f32)> = Vec::new();
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
                    blobs.push((t.translation, scale * t.scale.x));
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
                Matter::Empty | Matter::Terrain => {} // group / terrain render via other passes
            }
        }

        let clear = [0.02f32, 0.02, 0.05, 1.0];
        // Build raymarch globals for a set of blobs (all of them, or just one for the
        // selection mask). Up to 16 blobs are folded together in one march.
        let make_rm = |set: &[(DVec3, f32)]| -> RaymarchGlobals {
            let mut arr = [[0.0f32; 4]; 16];
            let n = set.len().min(16);
            for (i, (center, scale)) in set.iter().take(16).enumerate() {
                let c = (*center - cam.world_position).as_vec3();
                arr[i] = [c.x, c.y, c.z, scale.max(0.05)];
            }
            RaymarchGlobals {
                view_proj: view_proj.to_cols_array_2d(),
                inv_view_proj: view_proj.inverse().to_cols_array_2d(),
                light_dir: [light.x, light.y, light.z, 0.0],
                light_color: [light_node.color[0], light_node.color[1], light_node.color[2], 0.0],
                ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
                bg: [clear[0], clear[1], clear[2], 1.0],
                center: [0.0; 4],
                params: [elapsed, n as f32, 0.0, 0.0],
                vol_center: [0.0, 0.0, 0.0, 0.0], // no baked volume in v1
                vol_half: [1.0, 1.0, 1.0, 0.5],
                blobs: arr,
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
                        mask_blob = Some(make_rm(&[(t.translation, scale * t.scale.x)]));
                    }
                    Matter::Empty | Matter::Terrain => {}
                }
            }
        }

        // The raymarch pass renders the blob matter (gated by the SDF-matter toggle)
        // and/or the editable terrain volume. Build its globals if either is present.
        let show_blobs = self.project.matter && !blobs.is_empty();
        let rm = if show_blobs || self.terrain.is_some() {
            let mut g = make_rm(if show_blobs { &blobs } else { &[] });
            if let Some((hf, bc)) =
                self.terrain.as_ref().map(|t| (t.baked.half_extent, t.baked.center))
            {
                // The terrain node's transform places the volume box. (Inlined direct
                // field access — `terrain_origin()` would borrow all of `self` while
                // gpu/egui are mutably borrowed here.) The field's own `baked.center`
                // shifts as the slab expands asymmetrically, so the box center is the
                // node origin plus that local offset.
                let origin = self
                    .terrain_entity
                    .map(|e| floptle_core::world_transform(&self.world, e).translation)
                    .unwrap_or(DVec3::ZERO);
                let cr = origin + DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64)
                    - cam.world_position;
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
        let project = &mut self.project;
        let show_project_settings = &mut self.show_project_settings;
        let show_project_mgr = &mut self.show_project_mgr;
        let project_path_buf = &mut self.project_path_buf;
        let grid = &mut self.grid;
        let show_grid_settings = &mut self.show_grid_settings;
        let rename_target = &mut self.rename_target;
        let new_scene_buf = &mut self.new_scene_buf;
        let show_terrain = &mut self.show_terrain;
        let terrain_brush = &mut self.terrain_brush;
        let terrain_detail = &mut self.terrain_detail;
        let terrain_textures = &mut self.terrain_textures;
        let terrain_present = self.terrain.is_some();
        let terrain_voxels = self.terrain.as_ref().map(|t| {
            let [a, b, c] = t.baked.dims;
            (a, b, c)
        });
        let external_editor = &mut self.external_editor;
        let prefer_external = &mut self.prefer_external_editor;
        let asset_tree = &self.asset_tree;
        let project_root = self.project_root.as_path();
        let playing = self.playing;
        let paused = self.paused;
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
                        ui.checkbox(&mut *show_terrain, "Terrain");
                    });
                    ui.menu_button("Project", |ui| {
                        if ui.button("Settings…").clicked() {
                            *show_project_settings = true;
                            ui.close();
                        }
                    });
                    ui.separator();
                    let play_label = if playing { "⏹ Stop  (F1)" } else { "▶ Play  (F1)" };
                    if ui.button(play_label).clicked() {
                        cmd.toggle_play = true;
                    }
                    if playing {
                        let pause_label = if paused { "▶ Resume  (F2)" } else { "⏸ Pause  (F2)" };
                        if ui.button(pause_label).clicked() {
                            cmd.toggle_pause = true;
                        }
                    }
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
                entity_names: &entity_names,
                materials,
                mat_name_buf,
                show_material_editor,
                asset_tree,
                project_root,
                selected_asset,
                ide,
                script_errors,
                ide_diag,
                gizmo,
                terrain_viz,
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

            // ---- new / open project window (rfd unavailable → a text path) ----
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

            // ---- terrain window ----
            egui::Window::new("⛰ Terrain")
                .open(show_terrain)
                .resizable(false)
                .default_width(260.0)
                .show(ui.ctx(), |ui| {
                    use floptle_field::Brush;
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
                        ui.small(format!("current: {a}×{b}×{c} voxels"));
                    }
                    if !terrain_present {
                        if ui.button("➕ Create flat terrain").clicked() {
                            cmd.create_terrain = true;
                        }
                        ui.small("Then press 5 (Sculpt tool) and LMB-drag in the viewport.");
                        return;
                    }
                    if ui.button("↻ Recreate at this detail").on_hover_text("clears the current terrain").clicked() {
                        cmd.create_terrain = true;
                    }
                    ui.separator();
                    ui.label("Sculpt tool (key 5) — LMB-drag to brush.");
                    ui.label("Brush");
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(&mut terrain_brush.mode, Brush::Raise, "⬆ Raise");
                        ui.selectable_value(&mut terrain_brush.mode, Brush::Lower, "⬇ Lower");
                        ui.selectable_value(&mut terrain_brush.mode, Brush::Flatten, "▱ Flatten");
                        ui.selectable_value(&mut terrain_brush.mode, Brush::Smooth, "～ Smooth");
                        ui.selectable_value(&mut terrain_brush.mode, Brush::Paint, "🎨 Paint");
                    });
                    ui.add(egui::Slider::new(&mut terrain_brush.radius, 0.5..=8.0).text("radius"));
                    ui.add(egui::Slider::new(&mut terrain_brush.strength, 0.05..=1.0).text("strength"));
                    if terrain_brush.mode == Brush::Paint {
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("paint:");
                            ui.selectable_value(&mut terrain_brush.tex_slot, -1, "Color");
                        });
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
                        // Texture palette: assign an image per slot, then click a slot
                        // to paint that texture (triplanar) onto the terrain.
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
                                if ui.selectable_label(sel, format!("🖌 {label}")).clicked() {
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
                    if ui.button("🗑 Clear terrain").clicked() {
                        cmd.clear_terrain = true;
                    }
                });
            // (the gizmo now paints inside the Scene tab, clipped to its rect)
        });
        egui.state.handle_platform_output(&window, full_output.platform_output);
        if self.project.retro_height != old_retro_h {
            retro.resize(gpu, self.project.retro_height.max(80));
        }

        // ---- draw: scene into the retro target, blit, then egui on top ----
        match gpu.acquire() {
            Some(frame) => {
                let (color, depth) = if self.project.retro {
                    (retro.color_view(), retro.depth_view())
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
                        [c[0], c[1], c[2], self.grid.alpha],
                    );
                }
                if self.project.retro {
                    retro.blit(gpu, &frame);
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
                MatterDoc::Blob { .. } => "Blob",
                MatterDoc::Mesh { .. } => "Mesh",
                MatterDoc::Empty => "Group",
                MatterDoc::Terrain => "Terrain",
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
            self.show_terrain = true;
        }
        if cmd.clear_terrain {
            self.terrain = None;
            if let Some(e) = self.terrain_entity.take() {
                self.world.despawn(e);
            }
        }
        if cmd.terrain_palette_changed {
            self.terrain_textures_dirty = true;
        }
        if cmd.show_terrain {
            self.show_terrain = true;
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

    /// Load + register a material texture (cached by path), returning its handle.
    fn ensure_texture(&mut self, path: &str) -> Option<TexId> {
        if let Some(id) = self.texture_registry.get(path) {
            return Some(*id);
        }
        let data = floptle_assets::load_texture(Path::new(path))?;
        let (gpu, raster) = (self.gpu.as_ref()?, self.raster.as_mut()?);
        let id = raster.register_texture(gpu, &data);
        self.texture_registry.insert(path.to_string(), id);
        Some(id)
    }

    /// Switch the active tool and cancel any in-progress gizmo drag.
    fn set_tool(&mut self, tool: Tool) {
        self.tool = tool;
        self.grabbed = None;
        self.drag = None;
        // Selecting Sculpt pops the Terrain tools so the brush controls are at hand.
        if tool == Tool::Sculpt {
            self.show_terrain = true;
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
    fn apply_terrain_bytes(&mut self, bytes: &[u8]) {
        if let Some(t) = floptle_field::Terrain::from_bytes(bytes) {
            self.terrain = Some(t);
            self.terrain_dirty = true;
        }
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
            Some(Snapshot::Terrain(prev)) => {
                if let Some(cur) = self.terrain.as_ref().map(|t| t.to_bytes()) {
                    self.history.redo.push(Snapshot::Terrain(cur));
                }
                self.apply_terrain_bytes(&prev);
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
            Some(Snapshot::Terrain(next)) => {
                if let Some(cur) = self.terrain.as_ref().map(|t| t.to_bytes()) {
                    self.history.undo.push(Snapshot::Terrain(cur));
                }
                self.apply_terrain_bytes(&next);
            }
            None => {}
        }
    }

    /// Enter/leave play mode. Play snapshots the authored scene and runs scripts;
    /// Stop restores the authored scene so script-driven changes aren't persisted.
    fn toggle_play(&mut self) {
        if self.playing {
            self.playing = false;
            self.paused = false;
            if let Some(snap) = self.play_snapshot.take() {
                self.restore(snap);
            }
        } else {
            self.play_snapshot = Some(self.snapshot());
            self.play_t = 0.0;
            self.paused = false;
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
        Some(NodeDoc { name, transform, matter: MatterDoc::from(matter), scripts, material, parent: None })
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
            if self.terrain_entity == Some(e) {
                self.terrain = None;
                self.terrain_entity = None;
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
                Matter::Empty | Matter::Terrain => None, // no mesh — select via the hierarchy
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
    /// start-of-drag snapshot (no per-event accumulation → no drift).
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
        if self.tool != Tool::Sculpt || self.terrain.is_none() || !self.cursor_over_scene() {
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
        // The field lives in the terrain node's local space, so cast in that space.
        let origin = self.terrain_origin();
        let ro_local = cam.world_position + ro_rel.as_dvec3() - origin;
        let ro = [ro_local.x as f32, ro_local.y as f32, ro_local.z as f32];
        let rd_a = [rd.x, rd.y, rd.z];

        let terrain = self.terrain.as_ref().unwrap();
        let Some(hit) = terrain.raycast(ro, rd_a) else {
            return;
        };
        let nrm = terrain.normal(hit);
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
            let terrain = self.terrain.as_mut().unwrap();
            // Infinite terrain: grow the field outward when the brush nears an edge,
            // so the slab has no fixed bounds. (Skip for Paint — painting never
            // extends the shape.) Growth keeps voxel size constant.
            if !matches!(brush.mode, floptle_field::Brush::Paint) {
                terrain.ensure_contains(hit, brush.radius * 1.5);
            }
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
            self.terrain_dirty = true;
            self.stroke_dabbed = true; // mark this stroke as worth an undo step
        }
    }

    /// Voxel dims for the current detail setting over the terrain box (≈2:1:2).
    fn terrain_dims(&self) -> [u32; 3] {
        let d = self.terrain_detail.clamp(24, 192);
        [d, (d * 3 / 8).max(8), d]
    }

    /// Create a fresh flat terrain at the current detail, as a scene node. Its
    /// transform places the volume; the field is centered in the node's local space.
    fn create_terrain(&mut self) {
        self.terrain = Some(floptle_field::Terrain::flat(
            self.terrain_dims(),
            [0.0, 0.0, 0.0],
            [16.0, 6.0, 16.0],
            0.0,
            [0.35, 0.6, 0.28],
        ));
        self.terrain_dirty = true;
        // Reuse the existing terrain node, or spawn one.
        if self.terrain_entity.and_then(|e| self.world.get::<Matter>(e)).is_none() {
            self.record();
            let e = self.world.spawn();
            self.world.insert(e, Transform::IDENTITY);
            self.world.insert(e, Name("Terrain".into()));
            self.world.insert(e, Matter::Terrain);
            self.terrain_entity = Some(e);
            self.select_single(e);
        }
    }

    /// Where a scene's terrain field is stored (one file per scene).
    fn terrain_field_path(&self) -> PathBuf {
        self.project_root.join("terrain").join(format!("{}.tfield", self.scene_name))
    }

    /// After loading a scene, adopt its terrain node (if any) and load its field
    /// from disk; otherwise clear any terrain. Call once `scene_name` is set.
    fn adopt_terrain(&mut self) {
        let e = self
            .world
            .query::<Matter>()
            .find(|(_, m)| matches!(m, Matter::Terrain))
            .map(|(e, _)| e);
        self.terrain_entity = e;
        self.terrain = None;
        if e.is_some() {
            self.terrain = std::fs::read(self.terrain_field_path())
                .ok()
                .and_then(|b| floptle_field::Terrain::from_bytes(&b));
            // A terrain node with no/garbled field → start it flat.
            if self.terrain.is_none() {
                self.terrain = Some(floptle_field::Terrain::flat(
                    self.terrain_dims(),
                    [0.0, 0.0, 0.0],
                    [16.0, 6.0, 16.0],
                    0.0,
                    [0.35, 0.6, 0.28],
                ));
            }
            self.terrain_dirty = true;
        }
    }

    /// The world position of the terrain volume's box center (the node's placement).
    fn terrain_origin(&self) -> DVec3 {
        self.terrain_entity
            .map(|e| floptle_core::world_transform(&self.world, e).translation)
            .unwrap_or(DVec3::ZERO)
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
            nodes: Vec::new(),
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
        self.asset_tree = build_assets(&self.project_root);
        println!("  new scene: {}", path.display());
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
        self.terrain = None;
        self.terrain_entity = None;
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
        // The terrain field is large, so it lives beside the scene (not inline).
        if let Some(t) = &self.terrain {
            let tp = self.terrain_field_path();
            let _ = std::fs::create_dir_all(tp.parent().unwrap_or(Path::new(".")));
            if let Err(e) = std::fs::write(&tp, t.to_bytes()) {
                eprintln!("  save terrain failed: {e}");
            }
        }
    }

    /// Ctrl+S: save everything — the project config, the open scene, and every
    /// dirty script open in the IDE (so "the script you're editing" is saved too).
    fn save_all(&mut self) {
        self.save_scene();
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
fn empty_scene() -> floptle_scene::SceneDoc {
    floptle_scene::SceneDoc {
        name: "untitled".into(),
        lighting: floptle_scene::LightDoc::default(),
        nodes: Vec::new(),
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
                parent: None,
            },
            NodeDoc {
                name: "sphere".into(),
                transform: TransformDoc { translation: [2.0, 0.0, 0.0], ..Default::default() },
                matter: MatterDoc::Primitive { shape: ShapeDoc::Sphere, color: [0.4, 0.7, 0.95] },
                scripts: Vec::new(),
                material: None,
                parent: None,
            },
            NodeDoc {
                name: "blob".into(),
                transform: TransformDoc { translation: [0.0, 1.6, 0.0], ..Default::default() },
                matter: MatterDoc::Blob { scale: 1.0 },
                scripts: Vec::new(),
                material: None,
                parent: None,
            },
        ],
    }
}
