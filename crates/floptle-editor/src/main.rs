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

// Animation: editor-side glue (registries, binding, extraction, advance) and the
// animation UI (Inspector panels, controller graph window, Animating tab). New
// subsystems live in their own modules — main.rs only wires them in.
mod anim;
mod anim_ui;
mod assets;
mod gizmo;
mod ide;
mod lua_support;
mod matter_catalog;
mod prefs;
mod shading;
mod theme;
mod viz;

use assets::*;
use gizmo::*;
use ide::*;
use lua_support::*;
use matter_catalog::*;
use prefs::*;
use shading::*;
use theme::*;
use viz::*;

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

/// A copied component's values, held on the editor clipboard so they can be pasted
/// onto another component of the same kind (Inspector ⎘ copy / 📋 paste).
#[derive(Clone)]
enum ComponentClip {
    Transform(Transform),
    /// The node's "type" component (geometry / camera / light / …).
    Matter(Matter),
    Material(Box<Material>),
    RigidBody(floptle_core::RigidBody),
    /// A single attached script (its kind, enabled flag, and tuned params).
    Script(floptle_core::ScriptInst),
}

impl ComponentClip {
    /// A short human label for the clipboard's current contents.
    fn label(&self) -> String {
        match self {
            ComponentClip::Transform(_) => "Transform".into(),
            ComponentClip::Matter(_) => "Type".into(),
            ComponentClip::Material(_) => "Material".into(),
            ComponentClip::RigidBody(_) => "Rigidbody".into(),
            ComponentClip::Script(s) => format!("Script: {}", s.kind),
        }
    }
}

/// A component section header row: bold title on the left, a right-aligned `⋮`
/// overflow menu (Copy ⎘ always; Paste 📋 when `can_paste`; Remove 🗑 when
/// `can_remove`). Returns `(copy, paste, remove)` — which item was clicked.
fn component_header(
    ui: &mut egui::Ui,
    title: &str,
    can_paste: bool,
    can_remove: bool,
) -> (bool, bool, bool) {
    let mut copy = false;
    let mut paste = false;
    let mut remove = false;
    ui.horizontal(|ui| {
        ui.strong(title);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button("…", |ui| {
                if ui.button("⎘  Copy values").clicked() {
                    copy = true;
                    ui.close();
                }
                if can_paste && ui.button("📋  Paste values").clicked() {
                    paste = true;
                    ui.close();
                }
                if can_remove {
                    ui.separator();
                    if ui.button("🗑  Remove component").clicked() {
                        remove = true;
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("component options");
        });
    });
    (copy, paste, remove)
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
    /// Toggle the static Collidable marker on any node (`true` = add, `false` = remove).
    set_collidable: Option<(Entity, bool)>,
    /// Change a node's "type" (its `Matter`) — geometry/camera/light/… are mutually
    /// exclusive, so picking one in "Add Component" replaces the current type.
    set_matter: Option<(Entity, Matter)>,
    /// Import (GPU-load) a model so a freshly-assigned/swapped mesh path renders.
    import_model: Option<String>,
    /// Show / hide a node's geometry (the `Visible` component).
    set_visible: Option<(Entity, bool)>,
    /// Copy a component's current values onto the editor clipboard.
    copy_component: Option<ComponentClip>,
    /// Paste the editor clipboard onto this entity (the held clip decides the kind).
    paste_component: Option<Entity>,
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
    /// Open the "new terrain" size/thickness/color/texture dialog.
    open_new_terrain: bool,
    /// Create a fresh flat terrain with this config (from the "New terrain" dialog).
    create_terrain: Option<NewTerrainCfg>,
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
    /// Persist the play-mode tint preference: (enabled, additive RGB offset).
    set_play_tint: Option<(bool, [u8; 3])>,
    /// Persist the grid settings (any Grid Settings control changed).
    save_grid: bool,
    /// Select + persist the engine chrome theme (index into `ENGINE_THEMES`).
    set_engine_theme: Option<usize>,
    /// Select + persist the code-editor theme (index into `CODE_THEMES`).
    set_code_theme: Option<usize>,
    /// Open the rename modal for this asset (absolute path).
    rename_asset: Option<String>,
    /// Commit a rename from the modal: (current path, new file/folder name).
    do_rename: Option<(String, String)>,
    /// Delete this asset file/folder (absolute path).
    delete_asset: Option<String>,
    /// Extract a model's embedded animation clips to assets/animations/ (a model path).
    extract_anims: Option<String>,
    /// Attach / change / remove a node's AnimationController: (entity, Some(key) | None).
    set_anim_controller: Option<(Entity, Option<String>)>,
    /// Open the Animation Controller graph window on this controller asset key.
    open_anim_graph: Option<String>,
    /// Open the graph window with the new-controller name prompt; the inner Entity
    /// (if any) gets the created controller attached.
    new_anim_controller: Option<Option<Entity>>,
    /// Focus (or open) the ✎ Animating dock tab.
    focus_animating: bool,
    /// Focus (or open) the ◉ Controller graph dock tab.
    focus_anim_graph: bool,
    /// CONFIRMED asset deletion (from the delete modal) — actually deletes.
    do_delete_asset: Option<String>,
    /// Folder the new controller should be created in (absolute; None = default).
    new_anim_controller_dir: Option<String>,
}

/// Editor reference-grid display + snapping settings.

/// What a hierarchy row carries while dragged — its entity, so dropping it on
/// another row re-parents it.
#[derive(Clone)]
struct NodePayload(Entity);

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
fn snap_dvec3(v: DVec3, step: f64) -> DVec3 {
    if step <= 1e-6 {
        return v;
    }
    DVec3::new((v.x / step).round() * step, (v.y / step).round() * step, (v.z / step).round() * step)
}

/// Nearest positive ray–sphere hit `t` (general — `rd` need not be unit), else None.
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
    /// The animation timeline (dopesheet): preview, scrub, record keys, events.
    Animation,
    /// The animation controller graph: states, transitions, fades, layers.
    AnimGraph,
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
            EditorTab::Animation => "✎ Animating",
            EditorTab::AnimGraph => "◉ Controller",
        }
    }
}

/// True when the Game tab is the front (active) tab of its dock leaf — i.e. the game
/// (active-camera) view should drive the full-window 3D render this frame. (When
/// false the editor free-fly camera renders, for the Scene tab.)
fn game_tab_active(dock: &egui_dock::DockState<EditorTab>) -> bool {
    tab_is_front(dock, EditorTab::Game)
}

/// True when `tab` is the front (active) tab of some dock leaf — i.e. it's actually
/// visible (egui_dock only runs the active tab's `ui` per leaf).
fn tab_is_front(dock: &egui_dock::DockState<EditorTab>, tab: EditorTab) -> bool {
    dock.main_surface()
        .iter()
        .any(|n| n.get_leaf().and_then(|l| l.tabs.get(l.active.0)) == Some(&tab))
}

/// True when BOTH the Scene and Game tabs are visible at once (split into separate
/// leaves), so they must render independent camera views rather than sharing one.
fn scene_and_game_split(dock: &egui_dock::DockState<EditorTab>) -> bool {
    tab_is_front(dock, EditorTab::Scene) && tab_is_front(dock, EditorTab::Game)
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
    // Console + the animation tabs sit beside Assets in the bottom dock.
    let [_, _] = surface.split_below(
        central,
        0.72,
        vec![
            EditorTab::Assets,
            EditorTab::Console,
            EditorTab::Animation,
            EditorTab::AnimGraph,
        ],
    );
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


/// Renders each dockable tab against borrowed slices of the editor's state, and
/// records UI intents on `cmd` to be applied after the frame.
struct EditorTabViewer<'a> {
    world: &'a mut World,
    selection: &'a mut Vec<Entity>,
    /// Double-clicking a tab toggles it into this slot (maximized full-window).
    fullscreen_tab: &'a mut Option<EditorTab>,
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
    /// The component clipboard (read-only here; copy/paste route through `cmd`).
    component_clip: &'a Option<ComponentClip>,
    /// Search text for the Inspector's "➕ Add Component" menu.
    add_component_filter: &'a mut String,
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
    terrain_voxels: Option<(usize, u64)>,
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
    /// Script `gizmo.*` debug lines (projected px + 0-1 color) — Scene view only.
    script_gizmo_lines: &'a [(Vec2, Vec2, [f32; 3])],
    terrain_wire: &'a [(Vec2, Vec2)],
    mesh_wire: &'a [(Vec2, Vec2)],
    show_gizmos: &'a mut bool,
    grabbed: Option<Handle>,
    tool: Tool,
    scene_rect: &'a mut Option<egui::Rect>,
    /// The Game tab's rect (captured each frame it draws), so the editor can size the
    /// Game viewport target to it on the next frame.
    game_rect: &'a mut Option<egui::Rect>,
    /// When true the Scene + Game tabs are split, so the Game tab paints its own offscreen
    /// render (`game_tex`) instead of being transparent over the surface.
    game_split: bool,
    game_tex: Option<egui::TextureId>,
    aspect: &'a mut AspectMode,
    zoom: &'a mut f32,
    scene_name: &'a str,
    ppp: f32,
    /// The selected code-editor theme index (into `CODE_THEMES`) for the Scripting tab.
    code_theme: usize,
    /// Animation registries + live runtimes (the animation UI reads/edits them).
    anim: &'a mut anim::AnimSystem,
    /// Animation UI state (graph window + Animating tab).
    anim_ui: &'a mut anim_ui::AnimUiState,
    /// Registered models — rig lookups for the animation UI.
    mesh_registry: &'a HashMap<String, MeshAsset>,
    /// A pointer button is down this frame (asset saves coalesce to release).
    pointer_down: bool,
    /// Play mode is running (the Animating tab disables preview/record).
    playing: bool,
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

    // Double-click a tab to maximize it full-window; double-click again to restore.
    fn on_tab_button(&mut self, tab: &mut EditorTab, response: &egui::Response) {
        if response.double_clicked() {
            *self.fullscreen_tab =
                if *self.fullscreen_tab == Some(*tab) { None } else { Some(*tab) };
        }
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
            EditorTab::Animation => self.animating_ui(ui),
            EditorTab::AnimGraph => self.anim_graph_tab_ui(ui),
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
        node_new_menu(ui, self.cmd, parent);
    }
}

/// The shared node-creation catalog (Hierarchy ✚ New, ✚ Add child, and the
/// menu-bar Add menu all list the same things).
fn node_new_menu(ui: &mut egui::Ui, cmd: &mut EditorCmd, parent: Option<Entity>) {
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
        if ui
            .button("🗀 Empty")
            .on_hover_text("a blank node — just a transform. Build it up with the Inspector's ➕ Add Component (also groups / parents children).")
            .clicked()
        {
            pick = Some(MatterDoc::Empty);
            ui.close();
        }
        ui.separator();
        if ui.button("Δ Terrain").on_hover_text("a sculptable SDF terrain node").clicked() {
            cmd.open_new_terrain = true;
            ui.close();
        }
        if ui.button("⌖ Camera").on_hover_text("a viewpoint you can give play-mode authority").clicked() {
            cmd.add_camera = Some(parent);
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
        if ui.button("◐ Skybox").on_hover_text("the scene environment background (solid color or equirect texture)").clicked() {
            pick = Some(MatterDoc::from(&Matter::default_skybox()));
            ui.close();
        }
        if let Some(m) = pick {
            match parent {
                Some(p) => cmd.add_parented = Some((m, p)),
                None => cmd.add = Some(m),
            }
        }
}

impl<'a> EditorTabViewer<'a> {
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
        } else if matches!(matter, Some(Matter::Skybox { .. })) {
            "◐"
        } else if matches!(matter, Some(Matter::PostProcess { .. })) {
            "✨"
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
                self.model_asset_anim_ui(ui, &path);
            } else if anim_ui::is_anim_clip(&path) {
                self.clip_asset_ui(ui, &path);
            } else if anim_ui::is_anim_ctl(&path) {
                self.ctl_asset_ui(ui, &path);
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

                    ui.separator();
                    cmd.inspector_changed |= ui
                        .checkbox(&mut l.shadows, "sun shadows")
                        .on_hover_text(
                            "march the SDF field toward the sun — analytically soft shadows, \
                             no shadow maps. Terrain and blobs cast on everything; meshes cast \
                             via their collider shapes and receive like everything else.",
                        )
                        .changed();
                    ui.add_enabled_ui(l.shadows, |ui| {
                        cmd.inspector_changed |= ui
                            .add(egui::Slider::new(&mut l.shadow_softness, 0.0..=1.0).text("softness"))
                            .on_hover_text("0 = razor-hard edge (retro), 1 = dreamy-soft penumbra")
                            .changed();
                        cmd.inspector_changed |= ui
                            .add(egui::Slider::new(&mut l.shadow_strength, 0.0..=1.0).text("strength"))
                            .on_hover_text("how dark full shadow gets — ambient light still fills, so 1.0 isn't pitch black")
                            .changed();
                        ui.horizontal(|ui| {
                            ui.label("tint");
                            cmd.inspector_changed |= ui
                                .color_edit_button_rgb(&mut l.shadow_tint)
                                .on_hover_text("shadows darken toward this color — black is neutral; try purple dusk or sepia")
                                .changed();
                            ui.label("quantize");
                            let qlabel = match l.shadow_quantize {
                                0 => "smooth".to_string(),
                                n => format!("{n} bands"),
                            };
                            egui::ComboBox::from_id_salt("shadow_quantize")
                                .selected_text(qlabel)
                                .show_ui(ui, |ui| {
                                    cmd.inspector_changed |=
                                        ui.selectable_value(&mut l.shadow_quantize, 0, "smooth").clicked();
                                    for nb in 2..=4u32 {
                                        cmd.inspector_changed |= ui
                                            .selectable_value(&mut l.shadow_quantize, nb, format!("{nb} bands"))
                                            .clicked();
                                    }
                                });
                        });
                        ui.add_enabled_ui(l.shadow_quantize >= 2, |ui| {
                            cmd.inspector_changed |= ui
                                .checkbox(&mut l.shadow_dither, "dither the penumbra")
                                .on_hover_text("Bayer-pattern the quantized penumbra — the PS1 shadow edge; pairs with retro mode")
                                .changed();
                        });
                        cmd.inspector_changed |= ui
                            .add(
                                egui::Slider::new(&mut l.shadow_distance, 10.0..=1000.0)
                                    .logarithmic(true)
                                    .text("distance"),
                            )
                            .on_hover_text("max distance a shadow ray marches (a perf fence — farther geometry stops casting)")
                            .changed();
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
                // The component clipboard (read-only); copy/paste route through `cmd`.
                let clip = self.component_clip.as_ref();

                // ===== Type — the node's primary kind (mutually exclusive). =====
                {
                    let (icon, label, is_terrain) = match world.get::<Matter>(e) {
                        Some(m) => (matter_icon(m), matter_kind_label(m), matches!(m, Matter::Terrain { .. })),
                        None => ("◈", "Type", false),
                    };
                    let (copy, paste, _) = component_header(
                        ui,
                        &format!("{icon} {label}"),
                        !is_terrain && matches!(clip, Some(ComponentClip::Matter(_))),
                        false,
                    );
                    if copy && !is_terrain {
                        if let Some(m) = world.get::<Matter>(e) {
                            cmd.copy_component = Some(ComponentClip::Matter(m.clone()));
                        }
                    }
                    if paste {
                        cmd.paste_component = Some(e);
                    }
                }
                ui.indent("type_props", |ui| {
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
                                // Swap the model freely — pick any model in the project.
                                let mut models = Vec::new();
                                collect_model_paths(self.asset_tree, &mut models);
                                let file_label = |p: &str| {
                                    Path::new(p)
                                        .file_name()
                                        .map(|s| s.to_string_lossy().to_string())
                                        .unwrap_or_else(|| p.to_string())
                                };
                                ui.horizontal(|ui| {
                                    ui.label("model");
                                    egui::ComboBox::from_id_salt("mesh-model")
                                        .selected_text(file_label(asset_path))
                                        .show_ui(ui, |ui| {
                                            for p in &models {
                                                if ui.selectable_label(asset_path == p, file_label(p)).clicked()
                                                    && asset_path != p
                                                {
                                                    *asset_path = p.clone();
                                                    cmd.import_model = Some(p.clone());
                                                    cmd.inspector_changed = true;
                                                }
                                            }
                                        });
                                });
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
                            Matter::Skybox { color, size, texture, tint } => {
                                ui.label("skybox");
                                ui.small("the scene environment, drawn behind everything. Rotate this node (or a script) to spin the sky.");
                                let mut textured = texture.is_some();
                                ui.horizontal(|ui| {
                                    if ui.selectable_label(!textured, "■ Solid color").clicked() && textured {
                                        *texture = None;
                                        cmd.inspector_changed = true;
                                    }
                                    if ui.selectable_label(textured, "▦ Texture").clicked() && !textured {
                                        let mut tl = Vec::new();
                                        collect_texture_paths(self.asset_tree, &mut tl);
                                        *texture = Some(tl.first().cloned().unwrap_or_default());
                                        cmd.inspector_changed = true;
                                    }
                                });
                                textured = texture.is_some();
                                if !textured {
                                    ui.horizontal(|ui| {
                                        ui.label("color");
                                        cmd.inspector_changed |= ui.color_edit_button_rgb(color).changed();
                                    });
                                } else {
                                    let mut tl = Vec::new();
                                    collect_texture_paths(self.asset_tree, &mut tl);
                                    let cur = texture.clone().unwrap_or_default();
                                    let label = |p: &str| {
                                        Path::new(p).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string())
                                    };
                                    ui.horizontal(|ui| {
                                        ui.label("texture");
                                        egui::ComboBox::from_id_salt("sky-tex")
                                            .selected_text(if cur.is_empty() { "(pick a texture)".to_string() } else { label(&cur) })
                                            .show_ui(ui, |ui| {
                                                for p in &tl {
                                                    if ui.selectable_label(&cur == p, label(p)).clicked() {
                                                        *texture = Some(p.clone());
                                                        cmd.inspector_changed = true;
                                                    }
                                                }
                                            });
                                    });
                                    ui.small("an equirectangular (2:1) image, wrapped seamlessly around the sky.");
                                    ui.horizontal(|ui| {
                                        ui.label("tint");
                                        cmd.inspector_changed |= ui.color_edit_button_rgb(tint).changed();
                                    });
                                }
                                cmd.inspector_changed |= ui
                                    .add(egui::Slider::new(size, 10.0..=5000.0).logarithmic(true).text("size (radius)"))
                                    .changed();
                            }
                            Matter::PostProcess {
                                enabled,
                                bloom,
                                bloom_threshold,
                                bloom_intensity,
                                vignette,
                                vignette_strength,
                                vignette_radius,
                                ao,
                                ao_strength,
                                ao_radius,
                            } => {
                                use floptle_core::AoMode;
                                ui.label("post processing");
                                ui.small("this scene's full-screen effect chain — every scene has its own (the settings travel with the scene, not the project)");
                                cmd.inspector_changed |= ui
                                    .checkbox(enabled, "enabled")
                                    .on_hover_text("master switch for the whole chain")
                                    .changed();
                                ui.add_enabled_ui(*enabled, |ui| {
                                    ui.separator();
                                    ui.label("Ambient occlusion");
                                    ui.horizontal(|ui| {
                                        let mut m = *ao;
                                        if ui.selectable_label(m == AoMode::Off, "Off").clicked() {
                                            m = AoMode::Off;
                                        }
                                        if ui
                                            .selectable_label(m == AoMode::ScreenSpace, "Screen space")
                                            .on_hover_text("SSAO — cheap, from the depth buffer; shades everything on screen (meshes and terrain)")
                                            .clicked()
                                        {
                                            m = AoMode::ScreenSpace;
                                        }
                                        if ui
                                            .selectable_label(m == AoMode::Sdf, "SDF (true)")
                                            .on_hover_text("samples the real distance field — no screen-space artifacts; everything receives it, but only SDF matter (terrain/blobs) occludes — meshes are not in the field")
                                            .clicked()
                                        {
                                            m = AoMode::Sdf;
                                        }
                                        if m != *ao {
                                            *ao = m;
                                            cmd.inspector_changed = true;
                                        }
                                    });
                                    if *ao != AoMode::Off {
                                        cmd.inspector_changed |= ui
                                            .add(egui::Slider::new(ao_strength, 0.0..=1.0).text("strength"))
                                            .changed();
                                        cmd.inspector_changed |= ui
                                            .add(egui::Slider::new(ao_radius, 0.05..=5.0).logarithmic(true).text("radius (m)"))
                                            .changed();
                                    }
                                    ui.separator();
                                    cmd.inspector_changed |= ui.checkbox(bloom, "Bloom").changed();
                                    if *bloom {
                                        cmd.inspector_changed |= ui
                                            .add(egui::Slider::new(bloom_threshold, 0.0..=2.0).text("threshold"))
                                            .changed();
                                        cmd.inspector_changed |= ui
                                            .add(egui::Slider::new(bloom_intensity, 0.0..=2.0).text("intensity"))
                                            .changed();
                                    }
                                    cmd.inspector_changed |= ui.checkbox(vignette, "Vignette").changed();
                                    if *vignette {
                                        cmd.inspector_changed |= ui
                                            .add(egui::Slider::new(vignette_strength, 0.0..=1.0).text("strength"))
                                            .changed();
                                        cmd.inspector_changed |= ui
                                            .add(egui::Slider::new(vignette_radius, 0.3..=1.0).text("radius"))
                                            .changed();
                                    }
                                });
                            }
                        }
                    }
                });
                // Visibility (geometry nodes) — hide the node's visual without deleting it.
                if matches!(
                    world.get::<Matter>(e),
                    Some(Matter::Mesh { .. } | Matter::Primitive { .. } | Matter::Blob { .. })
                ) {
                    ui.indent("visible_toggle", |ui| {
                        let mut vis =
                            world.get::<floptle_core::Visible>(e).map(|v| v.0).unwrap_or(true);
                        if ui
                            .checkbox(&mut vis, "👁 visible")
                            .on_hover_text("uncheck to hide this node's geometry (scripts: node.visible = true/false)")
                            .changed()
                        {
                            cmd.set_visible = Some((e, vis));
                            cmd.inspector_changed = true;
                        }
                    });
                }

                // ===== Transform (always present) =====
                ui.separator();
                {
                    let (copy, paste, _) = component_header(
                        ui,
                        "✛ Transform",
                        matches!(clip, Some(ComponentClip::Transform(_))),
                        false,
                    );
                    if copy {
                        if let Some(t) = world.get::<Transform>(e) {
                            cmd.copy_component = Some(ComponentClip::Transform(*t));
                        }
                    }
                    if paste {
                        cmd.paste_component = Some(e);
                    }
                }
                ui.indent("xform_props", |ui| {
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
                });

                // ===== Material (only when the node has one) =====
                if world.get::<Material>(e).is_some() {
                    ui.separator();
                    let (copy, paste, remove) = component_header(
                        ui,
                        "◑ Material",
                        matches!(clip, Some(ComponentClip::Material(_))),
                        true,
                    );
                    if copy {
                        if let Some(mat) = world.get::<Material>(e) {
                            cmd.copy_component = Some(ComponentClip::Material(Box::new(mat.clone())));
                        }
                    }
                    if paste {
                        cmd.paste_component = Some(e);
                    }
                    if remove {
                        cmd.remove_material = Some(e);
                    }
                    let mut tex_list = Vec::new();
                    collect_texture_paths(self.asset_tree, &mut tex_list);
                    ui.indent("material_props", |ui| {
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
                        }
                    });
                }

                // ===== Rigidbody (only when the node has one) =====
                if world.get::<floptle_core::RigidBody>(e).is_some() {
                    ui.separator();
                    let (copy, paste, remove) = component_header(
                        ui,
                        "◆ Rigidbody",
                        matches!(clip, Some(ComponentClip::RigidBody(_))),
                        true,
                    );
                    if copy {
                        if let Some(rb) = world.get::<floptle_core::RigidBody>(e) {
                            cmd.copy_component = Some(ComponentClip::RigidBody(*rb));
                        }
                    }
                    if paste {
                        cmd.paste_component = Some(e);
                    }
                    if remove {
                        cmd.remove_rigidbody = Some(e);
                    }
                    ui.indent("rb_props", |ui| {
                        if let Some(rb) = world.get_mut::<floptle_core::RigidBody>(e) {
                            use floptle_core::BodyKind;
                            ui.horizontal(|ui| {
                                ui.label("shape");
                                egui::ComboBox::from_id_salt("rb-shape")
                                    .selected_text(match rb.kind {
                                        BodyKind::Sphere => "Sphere",
                                        BodyKind::Capsule => "Capsule",
                                        BodyKind::Box => "Box",
                                    })
                                    .show_ui(ui, |ui| {
                                        cmd.inspector_changed |=
                                            ui.selectable_value(&mut rb.kind, BodyKind::Sphere, "Sphere").changed();
                                        cmd.inspector_changed |=
                                            ui.selectable_value(&mut rb.kind, BodyKind::Capsule, "Capsule").changed();
                                        cmd.inspector_changed |=
                                            ui.selectable_value(&mut rb.kind, BodyKind::Box, "Box").changed();
                                    });
                            });
                            if rb.kind == BodyKind::Box {
                                ui.label("half-extents");
                                ui.horizontal(|ui| {
                                    for (i, ax) in ["x", "y", "z"].iter().enumerate() {
                                        cmd.inspector_changed |= ui
                                            .add(egui::DragValue::new(&mut rb.half_extents[i]).speed(0.02).range(0.02..=50.0).prefix(format!("{ax} ")))
                                            .changed();
                                    }
                                });
                            } else {
                                cmd.inspector_changed |=
                                    ui.add(egui::Slider::new(&mut rb.radius, 0.05..=10.0).text("radius")).changed();
                                if rb.kind == BodyKind::Capsule {
                                    cmd.inspector_changed |=
                                        ui.add(egui::Slider::new(&mut rb.height, 0.2..=20.0).text("height")).changed();
                                }
                            }
                            cmd.inspector_changed |=
                                ui.add(egui::Slider::new(&mut rb.restitution, 0.0..=1.0).text("bounce")).changed();
                            cmd.inspector_changed |=
                                ui.add(egui::Slider::new(&mut rb.friction, 0.0..=1.0).text("friction")).changed();
                            cmd.inspector_changed |= ui
                                .checkbox(&mut rb.gravity, "affected by gravity")
                                .on_hover_text("off = floats (still collides; a script can still move it)")
                                .changed();
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
                        }
                    });
                    // The body shape doubles as the node's sun-shadow proxy (see the
                    // Lighting node) — casting is the default; the component only
                    // exists to record an opt-out.
                    let mut casts =
                        world.get::<floptle_core::CastShadow>(e).map(|c| c.0).unwrap_or(true);
                    if ui
                        .checkbox(&mut casts, "casts shadows")
                        .on_hover_text("this body shape stands in for the mesh in the sun-shadow march — untick to stop this node casting")
                        .changed()
                    {
                        if casts {
                            world.remove::<floptle_core::CastShadow>(e);
                        } else {
                            world.insert(e, floptle_core::CastShadow(false));
                        }
                        cmd.inspector_changed = true;
                    }
                }

                // ===== Collider (static collision; only when the node has one) =====
                // Auto-shaped from the node's geometry (Cube → box, Sphere → sphere,
                // Capsule → capsule, Mesh → its triangles). A legacy MeshCollider counts.
                {
                    let has_collidable = world.get::<floptle_core::Collidable>(e).is_some()
                        || world.get::<floptle_core::MeshCollider>(e).is_some();
                    if has_collidable {
                        let kind = match world.get::<Matter>(e) {
                            Some(Matter::Mesh { .. }) => "triangle mesh",
                            Some(Matter::Primitive { shape, .. }) => match shape {
                                floptle_core::Shape::Cube => "box",
                                floptle_core::Shape::Sphere => "sphere",
                                floptle_core::Shape::Capsule => "capsule",
                            },
                            _ => "mesh",
                        };
                        ui.separator();
                        let (_, _, remove) = component_header(ui, "▦ Collider", false, true);
                        ui.small(format!(
                            "static {kind} collider — built from this node's geometry on Play. Walk on it / bump into it; no rigidbody needed. Scale the node to resize it."
                        ));
                        if world.get::<floptle_core::RigidBody>(e).is_some() {
                            ui.small("⚠ This node also has a Rigidbody, so on Play it's a dynamic body (it falls / gets moved) and this static Collider is ignored. To make it a solid obstacle the player bumps into, remove the Rigidbody so it becomes static world geometry (the solver has no body-vs-body pass, so two dynamic bodies pass through each other).");
                        } else {
                            // The collider doubles as the node's sun-shadow caster:
                            // primitives stand in as analytic proxy shapes, and a
                            // Collidable MESH is baked into a shadow-only occluder
                            // volume (its true silhouette — interiors go dark).
                            let mut casts = world
                                .get::<floptle_core::CastShadow>(e)
                                .map(|c| c.0)
                                .unwrap_or(true);
                            if ui
                                .checkbox(&mut casts, "casts shadows")
                                .on_hover_text("this collider stands in for the node in the sun-shadow march (primitives as proxy shapes, meshes as a baked occluder volume) — untick to stop this node casting")
                                .changed()
                            {
                                if casts {
                                    world.remove::<floptle_core::CastShadow>(e);
                                } else {
                                    world.insert(e, floptle_core::CastShadow(false));
                                }
                                cmd.inspector_changed = true;
                            }
                        }
                        if remove {
                            cmd.set_collidable = Some((e, false));
                            cmd.inspector_changed = true;
                        }
                    }
                }

                // ===== Scripts =====
                ui.separator();
                // Always-available drop target: drag a script here to attach it.
                {
                    let (_, dropped) = ui.dnd_drop_zone::<AssetPayload, ()>(
                        egui::Frame::group(ui.style()),
                        |ui| {
                            ui.set_min_height(18.0);
                            ui.small("⚙  drop a script here to attach (or use ➕ Add Component)");
                        },
                    );
                    if let Some(p) = dropped {
                        if is_script(&p.path) {
                            cmd.drop_script_on = Some((p.path.clone(), e));
                        }
                    }
                }
                if world.get::<Scripts>(e).map(|s| !s.0.is_empty()).unwrap_or(false) {
                    ui.horizontal(|ui| {
                        ui.strong("⚙ Scripts");
                        if matches!(clip, Some(ComponentClip::Script(_))) {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.menu_button("…", |ui| {
                                    if ui.button("📋  Paste script").clicked() {
                                        cmd.paste_component = Some(e);
                                        ui.close();
                                    }
                                })
                                .response
                                .on_hover_text("adds the copied script, or updates a matching one");
                            });
                        }
                    });
                    let mut remove: Option<usize> = None;
                    let mut copy_idx: Option<usize> = None;
                    ui.indent("script_list", |ui| {
                        if let Some(scr) = world.get_mut::<Scripts>(e) {
                            for (i, inst) in scr.0.iter_mut().enumerate() {
                                ui.horizontal(|ui| {
                                    cmd.inspector_changed |= ui.checkbox(&mut inst.enabled, "").changed();
                                    ui.strong(&inst.kind);
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        ui.menu_button("…", |ui| {
                                            if ui.button("⎘  Copy values").clicked() {
                                                copy_idx = Some(i);
                                                ui.close();
                                            }
                                            if ui.button("🖊  Edit script").clicked() {
                                                let p = self
                                                    .project_root
                                                    .join("scripts")
                                                    .join(format!("{}.lua", inst.kind));
                                                cmd.open_script_pref = Some(p.to_string_lossy().to_string());
                                                ui.close();
                                            }
                                            ui.separator();
                                            if ui.button("🗑  Remove").clicked() {
                                                remove = Some(i);
                                                ui.close();
                                            }
                                        });
                                    });
                                });
                                for (k, v) in inst.params.iter_mut() {
                                    cmd.inspector_changed |= ui
                                        .add(egui::DragValue::new(v).speed(0.05).prefix(format!("{k}  ")))
                                        .changed();
                                }
                                ui.add_space(4.0);
                            }
                            if let Some(i) = copy_idx {
                                cmd.copy_component = Some(ComponentClip::Script(scr.0[i].clone()));
                            }
                            if let Some(i) = remove {
                                scr.0.remove(i);
                                cmd.inspector_changed = true;
                            }
                        }
                    });
                }

                // ===== Animation Controller (when attached) =====
                anim_ui::anim_component_ui(ui, e, world, &*self.anim, self.anim_ui, cmd);

                // ===== ➕ Add Component (searchable, icon'd) =====
                ui.separator();
                ui.add_space(2.0);
                let add_btn = ui.button("➕  Add Component");
                let add_popup_id = egui::Popup::default_response_id(&add_btn);
                // True only on the frame the menu transitions closed → open, so we
                // focus the search box exactly once (start typing immediately).
                let add_opening =
                    add_btn.clicked() && !egui::Popup::is_id_open(ui.ctx(), add_popup_id);
                // CloseOnClickOutside (not the menu default CloseOnClick) so clicking
                // the search field doesn't dismiss the menu.
                egui::Popup::menu(&add_btn)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .width(236.0)
                    .show(|ui| {
                    let filter = &mut *self.add_component_filter;
                    let search = ui.add(
                        egui::TextEdit::singleline(filter)
                            .hint_text("🔍 search components…")
                            .desired_width(212.0),
                    );
                    if add_opening {
                        search.request_focus();
                    }
                    let f = filter.trim().to_lowercase();
                    let hit = |s: &str| f.is_empty() || s.to_lowercase().contains(&f);

                    // What the node already has decides what's offered.
                    let cur = world.get::<Matter>(e);
                    let is_terrain = matches!(cur, Some(Matter::Terrain { .. }));
                    let has_mat = world.get::<Material>(e).is_some();
                    let has_rb = world.get::<floptle_core::RigidBody>(e).is_some();
                    let has_collidable = world.get::<floptle_core::Collidable>(e).is_some()
                        || world.get::<floptle_core::MeshCollider>(e).is_some();
                    let collider_kind = match cur {
                        Some(Matter::Mesh { .. }) => Some("triangle mesh"),
                        Some(Matter::Primitive { shape, .. }) => Some(match shape {
                            floptle_core::Shape::Cube => "box",
                            floptle_core::Shape::Sphere => "sphere",
                            floptle_core::Shape::Capsule => "capsule",
                        }),
                        _ => None,
                    };
                    let cur_kind = cur.map(matter_kind_label);

                    // One catalog of (category, label, action) — built from current state.
                    enum Add {
                        Rb,
                        Coll,
                        Mat,
                        Preset(String),
                        Script(String),
                        Type(Matter),
                        AnimCtl(String),
                        AnimNew,
                    }
                    let mut items: Vec<(&str, String, Add)> = Vec::new();
                    if !has_rb {
                        items.push(("Physics", "◆  Rigidbody".into(), Add::Rb));
                    }
                    if !has_collidable {
                        if let Some(k) = collider_kind {
                            items.push(("Physics", format!("▦  Collider ({k})"), Add::Coll));
                        }
                    }
                    if !has_mat {
                        items.push(("Rendering", "◑  Material".into(), Add::Mat));
                    }
                    // Animation Controller: attach an existing controller asset, or
                    // create a fresh one (opens the graph editor).
                    if world.get::<floptle_core::AnimController>(e).is_none() {
                        items.push((
                            "Animation",
                            "▶  Animation Controller (new)".into(),
                            Add::AnimNew,
                        ));
                        for (k, _) in self.anim.controllers.iter() {
                            items.push(("Animation", format!("▶  {k}"), Add::AnimCtl(k.clone())));
                        }
                    }
                    for (name, _) in self.materials {
                        items.push(("Rendering", format!("◑  {name}  (preset)"), Add::Preset(name.clone())));
                    }
                    // Scripts not already attached.
                    let attached: std::collections::HashSet<String> = world
                        .get::<Scripts>(e)
                        .map(|s| s.0.iter().map(|i| i.kind.clone()).collect())
                        .unwrap_or_default();
                    let mut snames = Vec::new();
                    collect_script_names(self.asset_tree, &mut snames);
                    for n in snames {
                        if !attached.contains(&n) {
                            items.push(("Scripts", format!("⚙  {n}"), Add::Script(n)));
                        }
                    }
                    // Type switch (mutually exclusive). Terrain is special — leave it be.
                    if !is_terrain {
                        for (lbl, mt) in type_catalog() {
                            if cur_kind != Some(matter_kind_label(&mt)) {
                                items.push(("Type — replaces current", lbl.to_string(), Add::Type(mt)));
                            }
                        }
                        // Each importable model is a Mesh type you can become.
                        let mut models = Vec::new();
                        collect_model_paths(self.asset_tree, &mut models);
                        for p in models {
                            let name = Path::new(&p)
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| p.clone());
                            let is_cur = matches!(cur, Some(Matter::Mesh { asset_path }) if *asset_path == p);
                            if !is_cur {
                                items.push((
                                    "Mesh — replaces type",
                                    format!("✦  {name}"),
                                    Add::Type(Matter::Mesh { asset_path: p }),
                                ));
                            }
                        }
                    }

                    let mut picked = false;
                    egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
                        // Paste the clipboard onto a component the node doesn't have yet.
                        if let Some(c) = clip {
                            let can = match c {
                                ComponentClip::Material(_) => !has_mat,
                                ComponentClip::RigidBody(_) => !has_rb,
                                ComponentClip::Script(_) => true,
                                ComponentClip::Transform(_) | ComponentClip::Matter(_) => false,
                            };
                            if can {
                                let lbl = format!("📋  Paste {}", c.label());
                                if hit(&lbl) && ui.button(lbl).clicked() {
                                    cmd.paste_component = Some(e);
                                    picked = true;
                                    ui.close();
                                }
                            }
                        }
                        let mut shown = false;
                        for cat in [
                            "Physics",
                            "Rendering",
                            "Animation",
                            "Scripts",
                            "Type — replaces current",
                            "Mesh — replaces type",
                        ] {
                            if !items.iter().any(|(c, l, _)| *c == cat && hit(l)) {
                                continue;
                            }
                            ui.add_space(4.0);
                            ui.weak(cat);
                            for (c, l, a) in &items {
                                if *c != cat || !hit(l) {
                                    continue;
                                }
                                shown = true;
                                if ui.button(l).clicked() {
                                    match a {
                                        Add::Rb => cmd.add_rigidbody = Some(e),
                                        Add::Coll => cmd.set_collidable = Some((e, true)),
                                        Add::Mat => cmd.add_material = Some(e),
                                        Add::Preset(n) => cmd.apply_preset = Some((e, n.clone())),
                                        Add::Script(n) => cmd.attach_named = Some((n.clone(), e)),
                                        Add::Type(mt) => cmd.set_matter = Some((e, mt.clone())),
                                        Add::AnimCtl(k) => {
                                            cmd.set_anim_controller = Some((e, Some(k.clone())))
                                        }
                                        Add::AnimNew => cmd.new_anim_controller = Some(Some(e)),
                                    }
                                    picked = true;
                                    ui.close();
                                }
                            }
                        }
                        if !shown && !f.is_empty() {
                            ui.weak("no matching components");
                        }
                    });
                    // Reset the search for next open once something's been added.
                    if picked {
                        filter.clear();
                    }
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
        if let Some((n, total)) = terrain_voxels {
            ui.small(format!("{n} volume(s) · {total} voxels (native per-volume)"));
        }
        // New terrains can be added any time — each is a node you place + blend.
        if ui.button("✚ New terrain").on_hover_text("adds another terrain node at the cursor; overlapping terrains blend").clicked() {
            cmd.open_new_terrain = true;
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
        let draggable = is_model(path) || is_script(path) || anim_ui::is_anim_clip(path);
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
            } else if anim_ui::is_anim_ctl(path) {
                self.cmd.open_anim_graph = Some(anim::asset_key(
                    Path::new(path),
                    self.project_root,
                    floptle_scene::ANIM_CTL_EXT,
                ));
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
            if ui.button("⎘ Copy asset path").on_hover_text("the path after Assets/ — paste into assets.getFile(\"…\")").clicked() {
                ui.ctx().copy_text(asset_rel_path(path, self.project_root));
                ui.close();
            }
            if ui.button("⎘ Copy full path").clicked() {
                ui.ctx().copy_text(path.to_string());
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
        if ui.button("◉ New Animation Controller").clicked() {
            self.cmd.new_anim_controller = Some(None);
            self.cmd.new_anim_controller_dir = Some(dir.to_string_lossy().to_string());
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
                    let draggable = model || script || anim_ui::is_anim_clip(path);
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
                        } else if anim_ui::is_anim_ctl(path) {
                            self.cmd.open_anim_graph = Some(anim::asset_key(
                                Path::new(path),
                                self.project_root,
                                floptle_scene::ANIM_CTL_EXT,
                            ));
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
                        if ui.button("⎘ Copy asset path").on_hover_text("the path after Assets/ — paste into assets.getFile(\"…\")").clicked() {
                            ui.ctx().copy_text(asset_rel_path(path, self.project_root));
                            ui.close();
                        }
                        if ui.button("⎘ Copy full path").clicked() {
                            ui.ctx().copy_text(path.clone());
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
        // This tab's rect IS the 3D viewport. The Scene tab caches it for picking / gizmo
        // gating; the Game tab caches its own rect (so the editor can size the offscreen
        // Game target to it) and, when split, paints that offscreen render over itself.
        let rect = ui.max_rect();
        if game {
            *self.game_rect = Some(rect);
            if self.game_split {
                if let Some(tex) = self.game_tex {
                    egui::Image::new((tex, rect.size())).paint_at(ui, rect);
                }
            }
        } else {
            *self.scene_rect = Some(rect);
        }

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

        // Gizmos master toggle — top-right of the viewport (editor view only). Off hides
        // every overlay (colliders, camera/light/gravity gizmos, contacts), including the
        // selected node's.
        if !game {
            egui::Area::new(egui::Id::new("gizmo_toggle"))
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 8.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.toggle_value(self.show_gizmos, "◈ Gizmos")
                            .on_hover_text("show selection/collider/camera/light gizmos in the viewport");
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
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("gizmo")))
                .with_clip_rect(rect);
            paint_gizmo(&painter, g, self.tool, self.grabbed, self.ppp);
        }

        // Terrain brush telegraph: a ring at the surface + a normal line, so you can
        // see exactly where (and on what facing) a stroke will land.
        if let Some(viz) = self.terrain_viz.filter(|_| !game) {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("terrain_brush")))
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
        if !game && !self.camera_gizmos.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("camera_gizmos")))
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
        if !game && !self.light_gizmos.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("light_gizmos")))
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
        if !game && (!self.body_gizmos.is_empty() || !self.contact_gizmos.is_empty()) {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("physics_gizmos")))
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
        if !game && !self.terrain_wire.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("terrain_wire")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            let col = egui::Color32::from_rgba_unmultiplied(235, 225, 120, 130);
            for (a, b) in self.terrain_wire {
                painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(0.8, col));
            }
        }

        // Script debug gizmos (`gizmo.*`): Scene view only — like every other
        // gizmo, the Game view stays clean (what the player would actually see).
        if !game && !self.script_gizmo_lines.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("script_gizmos")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            for (a, b, c) in self.script_gizmo_lines {
                let col = egui::Color32::from_rgb(
                    (c[0].clamp(0.0, 1.0) * 255.0) as u8,
                    (c[1].clamp(0.0, 1.0) * 255.0) as u8,
                    (c[2].clamp(0.0, 1.0) * 255.0) as u8,
                );
                painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(2.0, col));
            }
        }

        // Mesh collider wireframes (imported maps flagged walkable) — a cyan triangle net.
        if !game && !self.mesh_wire.is_empty() {
            let painter = ui
                .ctx()
                .layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("mesh_wire")))
                .with_clip_rect(rect);
            let ppp = self.ppp;
            let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
            let col = egui::Color32::from_rgba_unmultiplied(120, 220, 220, 120);
            for (a, b) in self.mesh_wire {
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
                                // The file:line is a link — a single click jumps to that
                                // exact line in the editor.
                                if ui
                                    .link(egui::RichText::new(format!("{name}:{line}")).monospace())
                                    .on_hover_text("click to open this line in the editor")
                                    .clicked()
                                {
                                    jump = Some(((*name).clone(), *line));
                                }
                            }
                            // Selectable so you can drag-select + copy a line; a
                            // double-click on the row still jumps to its source too.
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
                    resp.on_hover_text("click the file:line to open the source (or double-click the row)");
                }
            });

        if do_clear {
            c.entries.clear();
        }
        if let Some(j) = jump {
            self.cmd.open_log_source = Some(j);
        }
    }

}

fn main() {
    env_logger::init();
    println!("{} editor v{}", floptle_core::ENGINE_NAME, floptle_core::ENGINE_VERSION);
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut editor = Editor::default();
    editor.show_gizmos = true; // gizmos/overlays on by default (toggle in the viewport)
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

/// Config gathered by the "New terrain" dialog before the node is actually created —
/// footprint size, thickness, and an initial color/texture painted across the whole
/// slab, so a terrain arrives already the size/look you want instead of always
/// starting as the same small default patch you have to sculpt/fill out by hand.
#[derive(Clone)]
struct NewTerrainCfg {
    /// Full width/depth of the flat slab (X and Z), world units.
    size_xz: f32,
    /// Full height of the slab (Y), world units.
    thickness: f32,
    color: [f32; 3],
    /// An asset texture path painted across the whole terrain (empty = none / flat
    /// color only). Resolved to a palette slot at creation time.
    texture: String,
}

impl Default for NewTerrainCfg {
    fn default() -> Self {
        Self { size_xz: 32.0, thickness: 12.0, color: [0.35, 0.6, 0.28], texture: String::new() }
    }
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

/// Grab (hide + pin) or release the OS cursor. Prefers a hard lock — the cursor
/// physically can't move (Wayland/macOS/Windows) — falling back to confining it
/// to the window (X11, which has no lock). Returns true when only the CONFINE
/// took, so the caller re-centers the cursor every frame to emulate the pin.
fn grab_cursor(window: &Window, want: bool) -> bool {
    if !want {
        let _ = window.set_cursor_grab(CursorGrabMode::None);
        window.set_cursor_visible(true);
        return false;
    }
    window.set_cursor_visible(false);
    if window.set_cursor_grab(CursorGrabMode::Locked).is_ok() {
        return false;
    }
    let _ = window.set_cursor_grab(CursorGrabMode::Confined);
    true
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
    /// node's LOCAL space). Empty until "New Terrain". Every volume is uploaded to
    /// the renderer's 3D atlas at native resolution and fused on the GPU.
    terrains: HashMap<Entity, floptle_field::Terrain>,
    /// The terrain the sculpt brush currently targets (the one under the cursor),
    /// chosen each frame.
    active_terrain: Option<Entity>,
    /// Atlas slot order: the terrain entities as uploaded to the renderer (sorted by
    /// terrain id). Each volume renders at its NATIVE resolution from its own slot;
    /// placement comes from the node's f64 translation, read fresh every frame — so
    /// moving a terrain needs zero GPU work and there is no combined field at all.
    terrain_slots: Vec<Entity>,
    /// The GPU volume set needs re-uploading (a terrain was added/edited/deleted/resized).
    terrain_gpu_dirty: bool,
    /// Shadow-occluder bakes for static collider MESHES (Collidable / MeshCollider,
    /// no RigidBody): each level mesh is baked once into an unsigned distance
    /// volume (`bake_occluder`) and uploaded into the SAME 3D atlas as the
    /// terrains, flagged shadow-only (`vol_center.w = 2`) — so a map casts sun
    /// shadows with its true silhouette (dark interiors) while never being drawn
    /// or collided as SDF matter. Keyed per node; the bake is shared through
    /// `occluder_cache` when several nodes place the same asset the same way.
    mesh_occluders: HashMap<Entity, (OccKey, std::sync::Arc<floptle_field::BakedSdf>)>,
    /// Bakes by (asset path, quantized world rotation + scale) — translation is
    /// free (the anchor is read per frame), so moving a map never rebakes.
    occluder_cache: HashMap<OccKey, std::sync::Arc<floptle_field::BakedSdf>>,
    /// Atlas slot order for the occluder volumes (appended AFTER `terrain_slots`).
    occluder_slots: Vec<Entity>,
    /// A paint/sculpt dab on a single terrain only dirties a small voxel box — uploaded
    /// to the GPU directly (no full re-clone + re-upload), so editing a big terrain stays
    /// smooth. `(entity, min inclusive, max exclusive, geometry-changed)`; `geometry` is
    /// true for sculpt (so the wireframe + combined re-sync) and false for paint (color).
    /// Merged across dabs in a frame.
    terrain_region_dirty: Option<(Entity, [u32; 3], [u32; 3], bool)>,
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
    /// The skybox texture path currently uploaded to the GPU (`None` = solid/white), so
    /// we only re-upload when the skybox node's texture actually changes.
    sky_texture_loaded: Option<String>,
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
    /// Script debug-draw commands from this frame's `gizmo.*` calls (world space).
    script_gizmos: Vec<floptle_script::GizmoCmd>,
    /// Their projected viewport segments (physical px) + color, rebuilt per frame.
    script_gizmo_lines: Vec<(Vec2, Vec2, [f32; 3])>,
    /// Master toggle for ALL viewport gizmos/overlays (a button at the viewport's top
    /// right). Off = a clean view; the selected node's collider still hides too.
    show_gizmos: bool,
    /// Show the terrain's collision surface as a wireframe overlay (View menu toggle).
    show_terrain_collider: bool,
    /// Show EVERY mesh collider's wireframe (View menu). The selected mesh-collider node
    /// always shows its wireframe regardless (as long as `show_gizmos` is on).
    show_mesh_colliders: bool,
    /// Cached WORLD-space wireframe of the combined terrain's collision surface; rebuilt
    /// when the terrain changes (cleared on `terrain_gpu_dirty`), projected each frame.
    /// Per terrain entity, in the node's LOCAL frame (the f64 anchor is added at
    /// projection, so a moved terrain's wireframe follows for free).
    terrain_wire_world: Vec<(Entity, Vec<(Vec3, Vec3)>)>,
    /// This frame's projected terrain-collider wireframe segments (screen space).
    terrain_wire_gizmo: Vec<(Vec2, Vec2)>,
    /// MODEL-LOCAL deduped triangle edges per mesh asset path (built once on demand),
    /// transformed by each node's world matrix + projected per frame for collider wires.
    mesh_wire_cache: HashMap<String, Vec<(Vec3, Vec3)>>,
    /// This frame's projected mesh-collider wireframe segments (screen space).
    mesh_wire_gizmo: Vec<(Vec2, Vec2)>,
    /// Project-wide render settings (retro / matter), edited in Project Settings.
    project: ProjectConfigDoc,
    /// The open project's root folder (holds `scenes/`, `models/`, `scripts/`…).
    project_root: PathBuf,
    /// Whether the Project Settings window is open.
    show_project_settings: bool,
    /// Whether the Preferences (user-wide editor settings) window is open.
    show_preferences: bool,
    /// Whether the New/Open Project window is open, + its path text field.
    show_project_mgr: bool,
    project_path_buf: String,
    /// Dockable panel layout (Hierarchy / Inspector / Assets / Scene / Scripting).
    dock_state: Option<egui_dock::DockState<EditorTab>>,
    /// When set, that one tab is shown maximized full-window (double-click a tab to
    /// toggle); the dock layout is bypassed until it's restored.
    fullscreen_tab: Option<EditorTab>,
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
    input_keys_released: std::collections::HashSet<String>,
    input_buttons: [bool; 3],
    input_buttons_pressed: [bool; 3],
    input_mouse_delta: (f32, f32),
    input_scroll: f32,
    /// A script asked (via `input.lockMouse()`) to hold the cursor grabbed + hidden for
    /// free-look. While set, the RMB-release handler won't release the grab, and Stop
    /// releases it. Reset when play ends.
    script_mouse_lock: bool,
    /// The active cursor grab is only a CONFINE (X11 has no OS-level lock): the
    /// cursor can still wander inside the window, so we re-center it every frame.
    cursor_lock_soft: bool,
    /// Offscreen target for the Inspector's spinning model / material preview.
    preview: Option<PreviewTarget>,
    /// Offscreen 16:9 target for the Inspector's selected-camera POV preview.
    cam_preview: Option<PreviewTarget>,
    /// Offscreen target for the Game viewport, used ONLY when the Scene and Game tabs are
    /// both visible (split) so each renders an independent camera view. Sized to the Game
    /// tab; `game_vp_dims` tracks its pixel size so it's only rebuilt on resize.
    game_vp: Option<PreviewTarget>,
    game_vp_dims: (u32, u32),
    /// The split Game viewport's own PostStack (sized with `game_vp`), so the scene's
    /// PostProcess node applies there exactly like in the full-window view.
    game_post: Option<floptle_render::PostStack>,
    /// The Game tab's screen rect (points), captured each frame it draws, used to size
    /// `game_vp` on the next frame.
    game_rect: Option<egui::Rect>,
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
    /// The component clipboard — values copied from one component, pasteable onto
    /// another of the same kind (Inspector ⎘ / 📋).
    component_clip: Option<ComponentClip>,
    /// Search text for the Inspector's "➕ Add Component" menu.
    add_component_filter: String,
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
    /// Animation: clip/controller registries + live per-entity runtimes.
    anim: anim::AnimSystem,
    /// Animation UI state (graph window + Animating tab).
    anim_ui: anim_ui::AnimUiState,
    /// Errors from the most recent script frame, shown in the Scripting tab.
    script_errors: Vec<String>,
    /// Cache of each script file's declared `defaults` keyed by path, with the file's
    /// mtime so we only re-parse when it changes — drives live inspector param sync.
    script_defaults_cache: HashMap<String, (std::time::SystemTime, Vec<(String, f32)>)>,
    /// Syntax diagnostic (line, message) for the active IDE file, for red squiggles.
    ide_diag: Option<(usize, String)>,
    /// The external editor command for "Open in IDE" (ADR-0011); a user preference.
    external_editor: String,
    /// Prefer the external editor over the in-engine IDE for opening scripts.
    prefer_external_editor: bool,
    /// Whether to tint the editor chrome while in play mode (a user preference).
    play_tint_enabled: bool,
    /// Additive RGB offset applied to the chrome bg in play mode (a user preference).
    play_tint: [u8; 3],
    /// Selected engine (chrome) theme — index into `ENGINE_THEMES` (a user preference).
    engine_theme: usize,
    /// Selected code-editor theme — index into `CODE_THEMES` (a user preference).
    code_theme: usize,
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
    /// New-terrain size/thickness/color/texture buffer (Some = the dialog is open).
    new_terrain_cfg: Option<NewTerrainCfg>,
    /// The scene has unsaved edits (drives the "save before opening?" prompt).
    scene_dirty: bool,
    /// A scene the user asked to open while there were unsaved changes — the
    /// confirm modal is shown until they Save / Discard / Cancel.
    pending_open_scene: Option<String>,
    /// Quit was requested with unsaved changes — the confirm modal is up.
    show_quit_confirm: bool,
    /// The quit modal confirmed — the next CloseRequested exits for real.
    quit_confirmed: bool,
    /// An asset delete awaiting confirmation (absolute path).
    delete_confirm: Option<String>,
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
/// Rigged models (any glTF with animations) also carry their skeleton/clips
/// and each part's node binding, so the draw arm can pose parts per frame.
struct MeshAsset {
    parts: Vec<MeshId>,
    size: f32,
    rig: Option<anim::RigAsset>,
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
        let (tint_on, tint_rgb) = load_play_tint();
        self.play_tint_enabled = tint_on;
        self.play_tint = tint_rgb;
        self.grid = load_grid();
        self.engine_theme = load_theme_index(engine_theme_path(), ENGINE_THEMES.len());
        self.code_theme = load_theme_index(code_theme_path(), CODE_THEMES.len());
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
        self.migrate_legacy_post(&doc);
        self.asset_tree = build_assets(&self.project_root);
        self.materials = self.load_materials();
        self.anim.rescan(&self.project_root);
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
            WindowEvent::CloseRequested => {
                if self.scene_dirty && !self.quit_confirmed {
                    self.show_quit_confirm = true;
                } else {
                    event_loop.exit();
                }
            }
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
                // The Game view plays like a build: no editor free-fly camera, no editor
                // shortcuts — only raw key state is tracked (below) for the game's scripts.
                let game_view = self.game_view();
                if let PhysicalKey::Code(code) = event.physical_key {
                    // Held movement keys. The bit is `pressed && !typing && !ctrl`:
                    // a RELEASE (pressed == false) always clears it, so a key can
                    // never stick on if the release lands while a field is focused
                    // (e.g. hold W, click into the IDE, release W). C moves DOWN.
                    // Fly-camera keys only arm while the pointer is over the Scene
                    // viewport — WASD in the Animating tab (or any other panel)
                    // must not drive the editor camera.
                    let mv = pressed && !typing && !game_view && self.cursor_over_scene();
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
                        } else if self.input_keys.remove(name) {
                            self.input_keys_released.insert(name.to_string());
                        }
                    }
                    // Discrete commands fire on press only.
                    if pressed && !typing {
                        // Engine controls work in any view (Play/Pause/Quit).
                        match code {
                            KeyCode::Escape => {
                                // Escape is a "cancel" gesture first: back out of an
                                // in-progress transition drag or the graph window, and
                                // never silently discard unsaved work.
                                if self.anim_ui.drag_from.is_some() {
                                    self.anim_ui.drag_from = None;
                                } else if self.scene_dirty {
                                    self.show_quit_confirm = true;
                                } else {
                                    event_loop.exit();
                                }
                            }
                            KeyCode::F1 => self.toggle_play(),
                            KeyCode::F2 => self.toggle_pause(),
                            // Everything else is an EDITOR shortcut — suppressed in the
                            // Game view so it behaves like a real build.
                            _ if !game_view => {
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
                                        KeyCode::Delete | KeyCode::Backspace => self.delete_selected(),
                                        KeyCode::KeyF => self.focus_selected(),
                                        KeyCode::KeyQ => self.selection.clear(), // unselect
                                        KeyCode::KeyG => self.grid.show = !self.grid.show, // toggle grid
                                        KeyCode::ArrowUp => self.step_selection(-1),
                                        KeyCode::ArrowDown => self.step_selection(1),
                                        KeyCode::Enter | KeyCode::NumpadEnter => {
                                            self.toggle_folder_selected()
                                        }
                                        _ => {
                                            if let Some(t) = digit_of(code).and_then(Tool::from_digit) {
                                                self.set_tool(t);
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
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
                    // Clicking anywhere outside a text field ends text editing —
                    // a click into the viewport (which egui never sees) included.
                    if let Some(eg) = self.egui.as_ref() {
                        if !eg.ctx.is_pointer_over_egui() {
                            if let Some(f) = eg.ctx.memory(|m| m.focused()) {
                                eg.ctx.memory_mut(|m| m.surrender_focus(f));
                            }
                        }
                    }
                    // In the Game view a left click is a GAME input only — never an editor
                    // pick/sculpt/gizmo-grab (it plays like a build), so treat it as not
                    // over the scene for editor purposes.
                    let over_scene = self.cursor_over_scene() && !self.game_view();
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
                // In the Game view, RMB still grabs the cursor for mouse-look (the game
                // reads the button + raw delta), but it drives no EDITOR camera and opens
                // no context menu.
                let editor = !self.game_view();
                if pressed {
                    // Begin a possible look; if the cursor barely moves before release
                    // it's a click ⏵ open a context menu instead.
                    self.rmb_press = self.cursor;
                    self.rmb_moved = 0.0;
                    self.context_menu = None;
                    if over_scene {
                        if editor {
                            self.input.looking = true;
                        }
                        if let Some(window) = self.window.as_ref() {
                            self.cursor_lock_soft = grab_cursor(window, true);
                        }
                        self.cursor = None;
                    }
                } else {
                    let was_looking = self.input.looking;
                    self.input.looking = false;
                    // Don't release the grab if a script is holding the mouse locked.
                    if !self.script_mouse_lock {
                        if let Some(window) = self.window.as_ref() {
                            self.cursor_lock_soft = grab_cursor(window, false);
                        }
                    }
                    // A click (negligible motion) over the viewport ⏵ context menu (editor only).
                    if editor && was_looking && self.rmb_moved < 6.0 {
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

        // Inspector camera POV preview: if a Camera node is selected, render the scene
        // from its viewpoint into the 16:9 offscreen target (before the destructure).
        let cam_elapsed = self.started.map(|s| s.elapsed().as_secs_f32()).unwrap_or(0.0);
        self.update_camera_preview(cam_elapsed);
        // When Scene + Game are split, render the Game view into its own offscreen target.
        self.update_game_viewport(cam_elapsed);
        // Keep the Inspector's script param list in sync with each script's `defaults`
        // (cheap: cached by file mtime, selected node only) so editing a script surfaces
        // new tunables and drops removed ones live.
        self.sync_selected_script_params();
        // Whether the Game viewport is focused (precomputed before the GPU borrow): game
        // input only feeds scripts here. `game_view()` is pointer-aware in split view, so
        // when both tabs show, input goes to whichever viewport the mouse is over and the
        // Scene view stays fully interactive.
        let game_focused = self.game_view();

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
        // Don't drive the editor (Scene) camera while the Game viewport is focused — that
        // input belongs to the game (e.g. the mouse is over the Game view in split mode).
        if !game_focused {
            self.camera.update(&self.input, dt);
        }

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
            // Feed the player input to scripts (the Lua `input` API) — but ONLY while the
            // Game view is focused. In the Scene view you're editing, not playing, so the
            // game gets neutral input (the character stops moving) even though physics
            // keeps simulating.
            self.script_host.set_input(if game_focused {
                floptle_script::InputSnapshot {
                    keys_down: self.input_keys.clone(),
                    keys_pressed: self.input_keys_pressed.clone(),
                    keys_released: self.input_keys_released.clone(),
                    mouse: self.cursor.map(|c| (c.x, c.y)).unwrap_or((0.0, 0.0)),
                    mouse_delta: self.input_mouse_delta,
                    scroll: self.input_scroll,
                    buttons_down: self.input_buttons,
                    buttons_pressed: self.input_buttons_pressed,
                }
            } else {
                floptle_script::InputSnapshot::default()
            });
            // Lend the sim's colliders to scripts so `raycast(...)` works this frame
            // (physics doesn't step until after scripts, so this is safe). The sim
            // origin rides along so ray coordinates convert world ↔ sim frame.
            if let Some(sim) = self.sim.as_mut() {
                self.script_host
                    .set_colliders(std::mem::take(&mut sim.world.colliders), sim.world.origin);
            }
            // Lend the asset root (for `assets.getFile/getContents`) and the material
            // presets (so `node.material = "Gold"` resolves) for this frame's scripts.
            self.script_host.set_project_root(self.project_root.clone());
            self.script_host.set_materials(
                self.materials.iter().map(|(n, d)| (n.clone(), d.to_material())).collect(),
            );
            // Feed each animator's state (layers/current/time) so scripts can read
            // anim:state()/:time()/:clips() this frame.
            self.script_host.set_anim_info(anim::build_info(&self.anim));
            self.script_host.run(&mut self.world, &dir, sdt, self.play_t);
            self.script_errors = self.script_host.errors().to_vec();
            // Apply any mouse lock/unlock a script requested this frame (grab + hide the
            // cursor for free-look, or release it). The state persists until changed/Stop.
            // Script debug gizmos queued this frame (drawn by the viewport overlay).
            self.script_gizmos = self.script_host.take_gizmos();
            if let Some(want) = self.script_host.take_mouse_lock() {
                self.script_mouse_lock = want;
                if let Some(window) = self.window.as_ref() {
                    self.cursor_lock_soft = grab_cursor(window, want);
                }
            }
            // GPU-load any models a script swapped via `node.model` (the Matter is already
            // updated by run; we re-import here so the new mesh renders THIS frame). Inlined
            // with the in-scope `gpu`/`raster` borrows — `self.import_model` can't run while
            // they're held.
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
            // Apply script velocity writes, then advance physics (writes transforms back).
            // Gravity field is rebuilt from the scene's GravityVolume node(s) every frame
            // (cheap scan) so tweaking mode/strength/radius — or moving the volume — takes
            // effect immediately instead of needing a Stop/Play. The active camera is the
            // floating-origin focus: drift far enough and the sim recenters on it.
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
                for (eid, v) in self.script_host.take_body_changes() {
                    sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
                }
                for (eid, h) in self.script_host.take_body_height_changes() {
                    sim.set_body_height(eid, h);
                }
                sim.advance(&mut self.world, sdt, focus);
            }
        } else if !self.script_errors.is_empty() {
            self.script_errors.clear();
        }
        // Clear per-frame input edges after scripts consumed them.
        self.input_keys_pressed.clear();
        self.input_keys_released.clear();
        self.input_buttons_pressed = [false; 3];
        self.input_mouse_delta = (0.0, 0.0);
        self.input_scroll = 0.0;
        // A CONFINE-only grab (X11 has no OS cursor lock) still lets the pointer
        // wander inside the window — pin it to the center ourselves while a
        // look/lock is active. Look input reads RAW device motion, so this
        // re-centering never pollutes the deltas.
        if self.cursor_lock_soft && (self.script_mouse_lock || self.input.looking) {
            if let Some(window) = self.window.as_ref() {
                let sz = window.inner_size();
                let _ = window.set_cursor_position(winit::dpi::PhysicalPosition::new(
                    sz.width / 2,
                    sz.height / 2,
                ));
            }
        }
        // Drain any script logs/errors into the Console (consecutive dups merge).
        for l in self.script_host.drain_logs() {
            self.console.push(l.level, l.msg, l.source);
        }

        // ---- gather the scene from the World ----
        let aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
        // The Game dock tab being front = render from the active camera node; otherwise
        // (Scene tab) use the editor's free-fly camera. Works whether or not we're
        // playing, so you can frame the active camera's shot without entering play.
        // (Inlined — self methods can't be called while gpu/egui are borrowed.) A
        // fullscreened tab overrides which view is front. When Scene + Game are split,
        // the SURFACE renders the editor view (for the transparent Scene tab) while the
        // Game tab shows its own offscreen render (update_game_viewport).
        let split_views = self.fullscreen_tab.is_none()
            && self.dock_state.as_ref().is_some_and(scene_and_game_split);
        let game_view = !split_views
            && match self.fullscreen_tab {
                Some(EditorTab::Game) => true,
                Some(_) => false,
                None => self.dock_state.as_ref().is_some_and(game_tab_active),
            };
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
        // Script debug gizmos (`gizmo.*` from Lua). Unlike the editor overlays these
        // draw in the GAME view too — they're the developer's own telegraphs — but
        // the viewport gizmos toggle still hides them. (Projected for the SURFACE
        // camera; in split view the tab viewer paints them on the Scene side only.)
        self.script_gizmo_lines.clear();
        if self.show_gizmos && !self.script_gizmos.is_empty() {
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
            if let Some(sim) = self.sim.as_ref() {
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
            if self.show_terrain_collider {
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
                if !self.show_mesh_colliders && !self.selection.contains(&e) {
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
                if !self.show_mesh_colliders && !self.selection.contains(&e) {
                    continue;
                }
                let wt = floptle_core::world_transform(&self.world, e);
                let s = wt.scale;
                let lines = match shape {
                    floptle_core::Shape::Cube => {
                        let m = Mat4::from_scale_rotation_translation(s, wt.rotation, wt.translation.as_vec3());
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
        // Sun shadows (Lighting node knobs) + the collider-proxy occluders that let
        // raster meshes cast — both ride the raymarch globals, which the raster pass
        // reads too through the shared field bind group.
        let (sh_params, sh_tint, sh_extra) = shadow_uniforms(&light_node);
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

        // Edit-mode animation preview (Animating tab): pose the bound node at the
        // playhead. Scene-node bindings apply transiently and are restored right
        // after the draw list below is built, so a preview never dirties the
        // authored scene (undo, save, and the Inspector all see real transforms).
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
                    if self.anim_ui.record {
                        if anim_ui::record_scan(&self.world, &mut self.anim_ui, target) {
                            self.anim_ui.clip_dirty = true;
                        }
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
            } else if !self.anim.poses.is_empty() || !self.anim.instances.is_empty() {
                // Tab hidden: drop stale preview runtimes so models return to rest.
                self.anim.poses.clear();
                self.anim.instances.clear();
            }
            self.anim_ui.tab_visible = false; // re-armed by the tab each frame it draws
        }

        let ents: Vec<(Entity, Matter)> =
            self.world.query::<Matter>().map(|(e, m)| (e, m.clone())).collect();
        let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
        let mut blobs: Vec<(DVec3, f32, MaterialParams)> = Vec::new();
        if let Some((path, pos)) = &drag_ghost {
            if let Some(asset) = self.mesh_registry.get(path) {
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
                        if let Some(rig) = asset.rig.as_ref() {
                            // Rigged: each part rides its (possibly animated) node.
                            let node_world =
                                self.anim.poses.get(e).unwrap_or(&rig.rest_world);
                            for (i, &mid) in asset.parts.iter().enumerate() {
                                let local = rig
                                    .part_nodes
                                    .get(i)
                                    .and_then(|&n| node_world.get(n))
                                    .copied()
                                    .unwrap_or(Mat4::IDENTITY);
                                instances.push((mid, tex, instance_of_mat(model * local, &mp)));
                            }
                        } else {
                            for &mid in &asset.parts {
                                instances.push((mid, tex, instance_of_mat(model, &mp)));
                            }
                        }
                    }
                }
                // group / terrain / camera / light / gravity / skybox / post render elsewhere.
                Matter::Empty
                | Matter::Terrain { .. }
                | Matter::Camera { .. }
                | Matter::PointLight { .. }
                | Matter::GravityVolume { .. }
                | Matter::Skybox { .. }
                | Matter::PostProcess { .. } => {}
            }
        }

        // Undo any transient scene-binding animation preview now that the draw list
        // is built — the ECS goes back to authored transforms before UI/undo/save.
        self.anim.restore_preview(&mut self.world);

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
            }
        };

        // Selection outline source: the selected object's silhouette into the mask —
        // a mesh instance, or (for a blob) a one-blob raymarch so the outline hugs
        // only the selected blob.
        let mut mask_mesh: Vec<(MeshId, InstanceRaw)> = Vec::new();
        let mut mask_blob: Option<RaymarchGlobals> = None;
        // The Game view plays like a build — no selection outline there.
        if let Some(e) = self.selection.last().copied().filter(|_| !game_view) {
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
                    Matter::Empty
                    | Matter::Terrain { .. }
                    | Matter::Camera { .. }
                    | Matter::PointLight { .. }
                    | Matter::GravityVolume { .. }
                    | Matter::Skybox { .. }
                    | Matter::PostProcess { .. } => {}
                }
            }
        }

        // The raymarch pass renders the blob matter (gated by the SDF-matter toggle)
        // and/or the combined terrain volume. The globals are built either way — on
        // frames with nothing to raymarch they're still uploaded (not drawn) so the
        // raster pass's field bind group has this frame's shadow/proxy data.
        let show_blobs = self.project.matter && !blobs.is_empty();
        let rm_draw = show_blobs || !self.terrains.is_empty();
        let rm = {
            let mut g = make_rm(if show_blobs { &blobs } else { &[] });
            Self::fill_terrain_volumes(&self.terrains, &self.terrain_slots, &self.mesh_occluders, &self.occluder_slots, &self.world, &mut g, cam.world_position);
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
        let fullscreen_tab = &mut self.fullscreen_tab;
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
        // Split view: the Game tab paints its own offscreen render this frame.
        let game_split = fullscreen_tab.is_none() && scene_and_game_split(dock_state);
        let game_tex = self.game_vp.as_ref().map(|p| p.tex_id);
        let game_rect = &mut self.game_rect;
        let materials = &self.materials;
        let mat_name_buf = &mut self.mat_name_buf;
        let component_clip = &self.component_clip;
        let add_component_filter = &mut self.add_component_filter;
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
        let script_gizmo_lines = self.script_gizmo_lines.as_slice();
        let terrain_wire = self.terrain_wire_gizmo.as_slice();
        let mesh_wire = self.mesh_wire_gizmo.as_slice();
        let show_gizmos = &mut self.show_gizmos;
        let grabbed = self.grabbed;
        let tool = self.tool;
        let context_menu = self.context_menu;
        let anim_sys = &mut self.anim;
        let anim_ui_state = &mut self.anim_ui;
        let mesh_registry = &self.mesh_registry;
        let mut cmd = EditorCmd::default();
        let mut want_save = false;
        let mut want_save_project = false;
        let mut frame_pointer_down = false;
        let full_output = ctx.run_ui(raw_input, |ui| {
            let pointer_down = ui.input(|i| i.pointer.any_down());
            frame_pointer_down = pointer_down;
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
                        if ui.button("◉ Animation Controller").on_hover_text("the state-graph editor: states, transitions, fades, layers").clicked() {
                            cmd.focus_anim_graph = true;
                            ui.close();
                        }
                        if ui.button("✎ Animating").on_hover_text("the animation timeline: preview, keys, events").clicked() {
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
                fullscreen_tab,
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
                component_clip,
                add_component_filter,
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
                script_gizmo_lines,
                terrain_wire,
                mesh_wire,
                show_gizmos,
                grabbed,
                tool,
                scene_rect: &mut *scene_rect,
                game_rect,
                game_split,
                game_tex,
                aspect: aspect_mode,
                zoom: viewport_zoom,
                scene_name: &scene_name,
                ppp,
                code_theme,
                anim: anim_sys,
                anim_ui: anim_ui_state,
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
                ui.horizontal(|ui| {
                    if ui
                        .button(format!("⛶ Restore  ·  {}", ft.title()))
                        .on_hover_text("double-click a tab to toggle fullscreen · Esc to restore")
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
                    ui.small("Post-processing (bloom, vignette, ambient occlusion) moved to each scene's ✨ Post Processing node — select it in the Hierarchy.");

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
                let ext = Path::new(path.as_str())
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| format!(".{e}"))
                    .unwrap_or_default();
                egui::Window::new("Name file")
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
            if let Some(path) = delete_confirm.clone() {
                let mut open = true;
                let mut close = false;
                let name = Path::new(&path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.clone());
                let is_dir = Path::new(&path).is_dir();
                egui::Window::new("Delete asset")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(340.0)
                    .show(ui.ctx(), |ui| {
                        if is_dir {
                            ui.label(format!("Delete the folder \"{name}\" and everything in it?"));
                        } else {
                            ui.label(format!("Delete \"{name}\"?"));
                        }
                        ui.small("This can't be undone.");
                        ui.horizontal(|ui| {
                            if ui.button("🗑 Delete").clicked() {
                                cmd.do_delete_asset = Some(path.clone());
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
        // gathered above) runs at full frame res after the scene is composited (and
        // after any retro downsample), before the outline + egui.
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
                // `rm_draw` already accounts for the matter toggle + terrain presence;
                // with nothing to raymarch the globals still upload so the raster
                // pass's field group (shadows/AO/proxies) sees this frame's data.
                let raster_clear = if rm_draw {
                    raymarch.draw_into(gpu, color, depth, rm);
                    None
                } else {
                    raymarch.upload_globals(gpu, rm);
                    Some(clear.map(|c| c as f64))
                };
                raster.draw_scene(
                    gpu, color, depth, globals, &instances, raster_clear,
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
                    // SSAO reads whichever depth the scene was rendered with — the
                    // low-res retro depth in retro mode (AO goes chunky with the
                    // pixels, which fits the look) or the full-res shared depth.
                    let proj = cam.proj_matrix(aspect);
                    let ssao_frame = floptle_render::SsaoFrame {
                        depth: if self.project.retro { retro.depth_view() } else { gpu.depth_view() },
                        proj: proj.to_cols_array_2d(),
                        inv_proj: proj.inverse().to_cols_array_2d(),
                    };
                    post.run(gpu, &post_settings, Some(&ssao_frame), &frame.view);
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
                MatterDoc::Skybox { .. } => "Skybox",
                MatterDoc::PostProcess { .. } => "Post Processing",
            };
            self.add_node(name, m);
        }
        if cmd.inspector_changed {
            self.begin_edit();
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
        if cmd.focus_animating {
            if let Some(dock) = self.dock_state.as_mut() {
                if let Some(path) = dock.find_tab(&EditorTab::Animation) {
                    let _ = dock.set_active_tab(path);
                } else {
                    dock.push_to_focused_leaf(EditorTab::Animation);
                }
            }
        }
        if cmd.focus_anim_graph {
            if let Some(dock) = self.dock_state.as_mut() {
                if let Some(path) = dock.find_tab(&EditorTab::AnimGraph) {
                    let _ = dock.set_active_tab(path);
                } else {
                    dock.push_to_focused_leaf(EditorTab::AnimGraph);
                }
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
                    self.terrain_gpu_dirty = true;
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
                    self.terrain_gpu_dirty = true;
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
            self.anim.rescan(&self.project_root);
            self.anim_ui.probes.clear(); // re-probe model animation lists
        }
        if let Some(dir) = cmd.new_folder_in {
            self.new_folder(&dir);
        }
        if let Some(dir) = cmd.new_script_in {
            self.new_script(&dir);
        }
        if let Some(path) = cmd.rename_asset {
            // Seed the rename modal with the current base name (the extension is shown as a
            // fixed suffix in the modal, so you edit just the name).
            let p = Path::new(&path);
            let name = if p.is_dir() {
                p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
            } else {
                p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
            };
            self.rename_target = Some((path, name));
        }
        if let Some((from, to)) = cmd.do_rename {
            self.rename_asset(&from, &to);
        }
        if let Some(path) = cmd.delete_asset {
            // Deleting a file/folder is irreversible — always confirm first.
            self.delete_confirm = Some(path);
        }
        if let Some(path) = cmd.do_delete_asset {
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
    /// One-time migration: a scene from before the PostProcess node inherits the
    /// legacy project-wide bloom/vignette settings (old `project.ron` fields) onto
    /// its self-healed node, so an old project keeps the look it was tuned for.
    /// Scenes that already carry a PostProcess node are left alone, as are legacy
    /// projects that never enabled an effect (the healed default — AO on — stands).
    fn migrate_legacy_post(&mut self, doc: &SceneDoc) {
        if doc.nodes.iter().any(|n| matches!(n.matter, MatterDoc::PostProcess { .. })) {
            return;
        }
        let p = self.project;
        if !(p.bloom || p.vignette) {
            return;
        }
        let node = self
            .world
            .query::<Matter>()
            .find_map(|(e, m)| matches!(m, Matter::PostProcess { .. }).then_some(e));
        if let Some(e) = node {
            if let Some(Matter::PostProcess {
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
    }

    fn restore(&mut self, doc: SceneDoc) {
        // Entities are respawned below — drop animator runtimes keyed by the old ones.
        self.anim.clear_instances();
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
            self.terrain_gpu_dirty = true;
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

    /// Build the physics gravity field from the scene's GravityVolume nodes: `Down`
    /// volumes add uniform −Y gravity (the level's base), `Radial` volumes add a planet
    /// gravity well at the node. No GravityVolume node → ZERO gravity (a space/zero-g
    /// world). Takes `&World` (not `&self`) so it can be called from the play loop
    /// while `self.gpu`/egui are mutably borrowed — see call site.
    /// Build the scene's gravity field for the sim. `origin` is the sim's world origin
    /// (ADR-0015): radial centers are converted to the sim frame in f64 here, so a
    /// planet placed far out pulls exactly.
    fn build_gravity_field(world: &floptle_core::World, origin: DVec3) -> floptle_physics::GravityField {
        use floptle_core::{GravityMode, Matter};
        let mut field = floptle_physics::GravityField::default();
        for (e, m) in world.query::<Matter>() {
            if let Matter::GravityVolume { mode, strength, radius } = m {
                match mode {
                    GravityMode::Down => field
                        .sources
                        .push(floptle_physics::GravitySource::Uniform(Vec3::new(0.0, -*strength, 0.0))),
                    GravityMode::Radial => {
                        let p = floptle_core::world_transform(world, e).translation;
                        field.sources.push(floptle_physics::GravitySource::Point {
                            center: (p - origin).as_vec3(),
                            strength: *strength,
                            radius: *radius,
                        });
                    }
                }
            }
        }
        field
    }

    /// Where the sim's local frame should be centered at Play (ADR-0015): the active
    /// camera if there is one, else the first rigidbody, else the world origin —
    /// rounded to whole units so every later rebase shift stays exact in f32.
    fn sim_origin_hint(&self) -> DVec3 {
        use floptle_core::Matter;
        let focus = self
            .world
            .query::<Matter>()
            .find_map(|(e, m)| matches!(m, Matter::Camera { active: true, .. }).then_some(e))
            .or_else(|| self.world.query::<floptle_core::RigidBody>().map(|(e, _)| e).next());
        focus
            .map(|e| floptle_core::world_transform(&self.world, e).translation.round())
            .unwrap_or(DVec3::ZERO)
    }

    /// Build every node's STATIC collider into the sim at Play. A node is a static
    /// collider if it carries `Collidable` (the "collidable" switch) or the legacy
    /// `MeshCollider` marker. The collider is auto-shaped from the node's `Matter`:
    /// a Mesh bakes its world-space triangles; a Cube/Sphere/Capsule primitive becomes
    /// a box/sphere/capsule sized to the primitive geometry × the node's scale (and
    /// oriented by its rotation). These are environment colliders, not dynamic bodies.
    /// Keep the shadow-occluder bakes in sync with the scene's static collider
    /// meshes (Collidable / MeshCollider on a `Matter::Mesh` node, no RigidBody —
    /// dynamic bodies cast via their shape proxies instead). Each eligible mesh
    /// bakes once per (asset, rotation, scale) into an unsigned occluder volume
    /// (`bake_occluder`), cached so duplicates and pure moves are free. Returns
    /// true when the SET changed and the atlas needs re-uploading; per-node
    /// "casts shadows" / visibility toggles are applied at fill time (no rebake).
    fn refresh_mesh_occluders(&mut self) -> bool {
        // The desired (entity → key) set this frame.
        let mut desired: Vec<(Entity, OccKey)> = Vec::new();
        let ents: Vec<(Entity, String)> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Mesh { asset_path } => Some((e, asset_path.clone())),
                _ => None,
            })
            .collect();
        for (e, path) in ents {
            let static_collider = (self.world.get::<floptle_core::Collidable>(e).is_some()
                || self.world.get::<floptle_core::MeshCollider>(e).is_some())
                && self.world.get::<floptle_core::RigidBody>(e).is_none();
            if !static_collider {
                continue;
            }
            let wt = floptle_core::world_transform(&self.world, e);
            let q = |v: f32| (v * 1000.0).round() as i32;
            let key: OccKey = (
                path,
                [q(wt.rotation.x), q(wt.rotation.y), q(wt.rotation.z), q(wt.rotation.w)],
                [q(wt.scale.x), q(wt.scale.y), q(wt.scale.z)],
            );
            desired.push((e, key));
        }
        let unchanged = desired.len() == self.mesh_occluders.len()
            && desired
                .iter()
                .all(|(e, key)| self.mesh_occluders.get(e).is_some_and(|(k, _)| k == key));
        if unchanged {
            return false;
        }

        let mut next: HashMap<Entity, (OccKey, std::sync::Arc<floptle_field::BakedSdf>)> =
            HashMap::new();
        for (e, key) in desired {
            let baked = if let Some(b) = self.occluder_cache.get(&key) {
                b.clone()
            } else {
                // Bake: rotation + scale applied to the vertices (like the physics
                // colliders); translation stays in the per-frame f64 anchor.
                let started = Instant::now();
                let Ok(model) =
                    floptle_assets::gltf_import::import(std::path::Path::new(&key.0))
                else {
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!("shadow occluder: failed to load {}", key.0),
                        None,
                    );
                    continue;
                };
                let rot = Quat::from_xyzw(
                    key.1[0] as f32 / 1000.0,
                    key.1[1] as f32 / 1000.0,
                    key.1[2] as f32 / 1000.0,
                    key.1[3] as f32 / 1000.0,
                )
                .normalize();
                let s = Vec3::new(
                    key.2[0] as f32 / 1000.0,
                    key.2[1] as f32 / 1000.0,
                    key.2[2] as f32 / 1000.0,
                );
                let m = Mat4::from_scale_rotation_translation(s, rot, Vec3::ZERO);
                let mut verts: Vec<[f32; 3]> = Vec::new();
                let mut indices: Vec<u32> = Vec::new();
                for part in &model.parts {
                    let base = verts.len() as u32;
                    verts.extend(
                        part.mesh
                            .vertices
                            .iter()
                            .map(|v| m.transform_point3(Vec3::from(v.pos)).to_array()),
                    );
                    indices.extend(part.mesh.indices.iter().map(|i| i + base));
                }
                // 128 voxels along the longest axis: a whole-map bake lands well
                // under a second and keeps doorways/rooms resolvable (the user's
                // ~80-unit map → ~0.6-unit voxels).
                let baked =
                    std::sync::Arc::new(floptle_field::bake_occluder(&verts, &indices, 128));
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!(
                        "baked shadow occluder for {} ({} tris → {}×{}×{} voxels, {} ms)",
                        key.0,
                        indices.len() / 3,
                        baked.dims[0],
                        baked.dims[1],
                        baked.dims[2],
                        started.elapsed().as_millis()
                    ),
                    None,
                );
                self.occluder_cache.insert(key.clone(), baked.clone());
                baked
            };
            next.insert(e, (key, baked));
        }
        // Drop cache entries nothing references anymore (a resized/removed map).
        self.occluder_cache.retain(|k, _| next.values().any(|(nk, _)| nk == k));
        self.mesh_occluders = next;
        true
    }

    fn add_static_colliders(&self, sim: &mut floptle_physics::Sim) {
        // Union of Collidable + legacy MeshCollider entities (dedup; a node flagged both
        // is added once). A node with a RigidBody is a *dynamic* body (Sim::build made it
        // one) — skip it here so its dynamic body doesn't fight a static collider sitting at
        // the same spot (which would freeze/eject it). Collidable = static world geometry
        // only when there's no RigidBody.
        let mut ents: Vec<Entity> = self
            .world
            .query::<floptle_core::Collidable>()
            .map(|(e, _)| e)
            .filter(|e| self.world.get::<floptle_core::RigidBody>(*e).is_none())
            .collect();
        for (e, _) in self.world.query::<floptle_core::MeshCollider>() {
            if !ents.contains(&e) && self.world.get::<floptle_core::RigidBody>(e).is_none() {
                ents.push(e);
            }
        }
        for e in ents {
            let wt = floptle_core::world_transform(&self.world, e);
            // Anchor each collider on its own node (full f64) and bake geometry
            // RELATIVE to it — the residuals stay small and exact no matter how far
            // out the node sits (ADR-0015); the sim re-anchors them per rebase.
            let anchor = wt.translation;
            let s = wt.scale;
            match self.world.get::<Matter>(e) {
                Some(Matter::Mesh { asset_path }) => {
                    let path = asset_path.clone();
                    let Ok(model) = floptle_assets::gltf_import::import(std::path::Path::new(&path)) else {
                        eprintln!("collidable mesh: failed to load {path}");
                        continue;
                    };
                    // Scale + rotate locally (f32 is exact here — model-sized numbers);
                    // the node's translation lives in the f64 anchor, never the verts.
                    let m = Mat4::from_scale_rotation_translation(s, wt.rotation, Vec3::ZERO);
                    let mut verts: Vec<Vec3> = Vec::new();
                    let mut indices: Vec<u32> = Vec::new();
                    for part in &model.parts {
                        let base = verts.len() as u32;
                        verts.extend(part.mesh.vertices.iter().map(|v| m.transform_point3(Vec3::from(v.pos))));
                        indices.extend(part.mesh.indices.iter().map(|i| i + base));
                    }
                    sim.add_static_mesh(anchor, &verts, &indices);
                }
                // Primitive geometry → matching analytic collider, sized to match the
                // mesh the renderer draws (cube half 0.7, sphere r 0.85, capsule r/half 0.5).
                Some(Matter::Primitive { shape, .. }) => match shape {
                    floptle_core::Shape::Cube => {
                        sim.add_static_box(anchor, Vec3::new(0.7 * s.x, 0.7 * s.y, 0.7 * s.z), wt.rotation);
                    }
                    floptle_core::Shape::Sphere => {
                        sim.add_static_sphere(anchor, 0.85 * s.max_element());
                    }
                    floptle_core::Shape::Capsule => {
                        let up = wt.rotation * Vec3::Y;
                        sim.add_static_capsule(anchor, up, 0.5 * s.y, 0.5 * s.x.max(s.z));
                    }
                },
                _ => {}
            }
        }
    }

    /// Rebuild the live physics sim from the current scene. A no-op unless playing —
    /// called after a physics component (rigidbody / collider / type) changes mid-Play
    /// so the edit takes effect immediately. Bodies re-seed at their current transforms.
    fn rebuild_sim(&mut self) {
        if !self.playing {
            return;
        }
        let origin = self.sim_origin_hint();
        let gravity = Self::build_gravity_field(&self.world, origin);
        let terrain_vols = self.terrain_volumes();
        let mut sim = floptle_physics::Sim::build(&self.world, &terrain_vols, gravity, origin);
        drop(terrain_vols);
        self.add_static_colliders(&mut sim);
        self.sim = Some(sim);
    }

    /// Every terrain volume as `(node world translation, node-local field)` — what the
    /// sim colliders anchor on. Each volume collides at its NATIVE resolution (the
    /// combined field is render-only), placed in full `f64` (ADR-0015).
    fn terrain_volumes(&self) -> Vec<(DVec3, &floptle_field::Terrain)> {
        self.terrains
            .iter()
            .map(|(&e, t)| (floptle_core::world_transform(&self.world, e).translation, t))
            .collect()
    }

    /// Paste the component clipboard onto `e` (the held clip decides the kind). Adds
    /// the component if missing, else overwrites its values; scripts add-or-update by
    /// name. Pasting a "type" (Matter) never morphs a Terrain node (its field is
    /// out-of-ECS).
    fn paste_onto(&mut self, e: Entity) {
        let Some(clip) = self.component_clip.clone() else { return };
        if !self.world.is_alive(e) {
            return;
        }
        self.record();
        let mut physics = false;
        match clip {
            ComponentClip::Transform(t) => {
                if let Some(cur) = self.world.get_mut::<Transform>(e) {
                    *cur = t;
                }
            }
            ComponentClip::Matter(m) => {
                // Terrain keeps its type (out-of-ECS field). The PostProcess node only
                // accepts PostProcess values (that's how settings copy between scenes),
                // and no other node may be turned into one by paste.
                let target_is_post =
                    matches!(self.world.get::<Matter>(e), Some(Matter::PostProcess { .. }));
                let clip_is_post = matches!(m, Matter::PostProcess { .. });
                if !matches!(self.world.get::<Matter>(e), Some(Matter::Terrain { .. }))
                    && target_is_post == clip_is_post
                {
                    self.world.insert(e, m);
                    physics = true;
                }
            }
            ComponentClip::Material(m) => {
                self.world.insert(e, *m);
            }
            ComponentClip::RigidBody(rb) => {
                self.world.insert(e, rb);
                physics = true;
            }
            ComponentClip::Script(si) => {
                let scripts = match self.world.get_mut::<Scripts>(e) {
                    Some(s) => s,
                    None => {
                        self.world.insert(e, Scripts::default());
                        self.world.get_mut::<Scripts>(e).unwrap()
                    }
                };
                if let Some(existing) = scripts.0.iter_mut().find(|i| i.kind == si.kind) {
                    existing.params = si.params;
                    existing.enabled = si.enabled;
                } else {
                    scripts.0.push(si);
                }
            }
        }
        if physics {
            self.rebuild_sim();
        }
    }

    /// Enter/leave play mode. Play snapshots the authored scene and runs scripts;
    /// Stop restores the authored scene so script-driven changes aren't persisted.
    /// Drop every animator runtime + the Animating tab's entity bindings —
    /// called whenever the World is rebuilt (scene/project switches), since
    /// entity handles from the old world alias entities in the new one.
    fn reset_anim_bindings(&mut self) {
        self.stop_recording();
        self.anim.clear_instances();
        self.anim_ui.target = None;
        self.anim_ui.sel_anim = None;
        self.anim_ui.clip_doc = None;
        self.anim_ui.preview_playing = false;
        self.anim_ui.last_scene_local.clear();
    }

    /// Turn ● Record off and put the posed subtree back exactly as it was
    /// when recording started — recording authors the CLIP, never the scene.
    fn stop_recording(&mut self) {
        if !self.anim_ui.record && self.anim_ui.record_restore.is_empty() {
            return;
        }
        self.anim_ui.record = false;
        for (e, tr) in self.anim_ui.record_restore.drain(..) {
            if let Some(slot) = self.world.get_mut::<Transform>(e) {
                *slot = tr;
            }
        }
        self.anim_ui.last_scene_local.clear();
    }

    fn toggle_play(&mut self) {
        // Fresh animator runtimes both ways (Play binds against the live scene;
        // Stop drops them so the restored scene isn't posed by stale animators).
        self.anim.clear_instances();
        self.anim_ui.preview_playing = false;
        // Recording must never run during Play (gameplay motion would bake into
        // the clip asset), and stale queued animator commands must not leak
        // across sessions.
        self.stop_recording();
        self.script_host.clear_anim_state();
        self.script_gizmos.clear();
        if self.playing {
            self.playing = false;
            self.paused = false;
            self.sim = None; // drop the physics sim; restore reverts moved transforms
            // Release any script-held mouse lock so you're not stuck grabbed after Stop.
            if self.script_mouse_lock {
                self.script_mouse_lock = false;
                if let Some(window) = self.window.as_ref() {
                    self.cursor_lock_soft = grab_cursor(window, false);
                }
            }
            if let Some(snap) = self.play_snapshot.take() {
                self.restore(snap);
            }
        } else {
            // Scripts run from what's on DISK — flush unsaved IDE edits first so
            // Play always tests the code you're looking at.
            let mut flushed = 0;
            for f in self.ide.open.iter_mut().filter(|f| f.dirty) {
                if std::fs::write(&f.path, &f.text).is_ok() {
                    f.dirty = false;
                    flushed += 1;
                }
            }
            self.play_snapshot = Some(self.snapshot());
            self.play_t = 0.0;
            self.paused = false;
            // Build the physics sim from the scene: RigidBody nodes + every terrain
            // volume (its own anchored SDF collider, native resolution) + the gravity
            // field from GravityVolume nodes.
            let origin = self.sim_origin_hint();
            let gravity = Self::build_gravity_field(&self.world, origin);
            let terrain_vols = self.terrain_volumes();
            let mut sim = floptle_physics::Sim::build(&self.world, &terrain_vols, gravity, origin);
            drop(terrain_vols);
            // Add static colliders (any node flagged "Collidable", plus legacy mesh
            // colliders) so a character can walk on / bump into them, not just terrain.
            self.add_static_colliders(&mut sim);
            self.sim = Some(sim);
            // Start play with a clean Console so you only see this run's output.
            self.console.entries.clear();
            if flushed > 0 {
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!("⏵ auto-saved {flushed} edited script(s)"),
                    None,
                );
            }
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
        let collidable = self.world.get::<floptle_core::Collidable>(e).is_some();
        let visible = self.world.get::<floptle_core::Visible>(e).map(|v| v.0).unwrap_or(true);
        let cast_shadow =
            self.world.get::<floptle_core::CastShadow>(e).map(|c| c.0).unwrap_or(true);
        let anim_controller =
            self.world.get::<floptle_core::AnimController>(e).map(|c| c.asset.clone());
        Some(NodeDoc {
            name,
            transform,
            matter: MatterDoc::from(matter),
            scripts,
            material,
            rigidbody,
            mesh_collider,
            collidable,
            visible,
            cast_shadow,
            anim_controller,
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
        if node.collidable {
            self.world.insert(e, floptle_core::Collidable);
        }
        if !node.visible {
            self.world.insert(e, floptle_core::Visible(false));
        }
        if let Some(ctl) = &node.anim_controller {
            self.world.insert(e, floptle_core::AnimController { asset: ctl.clone() });
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
            collidable: false,
            visible: true,
            cast_shadow: true,
            anim_controller: None,
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
                self.mesh_registry.insert(
                    path.to_string(),
                    MeshAsset { parts, size: model.size, rig: Some(rig) },
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

        // Resolve the subject into drawable parts + a bounding radius. Rigged
        // models supply a per-part rest matrix (their parts are node-local).
        let mut parts: Vec<(MeshId, Option<TexId>)> = Vec::new();
        let mut part_mats: Option<Vec<Mat4>> = None;
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
                if let Some(rig) = a.rig.as_ref() {
                    part_mats = Some(
                        rig.part_nodes
                            .iter()
                            .map(|&n| rig.rest_world.get(n).copied().unwrap_or(Mat4::IDENTITY))
                            .collect(),
                    );
                }
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
        let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = parts
            .iter()
            .enumerate()
            .map(|(i, (m, t))| {
                let local = part_mats
                    .as_ref()
                    .and_then(|v| v.get(i))
                    .copied()
                    .unwrap_or(Mat4::IDENTITY);
                let raw = if is_mat {
                    instance_of_mat(model * local, &mat)
                } else {
                    instance_of(model * local, [1.0, 1.0, 1.0])
                };
                (*m, *t, raw)
            })
            .collect();
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
                None, // no field: previews don't receive scene shadows/AO
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
        self.ensure_cam_preview_target();
        let Some((cv, dv)) =
            self.cam_preview.as_ref().map(|p| (p.color_view.clone(), p.depth_view.clone()))
        else {
            return;
        };
        self.render_world_into(&cv, &dv, &cam, 16.0 / 9.0, elapsed);
    }

    /// Render the whole scene from `cam` (at `aspect`) into offscreen color+depth views —
    /// the shared body behind the Inspector camera preview and the split-view Game render.
    fn render_world_into(
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
        let mut blobs: Vec<(DVec3, f32, MaterialParams)> = Vec::new();
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

        let (sky_params, sky_tint, sky_rot, sky_solid) = skybox_uniforms(&self.world);
        let clear = [sky_solid[0], sky_solid[1], sky_solid[2], 1.0];
        // SDF AO from the scene's PostProcess node shades SDF matter in offscreen
        // views too (previews + the split Game viewport).
        let (_, rm_ao_params) = post_process_uniforms(&self.world);
        let terrain_mat = self.terrain_material();
        let show_blobs = self.project.matter && !blobs.is_empty();
        let rm_draw = show_blobs || !self.terrains.is_empty();
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
            };
            Self::fill_terrain_volumes(&self.terrains, &self.terrain_slots, &self.mesh_occluders, &self.occluder_slots, &self.world, &mut g, cam.world_position);
            g
        };

        if let (Some(gpu), Some(raster), Some(raymarch)) =
            (self.gpu.as_ref(), self.raster.as_mut(), self.raymarch.as_mut())
        {
            let raster_clear = if rm_draw {
                raymarch.draw_into(gpu, color, depth, rm);
                None
            } else {
                // Nothing to raymarch, but the raster field group still needs this
                // frame's shadow/proxy data (mesh-only scenes cast via proxies).
                raymarch.upload_globals(gpu, rm);
                Some(clear.map(|c| c as f64))
            };
            raster.draw_scene(
                gpu, color, depth, globals, &instances, raster_clear,
                Some(raymarch.field_bind()),
            );
        }
    }

    /// Lazily (re)create the Game viewport's offscreen target at `w`×`h` pixels, freeing
    /// the previous egui texture registration on resize.
    fn ensure_game_vp(&mut self, w: u32, h: u32) {
        let (w, h) = (w.max(16), h.max(16));
        if self.game_vp.is_some() && self.game_vp_dims == (w, h) {
            return;
        }
        let (Some(gpu), Some(egui)) = (self.gpu.as_ref(), self.egui.as_mut()) else { return };
        if let Some(old) = self.game_vp.take() {
            egui.renderer.free_texture(&old.tex_id);
        }
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
            "game-vp-color",
        );
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        // TEXTURE_BINDING so the viewport's SSAO pass can sample its depth.
        let depth = make(
            Gpu::DEPTH_FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            "game-vp-depth",
        );
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let tex_id =
            egui.renderer.register_native_texture(&gpu.device, &color_view, wgpu::FilterMode::Linear);
        self.game_vp = Some(PreviewTarget { color_view, depth_view, tex_id });
        self.game_vp_dims = (w, h);
        // The viewport's own post chain, sized to match.
        match self.game_post.as_mut() {
            Some(p) => p.resize(gpu, w, h),
            None => self.game_post = Some(floptle_render::PostStack::new(gpu, w, h)),
        }
    }

    /// When the Scene and Game tabs are both visible (split), render the active-camera
    /// "game" view into its own offscreen target so the two viewports show independent
    /// views instead of the same surface render. (In single-view, the surface path draws
    /// whichever one view is shown — this is skipped.)
    fn update_game_viewport(&mut self, elapsed: f32) {
        let split = self.fullscreen_tab.is_none()
            && self.dock_state.as_ref().is_some_and(scene_and_game_split);
        if !split {
            return;
        }
        let ppp = self.egui.as_ref().map(|e| e.ctx.pixels_per_point()).unwrap_or(1.0);
        let (w, h) = match self.game_rect {
            Some(r) => ((r.width() * ppp).round() as u32, (r.height() * ppp).round() as u32),
            None => (640, 360),
        };
        self.ensure_game_vp(w, h);
        // The active gameplay camera, or the editor camera if the scene has none.
        let cam = {
            let active = self.world.query::<Matter>().find_map(|(e, m)| {
                matches!(m, Matter::Camera { active: true, .. }).then_some(e)
            });
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
        let aspect = w.max(1) as f32 / h.max(1) as f32;
        let Some((cv, dv)) =
            self.game_vp.as_ref().map(|p| (p.color_view.clone(), p.depth_view.clone()))
        else {
            return;
        };
        // The scene's PostProcess node applies here too: render into the viewport's
        // own PostStack input, then run the chain (SSAO reads this viewport's depth)
        // into the egui-registered color target.
        let (post_settings, _) = post_process_uniforms(&self.world);
        if post_settings.any() && self.game_post.is_some() {
            let input = self.game_post.as_ref().map(|p| p.input_view().clone()).unwrap();
            self.render_world_into(&input, &dv, &cam, aspect, elapsed);
            if let (Some(gpu), Some(post)) = (self.gpu.as_ref(), self.game_post.as_ref()) {
                let proj = cam.proj_matrix(aspect);
                let ssao_frame = floptle_render::SsaoFrame {
                    depth: &dv,
                    proj: proj.to_cols_array_2d(),
                    inv_proj: proj.inverse().to_cols_array_2d(),
                };
                post.run(gpu, &post_settings, Some(&ssao_frame), &cv);
            }
        } else {
            self.render_world_into(&cv, &dv, &cam, aspect, elapsed);
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
                collidable: false,
                visible: true,
                cast_shadow: true,
                anim_controller: None,
                parent: None,
            };
            let e = self.spawn_node(&node);
            self.select_single(e);
        } else if is_script(path) {
            self.attach_script_file(path, self.primary());
        }
    }
    /// A script's declared `defaults`, cached by file mtime so we only re-parse the Lua
    /// when the file actually changes (keeps the per-frame inspector sync cheap).
    fn cached_script_defaults(&mut self, name: &str) -> Vec<(String, f32)> {
        let path = self.project_root.join("scripts").join(format!("{name}.lua"));
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let key = name.to_string();
        if let (Some(mt), Some((cached_mt, vals))) = (mtime, self.script_defaults_cache.get(&key)) {
            if *cached_mt == mt {
                return vals.clone();
            }
        }
        let vals = self.script_host.script_defaults(&path);
        if let Some(mt) = mtime {
            self.script_defaults_cache.insert(key, (mt, vals.clone()));
        }
        vals
    }

    /// Keep the selected node's script `params` in step with each script's current
    /// `defaults`, so editing a script (adding/removing/renaming a `defaults` key)
    /// is reflected live in the Inspector: new defaults appear as tweakable params,
    /// keys removed from `defaults` drop off, and the user's overridden values for
    /// keys that still exist are preserved. Display-only (the runtime already merges
    /// defaults at call time) and not recorded as an undo step.
    fn sync_selected_script_params(&mut self) {
        let Some(e) = self.selection.last().copied() else { return };
        let names: Vec<String> = match self.world.get::<Scripts>(e) {
            Some(s) => s.0.iter().map(|i| i.kind.clone()).collect(),
            None => return,
        };
        // Resolve each script's current defaults first (needs &mut self for the cache).
        let defaults: Vec<Vec<(String, f32)>> =
            names.iter().map(|n| self.cached_script_defaults(n)).collect();
        let Some(scr) = self.world.get_mut::<Scripts>(e) else { return };
        for (inst, defs) in scr.0.iter_mut().zip(defaults) {
            // An empty result means "no defaults declared" OR a transient parse error
            // (e.g. mid-edit) — never wipe the user's overrides in that case.
            if defs.is_empty() {
                continue;
            }
            // Drop params no longer declared in defaults.
            inst.params.retain(|(k, _)| defs.iter().any(|(dk, _)| dk == k));
            // Add any newly-declared defaults (preserving the order defaults come in).
            for (dk, dv) in &defs {
                if !inst.params.iter().any(|(k, _)| k == dk) {
                    inst.params.push((dk.clone(), *dv));
                }
            }
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
        let mut targets = self.selected_matter();
        // The PostProcess node is mandatory — every scene has exactly one. Disable
        // the chain with its `enabled` switch instead of deleting the node.
        let n = targets.len();
        targets.retain(|&e| !matches!(self.world.get::<Matter>(e), Some(Matter::PostProcess { .. })));
        if targets.len() != n {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "Post Processing is a mandatory scene node and can't be deleted — untick 'enabled' on it to turn post-processing off".into(),
                None,
            );
        }
        if targets.is_empty() {
            return;
        }
        self.record();
        for e in targets {
            if self.terrains.remove(&e).is_some() {
                if self.active_terrain == Some(e) {
                    self.active_terrain = None;
                }
                self.terrain_gpu_dirty = true;
            }
            self.world.despawn(e);
        }
        self.selection.clear();
        self.grabbed = None;
        self.drag = None;
    }
    /// Selected entities minus the PostProcess node — a scene has exactly one, so
    /// copy/duplicate never clone it (copy its VALUES via the Type header instead).
    fn selected_matter_duplicable(&self) -> Vec<Entity> {
        let mut v = self.selected_matter();
        v.retain(|&e| !matches!(self.world.get::<Matter>(e), Some(Matter::PostProcess { .. })));
        v
    }
    fn copy_selected(&mut self) {
        let nodes: Vec<NodeDoc> =
            self.selected_matter_duplicable().iter().filter_map(|&e| self.node_of(e)).collect();
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
            self.selected_matter_duplicable().iter().filter_map(|&e| self.node_of(e)).collect();
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

    /// True when the Game viewport is the FOCUSED viewport — it renders the active-camera
    /// "as a build" view, so editor interactions (pick/select, sculpt, gizmos, editor
    /// keybinds + free-fly camera) are suppressed there; only the game's own inputs run.
    /// When the Scene and Game tabs are split (both visible), focus follows the pointer:
    /// the game is focused only while the mouse is over its viewport, so you can still
    /// edit in the Scene view and the game only gets input when you're in it.
    fn game_view(&self) -> bool {
        match self.fullscreen_tab {
            Some(EditorTab::Game) => return true,
            Some(_) => return false,
            None => {}
        }
        let Some(dock) = self.dock_state.as_ref() else { return false };
        if scene_and_game_split(dock) {
            return self
                .egui
                .as_ref()
                .is_some_and(|e| scene_hit(&e.ctx, self.cursor, self.game_rect));
        }
        game_tab_active(dock)
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
                | Matter::GravityVolume { .. }
                | Matter::Skybox { .. }
                | Matter::PostProcess { .. } => None,
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
            let is_paint = matches!(brush.mode, floptle_field::Brush::Paint);
            // Growing the bounds reallocates the grid (dims change) → must take the full
            // path. `resized` is checked below to decide partial vs full.
            let resized = if !is_paint { terrain.ensure_contains(hit, brush.radius * 1.5) } else { false };
            // Apply the brush; collect the voxel sub-box it actually changed (paint =
            // its brush box; sculpt = the box of cells whose distance moved).
            let region = match brush.mode {
                floptle_field::Brush::Paint if brush.tex_slot >= 0 => {
                    terrain.paint_texture(hit, brush.radius, brush.tex_slot as u8 + 1);
                    Some(terrain.brush_range(hit, brush.radius))
                }
                floptle_field::Brush::Paint => {
                    terrain.paint(hit, brush.radius, brush.strength, brush.color);
                    Some(terrain.brush_range(hit, brush.radius))
                }
                m => terrain.sculpt(m, hit, brush.radius, brush.strength),
            };
            self.stroke_dabbed = true; // mark this stroke as worth an undo step
            // Fast path: a single terrain that didn't resize uploads only the dabbed box
            // (no full re-clone + re-upload — that's the paint/sculpt lag). A resize, an
            // empty change, or multiple terrains fall back to a full rebuild.
            match region {
                Some([mn, mx]) if self.terrains.len() == 1 && !resized => {
                    let hi = [mx[0] + 1, mx[1] + 1, mx[2] + 1];
                    let geom = !is_paint; // sculpt changes geometry (resync wireframe + collider)
                    self.terrain_region_dirty = Some(match self.terrain_region_dirty {
                        Some((e, omn, omx, og)) if e == active => (
                            active,
                            [omn[0].min(mn[0]), omn[1].min(mn[1]), omn[2].min(mn[2])],
                            [omx[0].max(hi[0]), omx[1].max(hi[1]), omx[2].max(hi[2])],
                            og || geom,
                        ),
                        _ => (active, mn, hi, geom),
                    });
                }
                _ => self.terrain_gpu_dirty = true,
            }
        }
    }

    /// Voxel dims for the current detail setting over the terrain box (≈2:1:2).
    fn terrain_dims(&self) -> [u32; 3] {
        let d = self.terrain_detail.clamp(24, 192);
        [d, (d * 3 / 8).max(8), d]
    }

    /// Create a fresh flat terrain as a NEW scene node (you can have any number). It
    /// is placed at the cursor's ground point so multiple terrains can be laid out
    /// and blended; its field is centered in the node's local space. `cfg` (from the
    /// "New terrain" dialog) sizes the flat slab and paints it with a color/texture
    /// up front — a flat field renders exactly right at any voxel density (trilinear
    /// interpolation of a plane is exact), so a huge open field is just as clean as a
    /// tiny patch; `terrain_dims()`/detail only matters once you start sculpting bumps.
    fn create_terrain(&mut self, cfg: &NewTerrainCfg) {
        self.record();
        let id = self.next_terrain_id;
        self.next_terrain_id += 1;
        let pos = self.cursor_world();
        let half_xz = cfg.size_xz.max(0.1) * 0.5;
        let half_y = cfg.thickness.max(0.1) * 0.5;
        let mut field = floptle_field::Terrain::flat(
            self.terrain_dims(),
            [0.0, 0.0, 0.0],
            [half_xz, half_y, half_xz],
            0.0,
            cfg.color,
        );
        if let Some(slot) = self.ensure_texture_slot(&cfg.texture) {
            field.fill_texture(slot + 1);
        }
        let e = self.world.spawn();
        self.world.insert(e, Transform { translation: pos, ..Transform::IDENTITY });
        let n = self.terrains.len() + 1;
        self.world.insert(e, Name(format!("Terrain {n}")));
        self.world.insert(e, Matter::Terrain { id });
        self.terrains.insert(e, field);
        self.active_terrain = Some(e);
        self.terrain_gpu_dirty = true;
        self.select_single(e);
    }

    /// Resolve a texture asset path to a terrain-palette slot (0-based), assigning it
    /// to the first empty slot if it isn't already in the palette. `None` for an empty
    /// path (no texture wanted) or a full palette with no matching existing slot.
    fn ensure_texture_slot(&mut self, path: &str) -> Option<u8> {
        if path.is_empty() {
            return None;
        }
        if let Some(i) = self.terrain_textures.iter().position(|p| p == path) {
            return Some(i as u8);
        }
        let i = self.terrain_textures.iter().position(|p| p.is_empty())?;
        self.terrain_textures[i] = path.to_string();
        self.terrain_textures_dirty = true;
        Some(i as u8)
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
        self.terrain_slots.clear();
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
        self.terrain_gpu_dirty = !self.terrains.is_empty();
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

    /// Fill the raymarch globals' per-volume slots: each uploaded terrain's box,
    /// composed anchor (node f64 translation) + local center FIRST, then
    /// camera-relative — exact at any world distance (ADR-0015). Each volume samples
    /// its own atlas slot at native resolution; overlapping volumes fuse on the GPU
    /// with the same smin the old CPU combine used (k = 0.6).
    /// (Associated fn taking explicit fields — callers sit inside the render section
    /// where `self.gpu`/`self.egui` are mutably borrowed, so `&self` is unavailable.)
    fn fill_terrain_volumes(
        terrains: &HashMap<Entity, floptle_field::Terrain>,
        slots: &[Entity],
        occluders: &HashMap<Entity, (OccKey, std::sync::Arc<floptle_field::BakedSdf>)>,
        occ_slots: &[Entity],
        world: &floptle_core::World,
        g: &mut RaymarchGlobals,
        cam_world: DVec3,
    ) {
        g.params[2] = 0.1; // blob↔terrain blend k (the old single-field look)
        for (i, &e) in slots.iter().take(floptle_render::MAX_VOLUMES).enumerate() {
            // A just-deleted terrain leaves a stale slot for one frame — leave it
            // absent (w = 0); the dirty flag re-uploads the set next frame.
            let Some(t) = terrains.get(&e) else { continue };
            let anchor = floptle_core::world_transform(world, e).translation;
            let bc = t.baked.center;
            let hf = t.baked.half_extent;
            let cr = anchor + DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - cam_world;
            g.vol_center[i] = [cr.x as f32, cr.y as f32, cr.z as f32, 1.0];
            g.vol_half[i] = [hf[0], hf[1], hf[2], 0.6];
        }
        // Mesh shadow occluders ride the slots AFTER the terrains, flagged
        // shadow-only (w = 2): the shadow march folds them in, the drawn field
        // skips them. Per-node "casts shadows" / visibility opt-outs simply leave
        // the slot absent this frame — no re-upload needed to toggle.
        for (j, &e) in occ_slots.iter().enumerate() {
            let i = slots.len() + j;
            if i >= floptle_render::MAX_VOLUMES {
                break;
            }
            let Some((_, b)) = occluders.get(&e) else { continue };
            let casts = world.get::<floptle_core::CastShadow>(e).map(|c| c.0).unwrap_or(true)
                && !matches!(
                    world.get::<floptle_core::Visible>(e),
                    Some(floptle_core::Visible(false))
                );
            if !casts {
                continue;
            }
            let anchor = floptle_core::world_transform(world, e).translation;
            let bc = b.center;
            let hf = b.half_extent;
            let cr = anchor + DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - cam_world;
            g.vol_center[i] = [cr.x as f32, cr.y as f32, cr.z as f32, 2.0];
            g.vol_half[i] = [hf[0], hf[1], hf[2], 0.0];
        }
    }

    /// The surface [`Material`] that drives terrain shading. Terrain uses the same
    /// lighting model as the meshes, so this picks whose lighting params (ambient,
    /// specular/reflectiveness, rim, emissive, unlit, color tint) every terrain
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
    fn open_scene_file(&mut self, path: &str) {
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
        self.selected_asset = Some(p.clone());
        // Immediately prompt for the name: open the naming modal with an empty field (the
        // ".lua" suffix is fixed), so you just type a name and press Enter. Cancel keeps the
        // default "script.lua".
        self.rename_target = Some((p, String::new()));
    }

    /// Rename a file/folder to `new_name` within its current parent directory. If the
    /// typed name has no extension, the original file's extension is kept (so naming a new
    /// `.lua` script "player" yields "player.lua", and a rename can't drop the extension).
    fn rename_asset(&mut self, from: &str, new_name: &str) {
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
        collidable: false,
        visible: true,
        cast_shadow: true,
        anim_controller: None,
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
                collidable: false,
                visible: true,
                cast_shadow: true,
                anim_controller: None,
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
                collidable: false,
                visible: true,
                cast_shadow: true,
                anim_controller: None,
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
                collidable: false,
                visible: true,
                cast_shadow: true,
                anim_controller: None,
                parent: None,
            },
            default_camera_node(),
        ],
    }
}

